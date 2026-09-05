//! The OpenCode usage probes: z.ai plan, Zen/Go plan, and other providers.

use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::config::Harness;
use crate::exec::Exec;

use super::{
    curl_get, unix_seconds_to_ms, utilization_to_percent, window_label, OrgSpend, UsageMode,
    UsageRecord, UsageWindow,
};

/// The modern z.ai usage endpoint.
const ZAI_USAGE_URL: &str = "https://api.z.ai/api/monitor/usage";

/// The legacy z.ai usage endpoint that still answers when the modern one is gone.
const ZAI_LEGACY_USAGE_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

/// The opencode zen/go usage endpoint.
const ZEN_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// Read the OpenCode auth token for one provider.
///
/// `OPENCODE_AUTH_CONTENT` wins over the auth file, so a test or a sandboxed
/// operator can inject the token. `None` means no entry exists. The lookup
/// prefers the `zai-coding-plan` entry, then the `opencode` entry, and then
/// the first entry of the file that carries any token key.
pub(crate) fn read_opencode_auth(auth_path: &Path) -> Result<Option<String>> {
    let content = non_empty_env("OPENCODE_AUTH_CONTENT");
    read_opencode_auth_from(content.as_deref(), auth_path)
}

/// The token source of [`read_opencode_auth`] with the environment content
/// as an explicit argument, so a test never mutates the process environment.
fn read_opencode_auth_from(content: Option<&str>, auth_path: &Path) -> Result<Option<String>> {
    let text = match content {
        Some(text) => text.to_string(),
        None => match std::fs::read_to_string(auth_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow!("cannot read the opencode auth file: {error}")),
        },
    };
    let parsed: Value =
        serde_json::from_str(&text).map_err(|_| anyhow!("cannot parse the opencode auth file"))?;
    Ok(find_opencode_token(&parsed))
}

/// The entry keys that may carry a token, in probe preference order.
const TOKEN_KEYS: [&str; 5] = ["access", "key", "token", "apikey", "api_key"];

/// Take the first non-empty token string of one provider entry.
///
/// An entry of an unknown shape simply yields `None`.
fn entry_token(entry: &Value) -> Option<String> {
    let map = entry.as_object()?;
    for key in TOKEN_KEYS {
        if let Some(token) = map.get(key).and_then(Value::as_str) {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Find one usable token in the parsed auth file.
fn find_opencode_token(parsed: &Value) -> Option<String> {
    let map = parsed.as_object()?;
    for provider in ["zai-coding-plan", "opencode"] {
        if let Some(token) = map.get(provider).and_then(entry_token) {
            return Some(token);
        }
    }
    map.values().find_map(entry_token)
}

/// One environment variable that must be set and non-empty.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Probe the `zai-coding-plan` identity through the z.ai monitor endpoint.
///
/// A 404 of the modern path falls back to the legacy quota path. A
/// `pay_as_you_go` level becomes a reason row of mode [`UsageMode::Api`],
/// not a probe failure, because the factory spend stays visible.
pub(crate) fn probe_zai(exec: &dyn Exec, token: &str, now_ms: u64) -> Result<UsageRecord> {
    let (body, status) = match curl_get(exec, ZAI_USAGE_URL, token, &[]) {
        Ok((_, 404)) => {
            let (legacy_body, legacy_status) = curl_get(exec, ZAI_LEGACY_USAGE_URL, token, &[])?;
            (legacy_body, legacy_status)
        }
        other => other?,
    };
    if status == 429 {
        return Err(anyhow!(
            "rate limited: status {status} from the z.ai usage endpoint"
        ));
    }
    if status != 200 {
        return Err(anyhow!("status {status} from the z.ai usage endpoint"));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|_| anyhow!("cannot parse the z.ai usage response"))?;
    let report = parse_zai_report(&parsed);
    if report.plan.as_deref() == Some("pay_as_you_go") {
        return Ok(UsageRecord {
            harness: Harness::Opencode,
            mode: UsageMode::Api,
            error: Some("pay as you go key: factory spend only".to_string()),
            updated_ms: now_ms,
            ..UsageRecord::default()
        });
    }
    Ok(UsageRecord {
        harness: Harness::Opencode,
        mode: UsageMode::Plan,
        plan: report.plan,
        windows: report.windows,
        updated_ms: now_ms,
        ..UsageRecord::default()
    })
}

/// One parsed z.ai usage report: the plan level and the quota windows.
struct ZaiReport {
    plan: Option<String>,
    windows: Vec<UsageWindow>,
}

/// Parse a z.ai usage report tolerantly.
///
/// Both the modern `CREDIT_LIMIT` shape and the legacy `TOKENS_LIMIT` shape
/// carry the same numeric fields, so the parser walks the whole document and
/// collects every array named `limits` plus the first non-empty `level`.
fn parse_zai_report(parsed: &Value) -> ZaiReport {
    let mut report = ZaiReport {
        plan: None,
        windows: Vec::new(),
    };
    collect_zai_fields(parsed, &mut report);
    report
}

/// Walk one JSON value and gather the level and the limit entries.
fn collect_zai_fields(value: &Value, report: &mut ZaiReport) {
    match value {
        Value::Object(map) => {
            if report.plan.is_none() {
                if let Some(level) = map
                    .get("level")
                    .and_then(Value::as_str)
                    .filter(|level| !level.is_empty())
                {
                    report.plan = Some(level.to_string());
                }
            }
            for (key, child) in map {
                if key == "limits" {
                    if let Value::Array(items) = child {
                        for item in items {
                            report.windows.push(zai_window(item));
                        }
                        continue;
                    }
                }
                collect_zai_fields(child, report);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_zai_fields(item, report);
            }
        }
        _ => {}
    }
}

/// Build one normalized window from one z.ai limit entry.
///
/// The fields are read tolerantly: the used share may arrive as
/// `percentage` or `utilization`, the reset arrives as epoch milliseconds
/// in `nextResetTime`, and the label comes from the unit and the number.
///
/// `percentage` already counts in percent units, so it only clamps. A live
/// pro-plan payload of 2026-09-05 reported `percentage: 1` and
/// `percentage: 12` for the two windows that the z.ai dashboard showed as
/// 1% used and 12% used. A 0-1 heuristic here read `1` as a full window and
/// drew `0% left`. `utilization` keeps the share heuristic, because no
/// observed payload names its scale.
fn zai_window(entry: &Value) -> UsageWindow {
    let used_percent = entry
        .get("percentage")
        .and_then(Value::as_f64)
        .map(|value| value.clamp(0.0, 100.0))
        .or_else(|| {
            entry
                .get("utilization")
                .and_then(Value::as_f64)
                .map(utilization_to_percent)
        })
        .or_else(|| {
            // No utilization field: derive the share from used and allowance.
            let used = entry.get("currentValue").and_then(Value::as_f64)?;
            let allowance = entry
                .get("usage")
                .and_then(Value::as_f64)
                .filter(|value| *value > 0.0)?;
            Some(utilization_to_percent(used / allowance))
        })
        .unwrap_or(0.0);
    let resets_at_ms = entry
        .get("nextResetTime")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .map(|value| value as u64);
    UsageWindow {
        label: zai_window_label(entry),
        used_percent,
        resets_at_ms,
    }
}

/// The unit text of one z.ai limit entry.
///
/// The modern shape names the unit, for example `TIME_LIMIT_HOUR`. The
/// legacy shape sends a numeric code instead. A live pro-plan payload of
/// 2026-09-05 sent `unit: 3` for the window that the z.ai dashboard named
/// "5-hour limit" and `unit: 6` for the window it named "Weekly limit".
/// Only these two codes are proven, so every other code stays empty and
/// keeps the old label fallback.
fn zai_unit_text(entry: &Value) -> &str {
    let unit = entry.get("unit");
    if let Some(text) = unit.and_then(Value::as_str) {
        return text;
    }
    match unit.and_then(Value::as_u64) {
        Some(3) => "hour",
        Some(6) => "week",
        _ => "",
    }
}

/// Label one z.ai window from the unit and the number of its limit entry.
///
/// A week window is `weekly`, an hour window carries its length, for
/// example `5 hour`, and a credit or token window keeps its own kind name.
fn zai_window_label(entry: &Value) -> String {
    let number = entry
        .get("number")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0);
    let unit = zai_unit_text(entry);
    let unit_lower = unit.to_ascii_lowercase();
    if unit_lower.contains("week") {
        return window_label(10_080);
    }
    if unit_lower.contains("hour") {
        return match number {
            Some(value) => format!("{} hour", count_text(value)),
            None => "hourly".to_string(),
        };
    }
    if unit_lower.contains("day") {
        return match number {
            Some(value) => format!("{} day", count_text(value)),
            None => "daily".to_string(),
        };
    }
    if unit_lower.contains("minute") {
        return match number {
            Some(value) => format!("{} minute", count_text(value)),
            None => "minutes".to_string(),
        };
    }
    // A credit or token window is not a duration: the unit text names the
    // label, and the limit type is the fallback, for example `CREDIT`.
    let type_text = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let kind = if unit.is_empty() {
        type_text.strip_suffix("_LIMIT").unwrap_or(type_text)
    } else {
        unit
    };
    match number {
        Some(value) => format!("{} {kind}", count_text(value)),
        None if kind.is_empty() => "usage window".to_string(),
        None => kind.to_string(),
    }
}

/// Render one count without a trailing `.0`.
fn count_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1_000_000.0 {
        format!("{}", value as u64)
    } else {
        format!("{value}")
    }
}

/// Probe the `opencode` identity through the Zen/Go usage endpoint.
pub(crate) fn probe_zen(exec: &dyn Exec, token: &str, now_ms: u64) -> Result<UsageRecord> {
    let (body, status) = curl_get(exec, ZEN_USAGE_URL, token, &[])?;
    if status == 403 {
        return Ok(UsageRecord {
            harness: Harness::Opencode,
            error: Some("no OpenCode Go plan".to_string()),
            updated_ms: now_ms,
            ..UsageRecord::default()
        });
    }
    if status == 429 {
        return Err(anyhow!("rate limited"));
    }
    if status != 200 {
        return Err(anyhow!(
            "status {status} from the opencode zen usage endpoint"
        ));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|_| anyhow!("cannot parse the opencode zen usage response"))?;
    Ok(UsageRecord {
        harness: Harness::Opencode,
        mode: UsageMode::Plan,
        windows: parse_zen_windows(&parsed),
        updated_ms: now_ms,
        ..UsageRecord::default()
    })
}

/// Parse the rolling, weekly, and monthly windows of the zen/go response.
///
/// A window key may be null or may lack a percent field; both cases skip the
/// window. Every percent value reads as a used share, never as a remaining
/// share.
fn parse_zen_windows(parsed: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    for (key, label) in [
        ("rolling", "rolling"),
        ("weekly", "weekly"),
        ("monthly", "monthly"),
    ] {
        let Some(map) = parsed.get(key).and_then(Value::as_object) else {
            continue;
        };
        let Some(raw) = map
            .get("usedPercent")
            .or_else(|| map.get("percent"))
            .or_else(|| map.get("utilization"))
            .and_then(Value::as_f64)
        else {
            continue;
        };
        // Direction confirmed against the CodexBar fetcher source
        // (github.com/CodeEditor/sdk-monitor analysis, 2026-09): opencode
        // zen/go reports used percent, not remaining.
        let used_percent = utilization_to_percent(raw);
        let resets_at_ms = map
            .get("resetAtMs")
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0)
            .map(|value| value as u64)
            .or_else(|| {
                map.get("resetAt")
                    .or_else(|| map.get("reset_at"))
                    .and_then(Value::as_f64)
                    .filter(|value| *value > 0.0)
                    .map(|value| unix_seconds_to_ms(value as u64))
            });
        windows.push(UsageWindow {
            label: label.to_string(),
            used_percent,
            resets_at_ms,
        });
    }
    windows
}

/// Build the row of any other OpenCode provider.
///
/// A provider without a quota endpoint still gets a record: the factory
/// spend always shows, and an admin environment variable may add the
/// organization costs.
pub(crate) fn probe_other_provider(
    exec: &dyn Exec,
    provider: &str,
    now_ms: u64,
) -> Result<UsageRecord> {
    let admin_key = match provider {
        "anthropic" => non_empty_env("ANTHROPIC_ADMIN_KEY"),
        "openai" => non_empty_env("OPENAI_ADMIN_KEY"),
        _ => None,
    };
    probe_other_provider_with_admin(exec, provider, admin_key.as_deref(), now_ms)
}

/// The row of [`probe_other_provider`] with the admin key as an argument,
/// so a test never mutates the process environment.
fn probe_other_provider_with_admin(
    exec: &dyn Exec,
    provider: &str,
    admin_key: Option<&str>,
    now_ms: u64,
) -> Result<UsageRecord> {
    let mut record = UsageRecord {
        harness: Harness::Opencode,
        mode: UsageMode::Api,
        updated_ms: now_ms,
        ..UsageRecord::default()
    };
    let Some(admin_key) = admin_key else {
        return Ok(record);
    };
    if let Some(spend) = org_spend(exec, provider, admin_key, now_ms) {
        record.org_spend = Some(spend);
    }
    Ok(record)
}

/// Ask the provider for the organization costs of the current month.
///
/// The probe fails soft: any error leaves the spend unknown and never fails
/// the row, and no key ever appears in a message.
fn org_spend(exec: &dyn Exec, provider: &str, admin_key: &str, now_ms: u64) -> Option<OrgSpend> {
    let (body, status) = match provider {
        "openai" => {
            let url = format!(
                "https://api.openai.com/v1/organization/costs?start_time={}&end_time={}",
                first_of_month_seconds(now_ms),
                now_ms / 1_000,
            );
            curl_get(exec, &url, admin_key, &[]).ok()?
        }
        "anthropic" => {
            let header = format!("x-api-key: {admin_key}");
            curl_get(
                exec,
                "https://api.anthropic.com/api/organizations/cost_report",
                admin_key,
                &[&header],
            )
            .ok()?
        }
        _ => return None,
    };
    if status != 200 {
        return None;
    }
    let parsed: Value = serde_json::from_str(&body).ok()?;
    let total = match provider {
        "openai" => sum_numeric_field(&parsed, "value")? / 100.0,
        _ => sum_numeric_field(&parsed, "costUSD")?,
    };
    Some(OrgSpend {
        label: "org this month".to_string(),
        amount_usd: total,
    })
}

/// Sum one numeric field over every entry of the top `data` array.
fn sum_numeric_field(parsed: &Value, field: &str) -> Option<f64> {
    let items = parsed.get("data")?.as_array()?;
    let mut total = 0.0;
    for item in items {
        total += item.get(field).and_then(Value::as_f64).unwrap_or(0.0);
    }
    Some(total)
}

/// The first second of the month that contains `now_ms`, in Unix seconds.
///
/// The day number of the epoch maps to a civil date with the standard
/// era-based algorithm, so the code needs no date crate.
fn first_of_month_seconds(now_ms: u64) -> u64 {
    let days = (now_ms / 1_000) as i64 / 86_400;
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_of_month = doy - (153 * mp + 2) / 5 + 1;
    ((days - day_of_month + 1) as u64) * 86_400
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmdOut, ScriptExec};
    use crate::usage::fixture;

    /// One unique scratch file in the system temp directory.
    fn temp_file(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "aif-usage-opencode-{name}-{}.json",
            std::process::id()
        ))
    }

    /// A scripted curl call that answers one URL with a body and a status.
    fn curl_exec(url: &'static str, body: &str, status: u16) -> ScriptExec {
        ScriptExec::new().expect(
            move |call| call.program == "curl" && call.argv().contains(&url),
            CmdOut::ok(format!("{body}\n{status}")),
        )
    }

    #[test]
    fn read_opencode_auth_reads_the_token_from_the_auth_file() {
        let path = temp_file("auth-file");
        std::fs::write(
            &path,
            r#"{"zai-coding-plan":{"type":"oauth","access":"file-token-123"}}"#,
        )
        .unwrap();

        let token = read_opencode_auth_from(None, &path).unwrap();

        assert_eq!(token.as_deref(), Some("file-token-123"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_opencode_auth_wrapper_reads_the_file_path() {
        let path = temp_file("auth-wrapper");
        std::fs::write(
            &path,
            r#"{"opencode":{"type":"api","key":"wrapper-token"}}"#,
        )
        .unwrap();

        let token = read_opencode_auth(&path).unwrap();

        assert_eq!(token.as_deref(), Some("wrapper-token"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_opencode_auth_prefers_the_injected_content_over_the_file() {
        let path = temp_file("auth-env");
        let content = r#"{"opencode":{"type":"api","key":"env-token-456"}}"#;

        let token = read_opencode_auth_from(Some(content), &path).unwrap();

        assert_eq!(token.as_deref(), Some("env-token-456"));
        assert!(!path.exists());
    }

    #[test]
    fn read_opencode_auth_gives_none_without_an_auth_file() {
        let path = temp_file("auth-missing");
        let _ = std::fs::remove_file(&path);

        let token = read_opencode_auth_from(None, &path).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn read_opencode_auth_accepts_every_known_token_key_of_an_entry() {
        let content = r#"{
            "zai-coding-plan": {"access": "", "key": "", "api_key": "zai-token"}
        }"#;

        let token = read_opencode_auth_from(Some(content), Path::new("/unused")).unwrap();

        assert_eq!(token.as_deref(), Some("zai-token"));
    }

    #[test]
    fn read_opencode_auth_falls_back_to_the_first_entry_with_any_token() {
        let content = r#"{
            "anthropic": {"note": "no token here"},
            "grok-code": {"apikey": "grok-token"}
        }"#;

        let token = read_opencode_auth_from(Some(content), Path::new("/unused")).unwrap();

        assert_eq!(token.as_deref(), Some("grok-token"));
    }

    #[test]
    fn read_opencode_auth_gives_none_for_an_object_without_tokens() {
        let token = read_opencode_auth_from(Some("{}"), Path::new("/unused")).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn read_opencode_auth_reports_a_parse_error_without_any_token() {
        let content = r#"{"zai-coding-plan": {"access": "leaked-token-value""#;

        let error = read_opencode_auth_from(Some(content), Path::new("/unused")).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("cannot parse the opencode auth file"));
        assert!(!message.contains("leaked-token-value"));
    }

    #[test]
    fn probe_zai_parses_the_modern_credit_limit_fixture() {
        let exec = curl_exec(ZAI_USAGE_URL, &fixture("zai-usage.json"), 200);

        let record = probe_zai(&exec, "zai-token", 1_000).unwrap();

        assert_eq!(record.harness, Harness::Opencode);
        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.plan.as_deref(), Some("GLM Coding Plan"));
        assert_eq!(record.updated_ms, 1_000);
        assert_eq!(record.windows.len(), 2);
        assert_eq!(record.windows[0].label, "5 hour");
        assert_eq!(record.windows[0].used_percent, 25.0);
        assert_eq!(record.windows[0].resets_at_ms, Some(1_788_220_800_000));
        assert_eq!(record.windows[1].label, "CREDIT");
        assert_eq!(record.windows[1].used_percent, 75.0);
        assert_eq!(record.windows[1].resets_at_ms, None);
    }

    #[test]
    fn probe_zai_falls_back_to_the_legacy_path_on_a_404() {
        let legacy_body = r#"{"data":{"level":"GLM Coding Plan","limits":[
            {"type":"TOKENS_LIMIT","usage":120000,"currentValue":30000,"percentage":25,
             "nextResetTime":1788220800000}
        ]}}"#;
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "curl" && call.argv().contains(&ZAI_USAGE_URL),
                CmdOut::ok("not found\n404"),
            )
            .expect(
                |call| call.program == "curl" && call.argv().contains(&ZAI_LEGACY_USAGE_URL),
                CmdOut::ok(format!("{legacy_body}\n200")),
            );

        let record = probe_zai(&exec, "zai-token", 2_000).unwrap();

        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.plan.as_deref(), Some("GLM Coding Plan"));
        assert_eq!(record.windows.len(), 1);
        assert_eq!(record.windows[0].label, "TOKENS");
        assert_eq!(record.windows[0].used_percent, 25.0);
        assert_eq!(record.windows[0].resets_at_ms, Some(1_788_220_800_000));

        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].argv().contains(&ZAI_USAGE_URL));
        assert!(calls[1].argv().contains(&ZAI_LEGACY_USAGE_URL));
    }

    #[test]
    fn probe_zai_names_the_status_when_both_paths_return_a_404() {
        let exec = ScriptExec::new()
            .expect(
                |call| call.argv().contains(&ZAI_USAGE_URL),
                CmdOut::ok("gone\n404"),
            )
            .expect(
                |call| call.argv().contains(&ZAI_LEGACY_USAGE_URL),
                CmdOut::ok("gone\n404"),
            );

        let error = probe_zai(&exec, "zai-token", 0).unwrap_err();

        assert!(error.to_string().contains("404"));
    }

    #[test]
    fn probe_zai_reports_rate_limiting_by_name() {
        let exec = curl_exec(ZAI_USAGE_URL, "slow down", 429);

        let error = probe_zai(&exec, "zai-secret-token", 0).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("rate limited"));
        assert!(message.contains("429"));
        assert!(!message.contains("zai-secret-token"));
    }

    #[test]
    fn probe_zai_names_the_status_on_another_failure() {
        let exec = curl_exec(ZAI_USAGE_URL, "server error", 503);

        let error = probe_zai(&exec, "zai-secret-token", 0).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("503"));
        assert!(!message.contains("zai-secret-token"));
    }

    #[test]
    fn probe_zai_reports_an_unparseable_body() {
        let exec = curl_exec(ZAI_USAGE_URL, "<html>no json</html>", 200);

        let error = probe_zai(&exec, "zai-token", 0).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot parse the z.ai usage response"));
    }

    #[test]
    fn probe_zai_turns_a_pay_as_you_go_level_into_an_api_reason_row() {
        let body = r#"{"data":{"level":"pay_as_you_go","limits":[]}}"#;
        let exec = curl_exec(ZAI_USAGE_URL, body, 200);

        let record = probe_zai(&exec, "zai-token", 3_000).unwrap();

        assert_eq!(record.mode, UsageMode::Api);
        assert_eq!(record.plan, None);
        assert_eq!(record.windows.len(), 0);
        assert_eq!(
            record.error.as_deref(),
            Some("pay as you go key: factory spend only")
        );
        assert_eq!(record.updated_ms, 3_000);
    }

    #[test]
    fn probe_zai_reads_the_percent_field_and_the_share_field() {
        let body = r#"{"data":{"limits":[
            {"type":"CREDIT_LIMIT","unit":"CREDIT","percentage":42.0},
            {"type":"CREDIT_LIMIT","unit":"CREDIT","utilization":0.9}
        ]}}"#;
        let exec = curl_exec(ZAI_USAGE_URL, body, 200);

        let record = probe_zai(&exec, "zai-token", 0).unwrap();

        assert_eq!(record.windows[0].used_percent, 42.0);
        assert_eq!(record.windows[1].used_percent, 90.0);
    }

    #[test]
    fn probe_zai_reads_the_live_pro_plan_payload() {
        // The live pro-plan payload of 2026-09-05. The dashboard showed
        // 1% used and 12% used for these two windows.
        let body = r#"{"data":{"level":"pro","limits":[
            {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":12000,
             "currentValue":45,"remaining":11954,"percentage":1,
             "nextResetTime":1788610133982},
            {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":60000,
             "currentValue":7755,"remaining":52244,"percentage":12,
             "nextResetTime":1789044510997}
        ]}}"#;
        let exec = curl_exec(ZAI_USAGE_URL, body, 200);

        let record = probe_zai(&exec, "zai-token", 0).unwrap();

        assert_eq!(record.windows.len(), 2);
        assert_eq!(record.windows[0].label, "5 hour");
        assert_eq!(record.windows[0].used_percent, 1.0);
        assert_eq!(record.windows[1].label, "weekly");
        assert_eq!(record.windows[1].used_percent, 12.0);
    }

    #[test]
    fn probe_zai_keeps_the_kind_label_for_an_unknown_numeric_unit() {
        let body = r#"{"data":{"limits":[
            {"type":"CREDIT_LIMIT","unit":9,"number":2,"percentage":30}
        ]}}"#;
        let exec = curl_exec(ZAI_USAGE_URL, body, 200);

        let record = probe_zai(&exec, "zai-token", 0).unwrap();

        assert_eq!(record.windows[0].label, "2 CREDIT");
        assert_eq!(record.windows[0].used_percent, 30.0);
    }

    #[test]
    fn probe_zai_derives_the_share_when_no_utilization_field_exists() {
        let body = r#"{"data":{"limits":[
            {"type":"CREDIT_LIMIT","usage":200,"currentValue":50}
        ]}}"#;
        let exec = curl_exec(ZAI_USAGE_URL, body, 200);

        let record = probe_zai(&exec, "zai-token", 0).unwrap();

        assert_eq!(record.windows[0].label, "CREDIT");
        assert_eq!(record.windows[0].used_percent, 25.0);
        assert_eq!(record.windows[0].resets_at_ms, None);
    }

    #[test]
    fn probe_zen_parses_the_fixture_windows_and_their_resets() {
        let exec = curl_exec(ZEN_USAGE_URL, &fixture("zen-usage.json"), 200);

        let record = probe_zen(&exec, "zen-token", 5_000).unwrap();

        assert_eq!(record.harness, Harness::Opencode);
        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.updated_ms, 5_000);
        // The null weekly window of the fixture is skipped, so two windows
        // remain. The used_percent values pin the direction: 42.5 stays
        // 42.5 used, it never flips to a remaining share.
        assert_eq!(record.windows.len(), 2);
        assert_eq!(record.windows[0].label, "rolling");
        assert_eq!(record.windows[0].used_percent, 42.5);
        assert_eq!(record.windows[0].resets_at_ms, Some(1_788_307_200_000));
        assert_eq!(record.windows[1].label, "monthly");
        assert_eq!(record.windows[1].used_percent, 30.0);
        assert_eq!(record.windows[1].resets_at_ms, Some(1_788_220_800_000));
    }

    #[test]
    fn probe_zen_turns_a_403_into_a_reason_row() {
        let exec = curl_exec(ZEN_USAGE_URL, "forbidden", 403);

        let record = probe_zen(&exec, "zen-token", 6_000).unwrap();

        assert_eq!(record.error.as_deref(), Some("no OpenCode Go plan"));
        assert_eq!(record.updated_ms, 6_000);
    }

    #[test]
    fn probe_zen_reports_rate_limiting_by_name() {
        let exec = curl_exec(ZEN_USAGE_URL, "slow down", 429);

        let error = probe_zen(&exec, "zen-secret-token", 0).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("rate limited"));
        assert!(!message.contains("zen-secret-token"));
    }

    #[test]
    fn probe_zen_names_the_status_on_another_failure() {
        let exec = curl_exec(ZEN_USAGE_URL, "boom", 503);

        let error = probe_zen(&exec, "zen-secret-token", 0).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("503"));
        assert!(!message.contains("zen-secret-token"));
    }

    #[test]
    fn probe_zen_reports_an_unparseable_body() {
        let exec = curl_exec(ZEN_USAGE_URL, "<html>no json</html>", 200);

        let error = probe_zen(&exec, "zen-token", 0).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("cannot parse the opencode zen usage response"));
        assert!(!message.contains("zen-token"));
    }

    #[test]
    fn probe_other_provider_builds_an_api_row_without_any_network_call() {
        let exec = ScriptExec::new();

        let record = probe_other_provider(&exec, "google", 7_000).unwrap();

        assert_eq!(record.harness, Harness::Opencode);
        assert_eq!(record.mode, UsageMode::Api);
        assert_eq!(record.updated_ms, 7_000);
        assert!(record.org_spend.is_none());
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn probe_other_provider_queries_the_openai_org_costs_of_the_month() {
        // 1_789_430_400_000 ms is 2026-09-15T00:00:00Z, so the month starts
        // at 1_788_220_800 s and ends at 1_789_430_400 s.
        let exec = ScriptExec::new().expect(
            |call| {
                call.program == "curl"
                    && call.argv().iter().any(|arg| {
                        arg.starts_with("https://api.openai.com/v1/organization/costs?")
                            && arg.contains("start_time=1788220800")
                            && arg.contains("end_time=1789430400")
                    })
                    && call
                        .argv()
                        .contains(&"Authorization: Bearer admin-key-openai")
            },
            CmdOut::ok("{\"data\":[{\"value\":150000},{\"value\":25000}]}\n200"),
        );

        let record = probe_other_provider_with_admin(
            &exec,
            "openai",
            Some("admin-key-openai"),
            1_789_430_400_000,
        )
        .unwrap();

        assert_eq!(record.mode, UsageMode::Api);
        let spend = record.org_spend.unwrap();
        assert_eq!(spend.label, "org this month");
        assert_eq!(spend.amount_usd, 1_750.0);
    }

    #[test]
    fn probe_other_provider_queries_the_anthropic_org_cost_report() {
        let exec = ScriptExec::new().expect(
            |call| {
                call.program == "curl"
                    && call
                        .argv()
                        .contains(&"https://api.anthropic.com/api/organizations/cost_report")
                    && call.argv().contains(&"x-api-key: admin-key-anthropic")
            },
            CmdOut::ok("{\"data\":[{\"costUSD\":1.25},{\"costUSD\":0.5}]}\n200"),
        );

        let record =
            probe_other_provider_with_admin(&exec, "anthropic", Some("admin-key-anthropic"), 1_000)
                .unwrap();

        let spend = record.org_spend.unwrap();
        assert_eq!(spend.label, "org this month");
        assert_eq!(spend.amount_usd, 1.75);
    }

    #[test]
    fn probe_other_provider_survives_a_failing_org_cost_call() {
        let exec =
            ScriptExec::new().expect(|call| call.program == "curl", CmdOut::ok("denied\n500"));

        let record =
            probe_other_provider_with_admin(&exec, "openai", Some("admin-key"), 1_000).unwrap();

        assert!(record.org_spend.is_none());
        assert_eq!(record.mode, UsageMode::Api);
    }
}
