//! The subagent roster of the session view.
//!
//! The roster is pure, like [`crate::tui::transcript`]. It ingests one
//! already-parsed log line at a time and keeps an ordered row per spawned
//! subagent or backgrounded task, the child transcript of each row, and
//! the session meters: context tokens, spend, and model. It touches no
//! terminal, no file, and no clock, so tests run offline.
//!
//! The claude dialect reports the lifecycle through the `system` subtypes
//! `task_started`, `task_progress`, `task_notification`, and
//! `background_tasks_changed`; the spawn itself is an `Agent` `tool_use`
//! block on a parent `assistant` line. Child lines carry a non-null
//! `parent_tool_use_id`. The opencode dialect reports a subagent as one
//! `tool_use` line with `part.tool == "task"` that arrives once, already
//! completed. The codex dialect names no child agent, so no line matches.
//!
//! No input can panic the roster: a line of an unknown shape changes
//! nothing.

use serde_json::Value;

use super::transcript::{self, Entry};
use crate::config::Harness;

/// How many rows the roster keeps. The oldest closed row drops first.
pub const AGENT_CAP: usize = 64;

/// How many child entries one row keeps. The oldest entry drops first.
pub const AGENT_RING_CAP: usize = 500;

/// What kind of work a roster row reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    /// A spawned agent subagent. Shown under `subagents`.
    #[default]
    Agent,
    /// A backgrounded bash task. Shown under `background`.
    Bash,
}

/// The lifecycle state of one roster row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentStatus {
    /// The row opened and no notification closed it yet.
    #[default]
    Running,
    /// A `task_notification` with `status == "completed"` closed the row.
    Done,
    /// Any other closing status, with the reported status text.
    Failed(String),
}

/// One subagent or background task row of the panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentRow {
    /// The stable task id the lifecycle lines carry.
    pub task_id: String,
    /// The `id` of the `Agent` tool_use block that spawned the row.
    pub tool_use_id: Option<String>,
    /// Agent subagent or backgrounded bash task.
    pub kind: AgentKind,
    /// The `name` the spawn block input carried.
    pub name: Option<String>,
    /// The subagent type, for example `explore`.
    pub subagent_type: Option<String>,
    /// The human description the lifecycle lines carry.
    pub description: String,
    /// The lifecycle state.
    pub status: AgentStatus,
    /// The summed token count of the latest progress line.
    pub total_tokens: Option<u64>,
    /// The tool-use count of the latest progress line.
    pub tool_uses: Option<u64>,
    /// The duration of the latest progress line, in milliseconds.
    pub duration_ms: Option<u64>,
    /// The last tool name the latest progress line named.
    pub last_tool: Option<String>,
    /// The model, when the protocol reports one.
    pub model: Option<String>,
    /// The summary a closing notification carried.
    pub summary: Option<String>,
}

/// The session meters of the panel's `session` part.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionMeters {
    /// The prompt size of the newest parent turn, in tokens.
    pub context_tokens: Option<u64>,
    /// The session spend in dollars, when the log carries one.
    pub spend_usd: Option<f64>,
    /// The model the log names, when it names one.
    pub model: Option<String>,
}

/// The ordered roster of one session.
#[derive(Debug, Default)]
pub struct AgentRoster {
    rows: Vec<AgentRow>,
    /// One child transcript per row, oldest entry first.
    children: Vec<Vec<Entry>>,
    meters: SessionMeters,
    /// Agent block ids seen before their `task_started` row arrived.
    pending_names: Vec<(String, Option<String>)>,
    opencode_spend: f64,
    opencode_counter: u64,
}

impl AgentRoster {
    /// Create an empty roster.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every row, every child transcript, and every meter.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.children.clear();
        self.meters = SessionMeters::default();
        self.pending_names.clear();
        self.opencode_spend = 0.0;
    }

    /// The rows in creation order, oldest first.
    pub fn rows(&self) -> &[AgentRow] {
        &self.rows
    }

    /// The session meters of the ingest so far.
    pub fn meters(&self) -> &SessionMeters {
        &self.meters
    }

    /// The child transcript of one row, oldest entry first.
    ///
    /// An unknown task id yields an empty slice.
    pub fn child_entries(&self, task_id: &str) -> &[Entry] {
        let Some(index) = self.row_index(task_id) else {
            return &[];
        };
        &self.children[index]
    }

    /// Ingest one already-parsed log line.
    ///
    /// `harness` names the harness of the shown task, when the view has
    /// a binding. The line shapes are self-describing, so the value is
    /// accepted for signature stability and otherwise unused.
    pub fn ingest(&mut self, value: &Value, harness: Option<Harness>) {
        let _ = harness;
        let kind = value.get("type").and_then(Value::as_str);
        match kind {
            Some("assistant") => self.ingest_claude_assistant(value),
            Some("user") => self.ingest_claude_child(value),
            Some("system") => self.ingest_claude_system(value),
            Some("result") => self.ingest_claude_result(value),
            Some("tool_use") => self.ingest_opencode_task(value),
            Some("step_finish") => self.ingest_opencode_step_finish(value),
            _ => {}
        }
    }

    // -- claude lines --------------------------------------------------

    /// One claude `assistant` line: meters, the spawn block, or a child.
    fn ingest_claude_assistant(&mut self, value: &Value) {
        let parent = value.get("parent_tool_use_id").unwrap_or(&Value::Null);
        if !parent.is_null() {
            let parent = parent.as_str().unwrap_or_default().to_string();
            self.append_child_entries(&parent, transcript::child_entries(value));
            return;
        }
        if let Some(sum) = claude_context_tokens(value) {
            self.meters.context_tokens = Some(sum);
        }
        self.remember_agent_blocks(value);
    }

    /// One claude `user` line: only a child line contributes entries.
    fn ingest_claude_child(&mut self, value: &Value) {
        let parent = value.get("parent_tool_use_id").unwrap_or(&Value::Null);
        if parent.is_null() {
            return;
        }
        let parent = parent.as_str().unwrap_or_default().to_string();
        self.append_child_entries(&parent, transcript::child_entries(value));
    }

    /// One claude `system` line: the model, or one lifecycle subtype.
    fn ingest_claude_system(&mut self, value: &Value) {
        let subtype = value.get("subtype").and_then(Value::as_str);
        match subtype {
            Some("init") => {
                if let Some(model) = str_field(value, "model") {
                    self.meters.model = Some(model);
                }
            }
            Some("task_started") => self.task_started(value),
            Some("task_progress") => self.task_progress(value),
            Some("task_notification") => self.task_notification(value),
            Some("background_tasks_changed") => self.background_rows(value),
            _ => {}
        }
    }

    /// One claude `result` line: the newest spend wins.
    fn ingest_claude_result(&mut self, value: &Value) {
        if let Some(cost) = value.get("total_cost_usd").and_then(Value::as_f64) {
            self.meters.spend_usd = Some(cost);
        }
    }

    /// Apply one `task_started` line: create the row, or reopen it.
    fn task_started(&mut self, value: &Value) {
        let Some(task_id) = str_field(value, "task_id") else {
            return;
        };
        let kind = if str_field(value, "task_type").as_deref() == Some("local_bash") {
            AgentKind::Bash
        } else {
            AgentKind::Agent
        };
        let tool_use_id = str_field(value, "tool_use_id");
        let name = match &tool_use_id {
            Some(id) => self.take_pending_name(id),
            None => None,
        };
        if let Some(index) = self.row_index(&task_id) {
            let row = &mut self.rows[index];
            row.kind = kind;
            row.status = AgentStatus::Running;
            row.tool_use_id = tool_use_id;
            row.subagent_type = str_field(value, "subagent_type");
            row.description = str_field(value, "description").unwrap_or_default();
            if name.is_some() {
                row.name = name;
            }
            return;
        }
        let row = AgentRow {
            task_id,
            tool_use_id,
            kind,
            name,
            subagent_type: str_field(value, "subagent_type"),
            description: str_field(value, "description").unwrap_or_default(),
            status: AgentStatus::Running,
            ..AgentRow::default()
        };
        self.push_row(row);
    }

    /// Apply one `task_progress` line: the numbers and the last tool.
    fn task_progress(&mut self, value: &Value) {
        let Some(task_id) = str_field(value, "task_id") else {
            return;
        };
        let Some(index) = self.row_index(&task_id) else {
            return;
        };
        let row = &mut self.rows[index];
        if let Some(description) = str_field(value, "description") {
            row.description = description;
        }
        if let Some(usage) = value.get("usage") {
            row.total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
            row.tool_uses = usage.get("tool_uses").and_then(Value::as_u64);
            row.duration_ms = usage.get("duration_ms").and_then(Value::as_u64);
        }
        row.last_tool = str_field(value, "last_tool_name");
    }

    /// Apply one `task_notification` line: close the row.
    ///
    /// `completed` marks the row done; any other status marks it failed
    /// and the panel shows that status text.
    fn task_notification(&mut self, value: &Value) {
        let Some(task_id) = str_field(value, "task_id") else {
            return;
        };
        let Some(index) = self.row_index(&task_id) else {
            return;
        };
        let row = &mut self.rows[index];
        row.status = match str_field(value, "status").as_deref() {
            Some("completed") => AgentStatus::Done,
            Some(other) => AgentStatus::Failed(other.to_string()),
            None => return,
        };
        row.summary = str_field(value, "summary");
    }

    /// Apply one `background_tasks_changed` line.
    ///
    /// The line only creates a row that no `task_started` created; it
    /// never closes a row.
    fn background_rows(&mut self, value: &Value) {
        let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
            return;
        };
        for task in tasks {
            let Some(task_id) = str_field(task, "task_id") else {
                continue;
            };
            if self.row_index(&task_id).is_some() {
                continue;
            }
            let kind = if str_field(task, "task_type").as_deref() == Some("local_bash") {
                AgentKind::Bash
            } else {
                AgentKind::Agent
            };
            self.push_row(AgentRow {
                task_id,
                kind,
                description: str_field(task, "description").unwrap_or_default(),
                status: AgentStatus::Running,
                ..AgentRow::default()
            });
        }
    }

    /// Record the `Agent` spawn blocks of a parent assistant line.
    ///
    /// The block `id` and the input `name` attach to the row with the
    /// same `tool_use_id`, or wait in the pending list until the row
    /// arrives.
    fn remember_agent_blocks(&mut self, value: &Value) {
        let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use")
                || block.get("name").and_then(Value::as_str) != Some("Agent")
            {
                continue;
            }
            let Some(id) = str_field(block, "id") else {
                continue;
            };
            let name = block
                .pointer("/input/name")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(index) = self
                .rows
                .iter_mut()
                .position(|row| row.tool_use_id.as_deref() == Some(id.as_str()))
            {
                self.rows[index].name = name;
            } else {
                self.pending_names.push((id, name));
                if self.pending_names.len() > AGENT_CAP {
                    self.pending_names.remove(0);
                }
            }
        }
    }

    /// Take one pending spawn name for a row that just arrived.
    fn take_pending_name(&mut self, tool_use_id: &str) -> Option<String> {
        let position = self
            .pending_names
            .iter()
            .position(|(id, _)| id == tool_use_id)?;
        self.pending_names.remove(position).1
    }

    // -- opencode lines ------------------------------------------------

    /// One opencode `tool_use` line with `part.tool == "task"`.
    ///
    /// The line appears once, already completed, so the row closes at
    /// once. `time.end - time.start` gives the duration, the metadata
    /// model names the model, and the output becomes the one child entry.
    fn ingest_opencode_task(&mut self, value: &Value) {
        let part = value.get("part").unwrap_or(&Value::Null);
        if part.get("tool").and_then(Value::as_str) != Some("task") {
            return;
        }
        let state = part.get("state").unwrap_or(&Value::Null);
        let call_id = str_field(part, "callID");
        let task_id = call_id.clone().unwrap_or_else(|| {
            self.opencode_counter += 1;
            format!("opencode-task-{}", self.opencode_counter)
        });
        let duration_ms = match (
            part.pointer("/state/time/start").and_then(Value::as_u64),
            part.pointer("/state/time/end").and_then(Value::as_u64),
        ) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start)),
            _ => None,
        };
        let model = part.pointer("/state/metadata/model").map(|model| {
            let provider = model
                .get("providerID")
                .and_then(Value::as_str)
                .unwrap_or("");
            let id = model.get("modelID").and_then(Value::as_str).unwrap_or("");
            format!("{provider}/{id}")
        });
        let status = match state.get("status").and_then(Value::as_str) {
            Some("error") => AgentStatus::Failed("error".to_string()),
            _ => AgentStatus::Done,
        };
        let row = AgentRow {
            task_id: task_id.clone(),
            tool_use_id: call_id,
            kind: AgentKind::Agent,
            subagent_type: state
                .pointer("/input/subagent_type")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: state
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| state.pointer("/input/description").and_then(Value::as_str))
                .unwrap_or("subagent")
                .to_string(),
            status,
            duration_ms,
            model: model.filter(|model| model != "/"),
            ..AgentRow::default()
        };
        if let Some(index) = self.row_index(&task_id) {
            self.rows[index] = row;
        } else {
            self.push_row(row);
        }
        if let Some(index) = self.row_index(&task_id) {
            if let Some(output) = state.get("output").and_then(Value::as_str) {
                if !output.is_empty() {
                    self.children[index] = vec![Entry::Assistant {
                        text: output.to_string(),
                    }];
                }
            }
        }
        if let Some(model) = self
            .row_index(&task_id)
            .and_then(|index| self.rows[index].model.clone())
        {
            self.meters.model = Some(model);
        }
    }

    /// One opencode `step_finish` line: the newest context wins.
    ///
    /// The spend is the running sum of every `part.cost`, kept only when
    /// it stays above zero, because a subscription plan reports 0.
    fn ingest_opencode_step_finish(&mut self, value: &Value) {
        let tokens = value.pointer("/part/tokens");
        if let Some(tokens) = tokens {
            let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
            let read = tokens
                .pointer("/cache/read")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.meters.context_tokens = Some(input + read);
        }
        if let Some(cost) = value.pointer("/part/cost").and_then(Value::as_f64) {
            self.opencode_spend += cost.max(0.0);
            self.meters.spend_usd = (self.opencode_spend > 0.0).then_some(self.opencode_spend);
        }
    }

    // -- shared internals ----------------------------------------------

    /// The position of the row with `task_id`, if one exists.
    fn row_index(&self, task_id: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.task_id == task_id)
    }

    /// Append a row and drop the oldest closed row over the cap.
    fn push_row(&mut self, row: AgentRow) {
        self.rows.push(row);
        self.children.push(Vec::new());
        while self.rows.len() > AGENT_CAP {
            let victim = self
                .rows
                .iter()
                .position(|row| row.status != AgentStatus::Running)
                .unwrap_or(0);
            self.rows.remove(victim);
            self.children.remove(victim);
        }
    }

    /// Append the entries of one child line to the matching row's ring.
    fn append_child_entries(&mut self, tool_use_id: &str, entries: Vec<Entry>) {
        if entries.is_empty() {
            return;
        }
        let Some(index) = self
            .rows
            .iter()
            .position(|row| row.tool_use_id.as_deref() == Some(tool_use_id))
        else {
            return;
        };
        let ring = &mut self.children[index];
        for entry in entries {
            if ring.len() == AGENT_RING_CAP {
                ring.remove(0);
            }
            ring.push(entry);
        }
    }
}

/// The prompt size of one parent claude `assistant` line.
///
/// The verified protocol sums `input_tokens`, the cache-creation tokens,
/// and the cache-read tokens of `message.usage`.
fn claude_context_tokens(value: &Value) -> Option<u64> {
    let usage = value.pointer("/message/usage")?;
    let input = usage.get("input_tokens").and_then(Value::as_u64)?;
    let creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)?;
    let read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)?;
    Some(input + creation + read)
}

/// Read one string field of a JSON object.
fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ingest_all(roster: &mut AgentRoster, values: &[Value]) {
        for value in values {
            roster.ingest(value, Some(Harness::Claude));
        }
    }

    fn started(task_id: &str, tool_use_id: &str, task_type: &str) -> Value {
        json!({
            "type": "system",
            "subtype": "task_started",
            "uuid": "u1",
            "session_id": "s1",
            "task_id": task_id,
            "tool_use_id": tool_use_id,
            "description": "Refine the spec",
            "task_type": task_type,
            "subagent_type": "Explore",
            "is_backgrounded": false,
            "spawn_depth": 1,
            "prompt": "go"
        })
    }

    fn progress(task_id: &str, tokens: u64, tools: u64, last_tool: &str) -> Value {
        json!({
            "type": "system",
            "subtype": "task_progress",
            "uuid": "u2",
            "session_id": "s1",
            "task_id": task_id,
            "tool_use_id": "toolu_a",
            "description": "Refine the spec",
            "subagent_type": "Explore",
            "last_tool_name": last_tool,
            "usage": {"total_tokens": tokens, "tool_uses": tools, "duration_ms": 4200}
        })
    }

    #[test]
    fn a_task_started_line_creates_one_running_agent_row() {
        let mut roster = AgentRoster::new();

        ingest_all(&mut roster, &[started("task-1", "toolu_a", "local_agent")]);

        assert_eq!(roster.rows().len(), 1);
        let row = &roster.rows()[0];
        assert_eq!(row.task_id, "task-1");
        assert_eq!(row.kind, AgentKind::Agent);
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(row.description, "Refine the spec");
        assert_eq!(row.tool_use_id.as_deref(), Some("toolu_a"));
    }

    #[test]
    fn a_task_progress_line_updates_the_numbers_and_the_last_tool() {
        let mut roster = AgentRoster::new();

        ingest_all(
            &mut roster,
            &[
                started("task-1", "toolu_a", "local_agent"),
                progress("task-1", 1234, 3, "Read"),
            ],
        );

        let row = &roster.rows()[0];
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.total_tokens, Some(1234));
        assert_eq!(row.tool_uses, Some(3));
        assert_eq!(row.duration_ms, Some(4200));
        assert_eq!(row.last_tool.as_deref(), Some("Read"));
    }

    #[test]
    fn a_completed_notification_closes_the_row_done() {
        let mut roster = AgentRoster::new();
        let notification = json!({
            "type": "system",
            "subtype": "task_notification",
            "uuid": "u3",
            "session_id": "s1",
            "task_id": "task-1",
            "tool_use_id": "toolu_a",
            "status": "completed",
            "summary": "spec refined",
            "output_file": "/tmp/out"
        });

        ingest_all(
            &mut roster,
            &[started("task-1", "toolu_a", "local_agent"), notification],
        );

        let row = &roster.rows()[0];
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.summary.as_deref(), Some("spec refined"));
    }

    #[test]
    fn any_other_notification_status_marks_the_row_failed_with_that_text() {
        let mut roster = AgentRoster::new();
        let failed = json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "task-1",
            "tool_use_id": "toolu_a",
            "status": "failed_timeout",
            "summary": "gave up"
        });

        ingest_all(
            &mut roster,
            &[started("task-1", "toolu_a", "local_agent"), failed],
        );

        assert_eq!(
            roster.rows()[0].status,
            AgentStatus::Failed("failed_timeout".to_string())
        );
    }

    #[test]
    fn a_bash_task_lands_in_a_bash_row_and_never_becomes_an_agent_row() {
        let mut roster = AgentRoster::new();
        let background = json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": [
                {"task_id": "bash-1", "task_type": "local_bash", "description": "run the tests"}
            ]
        });

        let first = background.clone();
        ingest_all(&mut roster, &[first]);

        assert_eq!(roster.rows().len(), 1);
        assert_eq!(roster.rows()[0].kind, AgentKind::Bash);
        assert_eq!(roster.rows()[0].description, "run the tests");
        assert_eq!(roster.rows()[0].status, AgentStatus::Running);

        // A second announcement adds no duplicate and closes nothing.
        let again = background.clone();
        ingest_all(&mut roster, &[again]);
        assert_eq!(roster.rows().len(), 1);
        assert_eq!(roster.rows()[0].status, AgentStatus::Running);

        // A plain `task_started` bash row is a Bash row too, and it
        // carries no subagent type.
        let bash_started = json!({
            "type": "system", "subtype": "task_started",
            "task_id": "bash-2", "tool_use_id": "toolu_b",
            "description": "second bash", "task_type": "local_bash",
            "is_backgrounded": true
        });
        ingest_all(&mut roster, &[bash_started]);
        assert_eq!(roster.rows()[1].kind, AgentKind::Bash);
        assert_eq!(roster.rows()[1].subagent_type, None);
    }

    #[test]
    fn the_spawn_block_name_attaches_whatever_the_line_order() {
        let mut roster = AgentRoster::new();
        let spawn = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {
                "content": [
                    {"type": "tool_use", "id": "toolu_a", "name": "Agent",
                     "input": {"description": "Refine the spec",
                               "subagent_type": "Explore",
                               "name": "spec-reader", "prompt": "go"}}
                ]
            }
        });

        // The block arrives before the row.
        ingest_all(
            &mut roster,
            &[spawn.clone(), started("task-1", "toolu_a", "local_agent")],
        );
        assert_eq!(roster.rows()[0].name.as_deref(), Some("spec-reader"));

        // The block arrives after the row.
        let mut roster = AgentRoster::new();
        ingest_all(
            &mut roster,
            &[started("task-2", "toolu_a", "local_agent"), spawn],
        );
        assert_eq!(roster.rows()[0].name.as_deref(), Some("spec-reader"));
    }

    #[test]
    fn a_child_line_appends_its_entries_to_the_matching_row() {
        let mut roster = AgentRoster::new();
        let child_text = json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_a",
            "subagent_type": "Explore",
            "task_description": "Refine the spec",
            "message": {"content": [{"type": "text", "text": "child prose"}]}
        });
        let child_result = json!({
            "type": "user",
            "parent_tool_use_id": "toolu_a",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": "the result"}
            ]}
        });

        ingest_all(
            &mut roster,
            &[
                started("task-1", "toolu_a", "local_agent"),
                child_text,
                child_result,
            ],
        );

        let entries = roster.child_entries("task-1");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            Entry::Assistant {
                text: "child prose".to_string()
            }
        );
        assert!(matches!(entries[1], Entry::ToolResult { .. }));

        // A child line of an unknown row is dropped quietly.
        let orphan = json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_missing",
            "message": {"content": [{"type": "text", "text": "lost"}]}
        });
        ingest_all(&mut roster, &[orphan]);
        assert_eq!(roster.child_entries("task-1").len(), 2);
    }

    #[test]
    fn an_opencode_task_line_creates_one_closed_row() {
        let mut roster = AgentRoster::new();
        let task = json!({
            "type": "tool_use",
            "sessionID": "ses_1",
            "part": {
                "type": "tool",
                "tool": "task",
                "callID": "call_1",
                "state": {
                    "status": "completed",
                    "title": "Explore the code",
                    "input": {"description": "d", "prompt": "p", "subagent_type": "explore"},
                    "output": "the answer",
                    "time": {"start": 1000, "end": 5000},
                    "metadata": {
                        "parentSessionId": "p",
                        "sessionId": "c",
                        "model": {"providerID": "zai-coding-plan", "modelID": "glm-5.3-flash"},
                        "truncated": false
                    }
                }
            }
        });

        roster.ingest(&task, Some(Harness::Opencode));

        assert_eq!(roster.rows().len(), 1);
        let row = &roster.rows()[0];
        assert_eq!(row.kind, AgentKind::Agent);
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.description, "Explore the code");
        assert_eq!(row.subagent_type.as_deref(), Some("explore"));
        assert_eq!(row.duration_ms, Some(4000));
        assert_eq!(row.model.as_deref(), Some("zai-coding-plan/glm-5.3-flash"));

        let entries = roster.child_entries("call_1");
        assert_eq!(
            entries,
            vec![Entry::Assistant {
                text: "the answer".to_string()
            }]
        );
        assert_eq!(
            roster.meters().model.as_deref(),
            Some("zai-coding-plan/glm-5.3-flash")
        );

        // A non-task opencode tool line adds no row.
        let read = json!({
            "type": "tool_use",
            "part": {"type": "tool", "tool": "read", "state": {"status": "completed"}}
        });
        roster.ingest(&read, Some(Harness::Opencode));
        assert_eq!(roster.rows().len(), 1);
    }

    #[test]
    fn the_meters_follow_the_newest_parent_lines() {
        let mut roster = AgentRoster::new();
        let init = json!({
            "type": "system", "subtype": "init", "session_id": "s1",
            "model": "claude-opus-5[1m]"
        });
        let parent = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"usage": {
                "input_tokens": 100, "cache_creation_input_tokens": 20,
                "cache_read_input_tokens": 30, "output_tokens": 5
            }}
        });
        let newer_parent = json!({
            "type": "assistant",
            "message": {"usage": {
                "input_tokens": 200, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens": 5
            }}
        });
        let result = json!({
            "type": "result", "subtype": "success", "total_cost_usd": 0.21
        });

        ingest_all(&mut roster, &[init, parent, newer_parent, result]);

        let meters = roster.meters();
        assert_eq!(meters.context_tokens, Some(200));
        assert_eq!(meters.spend_usd, Some(0.21));
        assert_eq!(meters.model.as_deref(), Some("claude-opus-5[1m]"));
    }

    #[test]
    fn the_opencode_meters_sum_the_steps_and_keep_spend_above_zero() {
        let mut roster = AgentRoster::new();
        let step = |input: u64, read: u64, cost: f64| {
            json!({
                "type": "step_finish",
                "part": {"reason": "ok", "cost": cost,
                         "tokens": {"total": 10, "input": input, "output": 5,
                                    "reasoning": 0, "cache": {"write": 0, "read": read}}}
            })
        };

        roster.ingest(&step(60, 40, 0.0), Some(Harness::Opencode));
        assert_eq!(roster.meters().context_tokens, Some(100));
        assert_eq!(roster.meters().spend_usd, None, "a plan reports 0");

        roster.ingest(&step(50, 25, 0.25), Some(Harness::Opencode));
        assert_eq!(roster.meters().context_tokens, Some(75));
        assert_eq!(roster.meters().spend_usd, Some(0.25));
    }

    #[test]
    fn the_roster_drops_the_oldest_closed_row_over_the_cap() {
        let mut roster = AgentRoster::new();

        // Fill the cap with running rows, close the first, add one more.
        for index in 0..AGENT_CAP {
            ingest_all(
                &mut roster,
                &[started(
                    &format!("task-{index}"),
                    &format!("toolu_{index}"),
                    "local_agent",
                )],
            );
        }
        let closing = json!({
            "type": "system", "subtype": "task_notification",
            "task_id": "task-0", "tool_use_id": "toolu_0", "status": "completed"
        });
        ingest_all(&mut roster, &[closing]);
        ingest_all(
            &mut roster,
            &[started("task-extra", "toolu_extra", "local_agent")],
        );

        assert_eq!(roster.rows().len(), AGENT_CAP);
        assert_eq!(roster.rows()[0].task_id, "task-1", "the closed row dropped");
        assert_eq!(
            roster.rows().last().unwrap().task_id,
            "task-extra",
            "the newest row stays"
        );

        // All rows running again: the plain oldest row drops.
        ingest_all(
            &mut roster,
            &[started("task-one-more", "toolu_more", "local_agent")],
        );
        assert_eq!(roster.rows().len(), AGENT_CAP);
        assert_eq!(roster.rows()[0].task_id, "task-2");
    }

    #[test]
    fn a_child_ring_never_exceeds_its_bound() {
        let mut roster = AgentRoster::new();
        ingest_all(&mut roster, &[started("task-1", "toolu_a", "local_agent")]);

        let feed = AGENT_RING_CAP + 40;
        for index in 0..feed {
            let child = json!({
                "type": "assistant",
                "parent_tool_use_id": "toolu_a",
                "message": {"content": [{"type": "text", "text": format!("line {index}")}]}
            });
            roster.ingest(&child, Some(Harness::Claude));
        }

        let entries = roster.child_entries("task-1");
        assert_eq!(entries.len(), AGENT_RING_CAP);
        assert_eq!(
            entries[0],
            Entry::Assistant {
                text: format!("line {}", feed - AGENT_RING_CAP)
            }
        );
        assert_eq!(
            entries.last().unwrap(),
            &Entry::Assistant {
                text: format!("line {}", feed - 1)
            }
        );
    }

    #[test]
    fn unknown_shapes_and_garbage_never_panic_and_change_nothing() {
        let mut roster = AgentRoster::new();

        for value in [
            json!({"type": "mystery", "payload": 42}),
            json!({"type": "system", "subtype": "thinking_tokens"}),
            json!({"type": "assistant"}),
            json!({"type": "user"}),
            json!({"type": "system"}),
            json!({"type": "tool_use", "part": {"type": "tool"}}),
            json!({"type": "step_finish"}),
            json!({"type": "system", "subtype": "task_notification"}),
            json!({"type": "system", "subtype": "task_started"}),
            json!({"type": "system", "subtype": "background_tasks_changed"}),
            json!(null),
            json!("text"),
            json!([1, 2, 3]),
        ] {
            roster.ingest(&value, None);
        }

        assert!(roster.rows().is_empty());
        assert_eq!(roster.meters(), &SessionMeters::default());
    }

    #[test]
    fn clear_drops_every_piece_of_roster_state() {
        let mut roster = AgentRoster::new();
        ingest_all(
            &mut roster,
            &[
                started("task-1", "toolu_a", "local_agent"),
                progress("task-1", 10, 1, "Read"),
            ],
        );

        roster.clear();

        assert!(roster.rows().is_empty());
        assert_eq!(roster.meters(), &SessionMeters::default());
        assert!(roster.child_entries("task-1").is_empty());
    }
}
