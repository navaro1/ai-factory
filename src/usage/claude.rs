//! The claude usage probe: auth status, credentials read, OAuth usage parse.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::config::Harness;
use crate::exec::Exec;

use super::{
    curl_get, unix_seconds_to_ms, utilization_to_percent, OrgSpend, UsageMode, UsageRecord,
    UsageWindow,
};

/// The OAuth usage endpoint of the Anthropic account API.
const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

/// The organization cost report endpoint of the Anthropic Admin API.
const COST_REPORT_ENDPOINT: &str = "https://api.anthropic.com/api/organizations/cost_report";

/// The window keys of the usage response with their human labels, in order.
const WINDOW_KEYS: [(&str, &str); 5] = [
    ("five_hour", "5 hour"),
    ("seven_day", "weekly"),
    ("seven_day_opus", "weekly opus"),
    ("seven_day_sonnet", "weekly sonnet"),
    ("extra_usage", "extra usage"),
];

/// The header that unlocks the OAuth usage endpoint.
const OAUTH_EXTRA_HEADERS: [&str; 3] = [
    "anthropic-beta: oauth-2025-04-20",
    "anthropic-version: 2023-06-01",
    "User-Agent: claude-code/2.1.223",
];

/// Probe the claude identity.
///
/// The probe runs `<program> auth status` for the billing mode, re-reads the
/// OAuth access token from `credentials_path` on every call, and reads the
/// quota windows from the OAuth usage endpoint through `curl`. An API-key
/// identity skips the usage call and reads the organization costs instead,
/// but only when the `ANTHROPIC_ADMIN_KEY` environment variable is set.
pub(crate) fn probe_claude(
    exec: &dyn Exec,
    program: &str,
    credentials_path: &Path,
    now_ms: u64,
) -> Result<UsageRecord> {
    let status = exec
        .run(program, &["auth", "status"], None)
        .context("cannot run the claude auth status command")?;
    let (mode, plan) = detect_mode(&status.stdout);
    let token = read_access_token(credentials_path)?;
    let mut record = UsageRecord {
        harness: Harness::Claude,
        mode,
        plan,
        updated_ms: now_ms,
        ..UsageRecord::default()
    };
    if record.mode == UsageMode::Api {
        record.org_spend = probe_org_spend(exec);
    } else {
        record.windows = fetch_windows(exec, &token)?;
    }
    Ok(record)
}

/// Extract the first JSON object from mixed command output.
///
/// The CLI prints human lines around the JSON object, so the scan starts at
/// the first `{` and matches the braces with string awareness. `None` means
/// the output carries no complete object.
fn first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Decide the billing mode from the parsed `auth status` output.
///
/// A subscription type always names a plan. An API-key style auth method
/// selects the direct API mode, and a subscription or OAuth style auth
/// method selects the plan mode. Everything else stays unknown.
fn detect_mode(stdout: &str) -> (UsageMode, Option<String>) {
    let Some(slice) = first_json_object(stdout) else {
        return (UsageMode::Unknown, None);
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(slice) else {
        return (UsageMode::Unknown, None);
    };
    let subscription = parsed
        .get("subscriptionType")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(plan) = subscription {
        return (UsageMode::Plan, Some(plan.to_string()));
    }
    let method = parsed
        .get("authMethod")
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase);
    match method.as_deref() {
        Some(method) if method.contains("api") || method.contains("key") => (UsageMode::Api, None),
        Some(method)
            if method.contains("oauth")
                || method.contains("subscri")
                || method.contains("claude") =>
        {
            (UsageMode::Plan, None)
        }
        _ => (UsageMode::Unknown, None),
    }
}

/// Read the OAuth access token from the credentials file.
///
/// The token value never appears in an error message.
fn read_access_token(path: &Path) -> Result<String> {
    if !path.is_file() {
        return Err(anyhow!("claude credentials file missing"));
    }
    let text = std::fs::read_to_string(path).context("cannot read the claude credentials file")?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).context("cannot parse the claude credentials file")?;
    let token = parsed
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    match token {
        Some(token) if !token.is_empty() => Ok(token),
        _ => Err(anyhow!(
            "the claude credentials file carries no access token"
        )),
    }
}

/// Fetch the OAuth usage body and parse the quota windows.
///
/// The HTTP status decides the error: a rate limit and an expired token
/// carry their own short messages, and every other failure names the status
/// without echoing the body.
fn fetch_windows(exec: &dyn Exec, token: &str) -> Result<Vec<UsageWindow>> {
    let (body, status) = curl_get(exec, USAGE_ENDPOINT, token, &OAUTH_EXTRA_HEADERS)?;
    match status {
        200 => parse_windows(&body),
        429 => Err(anyhow!("rate limited")),
        401 => Err(anyhow!("credentials expired")),
        other => Err(anyhow!("the claude usage endpoint returned status {other}")),
    }
}

/// Parse the usage body into the normalized quota windows.
///
/// The community-observed shape carries the five known keys, where every
/// key is either null or an object with a `utilization` number and an
/// optional `resets_at` in Unix seconds. Unknown shapes skip their window
/// so one odd key never drops the rest of the panel.
fn parse_windows(body: &str) -> Result<Vec<UsageWindow>> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| anyhow!("cannot parse the claude usage response"))?;
    let mut windows = Vec::new();
    for (key, label) in WINDOW_KEYS {
        let Some(entry) = parsed.get(key) else {
            continue;
        };
        if entry.is_null() {
            continue;
        }
        let Some(raw) = entry.get("utilization").and_then(serde_json::Value::as_f64) else {
            continue;
        };
        let resets_at_ms = entry
            .get("resets_at")
            .and_then(serde_json::Value::as_u64)
            .map(unix_seconds_to_ms);
        windows.push(UsageWindow {
            label: label.to_string(),
            used_percent: utilization_to_percent(raw),
            resets_at_ms,
        });
    }
    Ok(windows)
}

/// Read the organization spend of the current month through the admin key.
///
/// The call fails soft: any problem leaves the record without an org spend
/// and never fails the whole probe. The key value never appears in a
/// message.
fn probe_org_spend(exec: &dyn Exec) -> Option<OrgSpend> {
    let key = std::env::var("ANTHROPIC_ADMIN_KEY")
        .ok()
        .filter(|value| !value.is_empty())?;
    let api_key_header = format!("x-api-key: {key}");
    let (body, status) =
        curl_get(exec, COST_REPORT_ENDPOINT, &key, &[api_key_header.as_str()]).ok()?;
    if status != 200 {
        return None;
    }
    parse_org_spend(&body)
}

/// Sum the `costUSD` values of the cost report rows.
///
/// Rows without a numeric cost are skipped. `None` means the body carries
/// no usable `data` array.
fn parse_org_spend(body: &str) -> Option<OrgSpend> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let rows = parsed.get("data")?.as_array()?;
    let mut total = 0.0;
    for row in rows {
        if let Some(cost) = row.get("costUSD").and_then(serde_json::Value::as_f64) {
            total += cost;
        }
    }
    Some(OrgSpend {
        label: "org this month".to_string(),
        amount_usd: total,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use serde_json::json;

    use super::*;
    use crate::exec::{CmdOut, ScriptExec};
    use crate::usage::fixture;

    const TOKEN: &str = "test-claude-access-token";
    const ADMIN_KEY: &str = "test-claude-admin-key";

    /// The tests that touch the admin key environment variable serialize on
    /// this lock, because the variable is process global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Build a unique path under the system temp directory.
    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("aif-claude-{name}-{}-{unique}", std::process::id()))
    }

    /// Write a credentials file that carries the given access token.
    fn write_credentials(token: &str) -> PathBuf {
        let path = temp_path("credentials");
        let document = json!({
            "claudeAiOauth": {
                "accessToken": token,
                "refreshToken": "test-refresh-token"
            }
        });
        std::fs::write(&path, document.to_string()).unwrap();
        path
    }

    /// Wrap a JSON object in the noise lines the CLI prints around it.
    fn auth_status_stdout(object: &str) -> String {
        format!("Signed in as op@example.com\n{object}\n")
    }

    /// The subscription style auth status output of the CLI.
    fn auth_status_oauth() -> String {
        auth_status_stdout(
            r#"{"account":{"emailAddress":"op@example.com"},"authMethod":"OAuth","subscriptionType":"max"}"#,
        )
    }

    /// The API-key style auth status output of the CLI.
    fn auth_status_api() -> String {
        auth_status_stdout(r#"{"authMethod":"api_key"}"#)
    }

    /// Script the plan flow: auth status plus a usage answer of `status`.
    fn plan_exec(body: &str, status: u16) -> ScriptExec {
        ScriptExec::new()
            .expect(
                |call| call.program == "claude" && call.argv() == ["auth", "status"],
                CmdOut::ok(auth_status_oauth()),
            )
            .expect(
                |call| call.program == "curl" && call.argv().contains(&USAGE_ENDPOINT),
                CmdOut::ok(format!("{body}\n{status}")),
            )
    }

    #[test]
    fn the_probe_scripts_the_auth_status_and_the_curl_calls_exactly() {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec(&fixture("claude-usage.json"), 200);

        let record = probe_claude(&exec, "claude", &credentials, 1_000).unwrap();

        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "claude");
        assert_eq!(calls[0].argv(), ["auth", "status"]);
        assert_eq!(calls[1].program, "curl");
        let argv = calls[1].argv();
        let auth_header = format!("Authorization: Bearer {TOKEN}");
        assert!(argv.contains(&USAGE_ENDPOINT));
        assert!(argv.contains(&auth_header.as_str()));
        assert!(argv.contains(&"anthropic-beta: oauth-2025-04-20"));
        assert!(argv.contains(&"anthropic-version: 2023-06-01"));
        assert!(argv.contains(&"User-Agent: claude-code/2.1.223"));
        assert!(argv.contains(&"-w"));
        assert!(argv.contains(&"\n%{http_code}"));
        assert_eq!(record.harness, Harness::Claude);
        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.plan.as_deref(), Some("max"));
        assert_eq!(record.updated_ms, 1_000);
        assert_eq!(record.windows.len(), 4);
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn the_fixture_parses_every_reported_window_in_order() {
        let windows = parse_windows(&fixture("claude-usage.json")).unwrap();

        let labels: Vec<&str> = windows.iter().map(|window| window.label.as_str()).collect();
        assert_eq!(labels, ["5 hour", "weekly", "weekly sonnet", "extra usage"]);
        // The five hour window reports a 0-1 share and normalizes to a
        // percentage, while the weekly window reports a plain percentage
        // and stays unchanged.
        assert_eq!(windows[0].used_percent, 42.0);
        assert_eq!(windows[0].resets_at_ms, Some(1_790_000_000_000));
        assert_eq!(windows[1].used_percent, 77.5);
        assert_eq!(windows[1].resets_at_ms, Some(1_790_500_000_000));
        assert_eq!(windows[2].used_percent, 12.0);
        assert_eq!(windows[3].used_percent, 100.0);
    }

    #[test]
    fn the_fixture_skips_the_null_window() {
        let windows = parse_windows(&fixture("claude-usage.json")).unwrap();

        assert_eq!(windows.len(), 4);
        assert!(windows.iter().all(|window| window.label != "weekly opus"));
    }

    #[test]
    fn subscription_and_oauth_auth_methods_select_the_plan_mode() {
        let (mode, plan) = detect_mode(&auth_status_oauth());
        assert_eq!(mode, UsageMode::Plan);
        assert_eq!(plan.as_deref(), Some("max"));

        let (mode, plan) = detect_mode(&auth_status_stdout(
            r#"{"authMethod":"Claude subscription"}"#,
        ));
        assert_eq!(mode, UsageMode::Plan);
        assert_eq!(plan, None);
    }

    #[test]
    fn an_api_key_auth_method_selects_the_api_mode() {
        let (mode, plan) = detect_mode(&auth_status_api());

        assert_eq!(mode, UsageMode::Api);
        assert_eq!(plan, None);
    }

    #[test]
    fn unparseable_auth_status_stays_unknown_without_an_error() {
        let (mode, plan) = detect_mode("You are not signed in.");
        assert_eq!(mode, UsageMode::Unknown);
        assert_eq!(plan, None);

        let (mode, plan) = detect_mode("{broken json");
        assert_eq!(mode, UsageMode::Unknown);
        assert_eq!(plan, None);
    }

    #[test]
    fn the_unknown_mode_still_reads_the_quota_windows() {
        let credentials = write_credentials(TOKEN);
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "claude" && call.argv() == ["auth", "status"],
                CmdOut::ok("You are not signed in.\n"),
            )
            .expect(
                |call| call.program == "curl",
                CmdOut::ok(format!("{}\n200", fixture("claude-usage.json"))),
            );

        let record = probe_claude(&exec, "claude", &credentials, 0).unwrap();

        assert_eq!(record.mode, UsageMode::Unknown);
        assert_eq!(record.plan, None);
        assert_eq!(record.windows.len(), 4);
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn a_missing_credentials_file_fails_with_the_exact_error() {
        let exec = ScriptExec::new().expect(
            |call| call.program == "claude" && call.argv() == ["auth", "status"],
            CmdOut::ok(auth_status_oauth()),
        );
        let path = temp_path("absent");

        let error = probe_claude(&exec, "claude", &path, 0).unwrap_err();

        assert_eq!(error.to_string(), "claude credentials file missing");
    }

    #[test]
    fn credentials_without_an_access_token_are_rejected() {
        let path = temp_path("no-token");
        std::fs::write(&path, r#"{"claudeAiOauth":{}}"#).unwrap();
        let exec = ScriptExec::new().expect(
            |call| call.program == "claude" && call.argv() == ["auth", "status"],
            CmdOut::ok(auth_status_oauth()),
        );

        let error = probe_claude(&exec, "claude", &path, 0).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the claude credentials file carries no access token"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_rate_limited_usage_response_maps_to_the_rate_limited_error() {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec("too many requests", 429);

        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();

        assert_eq!(error.to_string(), "rate limited");
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn an_expired_response_maps_to_the_credentials_expired_error() {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec("token rejected", 401);

        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();

        assert_eq!(error.to_string(), "credentials expired");
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn an_unexpected_status_names_the_status_without_the_body() {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec("server trouble\nsecond line", 503);

        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the claude usage endpoint returned status 503"
        );
        assert!(!error.to_string().contains("server trouble"));
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn an_unparseable_usage_body_maps_to_the_parse_error() {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec("not json at all", 200);

        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();

        assert_eq!(error.to_string(), "cannot parse the claude usage response");
        let _ = std::fs::remove_file(&credentials);
    }

    /// Run one plan probe against a scripted failing usage endpoint and
    /// return the error text.
    fn failure_text(body: &str, status: u16) -> String {
        let credentials = write_credentials(TOKEN);
        let exec = plan_exec(body, status);
        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();
        let _ = std::fs::remove_file(&credentials);
        error.to_string()
    }

    #[test]
    fn no_failure_error_ever_contains_the_access_token() {
        let missing_path = temp_path("absent");
        let missing_exec = ScriptExec::new().expect(
            |call| call.program == "claude" && call.argv() == ["auth", "status"],
            CmdOut::ok(auth_status_oauth()),
        );

        let texts = vec![
            failure_text("too many requests", 429),
            failure_text("token rejected", 401),
            failure_text("server trouble", 503),
            failure_text("not json at all", 200),
            probe_claude(&missing_exec, "claude", &missing_path, 0)
                .unwrap_err()
                .to_string(),
        ];

        for text in texts {
            assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
        }
    }

    #[test]
    fn a_curl_that_never_runs_reports_the_reason_without_the_token() {
        // The curl argument vector carries the bearer token, so a failure
        // to run curl must never carry the argument vector into the error.
        // The record error reaches the state file, the socket, and the band.
        let credentials = write_credentials(TOKEN);
        let exec = ScriptExec::new().expect(
            |call| call.program == "claude" && call.argv() == ["auth", "status"],
            CmdOut::ok(auth_status_oauth()),
        );

        let error = probe_claude(&exec, "claude", &credentials, 0).unwrap_err();

        let text = format!("{error:#}");
        let _ = std::fs::remove_file(&credentials);
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
        assert!(
            text.contains("cannot run curl"),
            "the error must name the failure: {text}"
        );
    }

    #[test]
    fn the_api_mode_skips_the_usage_call_without_an_admin_key() {
        let guard = lock_env();
        std::env::remove_var("ANTHROPIC_ADMIN_KEY");
        let credentials = write_credentials(TOKEN);
        // No curl step is scripted, so any usage call would end the probe
        // with an unexpected command error.
        let exec = ScriptExec::new().expect(
            |call| call.program == "claude" && call.argv() == ["auth", "status"],
            CmdOut::ok(auth_status_api()),
        );

        let record = probe_claude(&exec, "claude", &credentials, 5).unwrap();

        std::env::remove_var("ANTHROPIC_ADMIN_KEY");
        drop(guard);
        assert_eq!(record.mode, UsageMode::Api);
        assert!(record.windows.is_empty());
        assert!(record.org_spend.is_none());
        assert_eq!(exec.calls().len(), 1);
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn the_api_mode_adds_the_org_spend_through_the_admin_key() {
        let guard = lock_env();
        std::env::set_var("ANTHROPIC_ADMIN_KEY", ADMIN_KEY);
        let credentials = write_credentials(TOKEN);
        let body = r#"{"data":[{"costUSD":1.25,"model":"claude-opus"},{"costUSD":0.75},{"note":"no cost"}]}"#;
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "claude" && call.argv() == ["auth", "status"],
                CmdOut::ok(auth_status_api()),
            )
            .expect(
                |call| call.program == "curl" && call.argv().contains(&COST_REPORT_ENDPOINT),
                CmdOut::ok(format!("{body}\n200")),
            );

        let record = probe_claude(&exec, "claude", &credentials, 5).unwrap();

        assert_eq!(record.mode, UsageMode::Api);
        assert_eq!(record.windows.len(), 0);
        assert_eq!(
            record.org_spend,
            Some(OrgSpend {
                label: "org this month".to_string(),
                amount_usd: 2.0
            })
        );
        let calls = exec.calls();
        let argv = calls[1].argv();
        let bearer_header = format!("Authorization: Bearer {ADMIN_KEY}");
        let key_header = format!("x-api-key: {ADMIN_KEY}");
        assert!(argv.contains(&bearer_header.as_str()));
        assert!(argv.contains(&key_header.as_str()));
        std::env::remove_var("ANTHROPIC_ADMIN_KEY");
        drop(guard);
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn a_failing_org_spend_call_never_fails_the_api_probe() {
        let guard = lock_env();
        std::env::set_var("ANTHROPIC_ADMIN_KEY", ADMIN_KEY);
        let credentials = write_credentials(TOKEN);
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "claude" && call.argv() == ["auth", "status"],
                CmdOut::ok(auth_status_api()),
            )
            .expect(
                |call| call.program == "curl",
                CmdOut::ok("the cost report is down\n500"),
            );

        let record = probe_claude(&exec, "claude", &credentials, 5).unwrap();

        std::env::remove_var("ANTHROPIC_ADMIN_KEY");
        drop(guard);
        assert_eq!(record.mode, UsageMode::Api);
        assert!(record.org_spend.is_none());
        let _ = std::fs::remove_file(&credentials);
    }

    #[test]
    fn a_failing_auth_status_command_names_the_program_failure() {
        let exec = ScriptExec::new();

        let error = probe_claude(&exec, "claude", Path::new("/nonexistent"), 0).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot run the claude auth status command"));
    }
}
