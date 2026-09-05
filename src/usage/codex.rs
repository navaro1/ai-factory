//! The codex usage probe: auth mode, app-server rate limits, admin costs.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::config::Harness;
use crate::exec::Exec;

use super::{
    curl_get, unix_seconds_to_ms, window_label, Credits, OrgSpend, UsageMode, UsageRecord,
    UsageWindow,
};

/// The app-server request id of the rate limits read.
const RATE_LIMITS_ID: i64 = 2;

/// The overall conversation timeout of the production probe.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);

/// The transport that keeps the app server on stdin and stdout.
///
/// The help text calls this the default, but codex 0.153 writes nothing to
/// stdout and exits at once when the flag is absent. So the probe always
/// names the transport.
const STDIO_TRANSPORT: &str = "stdio://";

/// The billing mode that the auth file and the environment select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The ChatGPT plan with the app-server conversation.
    Plan,
    /// The direct API key with optional organization costs.
    Api,
}

/// Probe the codex identity.
///
/// The auth file decides the mode: a `tokens` object means a ChatGPT plan
/// and a JSON-RPC conversation with `<program> app-server`; an
/// `OPENAI_API_KEY` means the direct API mode with spend only.
pub(crate) fn probe_codex(
    exec: &dyn Exec,
    program: &str,
    auth_path: &Path,
    now_ms: u64,
) -> Result<UsageRecord> {
    probe_codex_with_timeout(exec, program, auth_path, now_ms, APP_SERVER_TIMEOUT)
}

/// Probe the codex identity with an injectable app-server timeout.
///
/// The tests call this function with a short timeout, so a stuck child
/// fails the probe in milliseconds instead of seconds.
fn probe_codex_with_timeout(
    exec: &dyn Exec,
    program: &str,
    auth_path: &Path,
    now_ms: u64,
    timeout: Duration,
) -> Result<UsageRecord> {
    let auth = read_auth(auth_path);
    let api_key = non_empty_env("OPENAI_API_KEY");
    let admin_key = non_empty_env("OPENAI_ADMIN_KEY");
    probe_codex_inner(
        exec,
        program,
        &auth,
        api_key.as_deref(),
        admin_key.as_deref(),
        now_ms,
        timeout,
    )
}

/// Probe the codex identity from explicit credential sources.
///
/// The environment variables stay in the wrapper above. This function takes
/// the keys as parameters, so a test never races a parallel test over the
/// process environment.
fn probe_codex_inner(
    exec: &dyn Exec,
    program: &str,
    auth: &Value,
    api_key: Option<&str>,
    admin_key: Option<&str>,
    now_ms: u64,
    timeout: Duration,
) -> Result<UsageRecord> {
    match codex_mode(auth, api_key) {
        Some(Mode::Plan) => probe_plan(program, now_ms, timeout),
        Some(Mode::Api) => Ok(probe_api(exec, now_ms, admin_key)),
        None => Err(anyhow!("no codex credentials")),
    }
}

/// The parsed auth file, or `null` when the file is missing or unreadable.
///
/// An unparsable file never carries a readable `tokens` object, so the mode
/// falls through to the API key the same way a missing file does.
fn read_auth(auth_path: &Path) -> Value {
    std::fs::read_to_string(auth_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// One environment variable, with an empty value read as unset.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The billing mode from the auth value and the API key.
///
/// A non-empty `tokens` object wins over the key, because it means the
/// ChatGPT plan.
fn codex_mode(auth: &Value, api_key: Option<&str>) -> Option<Mode> {
    let has_tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .is_some_and(|tokens| !tokens.is_empty());
    if has_tokens {
        return Some(Mode::Plan);
    }
    api_key.filter(|key| !key.is_empty()).map(|_| Mode::Api)
}

/// The plan-mode record from one app-server conversation.
fn probe_plan(program: &str, now_ms: u64, timeout: Duration) -> Result<UsageRecord> {
    let result = rate_limits_from_app_server(program, timeout)?;
    Ok(UsageRecord {
        harness: Harness::Codex,
        mode: UsageMode::Plan,
        plan: plan_name(&result),
        windows: rate_limit_windows(&result),
        credits: credits_remaining(&result).map(|remaining| Credits {
            label: "credits".to_string(),
            remaining,
        }),
        updated_ms: now_ms,
        ..UsageRecord::default()
    })
}

/// Run the app-server conversation and return the rate limits result object.
///
/// The probe spawns `<program> app-server --listen stdio://`, writes the
/// initialize request, the initialized notification, and the rate limits
/// read as JSONL lines, and reads the answer lines until the line with the
/// matching id arrives. The overall timeout covers the whole conversation:
/// on a timeout the probe kills the child and reports the reason. The child
/// never outlives this function, whatever the outcome is.
fn rate_limits_from_app_server(program: &str, timeout: Duration) -> Result<Value> {
    let mut child = Command::new(program)
        .arg("app-server")
        .arg("--listen")
        .arg(STDIO_TRANSPORT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| anyhow!("cannot start the codex app-server {program}: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("the codex app-server has no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("the codex app-server has no stdout pipe"))?;
    let deadline = Instant::now() + timeout;

    // The three protocol messages are a few hundred bytes, far below the
    // pipe buffer, so this write returns even if the child reads nothing.
    // A failed write leaves the diagnosis to the read loop below.
    let requests = format!(
        "{}\n{}\n{}\n",
        r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"aif-usage","version":"0.6.0"}}}"#,
        r#"{"method":"initialized","params":null}"#,
        r#"{"id":2,"method":"account/rateLimits/read"}"#,
    );
    let _ = stdin.write_all(requests.as_bytes());
    let _ = stdin.flush();

    // The reader forwards the answer lines to this thread. A blocking read
    // here could outlive the timeout, so the reader owns it and this thread
    // waits with a deadline instead.
    let (sender, receiver) = channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let mut answer = None;
    let mut timed_out = false;
    while answer.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(line) => answer = response_result(&line, RATE_LIMITS_ID),
            Err(RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // The app server shuts down when stdin reaches its end, and the rate
    // limits read needs a network round trip. So the pipe stays open until
    // the answer, the timeout, or the exit of the child ends the loop.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    match answer {
        Some(result) => Ok(result),
        None if timed_out => Err(anyhow!("the codex app-server did not answer in time")),
        None => Err(anyhow!("the codex app-server exited before it answered")),
    }
}

/// The `result` object of the answer line with the matching request id.
///
/// A notification or the answer of another request returns `None`.
fn response_result(line: &str, id: i64) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("id").and_then(Value::as_i64) != Some(id) {
        return None;
    }
    value.get("result").cloned()
}

/// The quota windows of the rate limits result, primary first.
///
/// A `usedPercent` stays the used share of the window, so the panel derives
/// the remaining share from it later.
fn rate_limit_windows(result: &Value) -> Vec<UsageWindow> {
    let rate_limits = result.get("rateLimits");
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|side| rate_limits.and_then(|limits| limits.get(side)))
        .filter_map(parse_window)
        .collect()
}

/// One quota window from one primary or secondary entry.
fn parse_window(window: &Value) -> Option<UsageWindow> {
    let minutes = window.get("windowDurationMins").and_then(Value::as_f64)? as u64;
    let used_percent = window.get("usedPercent").and_then(Value::as_f64)?;
    Some(UsageWindow {
        label: window_label(minutes),
        used_percent,
        resets_at_ms: window
            .get("resetsAt")
            .and_then(Value::as_u64)
            .map(unix_seconds_to_ms),
    })
}

/// One field of the result, from `rateLimits` first, then the top level.
///
/// Codex 0.153 moved `planType` and `credits` inside the `rateLimits`
/// object. The top-level read stays as the fallback for an older app
/// server. A JSON null counts as absent on both levels.
fn result_field<'a>(result: &'a Value, name: &str) -> Option<&'a Value> {
    result
        .get("rateLimits")
        .and_then(|limits| limits.get(name))
        .filter(|value| !value.is_null())
        .or_else(|| result.get(name).filter(|value| !value.is_null()))
}

/// One number that the app server reports as a JSON number or as a string.
///
/// The live `credits.balance` is a decimal string, so `as_f64` alone drops
/// it.
fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// The plan name from the `planType` string of the result.
fn plan_name(result: &Value) -> Option<String> {
    result_field(result, "planType")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(String::from)
}

/// The remaining credits of the result.
///
/// A `credits` object wins with its `balance` or `remaining` field, and the
/// reset credit count is the fallback.
fn credits_remaining(result: &Value) -> Option<f64> {
    let credits = result_field(result, "credits");
    for field in ["balance", "remaining"] {
        if let Some(value) = credits
            .and_then(|credits| credits.get(field))
            .and_then(number_value)
        {
            return Some(value);
        }
    }
    result
        .pointer("/rateLimitResetCredits/availableCount")
        .and_then(Value::as_f64)
}

/// The optional `rateLimitReachedType` flag of the result.
///
/// The record has no column for the flag, so the probe parses it but keeps
/// it out of the result until the panel grows a row for it.
#[cfg(test)]
fn rate_limit_reached_type(result: &Value) -> Option<String> {
    result_field(result, "rateLimitReachedType")
        .and_then(Value::as_str)
        .map(String::from)
}

/// The API-mode record, with the month costs when the admin key unlocks them.
///
/// A direct key has no quota endpoint, so the row stays empty unless the
/// admin costs answer arrives. A failure of the costs call only leaves the
/// spend unset; it never fails the probe.
fn probe_api(exec: &dyn Exec, now_ms: u64, admin_key: Option<&str>) -> UsageRecord {
    let mut record = UsageRecord {
        harness: Harness::Codex,
        mode: UsageMode::Api,
        updated_ms: now_ms,
        ..UsageRecord::default()
    };
    let Some(admin_key) = admin_key else {
        return record;
    };
    let url = format!(
        "https://api.openai.com/v1/organization/costs?start_time={}&end_time={}",
        first_of_month_unix(now_ms),
        now_ms / 1_000,
    );
    if let Ok((body, status)) = curl_get(exec, &url, admin_key, &[]) {
        if status == 200 {
            record.org_spend = sum_admin_cost_cents(&body).map(|cents| OrgSpend {
                label: "org this month".to_string(),
                amount_usd: cents / 100.0,
            });
        }
    }
    record
}

/// The summed `data[].value` of one admin costs answer, in cents.
///
/// `None` means the body is not a costs answer, and the spend stays unset.
fn sum_admin_cost_cents(body: &str) -> Option<f64> {
    let value: Value = serde_json::from_str(body).ok()?;
    let entries = value.get("data")?.as_array()?;
    let mut cents = 0.0;
    for entry in entries {
        cents += entry.get("value").and_then(Value::as_f64)?;
    }
    Some(cents)
}

/// The first day of the current month, in Unix seconds.
///
/// The admin costs endpoint takes a start time, and the spend row is the
/// calendar month to date. The conversion works in UTC only, like the
/// endpoint expects.
fn first_of_month_unix(now_ms: u64) -> u64 {
    let seconds = now_ms / 1_000;
    let today = seconds - seconds % 86_400;
    let days = (seconds / 86_400) as i64;
    today - (u64::from(civil_day_of_month(days)) - 1) * 86_400
}

/// The day of the month of one Unix day count, in UTC.
///
/// The arithmetic is the well-known civil-from-days conversion, kept inline
/// because the crate carries no calendar dependency.
fn civil_day_of_month(days: i64) -> u32 {
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    (doy - (153 * mp + 2) / 5 + 1) as u32
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;
    use crate::exec::{CmdOut, ScriptExec};
    use crate::usage::fixture;

    /// One fixed moment inside September 2026, so the dates stay stable.
    const NOW_MS: u64 = 1_788_572_918_000;

    /// The first day of September 2026, in Unix seconds.
    const SEPTEMBER_START_S: u64 = 1_788_220_800;

    /// The two fixture lines: the initialize answer, then the limits answer.
    fn fixture_lines() -> (String, String) {
        let text = fixture("codex-app-server.jsonl");
        let mut lines = text.lines();
        let init = lines
            .next()
            .expect("the fixture has an initialize answer")
            .to_string();
        let limits = lines
            .next()
            .expect("the fixture has a rate limits answer")
            .to_string();
        (init, limits)
    }

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aif-usage-codex-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        drop(file);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// The app server that answers both requests from the fixture and exits.
    ///
    /// The script refuses to answer without the stdio transport arguments,
    /// so the happy path also proves that the probe names the transport.
    fn happy_app_server(dir: &Path) -> PathBuf {
        let (init, limits) = fixture_lines();
        let body = r#"#!/bin/sh
case "$*" in
  "app-server --listen stdio://") ;;
  *) exit 3 ;;
esac
while IFS= read -r line; do
  case "$line" in
    *'"account/rateLimits/read"'*)
      printf '%s\n' '__LIMITS__'
      exit 0 ;;
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
  esac
done
"#
        .replace("__INIT__", &init)
        .replace("__LIMITS__", &limits);
        script(dir, "codex", &body)
    }

    /// The app server that answers the rate limits read only while stdin
    /// stays open, the way the real app server behaves.
    ///
    /// The real server shuts down at the end of stdin, and the rate limits
    /// read needs a network round trip. This script models that: it waits
    /// for more input after the read request, and it exits without an
    /// answer when that wait ends at the closed pipe.
    fn stdin_sensitive_app_server(dir: &Path) -> PathBuf {
        let (init, limits) = fixture_lines();
        let body = r#"#!/bin/bash
while IFS= read -r line; do
  case "$line" in
    *'"account/rateLimits/read"'*)
      IFS= read -r -t 1 _extra
      # bash returns 1 at the end of the pipe and above 128 on a timeout.
      if [ $? -eq 1 ]; then exit 0; fi
      printf '%s\n' '__LIMITS__'
      exit 0 ;;
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
  esac
done
"#
        .replace("__INIT__", &init)
        .replace("__LIMITS__", &limits);
        script(dir, "codex", &body)
    }

    /// The app server that answers only the initialize request and then
    /// stays alive, so a probe with a short timeout must kill it.
    fn silent_app_server(dir: &Path) -> PathBuf {
        let (init, _) = fixture_lines();
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
  esac
done
exec sleep 30
"#
        .replace("__INIT__", &init);
        script(dir, "codex", &body)
    }

    /// Probe the codex identity, retrying the transient `Text file busy`
    /// race.
    ///
    /// The test writes its fake app server and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file and fail with `Text file busy` for a few
    /// microseconds. Production never executes a file it just wrote, so the
    /// retry lives in this helper and not in the probe.
    fn probe_with_retry(
        program: &Path,
        auth_path: &Path,
        timeout: Duration,
    ) -> Result<UsageRecord> {
        for _ in 0..100 {
            let result = probe_codex_with_timeout(
                &ScriptExec::new(),
                program.to_str().unwrap(),
                auth_path,
                NOW_MS,
                timeout,
            );
            match result {
                Err(error) if error.to_string().contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return other,
            }
        }
        panic!("the fake app server did not start after 100 attempts");
    }

    /// An auth value with a non-empty `tokens` object.
    fn plan_auth() -> Value {
        json!({"tokens": {"id_token": "head.tail", "access_token": "secret"}})
    }

    /// An auth file in `dir` with the JSON value as its content.
    fn write_auth_file(dir: &Path, value: &Value) -> PathBuf {
        let path = dir.join("auth.json");
        fs::write(&path, serde_json::to_string(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn codex_mode_prefers_the_plan_tokens_over_the_api_key() {
        assert_eq!(codex_mode(&plan_auth(), None), Some(Mode::Plan));
        assert_eq!(codex_mode(&plan_auth(), Some("sk-key")), Some(Mode::Plan));
    }

    #[test]
    fn codex_mode_takes_a_non_empty_api_key_without_tokens() {
        assert_eq!(codex_mode(&json!({}), Some("sk-key")), Some(Mode::Api));
        assert_eq!(
            codex_mode(&json!({"tokens": {}}), Some("sk-key")),
            Some(Mode::Api)
        );
        assert_eq!(codex_mode(&json!({}), Some("")), None);
        assert_eq!(codex_mode(&json!({}), None), None);
        assert_eq!(codex_mode(&Value::Null, None), None);
    }

    #[test]
    fn without_any_credential_source_the_probe_reports_the_reason() {
        let exec = ScriptExec::new();

        let error = probe_codex_inner(
            &exec,
            "unused",
            &json!({}),
            None,
            None,
            NOW_MS,
            Duration::from_secs(1),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "no codex credentials");
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn the_plan_conversation_parses_windows_plan_and_credits() {
        let dir = temp_dir("plan");
        let program = happy_app_server(&dir);
        let auth_path = write_auth_file(&dir, &plan_auth());

        let record = probe_with_retry(&program, &auth_path, Duration::from_secs(2)).unwrap();

        assert_eq!(record.harness, Harness::Codex);
        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.plan.as_deref(), Some("pro"));
        assert_eq!(record.updated_ms, NOW_MS);
        assert_eq!(record.windows.len(), 2);
        assert_eq!(record.windows[0].label, "5 hour");
        // The used share stays the used share: the panel derives the
        // remaining share from it.
        assert_eq!(record.windows[0].used_percent, 12.5);
        assert_eq!(record.windows[0].resets_at_ms, Some(1_788_307_200_000));
        assert_eq!(record.windows[1].label, "weekly");
        assert_eq!(record.windows[1].used_percent, 3.25);
        assert_eq!(record.windows[1].resets_at_ms, Some(1_788_912_000_000));
        assert_eq!(
            record.credits,
            Some(Credits {
                label: "credits".to_string(),
                remaining: 18.5
            })
        );
    }

    /// Read the rate limits of one real codex binary.
    ///
    /// The test is ignored by default and needs `AIF_CODEX_PROGRAM` to name
    /// a real codex binary with a logged-in ChatGPT plan. Run it with
    /// `AIF_CODEX_PROGRAM=/path/to/codex cargo test real_codex_rate_limits
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_codex_rate_limits() {
        let Ok(program) = std::env::var("AIF_CODEX_PROGRAM") else {
            eprintln!("skipped: set AIF_CODEX_PROGRAM to a real codex binary to run this test");
            return;
        };

        let result = rate_limits_from_app_server(&program, Duration::from_secs(30))
            .expect("the real app server answers the rate limits read");

        eprintln!("plan: {:?}", plan_name(&result));
        eprintln!("credits: {:?}", credits_remaining(&result));
        let windows = rate_limit_windows(&result);
        for window in &windows {
            eprintln!("window: {} used {}%", window.label, window.used_percent);
        }
        assert!(plan_name(&result).is_some(), "the plan name must parse");
        assert!(!windows.is_empty(), "at least one window must parse");
    }

    #[test]
    fn the_timeout_kills_the_child_and_reports_the_reason() {
        let dir = temp_dir("timeout");
        let program = silent_app_server(&dir);
        let auth_path = write_auth_file(&dir, &plan_auth());

        let started = Instant::now();
        let error = probe_with_retry(&program, &auth_path, Duration::from_millis(100)).unwrap_err();

        assert!(
            error.to_string().contains("did not answer in time"),
            "error was: {error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_app_server_reports_the_start_failure() {
        let dir = temp_dir("missing");
        let program = dir.join("codex-not-there");
        let auth_path = write_auth_file(&dir, &plan_auth());

        let error = probe_codex_with_timeout(
            &ScriptExec::new(),
            program.to_str().unwrap(),
            &auth_path,
            NOW_MS,
            Duration::from_secs(1),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot start the codex app-server"));
    }

    #[test]
    fn the_answer_lines_parse_only_by_their_matching_id() {
        let (init, limits) = fixture_lines();

        assert_eq!(response_result(&init, 2), None);
        let init_result = response_result(&init, 1).unwrap();
        assert_eq!(
            init_result.get("codexHome").and_then(Value::as_str),
            Some("/home/agent/.codex")
        );
        assert_eq!(response_result("not json", 2), None);
        assert!(response_result(&limits, 2).is_some());
    }

    #[test]
    fn the_window_labels_come_from_the_reported_durations() {
        let result = json!({
            "rateLimits": {
                "primary": {"usedPercent": 40.0, "windowDurationMins": 45, "resetsAt": 1},
                "secondary": {"usedPercent": 5.0, "windowDurationMins": 120}
            }
        });

        let windows = rate_limit_windows(&result);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "45 minutes");
        assert_eq!(windows[0].used_percent, 40.0);
        assert_eq!(windows[0].resets_at_ms, Some(1_000));
        assert_eq!(windows[1].label, "2 hours");
        assert_eq!(windows[1].resets_at_ms, None);
    }

    #[test]
    fn the_credits_fall_back_to_the_reset_credit_count() {
        let fallback = json!({"rateLimitResetCredits": {"availableCount": 42}});
        assert_eq!(credits_remaining(&fallback), Some(42.0));
        let both = json!({
            "credits": {"remaining": 7.5},
            "rateLimitResetCredits": {"availableCount": 42}
        });
        assert_eq!(credits_remaining(&both), Some(7.5));
        assert_eq!(credits_remaining(&json!({})), None);
    }

    #[test]
    fn the_nested_plan_and_credits_win_over_the_top_level() {
        let live = json!({
            "rateLimits": {"planType": "pro", "credits": {"balance": "0"}},
            "rateLimitResetCredits": {"availableCount": 1}
        });
        assert_eq!(plan_name(&live).as_deref(), Some("pro"));
        // A zero balance is a real answer, so the reset count stays unused.
        assert_eq!(credits_remaining(&live), Some(0.0));

        let old = json!({"planType": "plus", "credits": {"balance": 4.0}});
        assert_eq!(plan_name(&old).as_deref(), Some("plus"));
        assert_eq!(credits_remaining(&old), Some(4.0));

        // A null on the nested level never hides the top-level value.
        let mixed = json!({
            "rateLimits": {"planType": null, "credits": null},
            "planType": "team",
            "credits": {"balance": 9.0}
        });
        assert_eq!(plan_name(&mixed).as_deref(), Some("team"));
        assert_eq!(credits_remaining(&mixed), Some(9.0));
    }

    #[test]
    fn a_null_secondary_window_leaves_only_the_primary_one() {
        let result = json!({
            "rateLimits": {
                "primary": {"usedPercent": 29.0, "windowDurationMins": 10080},
                "secondary": null
            }
        });

        let windows = rate_limit_windows(&result);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "weekly");
        assert_eq!(windows[0].used_percent, 29.0);
    }

    #[test]
    fn the_probe_keeps_stdin_open_until_the_answer_arrives() {
        let dir = temp_dir("stdin-open");
        let program = stdin_sensitive_app_server(&dir);
        let auth_path = write_auth_file(&dir, &plan_auth());

        let record = probe_with_retry(&program, &auth_path, Duration::from_secs(5)).unwrap();

        assert_eq!(record.plan.as_deref(), Some("pro"));
        assert_eq!(record.windows.len(), 2);
    }

    #[test]
    fn the_reached_flag_parses_when_present() {
        let result = json!({"rateLimits": {"rateLimitReachedType": "primary"}});
        assert_eq!(rate_limit_reached_type(&result).as_deref(), Some("primary"));
        let old = json!({"rateLimitReachedType": "secondary"});
        assert_eq!(rate_limit_reached_type(&old).as_deref(), Some("secondary"));
        assert_eq!(rate_limit_reached_type(&json!({})), None);
    }

    #[test]
    fn api_mode_stays_empty_without_the_admin_key() {
        let exec = ScriptExec::new();

        let record = probe_api(&exec, NOW_MS, None);

        assert_eq!(record.harness, Harness::Codex);
        assert_eq!(record.mode, UsageMode::Api);
        assert_eq!(record.updated_ms, NOW_MS);
        assert_eq!(record.org_spend, None);
        assert!(record.windows.is_empty());
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn api_mode_sums_the_month_costs_through_the_admin_key() {
        let body = r#"{"data":[{"value":1234},{"value":5678}]}"#;
        let exec = ScriptExec::new().expect(
            |call| {
                call.program == "curl"
                    && call
                        .argv()
                        .iter()
                        .any(|arg| arg.contains("organization/costs"))
                    && call
                        .argv()
                        .iter()
                        .any(|arg| arg.contains(&format!("start_time={SEPTEMBER_START_S}")))
                    && call
                        .argv()
                        .iter()
                        .any(|arg| arg.contains(&format!("end_time={}", NOW_MS / 1_000)))
            },
            CmdOut::ok(format!("{body}\n200")),
        );

        let record = probe_api(&exec, NOW_MS, Some("admin-secret"));

        let spend = record
            .org_spend
            .expect("the admin costs sum into the record");
        assert_eq!(spend.label, "org this month");
        assert_eq!(spend.amount_usd, 69.12);
        assert_eq!(record.mode, UsageMode::Api);
        assert!(record.windows.is_empty());
    }

    #[test]
    fn api_mode_keeps_the_record_when_the_costs_call_fails() {
        let exec = ScriptExec::new();

        let record = probe_api(&exec, NOW_MS, Some("admin-secret"));

        assert!(record.org_spend.is_none());
        assert_eq!(record.mode, UsageMode::Api);
    }

    #[test]
    fn the_admin_costs_body_sums_the_cents_of_every_entry() {
        assert_eq!(
            sum_admin_cost_cents(r#"{"data":[{"value":1234},{"value":5678}]}"#),
            Some(6_912.0)
        );
        assert_eq!(sum_admin_cost_cents(r#"{"data":[]}"#), Some(0.0));
        assert_eq!(sum_admin_cost_cents("not json"), None);
        assert_eq!(sum_admin_cost_cents(r#"{"data":[{"value":1},{}]}"#), None);
    }

    #[test]
    fn the_month_start_lands_on_the_first_day_in_utc() {
        assert_eq!(first_of_month_unix(NOW_MS), SEPTEMBER_START_S);
        assert_eq!(
            first_of_month_unix(SEPTEMBER_START_S * 1_000),
            SEPTEMBER_START_S
        );
    }
}
