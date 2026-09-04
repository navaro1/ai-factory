//! Reads and writes `state.json`, the daemon's only private file.
//!
//! The file holds only what GitHub cannot hold. It stores runtime overrides,
//! release train times, and active ticket conversation state. GitHub holds
//! all issue content and labels. Task logs hold full chat transcripts. The
//! daemon writes the file through a temporary file and a rename. A missing or
//! corrupt file is not an error. The loader logs once and uses the defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ReleasePolicy, ResolvedRoleSettings};
use crate::decisions::{Decision, DecisionKind};
use crate::model::Stage;
use crate::sock::TicketProposal;
use crate::tasks::{Task, MAX_ATTEMPTS};
use crate::usage::{SpendTotals, UsageRecord};

/// One issue conversation that survives a daemon restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketConversationState {
    /// The repository alias.
    pub repo: String,
    /// The issue number.
    pub number: u64,
    /// The Claude session identity, when the first run started.
    #[serde(default)]
    pub session_id: Option<String>,
    /// True while the current `to-refine` label interval sent its handoff.
    #[serde(default)]
    pub handoff_active: bool,
    /// The latest valid proposal. The full transcript stays in the task log.
    #[serde(default)]
    pub proposal: Option<TicketProposal>,
}

/// One persisted lane reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaneEntry {
    /// The stage the reservation applies to.
    stage: Stage,
    /// The repository alias of the reservation.
    repo: String,
    /// The reserved slots. 0 keeps a former reservation disabled.
    slots: usize,
}

/// One persisted lane pause mark.
///
/// A JSON map cannot key on a pair, so the `(stage, repo)` lane marks are
/// entries, as [`LaneEntry`] already shows for the lane reservations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanePauseEntry {
    /// The stage the pause mark applies to.
    pub stage: Stage,
    /// The repository alias of the pause mark.
    pub repo: String,
    /// The pause state of the lane.
    pub paused: bool,
}

/// The pause marks of one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PausedState {
    /// The whole factory takes no new work.
    #[serde(default)]
    pub global: bool,
    /// Explicit states for stages, by stage name.
    #[serde(default)]
    pub stages: BTreeMap<Stage, bool>,
    /// Explicit states for repository lanes.
    #[serde(default)]
    pub lanes: Vec<LanePauseEntry>,
    /// Explicit states for tasks, by stable task id.
    #[serde(default)]
    pub tasks: BTreeMap<String, bool>,
}

/// The runtime work state of one snapshot.
///
/// The object holds what the task table and the session bookkeeping hold,
/// so a restarted daemon resumes its work instead of losing it. Every field
/// defaults to empty, so an older file loads without an error.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    /// The pause marks the operator set.
    #[serde(default)]
    pub paused: PausedState,
    /// Every task, in insertion order.
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// The chat messages that wait for their turn, by task id.
    #[serde(default)]
    pub pending_chats: BTreeMap<String, Vec<String>>,
    /// The ticket set of each review task, pinned at admit time.
    #[serde(default)]
    pub review_tickets: BTreeMap<String, BTreeSet<u64>>,
    /// The pull request batch of each release task.
    #[serde(default)]
    pub release_batches: BTreeMap<String, Vec<u64>>,
    /// The stuck rows of the tasks that gave up.
    #[serde(default)]
    pub stuck: Vec<Decision>,
    /// The last good usage record of each billed identity.
    #[serde(default)]
    pub usage: BTreeMap<String, UsageRecord>,
    /// The accumulated factory spend of each billed identity.
    #[serde(default)]
    pub spend: BTreeMap<String, SpendTotals>,
}

/// The on-disk shape of `state.json`.
///
/// Every field defaults to empty, so a file written by an older daemon and a
/// file with missing sections both load.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct StateFile {
    /// The stage limit overrides, by stage.
    #[serde(default)]
    stage_limits: BTreeMap<Stage, usize>,
    /// The lane overrides. A JSON map cannot key on a pair, so they are
    /// entries.
    #[serde(default)]
    lanes: Vec<LaneEntry>,
    /// The release policy overrides, by repository alias.
    #[serde(default)]
    policies: BTreeMap<String, ReleasePolicy>,
    /// The last fire stamp of each train, by repository alias.
    #[serde(default)]
    last_fire_ms: BTreeMap<String, u64>,
    /// Active issue conversations.
    #[serde(default)]
    ticket_conversations: Vec<TicketConversationState>,
    /// Immutable role bindings, by stable task identity.
    #[serde(default)]
    role_bindings: BTreeMap<String, ResolvedRoleSettings>,
    /// The runtime work state of the last drive.
    #[serde(default)]
    runtime: RuntimeState,
}

/// The state the daemon persists across restarts.
///
/// The override fields contain only values that differ from the config file.
/// The conversation field contains each active ticket chat.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DaemonState {
    /// The stage limit overrides, by stage.
    pub stage_limits: BTreeMap<Stage, usize>,
    /// The lane overrides as `(stage, repo, slots)` entries.
    pub lanes: Vec<(Stage, String, usize)>,
    /// The release policy overrides, by repository alias.
    pub policies: BTreeMap<String, ReleasePolicy>,
    /// The last fire stamp of each train, by repository alias.
    pub last_fire_ms: BTreeMap<String, u64>,
    /// Active issue conversations.
    pub ticket_conversations: Vec<TicketConversationState>,
    /// Immutable role bindings, by stable task identity.
    pub role_bindings: BTreeMap<String, ResolvedRoleSettings>,
    /// The runtime work state: pause marks, tasks, queued chats, review
    /// ticket sets, release batches, and stuck rows.
    pub runtime: RuntimeState,
}

impl DaemonState {
    /// Read the state file, or return empty state when it is missing or invalid.
    ///
    /// A missing or corrupt file is not an error. The call logs it once on
    /// standard error, and the daemon continues with the config defaults.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "{}: no state file; the daemon starts with the config defaults",
                    path.display()
                );
                return DaemonState::default();
            }
            Err(e) => {
                eprintln!(
                    "{}: cannot read the state file ({e}); the daemon starts with the \
                     config defaults",
                    path.display()
                );
                return DaemonState::default();
            }
        };
        match serde_json::from_str::<StateFile>(&text) {
            Ok(file) => {
                if let Some(reason) = invalid_state(&file) {
                    eprintln!(
                        "{}: corrupt state file ({reason}); the daemon starts with the config \
                         defaults",
                        path.display()
                    );
                    return DaemonState::default();
                }
                DaemonState {
                    stage_limits: file.stage_limits,
                    lanes: file
                        .lanes
                        .into_iter()
                        .map(|entry| (entry.stage, entry.repo, entry.slots))
                        .collect(),
                    policies: file.policies,
                    last_fire_ms: file.last_fire_ms,
                    ticket_conversations: file.ticket_conversations,
                    role_bindings: file.role_bindings,
                    runtime: file.runtime,
                }
            }
            Err(e) => {
                eprintln!(
                    "{}: corrupt state file ({e}); the daemon starts with the config \
                     defaults",
                    path.display()
                );
                DaemonState::default()
            }
        }
    }

    /// Serialize the state to one JSON line.
    ///
    /// The daemon compares this text against the last written text, so a
    /// drive with no change writes nothing.
    pub fn to_json(&self) -> Result<String> {
        let file = StateFile {
            stage_limits: self.stage_limits.clone(),
            lanes: self
                .lanes
                .iter()
                .map(|(stage, repo, slots)| LaneEntry {
                    stage: *stage,
                    repo: repo.clone(),
                    slots: *slots,
                })
                .collect(),
            policies: self.policies.clone(),
            last_fire_ms: self.last_fire_ms.clone(),
            ticket_conversations: self.ticket_conversations.clone(),
            role_bindings: self.role_bindings.clone(),
            runtime: self.runtime.clone(),
        };
        serde_json::to_string(&file).context("cannot serialize the daemon state")
    }

    /// Write the state through a temporary file and a rename.
    ///
    /// A crash can therefore never leave a half-written `state.json`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_json()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("cannot rename {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Return the reason that a parsed state file is invalid.
fn invalid_state(file: &StateFile) -> Option<String> {
    if let Some((stage, _)) = file.stage_limits.iter().find(|(_, limit)| **limit == 0) {
        return Some(format!("the {stage} stage limit is 0"));
    }
    for (repo, policy) in &file.policies {
        match policy {
            ReleasePolicy::Interval { minutes: 0 } => {
                return Some(format!("the interval policy for {repo} has 0 minutes"));
            }
            ReleasePolicy::Threshold { count: 0 } => {
                return Some(format!("the threshold policy for {repo} has count 0"));
            }
            ReleasePolicy::Manual
            | ReleasePolicy::Interval { .. }
            | ReleasePolicy::Threshold { .. } => {}
        }
    }
    for (task, binding) in &file.role_bindings {
        if let Err(error) = crate::config::validate_persisted_settings(
            &binding.settings,
            &format!("role binding {task}"),
        ) {
            return Some(error.to_string());
        }
    }
    let runtime = &file.runtime;
    let mut task_ids = BTreeSet::new();
    for task in &runtime.tasks {
        if !task_ids.insert(task.id.as_str()) {
            return Some(format!("two tasks carry the id {}", task.id));
        }
        if task.attempt == 0 || task.attempt > MAX_ATTEMPTS {
            return Some(format!(
                "the task {} carries the attempt {}, outside 1 to {MAX_ATTEMPTS}",
                task.id, task.attempt
            ));
        }
    }
    for id in runtime.pending_chats.keys() {
        if !task_ids.contains(id.as_str()) {
            return Some(format!("a pending chat names the unknown task {id}"));
        }
    }
    for row in &runtime.stuck {
        if let DecisionKind::Stuck { task, .. } = &row.kind {
            if !task_ids.contains(task.as_str()) {
                return Some(format!(
                    "the stuck row {} names the unknown task {task}",
                    row.id
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ExecutionRole, Harness, ResolvedRoleSettings, RoleSettings, SettingsSource,
    };
    use crate::model::ItemKind;
    use std::path::PathBuf;

    /// A unique directory for one test's files.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-state-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One queued task for the runtime round trips.
    fn task(id: &str, repo: &str, number: u64, attempt: u32) -> Task {
        let mut task = Task::new(
            repo,
            crate::model::Stage::Implement,
            ItemKind::Issue,
            number,
            PathBuf::from(format!("logs/{id}.jsonl")),
            1_000,
        );
        task.id = id.to_string();
        task.attempt = attempt;
        task
    }

    #[test]
    fn the_runtime_object_defaults_to_empty() {
        let runtime = RuntimeState::default();
        assert!(!runtime.paused.global);
        assert!(runtime.paused.stages.is_empty());
        assert!(runtime.paused.lanes.is_empty());
        assert!(runtime.paused.tasks.is_empty());
        assert!(runtime.tasks.is_empty());
        assert!(runtime.pending_chats.is_empty());
        assert!(runtime.review_tickets.is_empty());
        assert!(runtime.release_batches.is_empty());
        assert!(runtime.stuck.is_empty());
        assert_eq!(
            DaemonState::default().runtime,
            RuntimeState::default(),
            "a daemon state without runtime data carries an empty runtime object"
        );
    }

    #[test]
    fn a_v0_6_state_file_loads_with_an_empty_runtime() {
        let dir = temp_dir("v0-6-file");
        let path = dir.join("state.json");
        fs::write(
            &path,
            r#"{"stage_limits":{"refine":3},"lanes":[],"policies":{},"last_fire_ms":{}}"#,
        )
        .unwrap();

        let loaded = DaemonState::load(&path);

        assert_eq!(loaded.stage_limits.get(&Stage::Refine), Some(&3));
        assert_eq!(loaded.runtime, RuntimeState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_state_file_with_a_runtime_section_and_unknown_fields_loads() {
        let dir = temp_dir("runtime-section");
        let path = dir.join("state.json");
        // An older daemon reads the same file and ignores the `runtime`
        // field, because serde drops unknown fields. This file also carries
        // an unknown top-level field in the other direction.
        fs::write(
            &path,
            r#"{"stage_limits":{},"lanes":[],"policies":{},"last_fire_ms":{},
                "future_field":{"x":1},
                "runtime":{"paused":{"global":true},"tasks":[],"pending_chats":{},
                "review_tickets":{},"release_batches":{},"stuck":[]}}"#,
        )
        .unwrap();

        let loaded = DaemonState::load(&path);

        assert!(loaded.runtime.paused.global);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_runtime_pause_marks_round_trip_with_lane_entries() {
        let dir = temp_dir("pause-marks");
        let path = dir.join("state.json");
        let mut state = DaemonState::default();
        state.runtime.paused.global = false;
        state.runtime.paused.stages.insert(Stage::Implement, true);
        state.runtime.paused.lanes.push(LanePauseEntry {
            stage: Stage::Review,
            repo: "borsuk".to_string(),
            paused: true,
        });
        state
            .runtime
            .paused
            .tasks
            .insert("borsuk/refine-i7".to_string(), false);

        state.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""lanes":[{"stage":"review","repo":"borsuk","paused":true}]"#),
            "the lane pause marks are entries, not a map: {text}"
        );
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded.runtime.paused, state.runtime.paused);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_runtime_tasks_keep_the_insertion_order() {
        let dir = temp_dir("task-order");
        let path = dir.join("state.json");
        let first = task("borsuk/refine-i1", "borsuk", 1, 1);
        let second = task("borsuk/implement-i2", "borsuk", 2, 2);
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![first, second.clone()];

        state.save(&path).unwrap();

        let loaded = DaemonState::load(&path);
        assert_eq!(
            loaded
                .runtime
                .tasks
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["borsuk/refine-i1", "borsuk/implement-i2"]
        );
        assert_eq!(loaded.runtime.tasks[1], second);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_runtime_stuck_rows_round_trip() {
        let dir = temp_dir("stuck-rows");
        let path = dir.join("state.json");
        let failed = task("borsuk/implement-i9", "borsuk", 9, MAX_ATTEMPTS);
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![failed.clone()];
        state.runtime.stuck = vec![Decision::stuck(&failed, "boom", 2_000)];

        state.save(&path).unwrap();

        let loaded = DaemonState::load(&path);
        assert_eq!(loaded.runtime.stuck, state.runtime.stuck);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_runtime_with_two_tasks_of_one_id_discards_the_complete_state() {
        let dir = temp_dir("duplicate-task");
        let path = dir.join("state.json");
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![
            task("borsuk/implement-i1", "borsuk", 1, 1),
            task("borsuk/implement-i1", "borsuk", 1, 2),
        ];
        state.stage_limits.insert(Stage::Review, 9);

        state.save(&path).unwrap();

        assert_eq!(DaemonState::load(&path), DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_runtime_with_an_out_of_range_attempt_discards_the_complete_state() {
        for attempt in [0_u32, MAX_ATTEMPTS + 1] {
            let dir = temp_dir("bad-attempt");
            let path = dir.join("state.json");
            let mut state = DaemonState::default();
            state.runtime.tasks = vec![task("borsuk/refine-i3", "borsuk", 3, attempt)];

            state.save(&path).unwrap();

            assert_eq!(DaemonState::load(&path), DaemonState::default());
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_runtime_with_an_unknown_pending_chat_discards_the_complete_state() {
        let dir = temp_dir("unknown-chat");
        let path = dir.join("state.json");
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![task("borsuk/refine-i3", "borsuk", 3, 1)];
        state
            .runtime
            .pending_chats
            .insert("borsuk/refine-i4".to_string(), vec!["hello".to_string()]);

        state.save(&path).unwrap();

        assert_eq!(DaemonState::load(&path), DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_runtime_with_an_unknown_stuck_task_discards_the_complete_state() {
        let dir = temp_dir("unknown-stuck");
        let path = dir.join("state.json");
        let failed = task("borsuk/implement-i9", "borsuk", 9, MAX_ATTEMPTS);
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![task("borsuk/refine-i3", "borsuk", 3, 1)];
        state.runtime.stuck = vec![Decision::stuck(&failed, "boom", 2_000)];

        state.save(&path).unwrap();

        assert_eq!(DaemonState::load(&path), DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_state_survives_a_round_trip() {
        let dir = temp_dir("round-trip");
        let path = dir.join("state.json");
        let state = DaemonState {
            stage_limits: BTreeMap::from([(Stage::Implement, 9)]),
            lanes: vec![(Stage::Review, "borsuk".to_string(), 2)],
            policies: BTreeMap::from([(
                "borsuk".to_string(),
                ReleasePolicy::Interval { minutes: 5 },
            )]),
            last_fire_ms: BTreeMap::from([("borsuk".to_string(), 1_000)]),
            ticket_conversations: vec![TicketConversationState {
                repo: "borsuk".to_string(),
                number: 42,
                session_id: Some("session-42".to_string()),
                handoff_active: true,
                proposal: None,
            }],
            role_bindings: BTreeMap::new(),
            runtime: RuntimeState {
                paused: PausedState {
                    global: false,
                    stages: BTreeMap::from([(Stage::Release, true)]),
                    lanes: Vec::new(),
                    tasks: BTreeMap::new(),
                },
                tasks: vec![task("borsuk/implement-i42", "borsuk", 42, 2)],
                pending_chats: BTreeMap::from([(
                    "borsuk/implement-i42".to_string(),
                    vec!["continue".to_string()],
                )]),
                review_tickets: BTreeMap::from([(
                    "borsuk/review-p5".to_string(),
                    BTreeSet::from([42]),
                )]),
                release_batches: BTreeMap::from([("borsuk/release".to_string(), vec![5])]),
                stuck: Vec::new(),
                usage: BTreeMap::new(),
                spend: BTreeMap::new(),
            },
        };
        state.save(&path).unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, state);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_runtime_section_loads_with_empty_usage_and_spend() {
        let dir = temp_dir("old-usage");
        let path = dir.join("state.json");
        fs::write(
            &path,
            r#"{"stage_limits":{},"lanes":[],"policies":{},"last_fire_ms":{},
                "runtime":{"paused":{"global":false},"tasks":[],"pending_chats":{},
                "review_tickets":{},"release_batches":{},"stuck":[]}}"#,
        )
        .unwrap();

        let loaded = DaemonState::load(&path);

        assert!(loaded.runtime.usage.is_empty());
        assert!(loaded.runtime.spend.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_usage_records_and_spend_survive_a_round_trip() {
        let dir = temp_dir("usage-round-trip");
        let path = dir.join("state.json");
        let record = UsageRecord {
            harness: crate::config::Harness::Opencode,
            mode: crate::usage::UsageMode::Plan,
            plan: Some("Pro".to_string()),
            models: vec!["zai-coding-plan/glm-5.3".to_string()],
            updated_ms: 5_000,
            ..UsageRecord::default()
        };
        let mut spend = SpendTotals::default();
        spend.add("zai-coding-plan/glm-5.3", 0.75);
        let mut state = DaemonState::default();
        state.runtime.usage.insert("zai-coding-plan".into(), record);
        state.runtime.spend.insert("zai-coding-plan".into(), spend);

        state.save(&path).unwrap();

        let loaded = DaemonState::load(&path);
        assert_eq!(loaded.runtime, state.runtime);
        assert_eq!(loaded.runtime.spend["zai-coding-plan"].total_usd, 0.75);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_state_file_loads_the_defaults() {
        let dir = temp_dir("missing");
        let loaded = DaemonState::load(&dir.join("absent.json"));
        assert_eq!(loaded, DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_state_file_loads_the_defaults() {
        let dir = temp_dir("corrupt");
        let path = dir.join("state.json");
        fs::write(&path, "not json at all {").unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, DaemonState::default());
        assert_eq!(loaded.stage_limits.get(&Stage::Refine), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_state_file_with_an_invalid_limit_loads_the_defaults() {
        let dir = temp_dir("invalid-limit");
        let path = dir.join("state.json");
        fs::write(
            &path,
            r#"{"stage_limits":{"refine":0},"lanes":[],"policies":{},"last_fire_ms":{}}"#,
        )
        .unwrap();

        let loaded = DaemonState::load(&path);

        assert_eq!(loaded, DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_state_still_round_trips() {
        let dir = temp_dir("empty");
        let path = dir.join("state.json");
        DaemonState::default().save(&path).unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_full_role_binding_survives_a_restart() {
        let dir = temp_dir("role-binding");
        let path = dir.join("state.json");
        let binding = ResolvedRoleSettings {
            role: ExecutionRole::Implement,
            source: SettingsSource::Repository {
                alias: "borsuk".to_string(),
            },
            settings: RoleSettings {
                harness: Harness::Codex,
                program: "codex-custom".to_string(),
                model: "gpt-test".to_string(),
                effort: Some("high".to_string()),
                extra_args: vec!["--color=never".to_string()],
                agent: None,
                profile: Some("factory".to_string()),
                permission_mode: None,
                permission_handler: None,
                tools: Vec::new(),
                disallowed_tools: Vec::new(),
                strict_mcp: None,
                auto_approve: None,
                approval_policy: Some("never".to_string()),
                sandbox: Some("workspace-write".to_string()),
            },
        };
        let mut state = DaemonState::default();
        state
            .role_bindings
            .insert("borsuk/implement-i142".to_string(), binding.clone());

        state.save(&path).unwrap();

        assert_eq!(
            DaemonState::load(&path)
                .role_bindings
                .get("borsuk/implement-i142"),
            Some(&binding)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_state_file_defaults_to_no_role_bindings() {
        let dir = temp_dir("old-role-bindings");
        let path = dir.join("state.json");
        fs::write(
            &path,
            r#"{"stage_limits":{},"lanes":[],"policies":{},"last_fire_ms":{}}"#,
        )
        .unwrap();

        assert!(DaemonState::load(&path).role_bindings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_role_binding_discards_the_complete_state() {
        let dir = temp_dir("corrupt-role-binding");
        let path = dir.join("state.json");
        let mut state = DaemonState::default();
        let mut binding = valid_binding();
        binding.settings.model.clear();
        state.role_bindings.insert("task".to_string(), binding);
        state.stage_limits.insert(Stage::Review, 9);
        state.save(&path).unwrap();
        assert_eq!(DaemonState::load(&path), DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stale_unsafe_role_binding_discards_the_complete_state() {
        let dir = temp_dir("unsafe-role-binding");
        let path = dir.join("state.json");
        let mut state = DaemonState::default();
        let mut binding = valid_binding();
        binding.settings.extra_args = vec!["--yolo".to_string()];
        state.role_bindings.insert("task".to_string(), binding);
        state.save(&path).unwrap();
        assert_eq!(DaemonState::load(&path), DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    fn valid_binding() -> ResolvedRoleSettings {
        ResolvedRoleSettings {
            role: ExecutionRole::Review,
            source: SettingsSource::Global,
            settings: RoleSettings {
                harness: Harness::Codex,
                program: "codex".to_string(),
                model: "gpt-test".to_string(),
                effort: None,
                extra_args: Vec::new(),
                agent: None,
                profile: None,
                permission_mode: None,
                permission_handler: None,
                tools: Vec::new(),
                disallowed_tools: Vec::new(),
                strict_mcp: None,
                auto_approve: None,
                approval_policy: Some("never".to_string()),
                sandbox: Some("workspace-write".to_string()),
            },
        }
    }
}
