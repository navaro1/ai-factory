//! The usage probe contract: one normalized record per billed identity.
//!
//! The daemon derives the identity set from the resolved execution roles,
//! runs at most one probe per identity at a time, and keeps the last
//! good result. A probe never blocks the event loop: the daemon spawns it on
//! a thread and applies the result when the [`Inbound::Usage`] message
//! arrives. HTTP goes through `curl` as a child process, the same pattern
//! as `gh` for GitHub, so no test ever touches the network.
//!
//! A billed identity is one account: `claude`, `codex`, or one OpenCode
//! provider id (the segment before the first `/` of a model). Plan rows
//! normalize to `used_percent` (0-100) with an optional reset time; direct
//! API rows carry spend instead of windows. The daemon never stores or logs
//! a bearer token or an API key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ExecutionRole, Harness};
use crate::exec::Exec;

pub mod claude;
pub mod codex;
pub mod opencode;

/// How the identity is billed.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageMode {
    /// A subscription plan with quota windows that reset over time.
    Plan,
    /// Direct API keys with prepaid spend and no reset window.
    Api,
    /// The probe could not tell yet.
    #[default]
    Unknown,
}

/// One quota window of one plan, normalized.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UsageWindow {
    /// The human label, for example `5 hour` or `weekly`.
    pub label: String,
    /// The used share of the window, 0-100.
    pub used_percent: f64,
    /// The next reset, in milliseconds since the Unix epoch, when known.
    #[serde(default)]
    pub resets_at_ms: Option<u64>,
}

/// One organization-level spend number, separate from the factory spend.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OrgSpend {
    /// The human label, for example `org this month`.
    pub label: String,
    /// The spend in US dollars.
    pub amount_usd: f64,
}

/// One remaining-credit balance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Credits {
    /// The human label, for example `credits`.
    pub label: String,
    /// The remaining amount.
    pub remaining: f64,
}

/// The last good usage data of one identity.
///
/// A failed probe keeps every field and only rewrites `error`, so the panel
/// keeps showing the last good windows with their age, like Claude Code
/// shows last-known usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// The harness the identity belongs to.
    pub harness: Harness,
    /// How the identity is billed.
    #[serde(default)]
    pub mode: UsageMode,
    /// The plan name, when the provider reports one.
    #[serde(default)]
    pub plan: Option<String>,
    /// The configured models that map to this identity.
    #[serde(default)]
    pub models: Vec<String>,
    /// The quota windows, empty for direct API rows.
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    /// The organization spend, when an admin key unlocked it.
    #[serde(default)]
    pub org_spend: Option<OrgSpend>,
    /// The remaining credits, when the provider reports them.
    #[serde(default)]
    pub credits: Option<Credits>,
    /// When the last good data was read, in milliseconds since the epoch.
    #[serde(default)]
    pub updated_ms: u64,
    /// The last probe error, when the newest probe failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl Default for UsageRecord {
    fn default() -> Self {
        Self {
            harness: Harness::Claude,
            mode: UsageMode::default(),
            plan: None,
            models: Vec::new(),
            windows: Vec::new(),
            org_spend: None,
            credits: None,
            updated_ms: 0,
            error: None,
        }
    }
}

/// The accumulated factory spend of one identity.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpendTotals {
    /// The total turn cost of the identity, in US dollars.
    #[serde(default)]
    pub total_usd: f64,
    /// The turn cost of each model, in US dollars.
    #[serde(default)]
    pub models: BTreeMap<String, f64>,
}

impl SpendTotals {
    /// Add one turn cost under the model that ran the turn.
    pub fn add(&mut self, model: &str, cost_usd: f64) {
        self.total_usd += cost_usd;
        *self.models.entry(model.to_string()).or_default() += cost_usd;
    }
}

/// One usage row of the state view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageView {
    /// The billed identity, for example `claude` or `zai-coding-plan`.
    pub identity: String,
    /// The harness the identity belongs to.
    pub harness: Harness,
    /// How the identity is billed.
    pub mode: UsageMode,
    /// The plan name, when known.
    #[serde(default)]
    pub plan: Option<String>,
    /// The configured models that map to this identity.
    #[serde(default)]
    pub models: Vec<String>,
    /// The quota windows, empty for direct API rows.
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    /// The accumulated factory spend of the identity, in US dollars.
    #[serde(default)]
    pub factory_spend_usd: f64,
    /// The organization spend, when known.
    #[serde(default)]
    pub org_spend: Option<OrgSpend>,
    /// The remaining credits, when known.
    #[serde(default)]
    pub credits: Option<Credits>,
    /// When the last good data was read, in milliseconds since the epoch.
    pub updated_ms: u64,
    /// The last probe error, when the newest probe failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl Default for UsageView {
    fn default() -> Self {
        Self {
            identity: String::new(),
            harness: Harness::Claude,
            mode: UsageMode::default(),
            plan: None,
            models: Vec::new(),
            windows: Vec::new(),
            factory_spend_usd: 0.0,
            org_spend: None,
            credits: None,
            updated_ms: 0,
            error: None,
        }
    }
}

impl UsageView {
    /// Build the wire row from the stored record and the live spend total.
    pub fn from_record(identity: &str, record: &UsageRecord, factory_spend_usd: f64) -> Self {
        Self {
            identity: identity.to_string(),
            harness: record.harness,
            mode: record.mode,
            plan: record.plan.clone(),
            models: record.models.clone(),
            windows: record.windows.clone(),
            factory_spend_usd,
            org_spend: record.org_spend.clone(),
            credits: record.credits.clone(),
            updated_ms: record.updated_ms,
            error: record.error.clone(),
        }
    }
}

/// One billed account the daemon probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The stable identity key: `claude`, `codex`, or the OpenCode provider
    /// segment of the model.
    pub id: String,
    /// The harness the identity belongs to.
    pub harness: Harness,
    /// The configured models that map to the identity, sorted.
    pub models: Vec<String>,
    /// The configured program of the first role that mapped here.
    pub program: String,
}

/// The billed identity of one harness and model pair.
pub fn identity_of(harness: Harness, model: &str) -> String {
    match harness {
        Harness::Claude => "claude".to_string(),
        Harness::Codex => "codex".to_string(),
        Harness::Opencode => model
            .split_once('/')
            .map_or(model, |(provider, _)| provider)
            .to_string(),
    }
}

/// The identity sort rank: claude first, then OpenCode providers, codex last.
fn harness_rank(harness: Harness) -> u8 {
    match harness {
        Harness::Claude => 0,
        Harness::Opencode => 1,
        Harness::Codex => 2,
    }
}

/// Derive the billed identity set from the resolved execution roles.
///
/// The set covers every configured global role and every repository
/// override. A theory role that the configuration omits adds nothing, and
/// a theory role never takes a repository override. The list is sorted
/// claude first, OpenCode providers second, codex last; within a harness
/// the identity keys sort alphabetically.
pub fn identities(config: &Config) -> Vec<Identity> {
    type IdentityParts = (Harness, BTreeMap<String, ()>, String);
    let mut pairs: BTreeMap<(u8, String), IdentityParts> = BTreeMap::new();
    let mut record = |harness: Harness, model: &str, program: &str| {
        let id = identity_of(harness, model);
        let entry = pairs
            .entry((harness_rank(harness), id.clone()))
            .or_insert_with(|| (harness, BTreeMap::new(), program.to_string()));
        entry.1.insert(model.to_string(), ());
    };
    for role in ExecutionRole::ALL {
        let Some(settings) = config.roles.get(&role) else {
            continue;
        };
        record(settings.harness, &settings.model, &settings.program);
    }
    for repo in config.repos.values() {
        for role in ExecutionRole::ALL {
            if !role.overridable() || !repo.role_overrides.contains_key(&role) {
                continue;
            }
            let Some(global) = config.roles.get(&role) else {
                continue;
            };
            let settings = config
                .resolved_role(Some(&repo.alias), role.table_name())
                .map_or_else(|_| global.clone(), |resolved| resolved.settings);
            record(settings.harness, &settings.model, &settings.program);
        }
    }
    pairs
        .into_iter()
        .map(|((_, id), (harness, models, program))| Identity {
            id,
            harness,
            models: models.into_keys().collect(),
            program,
        })
        .collect()
}

/// Run the probe of one identity.
///
/// The call is synchronous and may take seconds; the daemon runs it on a
/// thread. Every probe takes the scripted [`Exec`], so a test never touches
/// the network. The credential files are read under `home`, so a test
/// points the probe at a temporary home instead of the operator's own.
pub fn run_probe(
    exec: &dyn Exec,
    identity: &Identity,
    home: &Path,
    now_ms: u64,
) -> Result<UsageRecord> {
    match identity.id.as_str() {
        "claude" => claude::probe_claude(
            exec,
            &identity.program,
            &claude_credentials_path(home),
            now_ms,
        ),
        "codex" => codex::probe_codex(exec, &identity.program, &codex_auth_path(home), now_ms),
        "zai-coding-plan" => {
            let token = opencode::read_opencode_auth(&opencode_auth_path(home))?
                .ok_or_else(|| anyhow!("no opencode auth entry for zai-coding-plan"))?;
            opencode::probe_zai(exec, &token, now_ms)
        }
        "opencode" => {
            let token = opencode::read_opencode_auth(&opencode_auth_path(home))?
                .ok_or_else(|| anyhow!("no opencode auth entry for opencode"))?;
            opencode::probe_zen(exec, &token, now_ms)
        }
        provider => opencode::probe_other_provider(exec, provider, now_ms),
    }
}

/// The home directory of the operator, or an empty path without `HOME`.
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The Claude OAuth credentials file the usage probe re-reads every time.
pub(crate) fn claude_credentials_path(home: &Path) -> PathBuf {
    home.join(".claude").join(".credentials.json")
}

/// The codex auth file that decides the plan or API mode.
pub(crate) fn codex_auth_path(home: &Path) -> PathBuf {
    home.join(".codex").join("auth.json")
}

/// The OpenCode auth file that holds the provider tokens.
pub(crate) fn opencode_auth_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("opencode")
        .join("auth.json")
}

/// Run one authenticated HTTPS GET through `curl` and split body and status.
///
/// The argument vector ends with `-w '\n%{http_code}'`, so one capture
/// carries the body and the HTTP status. The token never appears in an
/// error message.
// The wave-2 probe chunks call this helper; the stubs do not yet.
#[allow(dead_code)]
pub(crate) fn curl_get(
    exec: &dyn Exec,
    url: &str,
    token: &str,
    extra_headers: &[&str],
) -> Result<(String, u16)> {
    let auth = format!("Authorization: Bearer {token}");
    let mut args: Vec<&str> = vec!["-sS", "--max-time", "20", "-H", &auth];
    for header in extra_headers {
        args.push("-H");
        args.push(header);
    }
    args.push(url);
    args.push("-w");
    args.push("\n%{http_code}");
    let output = exec
        .run("curl", &args, None)
        .context("cannot run curl for the usage probe")?;
    let Some((body, status)) = output.stdout.rsplit_once('\n') else {
        return Err(anyhow!("the usage probe response carries no status line"));
    };
    let status: u16 = status
        .trim()
        .parse()
        .map_err(|_| anyhow!("the usage probe status {status:?} is not a number"))?;
    Ok((body.to_string(), status))
}

/// Normalize a provider utilization figure to 0-100.
///
/// Providers report either a 0-1 share or a 0-100 percentage. A value in
/// `(0, 1]` reads as a share; every other value reads as a percentage and
/// clamps into range.
// The wave-2 probe chunks call this helper; the stubs do not yet.
#[allow(dead_code)]
pub(crate) fn utilization_to_percent(raw: f64) -> f64 {
    if raw > 0.0 && raw <= 1.0 {
        raw * 100.0
    } else {
        raw.clamp(0.0, 100.0)
    }
}

/// Label one quota window from the duration the server reports, in minutes.
///
/// The code never assumes a fixed minute count: known windows get their
/// human name and every other duration keeps its own length in the label.
// The wave-2 probe chunks call this helper; the stubs do not yet.
#[allow(dead_code)]
pub(crate) fn window_label(minutes: u64) -> String {
    match minutes {
        60 => "hourly".to_string(),
        300 => "5 hour".to_string(),
        10_080 => "weekly".to_string(),
        43_200 => "monthly".to_string(),
        other => {
            let days = other / 1_440;
            let hours = (other % 1_440) / 60;
            let minutes = other % 60;
            let mut label = String::new();
            for (value, unit) in [(days, "day"), (hours, "hour"), (minutes, "minute")] {
                if value == 0 {
                    continue;
                }
                if !label.is_empty() {
                    label.push(' ');
                }
                let plural = if value == 1 { "" } else { "s" };
                label.push_str(&format!("{value} {unit}{plural}"));
            }
            if label.is_empty() {
                label = "0 minutes".to_string();
            }
            label
        }
    }
}

/// Convert Unix seconds to Unix milliseconds, saturating on overflow.
// The wave-2 probe chunks call this helper; the stubs do not yet.
#[allow(dead_code)]
pub(crate) fn unix_seconds_to_ms(seconds: u64) -> u64 {
    seconds.saturating_mul(1_000)
}

/// Read one test fixture out of `src/usage/fixtures`.
// The wave-2 probe chunks call this helper; the stubs do not yet.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("usage")
            .join("fixtures")
            .join(name),
    )
    .unwrap_or_else(|error| panic!("cannot read the fixture {name}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six required roles with one repository override, as TOML.
    const OVERRIDES_TEXT: &str = r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.implement]
harness = "opencode"
model = "zai-coding-plan/glm-5.3-flash"

[stage.review]
harness = "codex"
model = "gpt-5.6-sol"

[stage.release]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.create]
harness = "opencode"
model = "openai/gpt-5.6-sol"

[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"
permission_mode = "manual"
permission_handler = "inbox"

[repo.demo]
path = "/tmp/demo"

[repo.demo.stage.review]
harness = "opencode"
model = "zai-coding-plan/glm-5.3"
"#;

    fn config_with_overrides() -> Config {
        Config::parse(OVERRIDES_TEXT).unwrap()
    }

    #[test]
    fn identity_of_uses_the_harness_and_the_provider_segment() {
        assert_eq!(identity_of(Harness::Claude, "claude-opus-5[1m]"), "claude");
        assert_eq!(identity_of(Harness::Codex, "gpt-5.6-sol"), "codex");
        assert_eq!(
            identity_of(Harness::Opencode, "zai-coding-plan/glm-5.3"),
            "zai-coding-plan"
        );
        assert_eq!(identity_of(Harness::Opencode, "grok-code"), "grok-code");
    }

    #[test]
    fn identities_cover_global_roles_and_repository_overrides_sorted() {
        let config = config_with_overrides();

        let identities = identities(&config);

        let ids: Vec<&str> = identities.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["claude", "openai", "zai-coding-plan", "codex"]);
        let claude = &identities[0];
        assert_eq!(claude.harness, Harness::Claude);
        assert_eq!(claude.models, ["claude-opus-5[1m]"]);
        assert_eq!(claude.program, "claude");
        let openai = &identities[1];
        assert_eq!(openai.models, ["openai/gpt-5.6-sol"]);
        let zai = &identities[2];
        assert_eq!(
            zai.models,
            ["zai-coding-plan/glm-5.3", "zai-coding-plan/glm-5.3-flash"]
        );
        let codex = &identities[3];
        assert_eq!(codex.harness, Harness::Codex);
        assert_eq!(codex.program, "codex");
    }

    #[test]
    fn the_optional_theory_roles_add_their_identity_only_when_configured() {
        // The two theory roles are optional global tables. A config without
        // them must still derive its identity set.
        let without = config_with_overrides();
        assert!(!without.roles.contains_key(&ExecutionRole::TheoryAudit));
        let ids: Vec<String> = identities(&without).into_iter().map(|one| one.id).collect();
        assert_eq!(ids, ["claude", "openai", "zai-coding-plan", "codex"]);

        let with = Config::parse(&format!(
            "{OVERRIDES_TEXT}\n[theory.audit]\nharness = \"opencode\"\n\
             model = \"grok/grok-5\"\n[theory.chat]\nharness = \"opencode\"\n\
             model = \"grok/grok-5\"\n"
        ))
        .unwrap();

        let identities = identities(&with);

        let grok = identities
            .iter()
            .find(|one| one.id == "grok")
            .expect("a theory role bills its own identity");
        assert_eq!(grok.harness, Harness::Opencode);
        assert_eq!(grok.models, ["grok/grok-5"]);
    }

    #[test]
    fn window_labels_come_from_the_reported_duration() {
        assert_eq!(window_label(300), "5 hour");
        assert_eq!(window_label(10_080), "weekly");
        assert_eq!(window_label(43_200), "monthly");
        assert_eq!(window_label(60), "hourly");
        assert_eq!(window_label(45), "45 minutes");
        assert_eq!(window_label(2_592), "1 day 19 hours 12 minutes");
        assert_eq!(window_label(1_440), "1 day");
        assert_eq!(window_label(120), "2 hours");
    }

    #[test]
    fn utilization_normalizes_both_scales() {
        assert_eq!(utilization_to_percent(0.42), 42.0);
        assert_eq!(utilization_to_percent(1.0), 100.0);
        assert_eq!(utilization_to_percent(42.0), 42.0);
        assert_eq!(utilization_to_percent(120.0), 100.0);
        assert_eq!(utilization_to_percent(0.0), 0.0);
    }

    #[test]
    fn curl_get_uses_one_capture_with_the_status_suffix() {
        use crate::exec::{CmdOut, ScriptExec};
        let body = r#"{"five_hour":{"utilization":0.2}}"#;
        let exec = ScriptExec::new().expect(
            |call| {
                call.program == "curl"
                    && call.argv().last() == Some(&"\n%{http_code}")
                    && call.argv().contains(&"https://example.test/usage")
                    && call.argv().contains(&"Authorization: Bearer secret-token")
            },
            CmdOut::ok(format!("{body}\n200")),
        );

        let (parsed_body, status) =
            curl_get(&exec, "https://example.test/usage", "secret-token", &[]).unwrap();

        assert_eq!(parsed_body, body);
        assert_eq!(status, 200);
        // The call argv must never carry the raw body split mistake: the
        // status is the last line, not part of the JSON body.
        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].argv().contains(&"-w"));
    }

    #[test]
    fn curl_get_reports_a_non_numeric_status_as_an_error() {
        use crate::exec::{CmdOut, ScriptExec};
        let exec = ScriptExec::new().expect(|call| call.program == "curl", CmdOut::ok("body\nNaN"));

        let error = curl_get(&exec, "https://example.test", "token", &[]).unwrap_err();

        assert!(error.to_string().contains("is not a number"));
    }

    #[test]
    fn the_probe_result_pushes_and_draws_through_the_real_seams() {
        use crate::daemon::{Daemon, Inbound};
        use crate::exec::{CmdOut, ScriptExec};
        use crate::runner::DefaultRunnerFactory;
        use std::fs;
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!(
            "aif-usage-e2e-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let credentials = dir.join(".claude").join(".credentials.json");
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"e2e-secret-token"}}"#,
        )
        .unwrap();

        // The probe reads the mode, then the usage endpoint through curl.
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "claude" && call.argv() == ["auth", "status"],
                CmdOut::ok("{\"subscriptionType\":\"max\"}"),
            )
            .expect(
                |call| {
                    call.program == "curl"
                        && call
                            .argv()
                            .contains(&"https://api.anthropic.com/api/oauth/usage")
                },
                CmdOut::ok(
                    "{\"five_hour\":{\"utilization\":0.42,\"resets_at\":6000000},\"seven_day\":null}\n200",
                ),
            );

        let text = r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.implement]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.review]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.release]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.create]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"

[usage]
enabled = true
minutes = 10
"#;
        let config = crate::config::Config::parse(text).unwrap();
        let (_poll_tx, poll_rx) = channel();
        let (wake_tx, _wake_rx) = channel();
        let mut wake = std::collections::BTreeMap::new();
        wake.insert("borsuk".to_string(), wake_tx);
        let (_action_tx, action_rx) = channel();
        let mut daemon = Daemon::with_runner_factory(
            config,
            dir.join("factory.toml"),
            String::new(),
            std::sync::Arc::new(exec),
            dir.join("state"),
            dir.join("prompts"),
            poll_rx,
            wake,
            action_rx,
            std::sync::Arc::new(DefaultRunnerFactory),
            false,
        );
        daemon.set_usage_home(dir.clone());
        let (view_tx, view_rx) = channel();
        daemon.set_pusher(Box::new(move |view| {
            let _ = view_tx.send(view);
        }));

        // The drive polls the due identity and spawns exactly one probe.
        daemon.drive();
        let usage_rx = daemon.take_usage_receiver().unwrap();
        let (identity, result) = usage_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the drive must spawn the claude probe");
        assert_eq!(identity, "claude");
        let record = result.expect("the scripted probe must succeed");
        assert_eq!(record.mode, UsageMode::Plan);
        assert_eq!(record.plan.as_deref(), Some("max"));
        assert_eq!(record.windows.len(), 1);
        assert_eq!(record.windows[0].used_percent, 42.0);
        assert_eq!(record.windows[0].label, "5 hour");

        // The daemon applies the result and pushes the usage row.
        daemon.handle(Inbound::Usage {
            identity,
            result: Ok(record),
        });
        let mut view = None;
        for _ in 0..8 {
            let pushed = view_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the drive must push a state view");
            // A view before the first result carries the placeholder row
            // with no probe time yet.
            if pushed.usage.iter().any(|row| row.updated_ms > 0) {
                view = Some(pushed);
                break;
            }
        }
        let view = view.expect("a pushed view must carry the usage rows");
        assert_eq!(view.usage.len(), 1);
        assert_eq!(view.usage[0].identity, "claude");
        assert_eq!(view.usage[0].windows[0].used_percent, 42.0);

        // The drawn band carries the probed numbers.
        let drawn = crate::tui::pipeline::render_state_board(&view, 120, 40, 5_000_000_000);
        assert!(drawn.contains("USAGE"), "board:\n{drawn}");
        assert!(drawn.contains("max plan"), "board:\n{drawn}");
        assert!(drawn.contains("58% left"), "board:\n{drawn}");
        assert!(drawn.contains("resets "), "board:\n{drawn}");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn spend_totals_accumulate_per_model_and_total() {
        let mut totals = SpendTotals::default();
        totals.add("m1", 0.5);
        totals.add("m2", 0.25);
        totals.add("m1", 0.25);

        assert_eq!(totals.total_usd, 1.0);
        assert_eq!(totals.models["m1"], 0.75);
        assert_eq!(totals.models["m2"], 0.25);
    }

    #[test]
    fn a_usage_view_round_trips_through_json() {
        let record = UsageRecord {
            harness: Harness::Opencode,
            mode: UsageMode::Plan,
            plan: Some("Pro".to_string()),
            models: vec!["zai-coding-plan/glm-5.3".to_string()],
            windows: vec![UsageWindow {
                label: "5 hour".to_string(),
                used_percent: 30.0,
                resets_at_ms: Some(1_000),
            }],
            updated_ms: 1_000,
            ..UsageRecord::default()
        };
        let view = UsageView::from_record("zai-coding-plan", &record, 1.25);
        let text = serde_json::to_string(&view).unwrap();
        assert_eq!(serde_json::from_str::<UsageView>(&text).unwrap(), view);
    }
}
