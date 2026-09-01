//! The claude runner: refine and release tasks over the claude control
//! channel.
//!
//! One task is one `claude -p --input-format stream-json` child. The child
//! speaks a bidirectional protocol on stdio, so this runner does more than
//! parse output: it completes the `initialize` handshake before the prompt
//! goes out, answers the permission channel, and stops the child with the
//! protocol interrupt before any signal.
//!
//! The yolo policy lives here, client side. With `job.yolo` on, an ordinary
//! `can_use_tool` request is answered `allow` at once and never reaches the
//! caller. A request that carries `requires_user_interaction` is a real
//! question to a human; it always becomes a [`RunEvent::Ask`] with
//! `needs_human` set, yolo or not. The `--dangerously-skip-permissions` flag
//! is never passed: it would close the control channel and take the question
//! path with it.
//!
//! [`crate::proc`] tees every raw line into the task log; this module parses
//! the same lines into [`RunEvent`]s. A malformed or unknown line is logged
//! and skipped, never fatal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::proc::{self, ProcEvent, ProcHandle, RunSpec, StopOutcome};
use crate::runner::{Answer, Job, RunEvent, Runner, Session};

/// The program the runner starts.
const PROGRAM: &str = "claude";

/// The request id of the initialize handshake, per the verified protocol.
const HANDSHAKE_REQUEST_ID: &str = "init-1";

/// How long the initialize handshake may take before the job fails.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// The extra time the starter waits past the worker's own handshake deadline.
const HANDSHAKE_WAIT_SLACK: Duration = Duration::from_secs(5);

/// How long a steering call waits for the worker to report its result.
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The summary length limit for a tool without a usable detail field.
const SUMMARY_CHARS: usize = 120;

/// Build the exact argument vector for one factory session.
///
/// The shape is the verified invocation: `-p --input-format stream-json
/// --output-format stream-json --verbose --model <model> --session-id <uuid>
/// --permission-prompt-tool stdio`. `--permission-prompt-tool stdio` is a
/// hidden but required flag; without it the CLI denies tools by itself and no
/// request ever reaches this runner. A resume run omits `--session-id` and
/// appends `--resume <id>` after the permission flag.
fn build_args(job: &Job, session_id: &str) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--model".to_string(),
        job.model.clone(),
    ];
    if job.resume.is_none() {
        args.push("--session-id".to_string());
        args.push(session_id.to_string());
    }
    args.push("--permission-prompt-tool".to_string());
    args.push("stdio".to_string());
    if let Some(tools) = job.allowed_tools.as_ref() {
        args.push("--tools".to_string());
        args.push(tools.join(","));
        args.push("--strict-mcp-config".to_string());
    }
    if let Some(resume_id) = job.resume.as_deref() {
        args.push("--resume".to_string());
        args.push(resume_id.to_string());
    }
    args
}

/// Build one user message line in the verified wire shape.
fn user_message(text: &str) -> String {
    json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
}

/// Build the initialize handshake request line.
fn initialize_request() -> String {
    json!({
        "type": "control_request",
        "request_id": HANDSHAKE_REQUEST_ID,
        "request": {"subtype": "initialize", "hooks": {}},
    })
    .to_string()
}

/// Build the interrupt request line that stops the current turn.
///
/// Each call mints a fresh request id, per the brief.
fn interrupt_line() -> String {
    json!({
        "type": "control_request",
        "request_id": Uuid::new_v4().to_string(),
        "request": {"subtype": "interrupt"},
    })
    .to_string()
}

/// Build the verified allow response line for one permission request.
fn allow_response(request_id: &str, updated_input: Value) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {"behavior": "allow", "updatedInput": updated_input},
        },
    })
    .to_string()
}

/// Build the verified deny response line for one permission request.
fn deny_response(request_id: &str, message: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {"behavior": "deny", "message": message},
        },
    })
    .to_string()
}

/// Derive a one-line summary for a tool use block.
///
/// The command wins for `Bash`, the file path for `Write` and `Edit`;
/// otherwise the first [`SUMMARY_CHARS`] characters of the input JSON.
fn tool_summary(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" => Some("command"),
        "Write" | "Edit" => Some("file_path"),
        _ => None,
    };
    match key.and_then(|key| input.get(key)).and_then(Value::as_str) {
        Some(detail) => detail.to_string(),
        None => truncate(&input.to_string()),
    }
}

/// Cut `text` to at most [`SUMMARY_CHARS`] characters.
fn truncate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_CHARS {
        text.to_string()
    } else {
        text.chars().take(SUMMARY_CHARS).collect()
    }
}

/// What one output line parses into.
#[derive(Debug)]
enum Parsed {
    /// Ordinary output, already mapped to run events.
    Events(Vec<RunEvent>),
    /// A `can_use_tool` control request that waits for an answer.
    ToolAsk(ToolRequest),
    /// A line this runner does not act on. The raw bytes are in the log.
    Ignored,
}

/// One `can_use_tool` request from the permission channel.
#[derive(Debug)]
struct ToolRequest {
    /// The id the answer must echo.
    request_id: String,
    /// The tool that asks, for example `Write` or `AskUserQuestion`.
    tool: String,
    /// The tool input, verbatim.
    input: Value,
    /// Permission suggestions the agent attached, verbatim.
    suggestions: Value,
    /// Whether a human, not a policy, must answer.
    requires_human: bool,
}

/// Map one output line into what the runner acts on.
///
/// The verified line types are `system`, `assistant`, `result`,
/// `control_request`, and `control_response`. A `system`/`init` line starts
/// the run, an assistant content block yields `Text` or `Tool`, and a
/// `result` line ends one turn. `thinking_tokens`, `rate_limit_event`,
/// `user`, and post-handshake `control_response` lines carry nothing this
/// runner acts on. A malformed line is ignored; the raw bytes are already in
/// the task log.
fn map_line(task: &str, line: &str) -> Parsed {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Ignored;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return Parsed::Ignored,
    };
    if let Some(request) = parse_tool_request(&value) {
        return Parsed::ToolAsk(request);
    }
    match value.get("type").and_then(Value::as_str) {
        Some("system") => match value.get("subtype").and_then(Value::as_str) {
            Some("init") => Parsed::Events(vec![RunEvent::Started {
                task: task.to_string(),
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(String::from),
            }]),
            _ => Parsed::Ignored,
        },
        Some("assistant") => assistant_events(task, &value),
        Some("result") => Parsed::Events(vec![RunEvent::TurnEnd {
            task: task.to_string(),
            ok: value.get("subtype").and_then(Value::as_str) == Some("success"),
            summary: value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        }]),
        _ => Parsed::Ignored,
    }
}

/// Map the content blocks of one assistant line into events.
///
/// A `text` block gives [`RunEvent::Text`]; a `tool_use` block gives
/// [`RunEvent::Tool`] with a summary derived from its input. Anything else in
/// the block list is skipped.
fn assistant_events(task: &str, value: &Value) -> Parsed {
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Parsed::Ignored;
    };
    let mut events = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    events.push(RunEvent::Text {
                        task: task.to_string(),
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                events.push(RunEvent::Tool {
                    task: task.to_string(),
                    name: name.to_string(),
                    summary: tool_summary(name, &input),
                });
            }
            _ => {}
        }
    }
    if events.is_empty() {
        Parsed::Ignored
    } else {
        Parsed::Events(events)
    }
}

/// Read one `can_use_tool` control request out of an output line.
///
/// Returns none for every other line shape. `requires_user_interaction`
/// defaults to false, per the verified protocol.
fn parse_tool_request(value: &Value) -> Option<ToolRequest> {
    if value.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let request = value.get("request")?;
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let request_id = value.get("request_id").and_then(Value::as_str)?.to_string();
    Some(ToolRequest {
        tool: request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        input: request.get("input").cloned().unwrap_or(Value::Null),
        suggestions: request
            .get("permission_suggestions")
            .cloned()
            .unwrap_or(Value::Null),
        requires_human: request
            .get("requires_user_interaction")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        request_id,
    })
}

/// The callback that persists the session id as soon as it is known, so a
/// restart can resume. The runner never touches the worktree module itself.
pub type SessionIdSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The runner for the refine and release stages.
///
/// Every [`Runner::start`] spawns one interactive `claude` child, completes
/// the initialize handshake, and hands back a [`ClaudeSession`]. The runner
/// can start many sessions in sequence.
pub struct ClaudeRunner {
    program: String,
    handshake_timeout: Duration,
    sink: SessionIdSink,
}

impl ClaudeRunner {
    /// A runner that starts the real `claude` program and reports each
    /// session id to `sink` exactly once, as soon as it is known.
    pub fn new(sink: SessionIdSink) -> Self {
        Self {
            program: PROGRAM.to_string(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            sink,
        }
    }

    /// Set the fake claude program for an offline test.
    #[cfg(test)]
    fn with_program(mut self, program: &std::path::Path) -> Self {
        self.program = program.to_string_lossy().into_owned();
        self
    }

    /// Set a short initialize timeout for an offline test.
    #[cfg(test)]
    fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Start `job` and return its concrete session.
    ///
    /// This is [`Runner::start`] with the concrete type kept, so the caller
    /// can read [`ClaudeSession::idle_for`]. On success the session id has
    /// already been reported to the sink exactly once.
    pub fn start_session(
        &mut self,
        job: &Job,
        tx: Sender<RunEvent>,
    ) -> anyhow::Result<ClaudeSession> {
        let session_id = match job.resume.as_deref() {
            Some(resume_id) => resume_id.to_string(),
            None => Uuid::new_v4().to_string(),
        };
        let args = build_args(job, &session_id);
        let (cmd_tx, cmd_rx) = channel::<WorkerMsg>();
        let (proc_tx, proc_rx) = channel::<ProcEvent>();
        let spec = RunSpec {
            task: job.task.clone(),
            cwd: job.cwd.clone(),
            program: self.program.clone(),
            args,
            env: Vec::new(),
            log: job.log.clone(),
        };
        let handle = proc::spawn(spec, proc_tx).with_context(|| {
            format!(
                "task {}: failed to start the claude program {}",
                job.task, self.program
            )
        })?;
        (self.sink)(&session_id);

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
            handle: Some(handle),
            prompt_line: user_message(&job.prompt),
            timeout: self.handshake_timeout,
            phase: Phase::Handshake {
                deadline: Instant::now() + self.handshake_timeout,
            },
            tx,
            pending: HashMap::new(),
            idle_ms: Arc::clone(&idle_ms),
            started,
            hs_tx: Some(hs_tx),
        };
        thread::spawn(move || worker.run(cmd_rx));

        match hs_rx.recv_timeout(self.handshake_timeout + HANDSHAKE_WAIT_SLACK) {
            Ok(Ok(())) => Ok(ClaudeSession {
                task: job.task.clone(),
                cmd_tx,
                idle_ms,
                started,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!(
                "task {}: the claude initialize handshake did not report within {:?}",
                job.task,
                self.handshake_timeout + HANDSHAKE_WAIT_SLACK
            )),
        }
    }
}

impl Runner for ClaudeRunner {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
        Ok(Box::new(self.start_session(job, tx)?))
    }
}

/// The control handle for one live claude session.
///
/// The methods write protocol lines through the worker thread that owns the
/// child. Dropping the session starts the same graceful stop as [`Session::stop`].
#[derive(Debug)]
pub struct ClaudeSession {
    task: String,
    cmd_tx: Sender<WorkerMsg>,
    idle_ms: Arc<AtomicU64>,
    started: Instant,
}

impl ClaudeSession {
    /// How long the session has heard nothing from the agent.
    ///
    /// The runner records the time of the last event; it does not kill
    /// itself. The daemon decides, because only the daemon owns deadlines.
    pub fn idle_for(&self) -> Duration {
        let since_event = Duration::from_millis(self.idle_ms.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(since_event)
    }

    /// Write one raw protocol line through the worker thread and wait for
    /// the write result.
    fn write_via_worker(&self, line: String) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.cmd_tx
            .send(WorkerMsg::Write {
                line,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("task {}: the claude session worker is gone", self.task))?;
        self.wait_for_worker(reply_rx, "writing to the claude child")
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
                "task {}: the claude session worker stopped while {action}",
                self.task
            )),
        }
    }
}

impl Session for ClaudeSession {
    /// Send an extra user message into the live session.
    fn send_user(&mut self, text: &str) -> anyhow::Result<()> {
        self.write_via_worker(user_message(text))
    }

    /// Answer the [`RunEvent::Ask`] named by `request_id`.
    ///
    /// An allow without new input echoes the request's own input as
    /// `updatedInput`. Answering an unknown request id is an error, not a
    /// panic.
    fn answer(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.cmd_tx
            .send(WorkerMsg::Answer {
                request_id: request_id.to_string(),
                answer,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("task {}: the claude session worker is gone", self.task))?;
        self.wait_for_worker(reply_rx, "answering a claude request")
    }

    /// Stop the session: the interrupt line goes out first, then the
    /// escalation from [`crate::proc`] waits, sends SIGTERM, and finally
    /// SIGKILL. A second stop is a no-op.
    fn stop(&mut self) -> anyhow::Result<()> {
        self.cmd_tx
            .send(WorkerMsg::Stop)
            .map_err(|_| anyhow!("task {}: the claude session worker is gone", self.task))
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerMsg::Stop);
    }
}

/// One message to the worker thread that owns the child.
enum WorkerMsg {
    /// One event from the supervised child, forwarded from the proc channel.
    Proc(ProcEvent),
    /// One raw line to write to the child's stdin, with the reply channel
    /// for the write result.
    Write {
        /// The complete protocol line, newline terminated by the writer.
        line: String,
        /// Where the write result goes back to the caller.
        reply: Sender<anyhow::Result<()>>,
    },
    /// Answer one pending permission request.
    Answer {
        /// The request id the answer must echo.
        request_id: String,
        /// The caller's answer.
        answer: Answer,
        /// Where the answer result goes back to the caller.
        reply: Sender<anyhow::Result<()>>,
    },
    /// Stop the child: interrupt line first, then the escalation.
    Stop,
}

/// Which part of the protocol the worker is in.
enum Phase {
    /// Waiting for the initialize control response, until the deadline.
    Handshake {
        /// The instant after which the job fails.
        deadline: Instant,
    },
    /// The handshake is complete; lines map normally.
    Running,
}

/// The thread that owns the child and services the session.
struct SessionWorker {
    /// The task id stamped on every event.
    task: String,
    /// Whether ordinary tool asks are answered `allow` without a human.
    yolo: bool,
    /// The child handle, taken away by a stop or an exit.
    handle: Option<ProcHandle>,
    /// The prompt line written right after the handshake.
    prompt_line: String,
    /// The handshake wait, for the timeout message.
    timeout: Duration,
    /// The protocol phase the worker is in.
    phase: Phase,
    /// Where run events go.
    tx: Sender<RunEvent>,
    /// The request input for each ask that waits for a human answer.
    pending: HashMap<String, Value>,
    /// Milliseconds since `started` at the last emitted event.
    idle_ms: Arc<AtomicU64>,
    /// The shared start instant of the session.
    started: Instant,
    /// Where the handshake result goes; consumed once the handshake ends.
    hs_tx: Option<Sender<anyhow::Result<()>>>,
}

impl SessionWorker {
    /// Serve the child until the command channel closes.
    ///
    /// The worker writes the initialize request, then loops over the merged
    /// stream of child events and steering commands. During the handshake
    /// the loop runs against a deadline; past it, the job fails. When the
    /// session is dropped its handle sends the normal stop command.
    fn run(mut self, cmd_rx: Receiver<WorkerMsg>) {
        if let Err(error) = self.write_line(&initialize_request()) {
            let message = format!(
                "task {}: the claude initialize handshake request could not be written: {error}",
                self.task
            );
            self.fail_handshake(message);
            return;
        }
        let mut exit_sent = false;
        loop {
            let message = match self.phase {
                Phase::Handshake { deadline } => {
                    if Instant::now() >= deadline {
                        let message = format!(
                            "task {}: the claude initialize handshake got no control_response within {:?}",
                            self.task, self.timeout
                        );
                        self.fail_handshake(message);
                        return;
                    }
                    match cmd_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(message) => message,
                        Err(RecvTimeoutError::Timeout) => {
                            let message = format!(
                                "task {}: the claude initialize handshake got no control_response within {:?}",
                                self.task, self.timeout
                            );
                            self.fail_handshake(message);
                            return;
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            self.kill_child();
                            return;
                        }
                    }
                }
                Phase::Running => match cmd_rx.recv() {
                    Ok(message) => message,
                    Err(_) => {
                        self.kill_child();
                        return;
                    }
                },
            };
            let keep_going = match message {
                WorkerMsg::Proc(ProcEvent::Line(line)) => self.on_line(&line),
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
                WorkerMsg::Write { line, reply } => {
                    let _ = reply.send(self.write_line(&line));
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
        let parsed: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "task {}: skipped one claude line: malformed line: {error}",
                    self.task
                );
                return true;
            }
        };
        if matches!(self.phase, Phase::Handshake { .. })
            && parsed.get("type").and_then(Value::as_str) == Some("control_response")
            && parsed
                .pointer("/response/request_id")
                .and_then(Value::as_str)
                == Some(HANDSHAKE_REQUEST_ID)
        {
            return self.complete_handshake(&parsed);
        }
        match map_line(&self.task, line) {
            Parsed::Events(events) => {
                for event in events {
                    self.emit(event);
                }
            }
            Parsed::ToolAsk(request) => self.on_tool_ask(request),
            Parsed::Ignored => {}
        }
        true
    }

    /// Finish the handshake with the first control response.
    ///
    /// A success response completes the handshake and the prompt goes out
    /// before the starter is released. An error response, or a prompt write
    /// that fails, fails the job. Returns false when the worker must stop.
    fn complete_handshake(&mut self, response: &Value) -> bool {
        let payload = response.get("response").cloned().unwrap_or(Value::Null);
        match payload.get("subtype").and_then(Value::as_str) {
            Some("success") => {}
            Some("error") => {
                let reason = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                let message = format!(
                    "task {}: the claude initialize handshake was refused: {reason}",
                    self.task
                );
                self.fail_handshake(message);
                return false;
            }
            _ => {
                let message = format!(
                    "task {}: the claude initialize handshake got an invalid response",
                    self.task
                );
                self.fail_handshake(message);
                return false;
            }
        }
        match self.write_line(&self.prompt_line) {
            Ok(()) => {
                self.phase = Phase::Running;
                if let Some(hs) = self.hs_tx.take() {
                    let _ = hs.send(Ok(()));
                }
                true
            }
            Err(error) => {
                let message = format!(
                    "task {}: the claude initialize handshake finished but the prompt could not be written: {error}",
                    self.task
                );
                self.fail_handshake(message);
                false
            }
        }
    }

    /// Fail a handshake in progress: report, kill, stop the worker.
    fn fail_handshake(&mut self, message: String) {
        if let Some(hs) = self.hs_tx.take() {
            let _ = hs.send(Err(anyhow!("{message}")));
        }
        self.kill_child();
    }

    /// Handle one `can_use_tool` request under the yolo policy.
    fn on_tool_ask(&mut self, request: ToolRequest) {
        if self.yolo && !request.requires_human {
            let response = allow_response(&request.request_id, request.input.clone());
            if let Err(error) = self.write_line(&response) {
                eprintln!(
                    "task {}: the automatic allow could not be written: {error}",
                    self.task
                );
            }
            return;
        }
        self.pending
            .insert(request.request_id.clone(), request.input.clone());
        self.emit(RunEvent::Ask {
            task: self.task.clone(),
            request_id: request.request_id,
            tool: request.tool,
            input: request.input,
            suggestions: request.suggestions,
            needs_human: request.requires_human,
        });
    }

    /// Write one answer for a pending request.
    fn answer_request(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()> {
        let Some(request_input) = self.pending.remove(request_id) else {
            return Err(anyhow!(
                "task {}: no pending claude ask with request id {request_id}",
                self.task
            ));
        };
        let line = match answer {
            Answer::Allow { updated_input } => {
                allow_response(request_id, updated_input.unwrap_or(request_input))
            }
            Answer::Deny { message } => deny_response(request_id, &message),
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

    /// Handle the child's exit. Returns false when the worker must stop.
    ///
    /// An exit during the handshake fails the job instead of emitting an
    /// exit event, because the starter will return an error and no exit
    /// event may follow it. After a normal exit the handle is dropped so the
    /// proc channel can close.
    fn on_exit(&mut self, code: Option<i32>, ok: bool, exit_sent: &mut bool) -> bool {
        let detail = match code {
            Some(code) => format!("claude exited with code {code}"),
            None => "claude was killed by a signal".to_string(),
        };
        if matches!(self.phase, Phase::Handshake { .. }) {
            let message = format!(
                "task {}: the claude initialize handshake failed: the child exited before its control_response ({detail})",
                self.task
            );
            self.fail_handshake(message);
            return false;
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
                    detail: format!("the claude stop escalation failed: {notes}"),
                });
            }
            self.pending.clear();
            return true;
        }
        eprintln!("task {}: claude stop outcome: {outcome:?}", self.task);
        true
    }

    /// Write one line to the child's stdin.
    fn write_line(&self, line: &str) -> anyhow::Result<()> {
        match self.handle.as_ref() {
            Some(handle) => handle.write_line(line),
            None => Err(anyhow!(
                "task {}: the claude child is no longer running",
                self.task
            )),
        }
    }

    /// Stop the child politely: the interrupt line first, then the
    /// escalation from chunk 9, which waits, sends SIGTERM, and finally
    /// SIGKILL. The worker hands the handle over to the escalation thread.
    fn stop_child(&mut self) {
        self.pending.clear();
        if let Some(handle) = self.handle.take() {
            if let Err(error) = handle.write_line(&interrupt_line()) {
                eprintln!(
                    "task {}: the interrupt line could not be written: {error}",
                    self.task
                );
            }
            proc::stop_gracefully(handle, false);
        }
    }

    /// Kill the child outright, for a dropped session or a failed handshake.
    fn kill_child(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Err(error) = handle.kill() {
                eprintln!(
                    "task {}: the claude child could not be killed: {error}",
                    self.task
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stage;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::{Mutex, PoisonError as TestPoisonError};
    use std::time::Duration as TestDuration;

    use serde_json::json;

    /// The timeout one test waits for a child exit or a file.
    const TEST_TIMEOUT: Duration = TestDuration::from_secs(2);

    /// The task id every test job works for.
    const TASK: &str = "borsuk/refine-i7";

    /// The fixed initialize response every fake child prints.
    const INIT_RESPONSE: &str =
        r#"{"type":"control_response","response":{"subtype":"success","request_id":"init-1"}}"#;

    /// A recorded happy-path claude session in the shapes of the verified
    /// protocol: the init system line, assistant text and tool blocks, the
    /// noise lines, one malformed line, and the result line. The run must
    /// survive the noise and the malformed line.
    const FIXTURE: &str = r#"{"type":"system","subtype":"init","session_id":"sess-fix","cwd":"/w","model":"claude-opus-5","tools":["Bash","Write"]}
{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"Refining the ticket."}]}}
{"type":"system","subtype":"thinking_tokens","tokens":12}
{"type":"assistant","message":{"id":"msg_2","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"gh issue view 7"}}]}}
{"type":"assistant","message":{"id":"msg_3","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Write","input":{"file_path":"docs/ticket.md","content":"draft"}}]}}
{"type":"assistant","message":{"id":"msg_4","role":"assistant","content":[{"type":"tool_use","id":"toolu_3","name":"WebSearch","input":{"query":"rust eof"}}]}}
{"type":"rate_limit_event","reset":123}
not json at all
{"type":"control_response","response":{"subtype":"success","request_id":"init-1"}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}
{"type":"result","subtype":"success","result":"Ticket refined.","session_id":"sess-fix","total_cost_usd":0.21,"usage":{"input_tokens":9,"output_tokens":4}}"#;

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-claude-{name}-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
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

    /// A job pointing into `dir`, in the verified refine shape.
    fn job(dir: &Path, resume: Option<&str>, yolo: bool) -> Job {
        Job {
            task: TASK.to_string(),
            stage: Stage::Refine,
            repo: "borsuk".to_string(),
            model: "claude-opus-5[1m]".to_string(),
            variant: None,
            prompt: "Refine issue 7.".to_string(),
            cwd: dir.to_path_buf(),
            log: dir.join("task.jsonl"),
            resume: resume.map(String::from),
            yolo,
            allowed_tools: None,
        }
    }

    /// Build a runner that starts this test's fake program by absolute path.
    fn test_runner(dir: &Path, sink: SessionIdSink) -> ClaudeRunner {
        ClaudeRunner::new(sink).with_program(&dir.join(PROGRAM))
    }

    /// Start the run, retrying the transient `Text file busy` race.
    ///
    /// The test writes its fake child and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file and fail with `Text file busy` for a few
    /// microseconds. Production never executes a file it just wrote, so the
    /// retry lives in this helper and not in the runner.
    fn start_with_retry(
        runner: &mut ClaudeRunner,
        job: &Job,
    ) -> (ClaudeSession, Receiver<RunEvent>) {
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

    /// Start the run and hand back its failure, retrying the transient
    /// `Text file busy` race.
    ///
    /// The test writes its fake child and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file and fail with `Text file busy` for a few
    /// microseconds. Production never executes a file it just wrote, so the
    /// retry lives in this helper and not in the runner.
    fn failed_start_with_retry(runner: &mut ClaudeRunner, job: &Job) -> anyhow::Error {
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

    /// Collect run events until [`RunEvent::Exit`] arrives.
    fn collect_until_exit(rx: &Receiver<RunEvent>) -> Vec<RunEvent> {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let mut events = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the run did not exit in time");
            match rx.recv_timeout(left) {
                Ok(event) => {
                    let exited = matches!(event, RunEvent::Exit { .. });
                    events.push(event);
                    if exited {
                        return events;
                    }
                }
                Err(error) => panic!("the run exit was not reported: {error}"),
            }
        }
    }

    /// Wait until `path` exists and is not empty, then return its text.
    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the child never wrote {}",
                path.display()
            );
            std::thread::sleep(TestDuration::from_millis(10));
        }
    }

    /// Every log line of the task log.
    fn log_lines(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("task.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Every log line that parses to a control response for `request_id`.
    fn responses_in_log(dir: &Path, request_id: &str) -> Vec<Value> {
        log_lines(dir)
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| {
                value.get("type").and_then(Value::as_str) == Some("control_response")
                    && value
                        .pointer("/response/request_id")
                        .and_then(Value::as_str)
                        == Some(request_id)
            })
            .collect()
    }

    /// The fake child that answers the handshake, then echoes one turn back:
    /// the init system line, one assistant text line, and the result line.
    fn happy_child(dir: &Path) -> std::path::PathBuf {
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"interrupt"'*) exit 0 ;;
    *'"user"'*) printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fix","cwd":"/w","model":"claude-opus-5"}'
      printf '%s\n' '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"Working on it."}]}}'
      printf '%s\n' '{"type":"result","subtype":"success","result":"done","session_id":"sess-fix","total_cost_usd":0.4,"usage":{"input_tokens":1}}'
      exit 0 ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE);
        script(dir, PROGRAM, &body)
    }

    /// The quiet child: it answers the handshake, answers any user message
    /// with the init system line, and answers the interrupt with a marker
    /// text before it exits.
    fn init_only_child(dir: &Path) -> std::path::PathBuf {
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"interrupt"'*) printf '%s\n' '{"type":"assistant","message":{"id":"msg_i","role":"assistant","content":[{"type":"text","text":"got-interrupt"}]}}'
      exit 0 ;;
    *'"user"'*) printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fix","cwd":"/w","model":"claude-opus-5"}' ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE);
        script(dir, PROGRAM, &body)
    }

    /// The fake child that emits one `can_use_tool` ask on the first user
    /// message, echoes the answer it reads back verbatim, and ends the turn.
    fn ask_child(dir: &Path, ask_line: &str) -> std::path::PathBuf {
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"interrupt"'*) exit 0 ;;
    *'"user"'*) printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fix","cwd":"/w","model":"claude-opus-5"}'
      printf '%s\n' '__ASK__'
      IFS= read -r reply
      printf '%s\n' "$reply"
      printf '%s\n' '{"type":"result","subtype":"success","result":"done","session_id":"sess-fix","total_cost_usd":0.4,"usage":{"input_tokens":1}}'
      exit 0 ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE)
        .replace("__ASK__", ask_line);
        script(dir, PROGRAM, &body)
    }

    /// The fake child that captures its argument vector and answers the
    /// handshake, then reads until end of file.
    fn args_child(dir: &Path, argv: &Path) -> std::path::PathBuf {
        let body = r#"#!/bin/sh
printf '%s\n' "$@" > '__ARGV__'
printf '%s\n' '__INIT__'
cat > /dev/null
"#
        .replace("__ARGV__", &argv.display().to_string())
        .replace("__INIT__", INIT_RESPONSE);
        script(dir, PROGRAM, &body)
    }

    /// The ordinary `Write` permission request, in the verified shape.
    fn write_ask_line() -> String {
        json!({
            "type": "control_request",
            "request_id": "req-write",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "display_name": "Write",
                "input": {"file_path": "probe.txt", "content": "hi"},
                "description": "probe.txt",
                "tool_use_id": "toolu_1",
                "permission_suggestions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
                ],
            },
        })
        .to_string()
    }

    /// An `AskUserQuestion` request that carries the human flag, in the
    /// verified shape.
    fn question_ask_line() -> String {
        json!({
            "type": "control_request",
            "request_id": "req-ask",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "display_name": "AskUserQuestion",
                "input": {
                    "questions": [
                        {
                            "question": "Which database?",
                            "header": "Database",
                            "options": [
                                {"label": "sqlite", "description": "embedded"},
                                {"label": "postgres", "description": "server"},
                            ],
                            "multiSelect": false,
                        }
                    ]
                },
                "requires_user_interaction": true,
            },
        })
        .to_string()
    }

    #[test]
    fn the_argument_vector_matches_the_verified_invocation() {
        let args = build_args(
            &job(Path::new("/w"), None, true),
            "11111111-2222-4333-8444-555555555555",
        );
        assert_eq!(
            args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-opus-5[1m]",
                "--session-id",
                "11111111-2222-4333-8444-555555555555",
                "--permission-prompt-tool",
                "stdio",
            ]
        );
    }

    #[test]
    fn the_resume_argument_vector_carries_resume_and_no_session_id() {
        let job = job(
            Path::new("/w"),
            Some("11111111-2222-4333-8444-555555555555"),
            true,
        );
        let args = build_args(&job, "99999999-8888-4777-8666-555555555555");
        assert_eq!(
            args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-opus-5[1m]",
                "--permission-prompt-tool",
                "stdio",
                "--resume",
                "11111111-2222-4333-8444-555555555555",
            ]
        );
    }

    #[test]
    fn a_ticket_job_exposes_only_the_read_only_tools() {
        let mut job = job(Path::new("/w"), None, true);
        job.allowed_tools = Some(vec![
            "Read".to_string(),
            "Glob".to_string(),
            "Grep".to_string(),
        ]);
        let args = build_args(&job, "11111111-2222-4333-8444-555555555555");

        let tools = args.iter().position(|arg| arg == "--tools").unwrap();
        assert_eq!(args[tools + 1], "Read,Glob,Grep");
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        for forbidden in ["Write", "Edit", "Bash", "WebFetch", "WebSearch"] {
            assert!(!args.iter().any(|arg| arg.contains(forbidden)));
        }
    }

    #[test]
    fn the_initialize_request_matches_the_verified_shape() {
        assert_eq!(
            serde_json::from_str::<Value>(&initialize_request()).unwrap(),
            json!({
                "type": "control_request",
                "request_id": "init-1",
                "request": {"subtype": "initialize", "hooks": {}},
            })
        );
    }

    #[test]
    fn fixture_replay_produces_the_expected_run_events() {
        let dir = temp_dir("fixture-replay");
        let fixture = dir.join("recorded.jsonl");
        fs::write(&fixture, FIXTURE).unwrap();
        let recorded = fs::read_to_string(&fixture).unwrap();
        let mut events = Vec::new();
        for line in recorded.lines() {
            match map_line("borsuk/refine-i7", line) {
                Parsed::Events(mut parsed) => events.append(&mut parsed),
                Parsed::ToolAsk(_) => panic!("the fixture carries no asks"),
                Parsed::Ignored => {}
            }
        }

        assert_eq!(
            events,
            vec![
                RunEvent::Started {
                    task: TASK.to_string(),
                    session_id: Some("sess-fix".to_string()),
                },
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "Refining the ticket.".to_string(),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "Bash".to_string(),
                    summary: "gh issue view 7".to_string(),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "Write".to_string(),
                    summary: "docs/ticket.md".to_string(),
                },
                RunEvent::Tool {
                    task: TASK.to_string(),
                    name: "WebSearch".to_string(),
                    summary: r#"{"query":"rust eof"}"#.to_string(),
                },
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: true,
                    summary: "Ticket refined.".to_string(),
                    cost_usd: Some(0.21),
                },
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tool_summaries_use_command_path_and_truncated_json() {
        let line_for = |name: &str, input: Value| {
            let line = json!({
                "type": "assistant",
                "message": {"content": [{"type": "tool_use", "name": name, "input": input}]},
            })
            .to_string();
            match map_line("t/x", &line) {
                Parsed::Events(events) => match &events[..] {
                    [RunEvent::Tool { summary, .. }] => summary.clone(),
                    other => panic!("expected one tool event, got {other:?}"),
                },
                other => panic!("expected events, got {other:?}"),
            }
        };
        assert_eq!(
            line_for("Bash", json!({"command": "gh issue view 7"})),
            "gh issue view 7"
        );
        assert_eq!(
            line_for("Write", json!({"file_path": "src/main.rs", "content": "x"})),
            "src/main.rs"
        );
        assert_eq!(
            line_for("Edit", json!({"file_path": "src/lib.rs", "old": "a"})),
            "src/lib.rs"
        );
        // An unknown tool and a Bash without a command fall back to the JSON.
        assert_eq!(
            line_for("Frobnicate", json!({"depth": 3})),
            r#"{"depth":3}"#
        );
        assert_eq!(line_for("Bash", json!({"x": 1})), r#"{"x":1}"#);

        // The fallback is cut to the summary limit, by characters.
        let long = "a".repeat(300);
        let summary = line_for("Frobnicate", json!({"blob": long}));
        assert_eq!(summary.chars().count(), SUMMARY_CHARS);
    }

    #[test]
    fn a_can_use_tool_request_parses_into_a_tool_ask() {
        let parsed = map_line("t/x", &write_ask_line());
        match parsed {
            Parsed::ToolAsk(request) => {
                assert_eq!(request.request_id, "req-write");
                assert_eq!(request.tool, "Write");
                assert_eq!(
                    request.input,
                    json!({"file_path": "probe.txt", "content": "hi"})
                );
                assert!(!request.requires_human);
                assert_eq!(
                    request.suggestions,
                    json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}])
                );
            }
            other => panic!("expected a tool ask, got {other:?}"),
        }
    }

    #[test]
    fn a_can_use_tool_request_defaults_the_human_flag_to_false() {
        let line = json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {"subtype": "can_use_tool", "tool_name": "Read", "input": {}},
        })
        .to_string();
        match map_line("t/x", &line) {
            Parsed::ToolAsk(request) => assert!(!request.requires_human),
            other => panic!("expected a tool ask, got {other:?}"),
        }
    }

    #[test]
    fn a_non_control_line_cannot_create_a_tool_ask() {
        let line = json!({
            "type": "assistant",
            "request_id": "req-1",
            "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {}},
        })
        .to_string();
        assert!(matches!(map_line("t/x", &line), Parsed::Ignored));
    }

    #[test]
    fn the_session_id_callback_fires_once_with_the_minted_id() {
        let dir = temp_dir("callback-fresh");
        let argv = dir.join("argv.txt");
        args_child(&dir, &argv);

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_sink = Arc::clone(&seen);
        let sink: SessionIdSink = Arc::new(move |id| {
            seen_for_sink
                .lock()
                .unwrap_or_else(TestPoisonError::into_inner)
                .push(id.to_string());
        });
        let mut runner = test_runner(&dir, sink);
        let (session, _rx) = start_with_retry(&mut runner, &job(&dir, None, true));

        let called = seen
            .lock()
            .unwrap_or_else(TestPoisonError::into_inner)
            .clone();
        assert_eq!(called.len(), 1, "the callback must fire exactly once");
        let minted = called[0].clone();
        assert!(Uuid::parse_str(&minted).is_ok(), "{minted} is not a uuid");

        // The child saw the minted id in its exact argument vector.
        let child_args: Vec<String> = wait_for_file(&argv).lines().map(String::from).collect();
        assert_eq!(child_args, build_args(&job(&dir, None, true), &minted));

        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_resume_job_resumes_without_minting_a_session_id() {
        let dir = temp_dir("callback-resume");
        let argv = dir.join("argv.txt");
        args_child(&dir, &argv);
        let resume_id = "11111111-2222-4333-8444-555555555555";

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_sink = Arc::clone(&seen);
        let sink: SessionIdSink = Arc::new(move |id| {
            seen_for_sink
                .lock()
                .unwrap_or_else(TestPoisonError::into_inner)
                .push(id.to_string());
        });
        let mut runner = test_runner(&dir, sink);
        let (session, _rx) = start_with_retry(&mut runner, &job(&dir, Some(resume_id), true));

        let called = seen
            .lock()
            .unwrap_or_else(TestPoisonError::into_inner)
            .clone();
        assert_eq!(called, vec![resume_id.to_string()]);

        let child_args: Vec<String> = wait_for_file(&argv).lines().map(String::from).collect();
        assert_eq!(
            child_args,
            build_args(&job(&dir, Some(resume_id), true), resume_id)
        );
        assert!(!child_args.contains(&"--session-id".to_string()));

        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_control_response_fails_the_job_naming_the_handshake() {
        let dir = temp_dir("handshake-timeout");
        script(&dir, PROGRAM, "#!/bin/sh\nwhile :; do sleep 0.05; done\n");
        let mut runner = test_runner(&dir, Arc::new(|_| {}))
            .with_handshake_timeout(TestDuration::from_millis(300));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        assert!(
            error.to_string().contains("initialize handshake"),
            "wrong error: {error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_noisy_child_cannot_postpone_the_handshake_timeout() {
        let dir = temp_dir("handshake-noise");
        let body = r#"#!/bin/sh
IFS= read -r initialize
while :; do
  printf '%s\n' '{"type":"rate_limit_event","reset":123}'
done
"#;
        script(&dir, PROGRAM, body);
        let mut runner = test_runner(&dir, Arc::new(|_| {}))
            .with_handshake_timeout(TestDuration::from_millis(300));
        let started = Instant::now();

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let elapsed = started.elapsed();
        assert!(
            elapsed < TestDuration::from_secs(2),
            "the handshake ignored its deadline for {elapsed:?}"
        );
        assert!(
            error.to_string().contains("within 300ms"),
            "wrong error: {error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unrelated_control_response_does_not_finish_the_handshake() {
        let dir = temp_dir("handshake-unrelated");
        let body = r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"other"}}'
IFS= read -r prompt
"#;
        script(&dir, PROGRAM, body);
        let mut runner = test_runner(&dir, Arc::new(|_| {}))
            .with_handshake_timeout(TestDuration::from_millis(300));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        assert!(
            error.to_string().contains("initialize handshake"),
            "wrong error: {error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_matching_response_without_success_fails_the_handshake() {
        let dir = temp_dir("handshake-invalid");
        let body = r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"type":"control_response","response":{"request_id":"init-1"}}'
cat > /dev/null
"#;
        script(&dir, PROGRAM, body);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(text.contains("initialize handshake"), "wrong error: {text}");
        assert!(text.contains("invalid response"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_error_control_response_fails_the_handshake() {
        let dir = temp_dir("handshake-error");
        let body = r#"#!/bin/sh
printf '%s\n' '{"type":"control_response","response":{"subtype":"error","request_id":"init-1","error":"nope"}}'
cat > /dev/null
"#;
        script(&dir, PROGRAM, body);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        let text = error.to_string();
        assert!(text.contains("initialize handshake"), "wrong error: {text}");
        assert!(text.contains("nope"), "wrong error: {text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_child_that_exits_during_the_handshake_fails_the_job() {
        let dir = temp_dir("handshake-exit");
        script(&dir, PROGRAM, "#!/bin/sh\nexit 7\n");
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let error = failed_start_with_retry(&mut runner, &job(&dir, None, true));
        assert!(
            error.to_string().contains("initialize handshake"),
            "wrong error: {error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fake_child_drives_the_full_happy_path() {
        let dir = temp_dir("wiring");
        happy_child(&dir);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        let events = collect_until_exit(&rx);

        assert_eq!(
            events,
            vec![
                RunEvent::Started {
                    task: TASK.to_string(),
                    session_id: Some("sess-fix".to_string()),
                },
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "Working on it.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: TASK.to_string(),
                    ok: true,
                    summary: "done".to_string(),
                    cost_usd: Some(0.4),
                },
                RunEvent::Exit {
                    task: TASK.to_string(),
                    ok: true,
                    detail: "claude exited with code 0".to_string(),
                },
            ]
        );
        session.stop().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_lines_reach_the_child_in_the_exact_wire_shape() {
        let dir = temp_dir("send-user");
        // The parrot echoes every non-handshake line back verbatim and exits
        // on the interrupt, so the log carries the exact lines we wrote.
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '__INIT__' ;;
    *'"interrupt"'*) exit 0 ;;
    *) printf '%s\n' "$line" ;;
  esac
done
"#
        .replace("__INIT__", INIT_RESPONSE);
        script(&dir, PROGRAM, &body);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        session.send_user("one more turn").unwrap();
        session.stop().unwrap();
        let events = collect_until_exit(&rx);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Exit { ok: true, .. })
        ));

        let logged = log_lines(&dir);
        assert!(
            logged.contains(&user_message("Refine issue 7.")),
            "the prompt line never reached the child: {logged:?}"
        );
        assert!(
            logged.contains(&user_message("one more turn")),
            "the extra user line never reached the child: {logged:?}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn yolo_auto_allows_an_ordinary_ask_in_the_verified_shape() {
        let dir = temp_dir("yolo-allow");
        ask_child(&dir, &write_ask_line());
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        let events = collect_until_exit(&rx);
        session.stop().unwrap();

        // No Ask event may reach the caller under yolo.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Ask { .. }))
                .count(),
            0
        );
        assert!(matches!(events.first(), Some(RunEvent::Started { .. })));
        assert!(matches!(
            events.last(),
            Some(RunEvent::Exit { ok: true, .. })
        ));

        // The exact answer line, echoed back verbatim by the fake child.
        let answers = responses_in_log(&dir, "req-write");
        assert_eq!(answers.len(), 1, "one answer expected: {answers:?}");
        assert_eq!(
            answers[0],
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "req-write",
                    "response": {
                        "behavior": "allow",
                        "updatedInput": {"file_path": "probe.txt", "content": "hi"},
                    },
                },
            })
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_human_question_is_never_auto_answered_even_under_yolo() {
        let dir = temp_dir("yolo-question");
        ask_child(&dir, &question_ask_line());
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));

        // The ask reaches the caller as a human question, yolo or not.
        let (request_id, tool, _input, _suggestions, needs_human) = wait_for_ask(&rx);
        assert_eq!(request_id, "req-ask");
        assert_eq!(tool, "AskUserQuestion");
        assert!(needs_human);
        // The runner wrote no answer of its own: the child is still waiting.
        assert!(
            responses_in_log(&dir, "req-ask").is_empty(),
            "the question was answered without a human"
        );

        // The human answer goes out in the verified shape and the turn ends.
        session
            .answer(
                "req-ask",
                Answer::Allow {
                    updated_input: Some(json!({"answers": {"Database": "postgres"}})),
                },
            )
            .unwrap();
        collect_until_exit(&rx);

        let answers = responses_in_log(&dir, "req-ask");
        assert_eq!(answers.len(), 1, "one answer expected: {answers:?}");
        assert_eq!(
            answers[0],
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "req-ask",
                    "response": {
                        "behavior": "allow",
                        "updatedInput": {"answers": {"Database": "postgres"}},
                    },
                },
            })
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_ordinary_ask_reaches_the_caller_and_a_deny_goes_out() {
        let dir = temp_dir("deny");
        ask_child(&dir, &write_ask_line());
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, false));

        let (request_id, _tool, input, suggestions, needs_human) = wait_for_ask(&rx);
        assert_eq!(request_id, "req-write");
        assert!(!needs_human);
        assert_eq!(input, json!({"file_path": "probe.txt", "content": "hi"}));
        assert_eq!(
            suggestions,
            json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}])
        );

        session
            .answer(
                "req-write",
                Answer::Deny {
                    message: "no way".to_string(),
                },
            )
            .unwrap();
        collect_until_exit(&rx);

        let answers = responses_in_log(&dir, "req-write");
        assert_eq!(answers.len(), 1, "one answer expected: {answers:?}");
        assert_eq!(
            answers[0],
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "req-write",
                    "response": {"behavior": "deny", "message": "no way"},
                },
            })
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unknown_request_id_is_an_error_and_a_plain_allow_echoes_the_input() {
        let dir = temp_dir("unknown-id");
        ask_child(&dir, &write_ask_line());
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, false));
        let ask = wait_for_ask(&rx);

        let error = session
            .answer(
                "no-such-id",
                Answer::Deny {
                    message: "late".to_string(),
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("no-such-id"),
            "wrong error: {error}"
        );

        // A plain allow echoes the request's own input as updatedInput.
        session
            .answer(
                "req-write",
                Answer::Allow {
                    updated_input: None,
                },
            )
            .unwrap();
        collect_until_exit(&rx);

        let answers = responses_in_log(&dir, "req-write");
        assert_eq!(answers.len(), 1, "one answer expected: {answers:?}");
        assert_eq!(
            answers[0].pointer("/response/response/updatedInput"),
            Some(&json!({"file_path": "probe.txt", "content": "hi"}))
        );
        drop(ask);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_clears_every_pending_ask() {
        let dir = temp_dir("stop-clears-asks");
        ask_child(&dir, &write_ask_line());
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, false));
        let ask = wait_for_ask(&rx);
        session.stop().unwrap();

        let error = session
            .answer(
                "req-write",
                Answer::Deny {
                    message: "too late".to_string(),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("no pending claude ask"));
        collect_until_exit(&rx);
        drop(ask);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_writes_the_interrupt_line_before_any_signal() {
        let dir = temp_dir("stop");
        init_only_child(&dir);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        session.stop().unwrap();

        let events = collect_until_exit(&rx);
        // The marker text proves the interrupt line reached the child, and
        // the natural code-0 exit proves it went out before any signal.
        assert_eq!(
            events,
            vec![
                RunEvent::Started {
                    task: TASK.to_string(),
                    session_id: Some("sess-fix".to_string()),
                },
                RunEvent::Text {
                    task: TASK.to_string(),
                    text: "got-interrupt".to_string(),
                },
                RunEvent::Exit {
                    task: TASK.to_string(),
                    ok: true,
                    detail: "claude exited with code 0".to_string(),
                },
            ]
        );

        // A second stop is a no-op.
        session.stop().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dropping_a_session_stops_its_child_with_an_interrupt() {
        let dir = temp_dir("drop");
        init_only_child(&dir);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));
        drop(session);

        let events = collect_until_exit(&rx);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                RunEvent::Text { text, .. } if text == "got-interrupt"
            )
        }));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_interrupt_line_carries_a_fresh_uuid_request_id() {
        let first = serde_json::from_str::<Value>(&interrupt_line()).unwrap();
        let second = serde_json::from_str::<Value>(&interrupt_line()).unwrap();
        for line in [first, second] {
            assert_eq!(
                line.get("type").and_then(Value::as_str),
                Some("control_request")
            );
            assert_eq!(
                line.pointer("/request/subtype").and_then(Value::as_str),
                Some("interrupt")
            );
            let request_id = line
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                Uuid::parse_str(request_id).is_ok(),
                "{request_id} is not a uuid"
            );
        }
    }

    #[test]
    fn idle_for_grows_between_events_and_resets_on_one() {
        let dir = temp_dir("idle");
        // The quiet child answers the handshake, then answers one user
        // message with the init system line, so the session emits an event
        // on demand and stays alive on the interrupt exit.
        init_only_child(&dir);
        let mut runner =
            test_runner(&dir, Arc::new(|_| {})).with_handshake_timeout(TestDuration::from_secs(5));

        let (mut session, rx) = start_with_retry(&mut runner, &job(&dir, None, true));

        // The prompt already produced one Started event; drain it first.
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the first Started event never arrived");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Started { .. }) => break,
                Ok(_) => {}
                Err(error) => panic!("the first Started was not reported: {error}"),
            }
        }
        assert!(
            session.idle_for() < TestDuration::from_secs(2),
            "a fresh session is not idle"
        );
        std::thread::sleep(TestDuration::from_millis(150));
        assert!(session.idle_for() >= TestDuration::from_millis(100));

        // One emitted event resets the idle clock to about zero.
        session.send_user("tick").unwrap();
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the reset Started never arrived");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Started { .. }) => break,
                Ok(_) => {}
                Err(error) => panic!("the reset event was not reported: {error}"),
            }
        }
        assert!(session.idle_for() < TestDuration::from_millis(100));

        session.stop().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    /// Wait for the first [`RunEvent::Ask`], skipping any earlier events.
    /// Returns the ask's request id, tool, input, suggestions, and human
    /// flag.
    fn wait_for_ask(rx: &Receiver<RunEvent>) -> (String, String, Value, Value, bool) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the ask never arrived");
            match rx.recv_timeout(left) {
                Ok(RunEvent::Ask {
                    task: _,
                    request_id,
                    tool,
                    input,
                    suggestions,
                    needs_human,
                }) => return (request_id, tool, input, suggestions, needs_human),
                Ok(_) => {}
                Err(error) => panic!("the ask was not reported: {error}"),
            }
        }
    }
}
