//! The codex runner: a live, steerable session over `codex app-server`.
//!
//! One task is one `codex [-c ...] app-server --listen stdio://` child. The
//! child speaks newline-delimited JSON-RPC 2.0 without the `jsonrpc` field:
//! a request carries `id` and `method`, a response carries `id` and either
//! `result` or `error`, and a notification carries `method` and `params`
//! only. The server sends requests of its own with numeric ids, and each one
//! waits for a response line `{"id": <same id>, "result": {...}}`.
//!
//! The start is a three-step handshake against one deadline:
//!
//! 1. `initialize` (id 1), then the `initialized` notification.
//! 2. `thread/start` (id 2) for a fresh job, or `thread/resume` (id 2) when
//!    the job names a thread. The result carries the thread id, which
//!    becomes the session id of [`RunEvent::Started`].
//! 3. `turn/start` (id 3) with the prompt. The result carries the turn id,
//!    which the stop path needs for `turn/interrupt`.
//!
//! After a turn ends the child stays alive and parked, exactly like the
//! claude child. [`Session::send_user`] opens the next turn with a fresh
//! request id. [`Session::stop`] writes `turn/interrupt` for an open turn,
//! then closes the stdin pipe, because a `stdio://` app server exits on end
//! of file. The signal escalation of [`crate::proc`] stays as the fallback.
//!
//! Two server request lines carry a decision:
//!
//! - `item/tool/requestUserInput` is a question for a person. Its
//!   `params.questions` array travels to the inbox unchanged, and the answer
//!   maps each question header back to its question id.
//! - `item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval`, and `item/permissions/requestApproval`
//!   are approvals. They open ordinary permission rows.
//!
//! The question tool stays locked until the feature flag
//! `features.default_mode_request_user_input=true` unlocks it. The runner
//! passes that flag as a `-c` override on every start. Without it codex
//! answers "request_user_input is unavailable in Default mode" and never
//! asks.
//!
//! Every thread starts the MCP servers of `~/.codex/config.toml`. The
//! recorded probe started telegram, stripe, and todoist for a plain review
//! thread. A codex role that must not start them needs its own `--profile`
//! with a smaller server list; the runner passes `--profile` when the role
//! sets one.
//!
//! The yolo policy lives here, client side. With `job.yolo` on, an approval
//! is accepted at once and never reaches the caller, and a permission
//! request is granted as asked. A question always becomes a
//! [`RunEvent::Ask`] with `needs_human` set, yolo or not.
//!
//! [`crate::proc`] tees every raw line into the task log; this module parses
//! the same lines into [`RunEvent`]s. A malformed or unknown line is logged
//! and skipped, never fatal. [`crate::usage::codex`] runs a second, shorter
//! `initialize` conversation for the rate-limit probe; its client name and
//! capabilities differ on purpose, so the two handshakes stay apart.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde_json::{json, Map, Value};

use crate::config::RoleSettings;
use crate::proc::{self, ProcEvent, ProcHandle, RunSpec, StopOutcome};
use crate::runner::{Answer, Job, RunEvent, Runner, Session};

/// The request id of the `initialize` handshake.
const INITIALIZE_ID: i64 = 1;

/// The request id of `thread/start` and of `thread/resume`.
const THREAD_ID: i64 = 2;

/// The request id of the first `turn/start`.
const FIRST_TURN_ID: i64 = 3;

/// The approval policy a role without one runs under.
const DEFAULT_APPROVAL_POLICY: &str = "on-request";

/// The sandbox a role without one runs under.
const DEFAULT_SANDBOX: &str = "workspace-write";

/// How long the whole start handshake may take before the job fails.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// The extra time the starter waits past the worker's own deadline.
const HANDSHAKE_WAIT_SLACK: Duration = Duration::from_secs(5);

/// How long a steering call waits for the worker to report its result.
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The summary length limit for a tool without a usable detail field.
const SUMMARY_CHARS: usize = 120;

/// Build the exact argument vector for one factory session.
///
/// The order is: the `-c` overrides, the profile, the role's extra
/// arguments, and the `app-server` subcommand with its stdio transport. The
/// working directory, the model, the effort, and the prompt all travel over
/// the protocol, so `job` takes no part in the argument vector.
fn build_args(_job: &Job, settings: &RoleSettings) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(effort) = settings.effort.as_ref() {
        args.push("-c".to_string());
        args.push(format!(
            "model_reasoning_effort={}",
            toml::Value::String(effort.clone())
        ));
    }
    // Without this flag codex refuses the question tool in Default mode.
    args.push("-c".to_string());
    args.push("features.default_mode_request_user_input=true".to_string());
    args.push("-c".to_string());
    args.push("suppress_unstable_features_warning=true".to_string());
    if let Some(profile) = settings.profile.as_ref() {
        args.push("--profile".to_string());
        args.push(profile.clone());
    }
    args.extend(settings.extra_args.iter().cloned());
    args.push("app-server".to_string());
    args.push("--listen".to_string());
    args.push("stdio://".to_string());
    args
}

/// The approval policy of one role: the configured value, or the default.
fn approval_policy(settings: &RoleSettings) -> String {
    settings
        .approval_policy
        .clone()
        .unwrap_or_else(|| DEFAULT_APPROVAL_POLICY.to_string())
}

/// The sandbox of one role: the configured value, or the default.
fn sandbox(settings: &RoleSettings) -> String {
    settings
        .sandbox
        .clone()
        .unwrap_or_else(|| DEFAULT_SANDBOX.to_string())
}

/// Build the `initialize` request line.
fn initialize_request() -> String {
    json!({
        "id": INITIALIZE_ID,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "aif",
                "title": "aif",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"experimentalApi": true},
        },
    })
    .to_string()
}

/// Build the `initialized` notification line.
fn initialized_notification() -> String {
    json!({"method": "initialized", "params": null}).to_string()
}

/// Build the `thread/start` request line for a fresh job.
fn thread_start_request(cwd: &str, policy: &str, sandbox: &str) -> String {
    json!({
        "id": THREAD_ID,
        "method": "thread/start",
        "params": {"cwd": cwd, "approvalPolicy": policy, "sandbox": sandbox},
    })
    .to_string()
}

/// Build the `thread/resume` request line for a job that continues a thread.
fn thread_resume_request(thread_id: &str) -> String {
    json!({
        "id": THREAD_ID,
        "method": "thread/resume",
        "params": {"threadId": thread_id},
    })
    .to_string()
}

/// Build one `turn/start` request line.
///
/// The model always rides on the turn; the effort rides with it when the
/// role sets one.
fn turn_start_request(id: i64, thread_id: &str, text: &str, settings: &RoleSettings) -> String {
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert("input".to_string(), json!([{"type": "text", "text": text}]));
    params.insert("model".to_string(), json!(settings.model));
    if let Some(effort) = settings.effort.as_ref() {
        params.insert("effort".to_string(), json!(effort));
    }
    json!({"id": id, "method": "turn/start", "params": params}).to_string()
}

/// Build the `turn/interrupt` request line that stops the open turn.
fn turn_interrupt_request(id: i64, thread_id: &str, turn_id: &str) -> String {
    json!({
        "id": id,
        "method": "turn/interrupt",
        "params": {"threadId": thread_id, "turnId": turn_id},
    })
    .to_string()
}

/// Build the response line for one question request.
///
/// Every supplied answer names a question by header, then by question text,
/// then by position. A question without an answer, and every question of a
/// denied row, gets an empty answer list.
fn question_answers_line(id: &Value, questions: &[QuestionSpec], answer: &Answer) -> String {
    let supplied = match answer {
        Answer::Allow { updated_input } => updated_input
            .as_ref()
            .and_then(|value| value.get("answers"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        Answer::Deny { .. } => Map::new(),
    };
    let mut used = vec![false; questions.len()];
    let mut answers = Map::new();
    for (position, (key, value)) in supplied.iter().enumerate() {
        let Some(index) = match_question(questions, &used, key, position) else {
            continue;
        };
        used[index] = true;
        answers.insert(
            questions[index].id.clone(),
            json!({"answers": answer_texts(value)}),
        );
    }
    for (index, question) in questions.iter().enumerate() {
        if !used[index] {
            answers.insert(question.id.clone(), json!({"answers": []}));
        }
    }
    json!({"id": id, "result": {"answers": answers}}).to_string()
}

/// Find the question one answer key names.
///
/// The header wins, then the question text, then the position of the answer
/// in the supplied map. An already answered question never matches twice.
fn match_question(
    questions: &[QuestionSpec],
    used: &[bool],
    key: &str,
    position: usize,
) -> Option<usize> {
    let free = |index: &usize| used.get(*index).copied() == Some(false);
    if let Some(index) = (0..questions.len())
        .filter(free)
        .find(|index| questions[*index].header == key)
    {
        return Some(index);
    }
    if let Some(index) = (0..questions.len())
        .filter(free)
        .find(|index| questions[*index].question == key)
    {
        return Some(index);
    }
    Some(position).filter(free)
}

/// The answer texts of one supplied value.
///
/// A single-select answer is one string; a multi-select answer is a list of
/// strings. Any other shape travels as its JSON text, so nothing is lost.
fn answer_texts(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect(),
        other => vec![other.to_string()],
    }
}

/// Build the response line for one command or file-change approval.
fn approval_decision_line(id: &Value, accept: bool) -> String {
    let decision = if accept { "accept" } else { "decline" };
    json!({"id": id, "result": {"decision": decision}}).to_string()
}

/// Build the response line for one permission request.
///
/// An allow grants exactly the permissions the server asked for, for the
/// rest of the turn. A deny grants none of them.
fn permissions_line(id: &Value, requested: &Value, allow: bool) -> String {
    let result = if allow {
        json!({"scope": "turn", "permissions": requested})
    } else {
        json!({"permissions": {}})
    };
    json!({"id": id, "result": result}).to_string()
}

/// Build the refusal line for a server request this runner cannot answer.
fn unsupported_request_line(id: &Value, method: &str) -> String {
    json!({
        "id": id,
        "error": {"code": -32601, "message": format!("aif does not answer {method}")},
    })
    .to_string()
}

/// Cut `text` to at most [`SUMMARY_CHARS`] characters.
fn truncate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_CHARS {
        text.to_string()
    } else {
        text.chars().take(SUMMARY_CHARS).collect()
    }
}

/// One question the server asked, with the id its answer must name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionSpec {
    /// The id the answer map is keyed by.
    id: String,
    /// The short header the inbox shows and the answer names.
    header: String,
    /// The full question text, the second way to name the question.
    question: String,
}

/// Read the question list of one `item/tool/requestUserInput` request.
fn question_specs(params: &Value) -> Vec<QuestionSpec> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .map(|question| QuestionSpec {
                    id: question
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    header: question
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    question: question
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One request the server sent, which waits for a response line.
#[derive(Debug, Clone, PartialEq)]
struct ServerRequest {
    /// The id the response must echo, in its original JSON form.
    id: Value,
    /// The request method, for example `item/tool/requestUserInput`.
    method: String,
    /// The request parameters, verbatim.
    params: Value,
}

/// What one pending server request expects from an [`Answer`].
#[derive(Debug, Clone, PartialEq)]
enum PendingKind {
    /// A question list; the answer maps headers back to question ids.
    Questions(Vec<QuestionSpec>),
    /// A command or file-change approval; the answer accepts or declines.
    Approval,
    /// A permission request; the answer grants the asked set, or none.
    Permissions(Value),
}

/// What one output line parses into.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// Notification lines, already mapped to run events.
    Events(Vec<RunEvent>),
    /// A response to one request this runner sent.
    Response(Value),
    /// A request from the server that waits for a response line.
    Request(ServerRequest),
    /// An `error` notification, remembered as the failure reason.
    Failure(String),
    /// A line this runner does not act on. The raw bytes are in the log.
    Ignored,
}

/// Map one output line into what the runner acts on.
///
/// The worker parses each line itself, because it also tracks the ids the
/// protocol carries. This wrapper keeps the pure line-to-events path that
/// the fixture replays exercise.
#[cfg(test)]
fn map_line(task: &str, line: &str) -> Parsed {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Ignored;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => map_value(task, &value),
        Err(_) => Parsed::Ignored,
    }
}

/// Map one parsed output line into what the runner acts on.
///
/// A line with an `id` and a `result` or an `error` answers one of this
/// runner's requests. A line with an `id` and a `method` is a request from
/// the server. Everything else with a `method` is a notification.
fn map_value(task: &str, value: &Value) -> Parsed {
    let has_id = value.get("id").is_some();
    if has_id && (value.get("result").is_some() || value.get("error").is_some()) {
        return Parsed::Response(value.clone());
    }
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Parsed::Ignored;
    };
    if has_id {
        return Parsed::Request(ServerRequest {
            id: value.get("id").cloned().unwrap_or(Value::Null),
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        });
    }
    let params = value.get("params").unwrap_or(&Value::Null);
    match method {
        "item/started" => match started_item_event(task, params) {
            Some(event) => Parsed::Events(vec![event]),
            None => Parsed::Ignored,
        },
        "item/completed" => match completed_item_event(task, params) {
            Some(event) => Parsed::Events(vec![event]),
            None => Parsed::Ignored,
        },
        "turn/completed" => Parsed::Events(vec![turn_end_event(task, params)]),
        "error" => Parsed::Failure(error_message(params)),
        _ => Parsed::Ignored,
    }
}

/// The failure text of one `error` notification.
fn error_message(params: &Value) -> String {
    for pointer in ["/message", "/error/message"] {
        if let Some(text) = params.pointer(pointer).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    "codex reported an error".to_string()
}

/// The run event of one `item/started` notification, when it has one.
///
/// A started item is the moment the tool runs, so it is the only moment the
/// runner reports. The matching `item/completed` reports nothing, so no
/// command is counted twice. A reasoning item and every unknown item type
/// report nothing; their raw lines stay in the task log.
fn started_item_event(task: &str, params: &Value) -> Option<RunEvent> {
    let item = params.get("item")?;
    let (name, summary) = match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => ("command".to_string(), field_or_json(item, "command", item)),
        "fileChange" => ("file_change".to_string(), file_change_summary(item)),
        "mcpToolCall" => (
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp_tool_call")
                .to_string(),
            mcp_summary(item),
        ),
        "webSearch" => ("web_search".to_string(), field_or_json(item, "query", item)),
        _ => return None,
    };
    Some(RunEvent::Tool {
        task: task.to_string(),
        name,
        summary,
    })
}

/// One string field of an item, or the truncated JSON of `fallback`.
fn field_or_json(item: &Value, field: &str, fallback: &Value) -> String {
    match item.get(field).and_then(Value::as_str) {
        Some(text) => text.to_string(),
        None => truncate(&fallback.to_string()),
    }
}

/// The paths one file-change item touches, as one line.
fn file_change_summary(item: &Value) -> String {
    let paths: Vec<String> = item
        .get("changes")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    change
                        .get("path")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| change.as_str().map(String::from))
                })
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        field_or_json(item, "path", item)
    } else {
        truncate(&paths.join(", "))
    }
}

/// The server and tool of one MCP call, as one line.
fn mcp_summary(item: &Value) -> String {
    let server = item.get("server").and_then(Value::as_str);
    let tool = item.get("tool").and_then(Value::as_str);
    match (server, tool) {
        (Some(server), Some(tool)) => format!("{server}.{tool}"),
        (Some(server), None) => server.to_string(),
        (None, Some(tool)) => tool.to_string(),
        (None, None) => truncate(&item.to_string()),
    }
}

/// The run event of one `item/completed` notification, when it has one.
///
/// Only an agent message reports here: its text is the turn's prose. Every
/// other completed item already reported at its start.
fn completed_item_event(task: &str, params: &Value) -> Option<RunEvent> {
    let item = params.get("item")?;
    if item.get("type").and_then(Value::as_str)? != "agentMessage" {
        return None;
    }
    let text = item.get("text").and_then(Value::as_str)?;
    if text.is_empty() {
        return None;
    }
    Some(RunEvent::Text {
        task: task.to_string(),
        text: text.to_string(),
    })
}

/// The turn end of one `turn/completed` notification.
///
/// Only the status `completed` is a good turn; `failed` and `interrupted`
/// are not. The turn's own error message wins as the summary. Codex reports
/// no per-turn cost, so the cost stays empty.
fn turn_end_event(task: &str, params: &Value) -> RunEvent {
    let status = params
        .pointer("/turn/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = params
        .pointer("/turn/error/message")
        .and_then(Value::as_str)
        .map(String::from);
    RunEvent::TurnEnd {
        task: task.to_string(),
        ok: status == "completed" && message.is_none(),
        summary: message.unwrap_or_else(|| status.to_string()),
        cost_usd: None,
    }
}

/// The runner for every codex stage.
///
/// Every [`Runner::start`] spawns one `codex app-server` child, completes
/// the handshake, opens the thread, starts the first turn, and hands back a
/// [`CodexSession`]. The runner can start many sessions in sequence.
pub struct CodexRunner {
    settings: RoleSettings,
    handshake_timeout: Duration,
}

impl CodexRunner {
    /// A runner that starts the configured codex program.
    pub fn new(settings: RoleSettings) -> Self {
        Self {
            settings,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Set a short handshake timeout for an offline test.
    #[cfg(test)]
    fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Start `job` and return its concrete session.
    ///
    /// This is [`Runner::start`] with the concrete type kept, so a caller
    /// can read [`CodexSession::idle_for`]. On success the thread is open
    /// and the first turn is running.
    pub fn start_session(
        &mut self,
        job: &Job,
        tx: Sender<RunEvent>,
    ) -> anyhow::Result<CodexSession> {
        let args = build_args(job, &self.settings);
        let (cmd_tx, cmd_rx) = channel::<WorkerMsg>();
        let (proc_tx, proc_rx) = channel::<ProcEvent>();
        let spec = RunSpec {
            task: job.task.clone(),
            cwd: job.cwd.clone(),
            program: self.settings.program.clone(),
            args,
            env: Vec::new(),
            log: job.log.clone(),
        };
        let handle = proc::spawn(spec, proc_tx).with_context(|| {
            format!(
                "task {}: failed to start the codex program {}",
                job.task, self.settings.program
            )
        })?;

        let idle_ms = Arc::new(AtomicU64::new(0));
        let started = Instant::now();

        // Forward proc events into the worker's single command channel, so
        // the worker can select between child output and steering commands.
        {
            let cmd_tx = cmd_tx.clone();
            thread::spawn(move || {
                for event in proc_rx {
                    if cmd_tx.send(WorkerMsg::Proc(event)).is_err() {
                        break;
                    }
                }
            });
        }

        let (hs_tx, hs_rx) = channel::<anyhow::Result<()>>();
        let worker = SessionWorker {
            task: job.task.clone(),
            yolo: job.yolo,
            settings: self.settings.clone(),
            handle: Some(handle),
            cwd: job.cwd.display().to_string(),
            resume: job.resume.clone(),
            prompt: job.prompt.clone(),
            timeout: self.handshake_timeout,
            phase: Phase::Initialize {
                deadline: Instant::now() + self.handshake_timeout,
            },
            tx,
            pending: HashMap::new(),
            thread_id: None,
            turn_id: None,
            next_id: FIRST_TURN_ID + 1,
            pending_error: None,
            failed: false,
            idle_ms: Arc::clone(&idle_ms),
            started,
            hs_tx: Some(hs_tx),
        };
        thread::spawn(move || worker.run(cmd_rx));

        match hs_rx.recv_timeout(self.handshake_timeout + HANDSHAKE_WAIT_SLACK) {
            Ok(Ok(())) => Ok(CodexSession {
                task: job.task.clone(),
                cmd_tx,
                idle_ms,
                started,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!(
                "task {}: the codex handshake did not report within {:?}",
                job.task,
                self.handshake_timeout + HANDSHAKE_WAIT_SLACK
            )),
        }
    }
}

impl Runner for CodexRunner {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
        Ok(Box::new(self.start_session(job, tx)?))
    }
}

/// The control handle for one live codex session.
///
/// The methods write protocol lines through the worker thread that owns the
/// child. Dropping the session starts the same stop as [`Session::stop`].
#[derive(Debug)]
pub struct CodexSession {
    task: String,
    cmd_tx: Sender<WorkerMsg>,
    idle_ms: Arc<AtomicU64>,
    started: Instant,
}

impl CodexSession {
    /// How long the session has heard nothing from the agent.
    ///
    /// The runner records the time of the last event; it does not kill
    /// itself. The daemon decides, because only the daemon owns deadlines.
    pub fn idle_for(&self) -> Duration {
        let since_event = Duration::from_millis(self.idle_ms.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(since_event)
    }

    /// Wait for one command result from the worker.
    fn wait_for_worker(
        &self,
        reply_rx: Receiver<anyhow::Result<()>>,
        action: &str,
    ) -> anyhow::Result<()> {
        match reply_rx.recv_timeout(COMMAND_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("task {}: timed out {action}", self.task))
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!(
                "task {}: the codex session worker stopped while {action}",
                self.task
            )),
        }
    }
}

impl Session for CodexSession {
    /// Send an extra user message into the live session.
    ///
    /// The message opens a new turn on the same thread, with a fresh
    /// request id.
    fn send_user(&mut self, text: &str) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.cmd_tx
            .send(WorkerMsg::SendUser {
                text: text.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("task {}: the codex session worker is gone", self.task))?;
        self.wait_for_worker(reply_rx, "writing to the codex child")
    }

    /// Answer the [`RunEvent::Ask`] named by `request_id`.
    ///
    /// Answering an unknown request id is an error, not a panic.
    fn answer(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.cmd_tx
            .send(WorkerMsg::Answer {
                request_id: request_id.to_string(),
                answer,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("task {}: the codex session worker is gone", self.task))?;
        self.wait_for_worker(reply_rx, "answering a codex request")
    }

    /// Stop the session: `turn/interrupt` goes out first, then the
    /// escalation from [`crate::proc`] waits, sends SIGTERM, and finally
    /// SIGKILL. A second stop is a no-op.
    fn stop(&mut self) -> anyhow::Result<()> {
        self.cmd_tx
            .send(WorkerMsg::Stop)
            .map_err(|_| anyhow!("task {}: the codex session worker is gone", self.task))
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerMsg::Stop);
    }
}

/// One message to the worker thread that owns the child.
enum WorkerMsg {
    /// One event from the supervised child, forwarded from the proc channel.
    Proc(ProcEvent),
    /// Open one more turn with this text.
    SendUser {
        /// The user text of the new turn.
        text: String,
        /// Where the write result goes back to the caller.
        reply: Sender<anyhow::Result<()>>,
    },
    /// Answer one pending server request.
    Answer {
        /// The request id the answer must echo.
        request_id: String,
        /// The caller's answer.
        answer: Answer,
        /// Where the answer result goes back to the caller.
        reply: Sender<anyhow::Result<()>>,
    },
    /// Stop the child: the interrupt line first, then the escalation.
    Stop,
}

/// Which part of the start the worker is in.
enum Phase {
    /// Waiting for the `initialize` response, until the deadline.
    Initialize {
        /// The instant after which the job fails.
        deadline: Instant,
    },
    /// Waiting for the `thread/start` or `thread/resume` response.
    Thread {
        /// The instant after which the job fails.
        deadline: Instant,
    },
    /// The thread is open and the first turn runs; lines map normally.
    Running,
}

impl Phase {
    /// The deadline of a start step, or none once the thread is open.
    fn deadline(&self) -> Option<Instant> {
        match self {
            Phase::Initialize { deadline } | Phase::Thread { deadline } => Some(*deadline),
            Phase::Running => None,
        }
    }
}

/// The thread that owns the child and services the session.
struct SessionWorker {
    /// The task id stamped on every event.
    task: String,
    /// Whether approvals are answered without a human.
    yolo: bool,
    /// The role settings, for the model and the effort of every turn.
    settings: RoleSettings,
    /// The child handle, taken away by a stop or an exit.
    handle: Option<ProcHandle>,
    /// The working directory the thread opens in.
    cwd: String,
    /// The thread id to resume, when the job continues one.
    resume: Option<String>,
    /// The prompt of the first turn.
    prompt: String,
    /// The handshake wait, for the timeout message.
    timeout: Duration,
    /// The start phase the worker is in.
    phase: Phase,
    /// Where run events go.
    tx: Sender<RunEvent>,
    /// The open server requests, by their id in string form.
    pending: HashMap<String, (Value, PendingKind)>,
    /// The thread id, known once the thread result arrives.
    thread_id: Option<String>,
    /// The id of the open turn, for the interrupt.
    turn_id: Option<String>,
    /// The next request id this runner mints.
    next_id: i64,
    /// The last `error` notification, until a turn end reports it.
    pending_error: Option<String>,
    /// Whether any `error` notification arrived. The exit reads this.
    failed: bool,
    /// Milliseconds since `started` at the last emitted event.
    idle_ms: Arc<AtomicU64>,
    /// The shared start instant of the session.
    started: Instant,
    /// Where the handshake result goes; consumed once the start ends.
    hs_tx: Option<Sender<anyhow::Result<()>>>,
}

impl SessionWorker {
    /// Serve the child until the command channel closes.
    ///
    /// The worker writes the `initialize` request, then loops over the
    /// merged stream of child events and steering commands. Every start
    /// step runs against one deadline; past it, the job fails.
    fn run(mut self, cmd_rx: Receiver<WorkerMsg>) {
        if let Err(error) = self.write_line(&initialize_request()) {
            let message = format!(
                "task {}: the codex initialize request could not be written: {error}",
                self.task
            );
            self.fail_handshake(message);
            return;
        }
        let mut exit_sent = false;
        loop {
            let message = match self.phase.deadline() {
                Some(deadline) => {
                    if Instant::now() >= deadline {
                        self.fail_deadline();
                        return;
                    }
                    match cmd_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(message) => message,
                        Err(RecvTimeoutError::Timeout) => {
                            self.fail_deadline();
                            return;
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            self.kill_child();
                            return;
                        }
                    }
                }
                None => match cmd_rx.recv() {
                    Ok(message) => message,
                    Err(_) => {
                        self.kill_child();
                        return;
                    }
                },
            };
            let keep_going = match message {
                WorkerMsg::Proc(ProcEvent::Line(line)) => self.on_line(&line),
                WorkerMsg::Proc(ProcEvent::StderrLine(_)) => {
                    // Codex speaks its protocol on stdout; the stderr tee
                    // already reached the task log.
                    true
                }
                WorkerMsg::Proc(ProcEvent::Exit { code, ok }) => {
                    self.on_exit(code, ok, &mut exit_sent)
                }
                WorkerMsg::Proc(ProcEvent::Stopped(outcome)) => {
                    self.on_stopped(outcome, &mut exit_sent)
                }
                WorkerMsg::Proc(ProcEvent::Error(message)) => {
                    eprintln!("task {}: {message}", self.task);
                    true
                }
                WorkerMsg::SendUser { text, reply } => {
                    let result = self.start_turn(&text);
                    let _ = reply.send(result);
                    true
                }
                WorkerMsg::Answer {
                    request_id,
                    answer,
                    reply,
                } => {
                    let result = self.answer_request(&request_id, answer);
                    let _ = reply.send(result);
                    true
                }
                WorkerMsg::Stop => {
                    self.stop_child();
                    true
                }
            };
            if !keep_going {
                return;
            }
        }
    }

    /// Handle one output line. Returns false when the worker must stop.
    fn on_line(&mut self, line: &str) -> bool {
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "task {}: skipped one codex line: malformed line: {error}",
                    self.task
                );
                return true;
            }
        };
        // The open turn is the one the interrupt may stop. A turn opens on
        // its `turn/started` notification and closes on `turn/completed`.
        match value.get("method").and_then(Value::as_str) {
            Some("turn/started") => {
                if let Some(id) = value.pointer("/params/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(id.to_string());
                }
            }
            Some("turn/completed") => self.turn_id = None,
            _ => {}
        }
        match map_value(&self.task, &value) {
            Parsed::Response(response) => self.on_response(&response),
            Parsed::Request(request) => {
                self.on_server_request(request);
                true
            }
            Parsed::Events(events) => {
                for event in events {
                    let event = self.apply_failure(event);
                    self.emit(event);
                }
                true
            }
            Parsed::Failure(message) => {
                self.failed = true;
                self.pending_error = Some(message);
                true
            }
            Parsed::Ignored => true,
        }
    }

    /// Fold a remembered `error` notification into one turn end.
    ///
    /// The error is taken, so only the next turn end reports it. The sticky
    /// `failed` flag stays, because the exit reads it.
    fn apply_failure(&mut self, event: RunEvent) -> RunEvent {
        let RunEvent::TurnEnd {
            task,
            ok,
            summary,
            cost_usd,
        } = event
        else {
            return event;
        };
        match self.pending_error.take() {
            Some(message) => RunEvent::TurnEnd {
                task,
                ok: false,
                summary: message,
                cost_usd,
            },
            None => RunEvent::TurnEnd {
                task,
                ok,
                summary,
                cost_usd,
            },
        }
    }

    /// Handle one response to a request this runner sent.
    ///
    /// Returns false when the worker must stop.
    fn on_response(&mut self, response: &Value) -> bool {
        let id = response.get("id").and_then(Value::as_i64);
        match self.phase {
            Phase::Initialize { .. } if id == Some(INITIALIZE_ID) => self.open_thread(response),
            Phase::Thread { .. } if id == Some(THREAD_ID) => self.first_turn(response),
            _ => {
                if let Some(turn) = response.pointer("/result/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_string());
                }
                if let Some(error) = response.get("error") {
                    eprintln!("task {}: codex refused a request: {error}", self.task);
                }
                true
            }
        }
    }

    /// Finish `initialize` and open the thread.
    ///
    /// A fresh job starts a thread in its working directory; a job with a
    /// thread id resumes that thread instead.
    fn open_thread(&mut self, response: &Value) -> bool {
        if let Some(error) = response.get("error") {
            let message = format!("task {}: codex refused initialize: {error}", self.task);
            self.fail_handshake(message);
            return false;
        }
        let deadline = self
            .phase
            .deadline()
            .unwrap_or_else(|| Instant::now() + self.timeout);
        if let Err(error) = self.write_line(&initialized_notification()) {
            let message = format!(
                "task {}: the codex initialized notification could not be written: {error}",
                self.task
            );
            self.fail_handshake(message);
            return false;
        }
        let request = match self.resume.as_deref() {
            Some(thread_id) => thread_resume_request(thread_id),
            None => thread_start_request(
                &self.cwd,
                &approval_policy(&self.settings),
                &sandbox(&self.settings),
            ),
        };
        if let Err(error) = self.write_line(&request) {
            let message = format!(
                "task {}: the codex thread request could not be written: {error}",
                self.task
            );
            self.fail_handshake(message);
            return false;
        }
        self.phase = Phase::Thread { deadline };
        true
    }

    /// Report the open thread and start the first turn.
    fn first_turn(&mut self, response: &Value) -> bool {
        if let Some(error) = response.get("error") {
            let message = format!("task {}: codex refused the thread: {error}", self.task);
            self.fail_handshake(message);
            return false;
        }
        let Some(thread_id) = response
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
        else {
            let message = format!(
                "task {}: the codex thread response carried no thread id",
                self.task
            );
            self.fail_handshake(message);
            return false;
        };
        self.thread_id = Some(thread_id.to_string());
        self.emit(RunEvent::Started {
            task: self.task.clone(),
            session_id: Some(thread_id.to_string()),
        });
        let prompt = self.prompt.clone();
        let line = turn_start_request(FIRST_TURN_ID, thread_id, &prompt, &self.settings);
        if let Err(error) = self.write_line(&line) {
            let message = format!(
                "task {}: the codex prompt could not be written: {error}",
                self.task
            );
            self.fail_handshake(message);
            return false;
        }
        self.phase = Phase::Running;
        if let Some(hs) = self.hs_tx.take() {
            let _ = hs.send(Ok(()));
        }
        true
    }

    /// Open one more turn with `text` on the open thread.
    fn start_turn(&mut self, text: &str) -> anyhow::Result<()> {
        let thread_id = self
            .thread_id
            .clone()
            .ok_or_else(|| anyhow!("task {}: the codex thread is not open", self.task))?;
        let id = self.next_id;
        self.next_id += 1;
        let line = turn_start_request(id, &thread_id, text, &self.settings);
        self.write_line(&line)
    }

    /// Handle one server request under the yolo policy.
    ///
    /// A question always reaches the caller. An approval and a permission
    /// request are answered at once under yolo. Any other method is refused
    /// with a JSON-RPC method-not-found error.
    fn on_server_request(&mut self, request: ServerRequest) {
        let (tool, input, needs_human, kind) = match request.method.as_str() {
            "item/tool/requestUserInput" => {
                let questions = question_specs(&request.params);
                (
                    "request_user_input".to_string(),
                    json!({
                        "questions": request
                            .params
                            .get("questions")
                            .cloned()
                            .unwrap_or(Value::Null)
                    }),
                    true,
                    PendingKind::Questions(questions),
                )
            }
            "item/commandExecution/requestApproval" => (
                "command_execution".to_string(),
                json!({
                    "command": request.params.get("command").cloned().unwrap_or(Value::Null),
                    "cwd": request.params.get("cwd").cloned().unwrap_or(Value::Null),
                    "reason": request.params.get("reason").cloned().unwrap_or(Value::Null),
                }),
                false,
                PendingKind::Approval,
            ),
            "item/fileChange/requestApproval" => (
                "file_change".to_string(),
                json!({
                    "reason": request.params.get("reason").cloned().unwrap_or(Value::Null),
                    "changes": request.params.get("changes").cloned().unwrap_or(Value::Null),
                }),
                false,
                PendingKind::Approval,
            ),
            "item/permissions/requestApproval" => {
                let requested = request
                    .params
                    .get("permissions")
                    .cloned()
                    .unwrap_or(Value::Null);
                (
                    "permissions".to_string(),
                    json!({"permissions": requested.clone()}),
                    false,
                    PendingKind::Permissions(requested),
                )
            }
            other => {
                eprintln!(
                    "task {}: codex asked for {other}, which aif does not answer",
                    self.task
                );
                let line = unsupported_request_line(&request.id, other);
                if let Err(error) = self.write_line(&line) {
                    eprintln!(
                        "task {}: the codex refusal could not be written: {error}",
                        self.task
                    );
                }
                return;
            }
        };
        if self.yolo && !needs_human {
            let line = match &kind {
                PendingKind::Permissions(requested) => {
                    permissions_line(&request.id, requested, true)
                }
                _ => approval_decision_line(&request.id, true),
            };
            if let Err(error) = self.write_line(&line) {
                eprintln!(
                    "task {}: the automatic codex approval could not be written: {error}",
                    self.task
                );
            }
            return;
        }
        let request_id = id_key(&request.id);
        self.pending.insert(request_id.clone(), (request.id, kind));
        self.emit(RunEvent::Ask {
            task: self.task.clone(),
            request_id,
            tool,
            input,
            suggestions: Value::Null,
            needs_human,
        });
    }

    /// Write one answer for a pending server request.
    fn answer_request(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()> {
        let Some((id, kind)) = self.pending.remove(request_id) else {
            return Err(anyhow!(
                "task {}: no pending codex ask with request id {request_id}",
                self.task
            ));
        };
        let allow = matches!(answer, Answer::Allow { .. });
        let line = match &kind {
            PendingKind::Questions(questions) => {
                if !allow {
                    eprintln!(
                        "task {}: a denied codex question answers every question empty",
                        self.task
                    );
                }
                question_answers_line(&id, questions, &answer)
            }
            PendingKind::Approval => approval_decision_line(&id, allow),
            PendingKind::Permissions(requested) => permissions_line(&id, requested, allow),
        };
        self.write_line(&line)
    }

    /// Emit one run event and stamp the idle clock.
    ///
    /// A gone receiver is a run the daemon dropped; the child keeps going so
    /// the log stays complete.
    fn emit(&self, event: RunEvent) {
        let elapsed = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.idle_ms.store(elapsed, Ordering::Relaxed);
        let _ = self.tx.send(event);
    }

    /// Fail the start because a step passed its deadline.
    fn fail_deadline(&mut self) {
        let step = match self.phase {
            Phase::Initialize { .. } => "initialize response",
            Phase::Thread { .. } => "thread response",
            Phase::Running => "response",
        };
        let message = format!(
            "task {}: the codex handshake got no {step} within {:?}",
            self.task, self.timeout
        );
        self.fail_handshake(message);
    }

    /// Fail a start in progress: report, kill, stop the worker.
    fn fail_handshake(&mut self, message: String) {
        if let Some(hs) = self.hs_tx.take() {
            let _ = hs.send(Err(anyhow!("{message}")));
        }
        self.kill_child();
    }

    /// Handle the child's exit. Returns false when the worker must stop.
    ///
    /// An exit during the start fails the job instead of emitting an exit
    /// event, because the starter returns an error and no exit event may
    /// follow it.
    fn on_exit(&mut self, code: Option<i32>, ok: bool, exit_sent: &mut bool) -> bool {
        // A remembered `error` notification fails the run, whatever the
        // exit status says.
        let ok = ok && !self.failed;
        let mut detail = match code {
            Some(code) => format!("codex exited with code {code}"),
            None => "codex was killed by a signal".to_string(),
        };
        if self.phase.deadline().is_some() {
            let message = format!(
                "task {}: the codex handshake failed: the child exited before its response ({detail})",
                self.task
            );
            self.fail_handshake(message);
            return false;
        }
        if let Some(message) = self.pending_error.as_ref() {
            detail = format!("{detail}: {message}");
        }
        if !*exit_sent {
            *exit_sent = true;
            self.emit(RunEvent::Exit {
                task: self.task.clone(),
                ok,
                detail,
            });
        }
        self.pending.clear();
        self.handle = None;
        true
    }

    /// Handle a stop escalation outcome.
    ///
    /// The real exit event carries the exit code, so a finished escalation
    /// only logs. A failed escalation is the one case where the exit event
    /// would never come, so it emits the run's exit itself.
    fn on_stopped(&mut self, outcome: StopOutcome, exit_sent: &mut bool) -> bool {
        if let StopOutcome::Failed(notes) = &outcome {
            if !*exit_sent {
                *exit_sent = true;
                self.emit(RunEvent::Exit {
                    task: self.task.clone(),
                    ok: false,
                    detail: format!("the codex stop escalation failed: {notes}"),
                });
            }
            self.pending.clear();
            return true;
        }
        eprintln!("task {}: codex stop outcome: {outcome:?}", self.task);
        true
    }

    /// Write one line to the child's stdin.
    fn write_line(&self, line: &str) -> anyhow::Result<()> {
        match self.handle.as_ref() {
            Some(handle) => handle.write_line(line),
            None => Err(anyhow!(
                "task {}: the codex child is no longer running",
                self.task
            )),
        }
    }

    /// Stop the child politely, in three steps.
    ///
    /// An open turn gets `turn/interrupt` first. A turn that already ended
    /// gets nothing: the server answers "no active turn to interrupt", which
    /// is noise. Then the stdin pipe closes, because the transport is
    /// `stdio://` and the app server exits on end of file. The escalation of
    /// [`crate::proc`] stays as the fallback: it waits, sends SIGTERM, and
    /// finally SIGKILL.
    fn stop_child(&mut self) {
        self.pending.clear();
        let interrupt = match (self.thread_id.clone(), self.turn_id.take()) {
            (Some(thread_id), Some(turn_id)) => {
                let id = self.next_id;
                self.next_id += 1;
                Some(turn_interrupt_request(id, &thread_id, &turn_id))
            }
            _ => None,
        };
        if let Some(handle) = self.handle.take() {
            if let Some(line) = interrupt {
                if let Err(error) = handle.write_line(&line) {
                    eprintln!(
                        "task {}: the codex interrupt could not be written: {error}",
                        self.task
                    );
                }
            }
            handle.close_stdin();
            proc::stop_gracefully(handle, false);
        }
    }

    /// Kill the child outright, for a dropped session or a failed start.
    fn kill_child(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Err(error) = handle.kill() {
                eprintln!(
                    "task {}: the codex child could not be killed: {error}",
                    self.task
                );
            }
        }
    }
}

/// The string key of one request id.
///
/// The server numbers its requests, so the key is the number without
/// quotes. A string id keeps its text.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Harness, RoleSettings};
    use crate::model::Stage;
    use std::fs;
    use std::io::Write as TestWrite;
    use std::path::{Path, PathBuf};
    use std::time::Duration as TestDuration;

    use uuid::Uuid;

    /// The program name every fake child is written under.
    const PROGRAM: &str = "codex";

    /// The timeout one test waits for a child event or a file.
    const TEST_TIMEOUT: Duration = TestDuration::from_secs(3);

    /// The task id every test job works for.
    const TASK: &str = "borsuk/review-p142";

    /// The recorded question turn, one server line per line.
    const QUESTION_FIXTURE: &str = include_str!("fixtures/codex-app-server-question.jsonl");

    /// The recorded approval turn, one server line per line.
    const APPROVAL_FIXTURE: &str = include_str!("fixtures/codex-app-server-approval.jsonl");

    /// The `initialize` answer every fake child prints.
    const INIT_RESPONSE: &str = r#"{"id":1,"result":{"userAgent":"fake-codex"}}"#;

    /// The `thread/start` and `thread/resume` answer every fake child prints.
    const THREAD_RESPONSE: &str = r#"{"id":2,"result":{"thread":{"id":"thr-1"}}}"#;

    /// The `turn/start` answer every fake child prints.
    const TURN_RESPONSE: &str =
        r#"{"id":3,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}"#;

    /// The `turn/started` notification, which names the turn to interrupt.
    const TURN_STARTED: &str = r#"{"method":"turn/started","params":{"threadId":"thr-1","turn":{"id":"turn-1","status":"inProgress"}}}"#;

    /// One started command item.
    const COMMAND_ITEM: &str = r#"{"method":"item/started","params":{"threadId":"thr-1","turnId":"turn-1","item":{"type":"commandExecution","id":"exec-1","command":"cargo test","cwd":"/w","status":"inProgress"}}}"#;

    /// One completed agent message.
    const AGENT_MESSAGE: &str = r#"{"method":"item/completed","params":{"threadId":"thr-1","turnId":"turn-1","item":{"type":"agentMessage","id":"msg-1","text":"Working on it.","phase":"final_answer"}}}"#;

    /// One completed turn.
    const TURN_COMPLETED: &str = r#"{"method":"turn/completed","params":{"threadId":"thr-1","turn":{"id":"turn-1","status":"completed","error":null}}}"#;

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-codex-{name}-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        drop(file);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// The first recorded line of `fixture` that carries `needle`.
    fn fixture_line(fixture: &str, needle: &str) -> String {
        fixture
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("the fixture carries no {needle} line"))
            .to_string()
    }

    /// A job pointing into `dir`, in the verified review shape.
    fn job(dir: &Path, resume: Option<&str>, yolo: bool) -> Job {
        Job {
            task: TASK.to_string(),
            stage: Stage::Review,
            repo: "borsuk".to_string(),
            model: "unused".to_string(),
            variant: None,
            prompt: "Review pull request 142.".to_string(),
            cwd: dir.to_path_buf(),
            log: dir.join("task.jsonl"),
            resume: resume.map(String::from),
            yolo,
            allowed_tools: None,
            allowed_permissions: Vec::new(),
        }
    }

    /// The complete role settings the argument test pins.
    fn settings() -> RoleSettings {
        RoleSettings {
            harness: Harness::Codex,
            program: "codex-review".to_string(),
            model: "codex-review-model".to_string(),
            effort: Some("xhigh".to_string()),
            extra_args: vec!["--notice".to_string(), "review".to_string()],
            agent: None,
            profile: Some("reviewer".to_string()),
            permission_mode: None,
            permission_handler: None,
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            strict_mcp: None,
            auto_approve: None,
            approval_policy: Some("never".to_string()),
            sandbox: Some("read-only".to_string()),
        }
    }

    /// Build a runner that starts this test's fake program by absolute path.
    fn test_runner(dir: &Path) -> CodexRunner {
        let mut settings = settings();
        settings.program = dir.join(PROGRAM).display().to_string();
        settings.profile = None;
        settings.extra_args = Vec::new();
        CodexRunner::new(settings).with_handshake_timeout(TestDuration::from_secs(5))
    }

    /// Start the run, retrying the transient `Text file busy` race.
    ///
    /// The test writes its fake child and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file. Production never executes a file it just wrote, so
    /// the retry lives in this helper and not in the runner.
    fn start_with_retry(runner: &mut CodexRunner, job: &Job) -> (CodexSession, Receiver<RunEvent>) {
        let (tx, rx) = channel();
        for _ in 0..100 {
            match runner.start_session(job, tx.clone()) {
                Ok(session) => return (session, rx),
                Err(error)
                    if error
                        .chain()
                        .any(|cause| cause.to_string().contains("Text file busy")) =>
                {
                    std::thread::sleep(TestDuration::from_millis(10));
                }
                Err(error) => panic!("the fake child did not start: {error}"),
            }
        }
        panic!("the fake child did not start after 100 attempts");
    }

    /// Start the run and hand back its failure, retrying `Text file busy`.
    fn failed_start_with_retry(runner: &mut CodexRunner, job: &Job) -> anyhow::Error {
        for _ in 0..100 {
            match runner.start_session(job, channel().0) {
                Err(error)
                    if error
                        .chain()
                        .any(|cause| cause.to_string().contains("Text file busy")) =>
                {
                    std::thread::sleep(TestDuration::from_millis(10));
                }
                Err(error) => return error,
                Ok(_) => panic!("the job must fail, but the session started"),
            }
        }
        panic!("the fake child did not start after 100 attempts");
    }

    /// Collect run events until one satisfies `done`, that one included.
    fn collect_until(rx: &Receiver<RunEvent>, done: impl Fn(&RunEvent) -> bool) -> Vec<RunEvent> {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let mut events = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the run reported nothing in time");
            match rx.recv_timeout(left) {
                Ok(event) => {
                    let stop = done(&event);
                    events.push(event);
                    if stop {
                        return events;
                    }
                }
                Err(error) => {
                    panic!("the run stopped reporting: {error}; collected {events:?}")
                }
            }
        }
    }

    /// Collect run events until [`RunEvent::Exit`] arrives.
    fn collect_until_exit(rx: &Receiver<RunEvent>) -> Vec<RunEvent> {
        collect_until(rx, |event| matches!(event, RunEvent::Exit { .. }))
    }

    /// Wait for the first [`RunEvent::Ask`], skipping any earlier event.
    fn wait_for_ask(rx: &Receiver<RunEvent>) -> RunEvent {
        let events = collect_until(rx, |event| matches!(event, RunEvent::Ask { .. }));
        events.into_iter().next_back().unwrap()
    }

    /// Every log line of the task log.
    fn log_lines(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("task.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Every client line the fake child echoed into the task log.
    ///
    /// The task log tees only what the child prints, so a fake child that
    /// must prove what it received prints it back under a `client ` prefix.
    /// The prefix also keeps the echo out of the protocol: the runner reads
    /// the prefixed line as malformed and skips it.
    fn client_lines(dir: &Path) -> Vec<Value> {
        log_lines(dir)
            .iter()
            .filter_map(|line| line.strip_prefix("client "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// Wait until the task log carries a client line that `wanted` accepts.
    fn wait_for_client_line(dir: &Path, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(line) = client_lines(dir).into_iter().find(&wanted) {
                return line;
            }
            assert!(
                Instant::now() < deadline,
                "the child never echoed the wanted client line: {:?}",
                client_lines(dir)
            );
            std::thread::sleep(TestDuration::from_millis(10));
        }
    }

    /// The child that answers the handshake and runs one full turn.
    ///
    /// It stays alive after the turn, exactly like the real app server, and
    /// exits on the interrupt after it echoes that line.
    fn happy_child(dir: &Path) -> PathBuf {
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"thread/start"'*|*'"thread/resume"'*) printf '%s\n' '__THREAD__' ;;
    *'"turn/interrupt"'*) printf 'client %s\n' "$line"; exit 0 ;;
    *'"turn/start"'*) printf '%s\n' '__TURN__'
      printf '%s\n' '__STARTED__'
      printf '%s\n' '__COMMAND__'
      printf '%s\n' '__MESSAGE__'
      printf '%s\n' '__COMPLETED__' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE)
        .replace("__THREAD__", THREAD_RESPONSE)
        .replace("__TURN__", TURN_RESPONSE)
        .replace("__STARTED__", TURN_STARTED)
        .replace("__COMMAND__", COMMAND_ITEM)
        .replace("__MESSAGE__", AGENT_MESSAGE)
        .replace("__COMPLETED__", TURN_COMPLETED);
        script(dir, PROGRAM, &body)
    }

    /// The child that opens a turn and never ends it.
    ///
    /// It parks inside the turn, so a stop must interrupt it. It echoes the
    /// interrupt line, reports the interrupted turn, and exits.
    fn busy_child(dir: &Path) -> PathBuf {
        let interrupted = TURN_COMPLETED.replace("\"completed\"", "\"interrupted\"");
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"thread/start"'*|*'"thread/resume"'*) printf '%s\n' '__THREAD__' ;;
    *'"turn/interrupt"'*) printf 'client %s\n' "$line"
      printf '%s\n' '__INTERRUPTED__'
      exit 0 ;;
    *'"turn/start"'*) printf '%s\n' '__TURN__'
      printf '%s\n' '__STARTED__'
      printf '%s\n' '__COMMAND__' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE)
        .replace("__THREAD__", THREAD_RESPONSE)
        .replace("__TURN__", TURN_RESPONSE)
        .replace("__STARTED__", TURN_STARTED)
        .replace("__COMMAND__", COMMAND_ITEM)
        .replace("__INTERRUPTED__", &interrupted);
        script(dir, PROGRAM, &body)
    }

    /// The child that echoes every client line and ends each turn at once.
    fn parrot_child(dir: &Path) -> PathBuf {
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  printf 'client %s\n' "$line"
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"thread/start"'*|*'"thread/resume"'*) printf '%s\n' '__THREAD__' ;;
    *'"turn/interrupt"'*) exit 0 ;;
    *'"turn/start"'*) printf '%s\n' '__TURN__'
      printf '%s\n' '__STARTED__'
      printf '%s\n' '__COMPLETED__' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE)
        .replace("__THREAD__", THREAD_RESPONSE)
        .replace("__TURN__", TURN_RESPONSE)
        .replace("__STARTED__", TURN_STARTED)
        .replace("__COMPLETED__", TURN_COMPLETED);
        script(dir, PROGRAM, &body)
    }

    /// The child that sends one recorded server request, echoes the answer
    /// it reads, and then finishes the turn.
    ///
    /// The recorded line goes into a file and the script prints that file,
    /// so the shell never has to quote the recorded text.
    fn asking_child(dir: &Path, ask_line: &str) -> PathBuf {
        let ask_path = dir.join("ask.jsonl");
        fs::write(&ask_path, format!("{ask_line}\n")).unwrap();
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"thread/start"'*|*'"thread/resume"'*) printf '%s\n' '__THREAD__' ;;
    *'"turn/interrupt"'*) exit 0 ;;
    *'"turn/start"'*) printf '%s\n' '__TURN__'
      printf '%s\n' '__STARTED__'
      cat '__ASK__'
      IFS= read -r reply
      printf 'client %s\n' "$reply"
      printf '%s\n' '__MESSAGE__'
      printf '%s\n' '__COMPLETED__' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE)
        .replace("__THREAD__", THREAD_RESPONSE)
        .replace("__TURN__", TURN_RESPONSE)
        .replace("__STARTED__", TURN_STARTED)
        .replace("__ASK__", &ask_path.display().to_string())
        .replace("__MESSAGE__", AGENT_MESSAGE)
        .replace("__COMPLETED__", TURN_COMPLETED);
        script(dir, PROGRAM, &body)
    }

    /// The child that records its argument vector, then behaves happily.
    fn args_child(dir: &Path, argv: &Path) -> PathBuf {
        let happy = fs::read_to_string(happy_child(dir)).unwrap();
        let body = happy.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", argv.display()),
            1,
        );
        script(dir, PROGRAM, &body)
    }

    #[test]
    fn the_argument_vector_matches_the_verified_app_server_invocation() {
        assert_eq!(
            build_args(&job(Path::new("/w"), None, false), &settings()),
            vec![
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-c",
                "features.default_mode_request_user_input=true",
                "-c",
                "suppress_unstable_features_warning=true",
                "--profile",
                "reviewer",
                "--notice",
                "review",
                "app-server",
                "--listen",
                "stdio://",
            ]
        );
    }

    #[test]
    fn a_role_without_an_effort_or_a_profile_drops_those_arguments() {
        let mut settings = settings();
        settings.effort = None;
        settings.profile = None;
        settings.extra_args = Vec::new();
        assert_eq!(
            build_args(&job(Path::new("/w"), None, false), &settings),
            vec![
                "-c",
                "features.default_mode_request_user_input=true",
                "-c",
                "suppress_unstable_features_warning=true",
                "app-server",
                "--listen",
                "stdio://",
            ]
        );
        // Neither the prompt nor the model ever reaches the command line.
        let args = build_args(&job(Path::new("/w"), None, false), &settings);
        assert!(!args.iter().any(|arg| arg.contains("Review pull request")));
        assert!(!args.iter().any(|arg| arg.contains("codex-review-model")));
    }

    #[test]
    fn a_role_without_a_policy_or_a_sandbox_takes_the_documented_defaults() {
        let mut settings = settings();
        settings.approval_policy = None;
        settings.sandbox = None;
        assert_eq!(approval_policy(&settings), "on-request");
        assert_eq!(sandbox(&settings), "workspace-write");
        assert_eq!(approval_policy(&self::settings()), "never");
        assert_eq!(sandbox(&self::settings()), "read-only");
    }

    #[test]
    fn the_handshake_requests_match_the_recorded_client_lines() {
        assert_eq!(
            serde_json::from_str::<Value>(&initialize_request()).unwrap(),
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "aif",
                        "title": "aif",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {"experimentalApi": true},
                },
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&initialized_notification()).unwrap(),
            json!({"method": "initialized", "params": null})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&thread_start_request(
                "/w",
                "on-request",
                "workspace-write"
            ))
            .unwrap(),
            json!({
                "id": 2,
                "method": "thread/start",
                "params": {
                    "cwd": "/w",
                    "approvalPolicy": "on-request",
                    "sandbox": "workspace-write",
                },
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&thread_resume_request("thr-1")).unwrap(),
            json!({"id": 2, "method": "thread/resume", "params": {"threadId": "thr-1"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&turn_start_request(3, "thr-1", "go", &settings()))
                .unwrap(),
            json!({
                "id": 3,
                "method": "turn/start",
                "params": {
                    "threadId": "thr-1",
                    "input": [{"type": "text", "text": "go"}],
                    "model": "codex-review-model",
                    "effort": "xhigh",
                },
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&turn_interrupt_request(9, "thr-1", "turn-1")).unwrap(),
            json!({
                "id": 9,
                "method": "turn/interrupt",
                "params": {"threadId": "thr-1", "turnId": "turn-1"},
            })
        );
    }

    #[test]
    fn the_recorded_question_turn_replays_into_its_events() {
        let mut events = Vec::new();
        let mut requests = Vec::new();
        for line in QUESTION_FIXTURE.lines() {
            match map_line(TASK, line) {
                Parsed::Events(mut parsed) => events.append(&mut parsed),
                Parsed::Request(request) => requests.push(request),
                Parsed::Response(_) | Parsed::Failure(_) | Parsed::Ignored => {}
            }
        }
        assert_eq!(
            events,
            vec![
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "Your preferred colour is blue.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: true,
                    summary: "completed".to_string(),
                    cost_usd: None,
                },
            ]
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "item/tool/requestUserInput");
        assert_eq!(requests[0].id, json!(0));
        assert_eq!(
            question_specs(&requests[0].params),
            vec![QuestionSpec {
                id: "preferred_colour".to_string(),
                header: "Colour".to_string(),
                question: "Which colour do you prefer, red or blue?".to_string(),
            }]
        );
    }

    #[test]
    fn the_recorded_approval_turn_replays_into_its_events() {
        let mut events = Vec::new();
        let mut requests = Vec::new();
        for line in APPROVAL_FIXTURE.lines() {
            match map_line(TASK, line) {
                Parsed::Events(mut parsed) => events.append(&mut parsed),
                Parsed::Request(request) => requests.push(request),
                Parsed::Response(_) | Parsed::Failure(_) | Parsed::Ignored => {}
            }
        }
        let command = "/bin/bash -lc 'touch /tmp/codex-probe-deny-marker'";
        assert_eq!(
            events,
            vec![
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "I will run the specified command.".to_string(),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "command".to_string(),
                    summary: command.to_string(),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "command".to_string(),
                    summary: command.to_string(),
                },
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "Denied.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: true,
                    summary: "completed".to_string(),
                    cost_usd: None,
                },
            ]
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "item/commandExecution/requestApproval");
        assert_eq!(
            requests[0].params.get("command").and_then(Value::as_str),
            Some(command)
        );
    }

    #[test]
    fn a_failed_turn_and_an_error_notification_are_not_ok() {
        let failed = r#"{"method":"turn/completed","params":{"turn":{"id":"t","status":"failed","error":{"message":"the model refused"}}}}"#;
        assert_eq!(
            map_line(TASK, failed),
            Parsed::Events(vec![RunEvent::TurnEnd {
                task: TASK.to_string(),
                ok: false,
                summary: "the model refused".to_string(),
                cost_usd: None,
            }])
        );
        let interrupted =
            r#"{"method":"turn/completed","params":{"turn":{"id":"t","status":"interrupted"}}}"#;
        assert_eq!(
            map_line(TASK, interrupted),
            Parsed::Events(vec![RunEvent::TurnEnd {
                task: TASK.to_string(),
                ok: false,
                summary: "interrupted".to_string(),
                cost_usd: None,
            }])
        );
        assert_eq!(
            map_line(
                TASK,
                r#"{"method":"error","params":{"message":"stream error"}}"#
            ),
            Parsed::Failure("stream error".to_string())
        );
        assert_eq!(
            map_line(
                TASK,
                r#"{"method":"error","params":{"error":{"message":"nested"}}}"#
            ),
            Parsed::Failure("nested".to_string())
        );
        // A malformed line and an unknown notification are both skipped.
        assert_eq!(map_line(TASK, "not json at all"), Parsed::Ignored);
        assert_eq!(
            map_line(TASK, r#"{"method":"warning","params":{"message":"x"}}"#),
            Parsed::Ignored
        );
    }

    #[test]
    fn every_reported_item_type_maps_to_its_tool_event() {
        let started = |item: Value| {
            let line = json!({"method": "item/started", "params": {"item": item}}).to_string();
            match map_line(TASK, &line) {
                Parsed::Events(events) => match &events[..] {
                    [RunEvent::Tool { name, summary, .. }] => Some((name.clone(), summary.clone())),
                    other => panic!("expected one tool event, got {other:?}"),
                },
                Parsed::Ignored => None,
                other => panic!("expected events, got {other:?}"),
            }
        };
        assert_eq!(
            started(json!({"type": "commandExecution", "command": "ls"})),
            Some(("command".to_string(), "ls".to_string()))
        );
        assert_eq!(
            started(json!({
                "type": "fileChange",
                "changes": [{"path": "src/lib.rs"}, {"path": "README.md"}],
            })),
            Some((
                "file_change".to_string(),
                "src/lib.rs, README.md".to_string()
            ))
        );
        assert_eq!(
            started(json!({"type": "mcpToolCall", "server": "docs", "tool": "search"})),
            Some(("search".to_string(), "docs.search".to_string()))
        );
        assert_eq!(
            started(json!({"type": "webSearch", "query": "rust eof"})),
            Some(("web_search".to_string(), "rust eof".to_string()))
        );
        assert_eq!(
            started(json!({"type": "reasoning", "text": "thinking"})),
            None
        );
        assert_eq!(started(json!({"type": "userMessage"})), None);
        // A completed command reports nothing, so no command counts twice.
        let completed = json!({
            "method": "item/completed",
            "params": {"item": {"type": "commandExecution", "command": "ls"}},
        })
        .to_string();
        assert_eq!(map_line(TASK, &completed), Parsed::Ignored);
    }

    #[test]
    fn a_question_answer_maps_headers_texts_and_positions_to_question_ids() {
        let questions = vec![
            QuestionSpec {
                id: "q1".to_string(),
                header: "Colour".to_string(),
                question: "Which colour?".to_string(),
            },
            QuestionSpec {
                id: "q2".to_string(),
                header: "Size".to_string(),
                question: "Which sizes?".to_string(),
            },
        ];
        let line = |input: Value| {
            serde_json::from_str::<Value>(&question_answers_line(
                &json!(0),
                &questions,
                &Answer::Allow {
                    updated_input: Some(input),
                },
            ))
            .unwrap()
        };
        // The header wins, and a list answer stays a list.
        assert_eq!(
            line(json!({"answers": {"Colour": "Blue", "Size": ["S", "M"]}})),
            json!({
                "id": 0,
                "result": {
                    "answers": {
                        "q1": {"answers": ["Blue"]},
                        "q2": {"answers": ["S", "M"]},
                    },
                },
            })
        );
        // The question text is the second way to name a question.
        assert_eq!(
            line(json!({"answers": {"Which colour?": "Red"}})),
            json!({
                "id": 0,
                "result": {
                    "answers": {"q1": {"answers": ["Red"]}, "q2": {"answers": []}},
                },
            })
        );
        // An unknown key falls back to the position of the answer.
        assert_eq!(
            line(json!({"answers": {"anything": "Green"}})),
            json!({
                "id": 0,
                "result": {
                    "answers": {"q1": {"answers": ["Green"]}, "q2": {"answers": []}},
                },
            })
        );
        // A deny answers every question empty.
        assert_eq!(
            serde_json::from_str::<Value>(&question_answers_line(
                &json!(0),
                &questions,
                &Answer::Deny {
                    message: "no".to_string(),
                },
            ))
            .unwrap(),
            json!({
                "id": 0,
                "result": {
                    "answers": {"q1": {"answers": []}, "q2": {"answers": []}},
                },
            })
        );
    }

    #[test]
    fn approvals_permissions_and_unknown_methods_have_verified_answer_lines() {
        assert_eq!(
            serde_json::from_str::<Value>(&approval_decision_line(&json!(0), true)).unwrap(),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&approval_decision_line(&json!(0), false)).unwrap(),
            json!({"id": 0, "result": {"decision": "decline"}})
        );
        let requested = json!({"externalDirectory": ["/tmp"]});
        assert_eq!(
            serde_json::from_str::<Value>(&permissions_line(&json!(1), &requested, true)).unwrap(),
            json!({
                "id": 1,
                "result": {"scope": "turn", "permissions": {"externalDirectory": ["/tmp"]}},
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&permissions_line(&json!(1), &requested, false)).unwrap(),
            json!({"id": 1, "result": {"permissions": {}}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&unsupported_request_line(
                &json!(2),
                "mcpServer/elicitation/request"
            ))
            .unwrap(),
            json!({
                "id": 2,
                "error": {
                    "code": -32601,
                    "message": "aif does not answer mcpServer/elicitation/request",
                },
            })
        );
    }

    #[test]
    fn a_fake_child_drives_the_full_happy_path_and_a_parked_turn_needs_no_interrupt() {
        let dir = temp_dir("happy");
        happy_child(&dir);
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        let turn = collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        assert_eq!(
            turn,
            vec![
                RunEvent::Started {
                    task: TASK.to_string(),
                    session_id: Some("thr-1".to_string()),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "command".to_string(),
                    summary: "cargo test".to_string(),
                },
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "Working on it.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: true,
                    summary: "completed".to_string(),
                    cost_usd: None,
                },
            ]
        );

        // The child is parked. The turn already ended, so the stop writes no
        // interrupt; the closed stdin pipe is what ends the child.
        session.stop().unwrap();
        let rest = collect_until_exit(&rx);
        assert_eq!(
            rest,
            vec![RunEvent::Exit {
                task: TASK.to_string(),
                ok: true,
                detail: "codex exited with code 0".to_string(),
            }]
        );
        assert!(
            !client_lines(&dir)
                .iter()
                .any(|line| line.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
            "a finished turn must get no interrupt"
        );

        // A second stop is a no-op.
        session.stop().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stop_inside_an_open_turn_writes_the_interrupt_first() {
        let dir = temp_dir("interrupt");
        busy_child(&dir);
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        // The tool event proves the turn is open and still running.
        collect_until(&rx, |event| matches!(event, RunEvent::Tool { .. }));
        session.stop().unwrap();

        let rest = collect_until_exit(&rx);
        assert_eq!(
            rest,
            vec![
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: false,
                    summary: "interrupted".to_string(),
                    cost_usd: None,
                },
                RunEvent::Exit {
                    task: TASK.to_string(),
                    ok: true,
                    detail: "codex exited with code 0".to_string(),
                },
            ]
        );
        let interrupt = client_lines(&dir)
            .into_iter()
            .find(|line| line.get("method").and_then(Value::as_str) == Some("turn/interrupt"))
            .expect("an open turn must get turn/interrupt");
        assert_eq!(
            interrupt.get("params"),
            Some(&json!({"threadId": "thr-1", "turnId": "turn-1"}))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recorded_question_reaches_a_human_and_its_answer_names_the_question_id() {
        let dir = temp_dir("question");
        asking_child(
            &dir,
            &fixture_line(QUESTION_FIXTURE, "item/tool/requestUserInput"),
        );
        let mut runner = test_runner(&dir);

        // The job runs under yolo; a question still reaches a human.
        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        let ask = wait_for_ask(&rx);
        let RunEvent::Ask {
            request_id,
            tool,
            input,
            needs_human,
            ..
        } = &ask
        else {
            panic!("expected an ask, got {ask:?}");
        };
        assert_eq!(request_id, "0");
        assert_eq!(tool, "request_user_input");
        assert!(needs_human);
        let questions = input
            .get("questions")
            .and_then(Value::as_array)
            .expect("the ask carries the recorded question list");
        assert_eq!(questions.len(), 1);
        assert_eq!(
            questions[0].get("header").and_then(Value::as_str),
            Some("Colour")
        );
        assert_eq!(
            questions[0].get("id").and_then(Value::as_str),
            Some("preferred_colour")
        );

        session
            .answer(
                "0",
                Answer::Allow {
                    updated_input: Some(json!({"answers": {"Colour": "Blue"}})),
                },
            )
            .unwrap();
        let answer = wait_for_client_line(&dir, |line| line.get("result").is_some());
        assert_eq!(
            answer,
            json!({"id": 0, "result": {"answers": {"preferred_colour": {"answers": ["Blue"]}}}})
        );

        // The turn finishes after the answer.
        let end = collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        assert!(matches!(
            end.last(),
            Some(RunEvent::TurnEnd { ok: true, .. })
        ));
        session.stop().unwrap();
        collect_until_exit(&rx);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recorded_command_approval_opens_a_permission_row_that_allow_accepts() {
        let dir = temp_dir("approve");
        asking_child(
            &dir,
            &fixture_line(APPROVAL_FIXTURE, "item/commandExecution/requestApproval"),
        );
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, false));
        let ask = wait_for_ask(&rx);
        let RunEvent::Ask {
            request_id,
            tool,
            input,
            needs_human,
            ..
        } = &ask
        else {
            panic!("expected an ask, got {ask:?}");
        };
        assert_eq!(request_id, "0");
        assert_eq!(tool, "command_execution");
        assert!(!needs_human, "an approval is a policy row, not a question");
        assert_eq!(
            input.get("command").and_then(Value::as_str),
            Some("/bin/bash -lc 'touch /tmp/codex-probe-deny-marker'")
        );
        assert_eq!(
            input.get("cwd").and_then(Value::as_str),
            Some("/tmp/codex-probe-11p9jfzz")
        );
        assert!(input
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("Do you approve")));

        session
            .answer(
                "0",
                Answer::Allow {
                    updated_input: None,
                },
            )
            .unwrap();
        assert_eq!(
            wait_for_client_line(&dir, |line| line.get("result").is_some()),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
        session.stop().unwrap();
        collect_until_exit(&rx);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_denied_command_approval_declines_the_command() {
        let dir = temp_dir("decline");
        asking_child(
            &dir,
            &fixture_line(APPROVAL_FIXTURE, "item/commandExecution/requestApproval"),
        );
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, false));
        wait_for_ask(&rx);
        session
            .answer(
                "0",
                Answer::Deny {
                    message: "no way".to_string(),
                },
            )
            .unwrap();
        assert_eq!(
            wait_for_client_line(&dir, |line| line.get("result").is_some()),
            json!({"id": 0, "result": {"decision": "decline"}})
        );

        // A second answer for the same id is an error, not a panic.
        let error = session
            .answer(
                "0",
                Answer::Allow {
                    updated_input: None,
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("no pending codex ask"),
            "wrong error: {error}"
        );
        session.stop().unwrap();
        collect_until_exit(&rx);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_yolo_job_accepts_a_command_approval_without_a_row() {
        let dir = temp_dir("yolo-approve");
        asking_child(
            &dir,
            &fixture_line(APPROVAL_FIXTURE, "item/commandExecution/requestApproval"),
        );
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        assert_eq!(
            wait_for_client_line(&dir, |line| line.get("result").is_some()),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
        let events = collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Ask { .. })),
            "yolo must open no row: {events:?}"
        );
        session.stop().unwrap();
        collect_until_exit(&rx);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_resume_job_sends_thread_resume_and_never_thread_start() {
        let dir = temp_dir("resume");
        parrot_child(&dir);
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, Some("thr-1"), true));
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        session.stop().unwrap();
        collect_until_exit(&rx);

        let methods: Vec<String> = client_lines(&dir)
            .iter()
            .filter_map(|line| line.get("method").and_then(Value::as_str))
            .map(String::from)
            .collect();
        assert!(
            methods.contains(&"thread/resume".to_string()),
            "the resume job must resume: {methods:?}"
        );
        assert!(
            !methods.contains(&"thread/start".to_string()),
            "the resume job must not start a thread: {methods:?}"
        );
        let resume = client_lines(&dir)
            .into_iter()
            .find(|line| line.get("method").and_then(Value::as_str) == Some("thread/resume"))
            .unwrap();
        assert_eq!(resume.get("params"), Some(&json!({"threadId": "thr-1"})));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_user_message_starts_another_turn_with_that_text() {
        let dir = temp_dir("send-user");
        parrot_child(&dir);
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        session.send_user("one more turn").unwrap();
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        session.stop().unwrap();
        collect_until_exit(&rx);

        let turns: Vec<Value> = client_lines(&dir)
            .into_iter()
            .filter(|line| line.get("method").and_then(Value::as_str) == Some("turn/start"))
            .collect();
        assert_eq!(turns.len(), 2, "one turn per message: {turns:?}");
        assert_eq!(
            turns[0].pointer("/params/input"),
            Some(&json!([{"type": "text", "text": "Review pull request 142."}]))
        );
        assert_eq!(
            turns[1].pointer("/params/input"),
            Some(&json!([{"type": "text", "text": "one more turn"}]))
        );
        assert_eq!(turns[0].get("id"), Some(&json!(3)));
        assert_eq!(turns[1].get("id"), Some(&json!(4)));
        assert_eq!(
            turns[1].pointer("/params/threadId"),
            Some(&json!("thr-1")),
            "the second turn stays on the same thread"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fake_executable_receives_the_exact_program_and_arguments() {
        let dir = temp_dir("argv");
        let argv = dir.join("argv.txt");
        args_child(&dir, &argv);
        let mut runner = test_runner(&dir);
        let job = job(&dir, None, true);
        let expected = build_args(&job, &runner.settings);

        let (mut session, rx) = start_with_retry(&mut runner, &job);
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        session.stop().unwrap();
        collect_until_exit(&rx);

        let actual: Vec<String> = fs::read_to_string(&argv)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.last().map(String::as_str),
            Some("stdio://"),
            "the transport closes the argument vector"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_silent_child_fails_the_start_at_the_handshake_deadline() {
        let dir = temp_dir("handshake-timeout");
        script(&dir, PROGRAM, "#!/bin/sh\nwhile :; do sleep 0.05; done\n");
        let mut runner = CodexRunner::new({
            let mut settings = settings();
            settings.program = dir.join(PROGRAM).display().to_string();
            settings.profile = None;
            settings.extra_args = Vec::new();
            settings
        })
        .with_handshake_timeout(TestDuration::from_millis(300));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(text.contains("codex handshake"), "wrong error: {text}");
        assert!(text.contains("initialize response"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_child_that_never_opens_the_thread_fails_the_start() {
        let dir = temp_dir("thread-timeout");
        let body = r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '__INIT__'
while :; do sleep 0.05; done
"#
        .replace("__INIT__", INIT_RESPONSE);
        script(&dir, PROGRAM, &body);
        let mut runner = CodexRunner::new({
            let mut settings = settings();
            settings.program = dir.join(PROGRAM).display().to_string();
            settings.profile = None;
            settings.extra_args = Vec::new();
            settings
        })
        .with_handshake_timeout(TestDuration::from_millis(300));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(text.contains("thread response"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_child_that_exits_during_the_handshake_fails_the_start() {
        let dir = temp_dir("handshake-exit");
        // The child waits a moment, so the runner writes its request first
        // and the exit, not a broken pipe, is what fails the start.
        script(&dir, PROGRAM, "#!/bin/sh\nsleep 0.2\nexit 7\n");
        let mut runner = test_runner(&dir);

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(
            text.contains("codex handshake failed"),
            "wrong error: {text}"
        );
        assert!(text.contains("code 7"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_refused_thread_fails_the_start_with_the_server_reason() {
        let dir = temp_dir("thread-error");
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"thread/start"'*) printf '%s\n' '{"id":2,"error":{"code":-32000,"message":"no such cwd"}}' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE);
        script(&dir, PROGRAM, &body);
        let mut runner = test_runner(&dir);

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(text.contains("refused the thread"), "wrong error: {text}");
        assert!(text.contains("no such cwd"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dropping_a_session_stops_its_child() {
        let dir = temp_dir("drop");
        happy_child(&dir);
        let mut runner = test_runner(&dir);

        let (session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        drop(session);

        let events = collect_until_exit(&rx);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Exit { ok: true, .. })
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn idle_for_grows_between_events_and_resets_on_one() {
        let dir = temp_dir("idle");
        parrot_child(&dir);
        let mut runner = test_runner(&dir);

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        std::thread::sleep(TestDuration::from_millis(150));
        assert!(session.idle_for() >= TestDuration::from_millis(100));

        session.send_user("tick").unwrap();
        collect_until(&rx, |event| matches!(event, RunEvent::TurnEnd { .. }));
        assert!(session.idle_for() < TestDuration::from_millis(100));

        session.stop().unwrap();
        collect_until_exit(&rx);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Drive one real codex binary through a question turn.
    ///
    /// The test is ignored by default and needs `AIF_CODEX_PROGRAM` to name
    /// a real codex binary. Run it with
    /// `AIF_CODEX_PROGRAM=/path/to/codex cargo test real_codex_question_roundtrip
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_codex_question_roundtrip() {
        let Ok(program) = std::env::var("AIF_CODEX_PROGRAM") else {
            eprintln!("skipped: set AIF_CODEX_PROGRAM to a real codex binary to run this test");
            return;
        };
        let dir = temp_dir("real");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "aif@example.test"],
            vec!["config", "user.name", "aif"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .status()
                .expect("git must run");
            assert!(status.success(), "git {args:?} failed");
        }

        let mut settings = settings();
        settings.program = program;
        settings.model =
            std::env::var("AIF_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_string());
        settings.effort = None;
        settings.profile = None;
        settings.extra_args = Vec::new();
        settings.approval_policy = None;
        settings.sandbox = None;
        let mut runner = CodexRunner::new(settings);

        let mut job = job(&dir, None, true);
        job.prompt = "Use the request_user_input tool to ask me one question: which colour do I prefer, red or blue? After my answer, reply with one sentence that names my colour.".to_string();
        let (tx, rx) = channel();
        let mut session = runner
            .start_session(&job, tx)
            .expect("the real codex session must start");

        // The real agent needs time to think before it asks.
        let deadline = Instant::now() + TestDuration::from_secs(120);
        let ask = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "codex never asked its question");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Ask {
                    request_id,
                    input,
                    needs_human,
                    ..
                }) => break (request_id, input, needs_human),
                Ok(event) => eprintln!("event: {event:?}"),
                Err(error) => panic!("the run stopped reporting: {error}"),
            }
        };
        let (request_id, input, needs_human) = ask;
        assert!(needs_human, "a codex question needs a human");
        let header = input
            .pointer("/questions/0/header")
            .and_then(Value::as_str)
            .expect("the question carries a header")
            .to_string();
        session
            .answer(
                &request_id,
                Answer::Allow {
                    updated_input: Some(json!({"answers": {header: "Blue"}})),
                },
            )
            .expect("the answer must reach codex");

        let deadline = Instant::now() + TestDuration::from_secs(120);
        let mut named_colour = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the codex turn never ended");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Text { text, .. }) => {
                    eprintln!("text: {text}");
                    named_colour |= text.to_lowercase().contains("blue");
                }
                Ok(RunEvent::TurnEnd { ok, summary, .. }) => {
                    assert!(ok, "the turn must end well: {summary}");
                    break;
                }
                Ok(event) => eprintln!("event: {event:?}"),
                Err(error) => panic!("the run stopped reporting: {error}"),
            }
        }
        assert!(named_colour, "codex never named the answered colour");

        session.stop().unwrap();
        let deadline = Instant::now() + TestDuration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the codex child never exited");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Exit { detail, .. }) => {
                    eprintln!("exit: {detail}");
                    break;
                }
                Ok(event) => eprintln!("event: {event:?}"),
                Err(error) => panic!("the run stopped reporting: {error}"),
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
