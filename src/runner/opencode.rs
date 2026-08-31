//! The opencode runner: implement and review tasks as one-shot children.
//!
//! One task is one `opencode run` child. [`crate::proc`] tees the child's
//! raw NDJSON into the task log; this module parses the same lines into
//! [`RunEvent`]s: a `step_start` line starts the run, a `text` line carries
//! assistant text, a part with type `tool` carries tool activity, and a
//! `step_finish` line ends one step. One run can carry several steps, so a
//! step ending is not the task ending; the task ends only when the process
//! exits, as [`RunEvent::Exit`]. A malformed or unknown line is logged and
//! skipped, never fatal.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use serde_json::Value;

use crate::proc::{self, ProcEvent, ProcHandle, RunSpec};
use crate::runner::{Job, RunEvent, Runner, Session};

/// The program the runner starts.
const PROGRAM: &str = "opencode";

/// The summary length limit for a tool part without a usable title.
const SUMMARY_CHARS: usize = 120;

/// Build the exact argument vector for one factory task.
///
/// The shape is the verified invocation: `run --format json --auto --agent
/// build -m <model> [--variant <v>] [--session <id>] --dir <cwd> <prompt>`.
/// `--auto` is always present, because yolo is the factory policy and the
/// run is one-shot. A `Some` `job.resume` adds `--session <id>`, so the
/// child continues that opencode conversation. `None` starts a fresh one.
fn build_args(job: &Job) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--auto".to_string(),
        "--agent".to_string(),
        "build".to_string(),
        "-m".to_string(),
        job.model.clone(),
    ];
    if let Some(variant) = &job.variant {
        args.push("--variant".to_string());
        args.push(variant.clone());
    }
    if let Some(session) = &job.resume {
        args.push("--session".to_string());
        args.push(session.clone());
    }
    args.push("--dir".to_string());
    args.push(job.cwd.display().to_string());
    args.push(job.prompt.clone());
    args
}

/// The runner for the implement and review stages.
///
/// Every [`Runner::start`] spawns one short-lived child that runs the whole
/// task and exits on its own.
#[derive(Debug, Clone, Copy)]
pub struct OpenCodeRunner;

impl OpenCodeRunner {
    /// A runner that starts the real `opencode` program.
    pub fn new() -> Self {
        Self
    }
}

impl Default for OpenCodeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner for OpenCodeRunner {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
        let spec = RunSpec {
            task: job.task.clone(),
            cwd: job.cwd.clone(),
            program: PROGRAM.to_string(),
            args: build_args(job),
            env: Vec::new(),
            log: job.log.clone(),
        };
        let (proc_tx, proc_rx) = channel::<ProcEvent>();
        let handle = proc::spawn(spec, proc_tx)?;
        let task = job.task.clone();
        thread::spawn(move || forward_events(task, proc_rx, tx));
        Ok(Box::new(OpenCodeSession {
            handle: Some(handle),
        }))
    }
}

/// The control handle for one opencode child.
///
/// opencode is one-shot and has no steering channel, so `send_user` and
/// `answer` keep the trait defaults, which refuse steering.
struct OpenCodeSession {
    handle: Option<ProcHandle>,
}

impl Session for OpenCodeSession {
    /// Kill the one-shot child. A second stop is a no-op.
    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(handle) = self.handle.as_ref() {
            handle.kill()?;
        }
        self.handle = None;
        Ok(())
    }
}

/// Map proc events into run events until the child exits.
///
/// The parser keeps the step and session state across lines. The child's
/// exit becomes the one [`RunEvent::Exit`]; a stream that closes without an
/// exit becomes a synthetic failed exit, so the daemon always sees exactly
/// one exit per run.
fn forward_events(task: String, rx: Receiver<ProcEvent>, tx: Sender<RunEvent>) {
    let mut parser = NdjsonParser::new(task.as_str());
    let mut exited = false;
    for event in rx {
        match event {
            ProcEvent::Line(line) => {
                for run_event in parser.parse_line(&line) {
                    if tx.send(run_event).is_err() {
                        // The daemon dropped this run; stop feeding it.
                        return;
                    }
                }
            }
            ProcEvent::Exit { code, ok } => {
                let detail = match code {
                    Some(code) => format!("opencode exited with code {code}"),
                    None => "opencode was killed by a signal".to_string(),
                };
                exited = true;
                if tx
                    .send(RunEvent::Exit {
                        task: task.clone(),
                        ok,
                        detail,
                    })
                    .is_err()
                {
                    return;
                }
                break;
            }
            ProcEvent::Error(message) => eprintln!("task {task}: {message}"),
            ProcEvent::Stopped(outcome) => {
                eprintln!("task {task}: unexpected opencode stop outcome: {outcome:?}")
            }
        }
    }
    if !exited {
        let _ = tx.send(RunEvent::Exit {
            task,
            ok: false,
            detail: "the opencode event stream ended without an exit".to_string(),
        });
    }
}

/// The NDJSON parser for one opencode run.
///
/// It holds the run-wide state: the task id stamped on every event, whether
/// `Started` went out, and the first session id the output carried.
#[derive(Debug, Clone)]
struct NdjsonParser {
    task: String,
    started: bool,
    session_id: Option<String>,
}

impl NdjsonParser {
    /// A parser that emits events for `task`.
    fn new(task: impl AsRef<str>) -> Self {
        Self {
            task: task.as_ref().to_string(),
            started: false,
            session_id: None,
        }
    }

    /// Parse one output line into zero or more run events.
    ///
    /// The verified line types are `step_start`, `text`, and `step_finish`.
    /// Any line with a tool part yields a `Tool` event. The name sits at
    /// `part.tool`, and the state sits under `part.state`. An empty line
    /// produces nothing. A malformed line, a line without a usable shape,
    /// and an unknown line type without a tool part are logged to stderr and
    /// skipped. The raw line stays in the task log.
    fn parse_line(&mut self, line: &str) -> Vec<RunEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                self.log_skipped(&format!("malformed line: {error}"));
                return Vec::new();
            }
        };
        let part = value.get("part").cloned().unwrap_or(Value::Null);
        if self.session_id.is_none() {
            let id = value
                .get("sessionID")
                .and_then(Value::as_str)
                .or_else(|| part.get("sessionID").and_then(Value::as_str));
            if let Some(id) = id {
                self.session_id = Some(id.to_string());
            }
        }
        if part.get("type").and_then(Value::as_str) == Some("tool") {
            return self.on_tool_part(&part);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("step_start") => self.on_step_start(),
            Some("text") => self.on_text_part(&part),
            Some("step_finish") => self.on_step_finish(&part),
            _ => {
                self.log_skipped(&match value.get("type") {
                    Some(Value::String(kind)) => format!("unknown line type \"{kind}\""),
                    _ => "line without a usable type".to_string(),
                });
                Vec::new()
            }
        }
    }

    /// Emit `Started` once, on the first step start of the run.
    fn on_step_start(&mut self) -> Vec<RunEvent> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![RunEvent::Started {
            task: self.task.clone(),
            session_id: self.session_id.clone(),
        }]
    }

    /// Emit `Text` for an assistant text part.
    fn on_text_part(&mut self, part: &Value) -> Vec<RunEvent> {
        match part.get("text").and_then(Value::as_str) {
            Some(text) => vec![RunEvent::Text {
                task: self.task.clone(),
                text: text.to_string(),
            }],
            None => {
                self.log_skipped("a text line carries no part.text");
                Vec::new()
            }
        }
    }

    /// Emit `Tool` with the tool name and a one-line summary.
    fn on_tool_part(&mut self, part: &Value) -> Vec<RunEvent> {
        let name = part
            .get("tool")
            .or_else(|| part.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        vec![RunEvent::Tool {
            task: self.task.clone(),
            name,
            summary: tool_summary(part),
        }]
    }

    /// Emit `TurnEnd` for one finished step.
    ///
    /// One run carries several steps, so several `TurnEnd` events can
    /// precede the one [`RunEvent::Exit`]. A step ending is not the task
    /// ending. A step failed only when its reason names an error; the exit
    /// code of the whole child is reported separately as [`RunEvent::Exit`].
    fn on_step_finish(&mut self, part: &Value) -> Vec<RunEvent> {
        let reason = part.get("reason").and_then(Value::as_str);
        vec![RunEvent::TurnEnd {
            task: self.task.clone(),
            ok: reason != Some("error"),
            summary: reason.unwrap_or("step finished").to_string(),
            cost_usd: part.get("cost").and_then(Value::as_f64),
        }]
    }

    /// Log one skipped line to stderr. The raw line is already in the log.
    fn log_skipped(&self, reason: &str) {
        eprintln!("task {}: skipped one opencode line: {reason}", self.task);
    }
}

/// Derive a one-line summary for a tool part.
///
/// The title from the tool state wins, then a bare title; otherwise the
/// first [`SUMMARY_CHARS`] characters of the part itself.
fn tool_summary(part: &Value) -> String {
    match part
        .pointer("/state/title")
        .or_else(|| part.get("title"))
        .and_then(Value::as_str)
    {
        Some(title) => title.to_string(),
        None => truncate(&part.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Instant;
    use uuid::Uuid;

    /// The timeout one test waits for a child exit.
    const TEST_TIMEOUT: u64 = 2;

    /// Serializes tests that replace the process `PATH`.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// Restores `PATH` after one fake-program test.
    struct PathGuard {
        original: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl PathGuard {
        /// Put `dir` first on `PATH` until this guard drops.
        fn prepend(dir: &Path) -> Self {
            let lock = PATH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let original = env::var_os("PATH");
            let mut paths = vec![dir.to_path_buf()];
            if let Some(value) = original.as_ref() {
                paths.extend(env::split_paths(value));
            }
            env::set_var("PATH", env::join_paths(paths).unwrap());
            Self {
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.original.as_ref() {
                Some(value) => env::set_var("PATH", value),
                None => env::remove_var("PATH"),
            }
        }
    }

    /// A recorded opencode NDJSON run, in the verified shapes: one object per
    /// line, `sessionID` and a `part` object on each, assistant text at
    /// `part.text`, and one tool part with its name at `part.tool` and state
    /// under `part.state`. It also carries one malformed line and one unknown
    /// line type. The run must survive both lines.
    const FIXTURE: &str = r#"{"type":"step_start","sessionID":"ses_fix01","part":{"id":"prt_1","messageID":"msg_1","sessionID":"ses_fix01","type":"step-start"}}
{"type":"text","sessionID":"ses_fix01","part":{"id":"prt_2","messageID":"msg_1","sessionID":"ses_fix01","type":"text","text":"Reading the failing test first."}}
{"type":"tool_use","timestamp":1756500000000,"sessionID":"ses_fix01","part":{"type":"tool","tool":"read","callID":"call_01","state":{"status":"completed","input":{"filePath":"src/main.rs"},"output":"fn main() {}","metadata":{},"time":{"start":1756500000100,"end":1756500000150},"title":"src/main.rs"}}}
not json at all
{"type":"file_watched","sessionID":"ses_fix01","part":{"id":"prt_4","path":"src/main.rs"}}
{"type":"text","sessionID":"ses_fix01","part":{"id":"prt_5","messageID":"msg_1","sessionID":"ses_fix01","type":"text","text":"Fixed the parser and opened draft PR 142."}}
{"type":"step_finish","sessionID":"ses_fix01","part":{"id":"prt_6","messageID":"msg_1","sessionID":"ses_fix01","type":"step-finish","reason":"stop","cost":0.013,"tokens":{"input":912,"output":214}}}"#;

    /// The task id every test job works for.
    const TASK: &str = "borsuk/implement-i142";

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-runner-{name}-{}", Uuid::new_v4()));
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

    /// A job pointing into `dir`, in the verified implement shape.
    fn job(dir: &Path, variant: Option<String>) -> Job {
        Job {
            task: TASK.to_string(),
            stage: crate::model::Stage::Implement,
            repo: "borsuk".to_string(),
            model: "zai-coding-plan/glm-5.3-flash".to_string(),
            variant,
            prompt: "Fix issue 142.".to_string(),
            cwd: dir.to_path_buf(),
            log: dir.join("task.jsonl"),
            resume: None,
            yolo: true,
        }
    }

    /// Collect run events until [`RunEvent::Exit`] arrives.
    fn collect_until_exit(rx: &Receiver<RunEvent>) -> Vec<RunEvent> {
        let deadline = Instant::now() + std::time::Duration::from_secs(TEST_TIMEOUT);
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

    #[test]
    fn the_argument_vector_matches_the_verified_invocation() {
        let args = build_args(&job(Path::new("/state/worktrees/borsuk/issue-142"), None));
        assert_eq!(
            args,
            vec![
                "run",
                "--format",
                "json",
                "--auto",
                "--agent",
                "build",
                "-m",
                "zai-coding-plan/glm-5.3-flash",
                "--dir",
                "/state/worktrees/borsuk/issue-142",
                "Fix issue 142.",
            ]
        );
    }

    #[test]
    fn the_argument_vector_carries_the_variant_when_set() {
        let args = build_args(&job(
            Path::new("/state/worktrees/borsuk/train"),
            Some("xhigh".to_string()),
        ));
        assert_eq!(
            args,
            vec![
                "run",
                "--format",
                "json",
                "--auto",
                "--agent",
                "build",
                "-m",
                "zai-coding-plan/glm-5.3-flash",
                "--variant",
                "xhigh",
                "--dir",
                "/state/worktrees/borsuk/train",
                "Fix issue 142.",
            ]
        );
    }

    #[test]
    fn the_argument_vector_carries_the_session_of_a_resume() {
        let dir = Path::new("/state/worktrees/borsuk/issue-142");
        let fresh = build_args(&job(dir, None));
        assert!(!fresh.contains(&"--session".to_string()));

        let mut resumed = job(dir, None);
        resumed.resume = Some("ses_fix01".to_string());
        assert_eq!(
            build_args(&resumed),
            vec![
                "run",
                "--format",
                "json",
                "--auto",
                "--agent",
                "build",
                "-m",
                "zai-coding-plan/glm-5.3-flash",
                "--session",
                "ses_fix01",
                "--dir",
                "/state/worktrees/borsuk/issue-142",
                "Fix issue 142.",
            ]
        );
    }

    #[test]
    fn fixture_replay_produces_the_expected_run_events() {
        let dir = temp_dir("fixture-replay");
        let fixture = dir.join("recorded.ndjson");
        fs::write(&fixture, FIXTURE).unwrap();
        let recorded = fs::read_to_string(&fixture).unwrap();
        let mut parser = NdjsonParser::new("borsuk/review-p7");
        let mut events = Vec::new();
        for line in recorded.lines() {
            events.extend(parser.parse_line(line));
        }

        assert_eq!(
            events,
            vec![
                RunEvent::Started {
                    task: "borsuk/review-p7".to_string(),
                    session_id: Some("ses_fix01".to_string()),
                },
                RunEvent::Text {
                    task: "borsuk/review-p7".to_string(),
                    text: "Reading the failing test first.".to_string(),
                },
                RunEvent::Tool {
                    task: "borsuk/review-p7".to_string(),
                    name: "read".to_string(),
                    summary: "src/main.rs".to_string(),
                },
                RunEvent::Text {
                    task: "borsuk/review-p7".to_string(),
                    text: "Fixed the parser and opened draft PR 142.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: "borsuk/review-p7".to_string(),
                    ok: true,
                    summary: "stop".to_string(),
                    cost_usd: Some(0.013),
                },
            ]
        );
        assert_eq!(parser.session_id.as_deref(), Some("ses_fix01"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_malformed_line_is_skipped_and_the_run_continues() {
        let mut parser = NdjsonParser::new("t/x");
        assert_eq!(
            parser
                .parse_line(r#"{"type":"text","part":{"type":"text","text":"one"}}"#)
                .len(),
            1
        );
        assert!(parser.parse_line("} broken {").is_empty());
        assert_eq!(
            parser
                .parse_line(r#"{"type":"text","part":{"type":"text","text":"two"}}"#)
                .len(),
            1
        );
    }

    #[test]
    fn the_session_id_comes_from_the_first_line_that_carries_one() {
        let mut parser = NdjsonParser::new("t/x");
        let text_first = parser.parse_line(
            r#"{"type":"text","sessionID":"ses_early","part":{"type":"text","text":"early"}}"#,
        );
        assert_eq!(text_first.len(), 1);
        let started = parser.parse_line(
            r#"{"type":"step_start","sessionID":"ses_later","part":{"type":"step-start"}}"#,
        );
        assert_eq!(
            started,
            vec![RunEvent::Started {
                task: "t/x".to_string(),
                session_id: Some("ses_early".to_string()),
            }]
        );
    }

    #[test]
    fn an_invalid_top_level_session_id_does_not_hide_the_part_session_id() {
        let mut parser = NdjsonParser::new("t/x");
        let events = parser.parse_line(
            r#"{"type":"step_start","sessionID":null,"part":{"type":"step-start","sessionID":"ses_part"}}"#,
        );
        assert_eq!(
            events,
            vec![RunEvent::Started {
                task: "t/x".to_string(),
                session_id: Some("ses_part".to_string()),
            }]
        );
        assert_eq!(parser.session_id.as_deref(), Some("ses_part"));
    }

    #[test]
    fn only_the_first_step_start_emits_started() {
        let mut parser = NdjsonParser::new("t/x");
        let one = parser.parse_line(r#"{"type":"step_start","sessionID":"s1","part":{}}"#);
        let two = parser.parse_line(r#"{"type":"step_start","sessionID":"s1","part":{}}"#);
        assert_eq!(one.len(), 1);
        assert!(two.is_empty());
    }

    #[test]
    fn an_unknown_line_type_is_ignored_without_stopping_the_run() {
        let mut parser = NdjsonParser::new("t/x");
        let skipped =
            parser.parse_line(r#"{"type":"file_watched","sessionID":"s1","part":{"path":"x"}}"#);
        let after = parser.parse_line(
            r#"{"type":"text","sessionID":"s1","part":{"type":"text","text":"alive"}}"#,
        );
        assert!(skipped.is_empty());
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn a_tool_part_yields_a_tool_event_whatever_the_line_type() {
        for line_type in [
            "step_start",
            "text",
            "step_finish",
            "tool_use",
            "future_type",
        ] {
            let mut parser = NdjsonParser::new("t/x");
            let line = format!(
                r#"{{"type":"{line_type}","part":{{"type":"tool","tool":"write","state":{{"title":"src/proc.rs"}}}}}}"#,
            );
            assert_eq!(
                parser.parse_line(&line),
                vec![RunEvent::Tool {
                    task: "t/x".to_string(),
                    name: "write".to_string(),
                    summary: "src/proc.rs".to_string(),
                }],
                "outer line type {line_type}"
            );
        }
    }

    #[test]
    fn a_tool_use_line_without_a_tool_part_is_ignored() {
        let mut parser = NdjsonParser::new("t/x");
        let events =
            parser.parse_line(r#"{"type":"tool_use","part":{"type":"text","text":"not a tool"}}"#);
        assert!(events.is_empty());
    }

    #[test]
    fn a_tool_part_without_a_title_falls_back_to_the_truncated_part() {
        let mut parser = NdjsonParser::new("t/x");
        let events = parser.parse_line(
            r#"{"type":"tool","part":{"type":"tool","tool":"read","input":{"path":"src/very/long/path/that/goes/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/for/a/while.rs"}}}"#,
        );
        match &events[0] {
            RunEvent::Tool { name, summary, .. } => {
                assert_eq!(name, "read");
                // serde_json re-serialises with sorted keys, so the fallback
                // is the part's own JSON, cut to the summary limit.
                assert!(summary.starts_with("{\""));
                assert_eq!(summary.chars().count(), SUMMARY_CHARS);
            }
            other => panic!("expected a Tool event, got {other:?}"),
        }
    }

    #[test]
    fn a_step_finish_with_an_error_reason_fails_the_turn() {
        let mut parser = NdjsonParser::new("t/x");
        let events = parser
            .parse_line(r#"{"type":"step_finish","part":{"type":"step-finish","reason":"error"}}"#);
        assert_eq!(
            events,
            vec![RunEvent::TurnEnd {
                task: "t/x".to_string(),
                ok: false,
                summary: "error".to_string(),
                cost_usd: None,
            }]
        );
    }

    /// Start the run, retrying the transient `Text file busy` race.
    ///
    /// The test writes its fake child and executes it at once. On this
    /// kernel, that exec can lose against the write-count release of the
    /// just-closed file and fail with `Text file busy` for a few
    /// microseconds. Production never executes a file it just wrote, so the
    /// retry lives in this helper and not in the runner.
    fn start_with_retry(
        runner: &mut OpenCodeRunner,
        job: &Job,
    ) -> (Box<dyn Session>, Receiver<RunEvent>) {
        let (tx, rx) = channel();
        for _ in 0..100 {
            match runner.start(job, tx.clone()) {
                Ok(session) => return (session, rx),
                Err(error) if error.to_string().contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("the fake child did not start: {error}"),
            }
        }
        panic!("the fake child did not start after 100 attempts");
    }

    #[test]
    fn a_fake_opencode_child_drives_the_full_run() {
        let dir = temp_dir("wiring");
        let fixture = dir.join("fixture.ndjson");
        fs::write(&fixture, FIXTURE).unwrap();
        let argv = dir.join("argv.txt");
        script(
            &dir,
            PROGRAM,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                argv.display(),
                fixture.display(),
            ),
        );
        let job = job(&dir, None);
        let path = PathGuard::prepend(&dir);
        let mut runner = OpenCodeRunner::new();

        let (mut session, rx) = start_with_retry(&mut runner, &job);
        let events = collect_until_exit(&rx);
        session.stop().unwrap();
        session.stop().unwrap();

        let expected = [
            RunEvent::Started {
                task: TASK.to_string(),
                session_id: Some("ses_fix01".to_string()),
            },
            RunEvent::Text {
                task: TASK.to_string(),
                text: "Reading the failing test first.".to_string(),
            },
            RunEvent::Tool {
                task: TASK.to_string(),
                name: "read".to_string(),
                summary: "src/main.rs".to_string(),
            },
            RunEvent::Text {
                task: TASK.to_string(),
                text: "Fixed the parser and opened draft PR 142.".to_string(),
            },
            RunEvent::TurnEnd {
                task: TASK.to_string(),
                ok: true,
                summary: "stop".to_string(),
                cost_usd: Some(0.013),
            },
        ];
        assert_eq!(&events[..events.len() - 1], &expected[..]);
        assert_eq!(
            events.last(),
            Some(&RunEvent::Exit {
                task: TASK.to_string(),
                ok: true,
                detail: "opencode exited with code 0".to_string(),
            })
        );

        // The child received the exact argument vector, `--auto` included.
        let child_args: Vec<String> = fs::read_to_string(&argv)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(child_args, build_args(&job));

        // Every raw line, the malformed one included, reached the log.
        assert_eq!(fs::read_to_string(job.log).unwrap(), FIXTURE);
        drop(path);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_kills_the_child() {
        let dir = temp_dir("stop");
        script(&dir, PROGRAM, "#!/bin/sh\nsleep 0.3\nexit 0\n");
        let job = job(&dir, None);
        let path = PathGuard::prepend(&dir);
        let mut runner = OpenCodeRunner::new();

        let (mut session, rx) = start_with_retry(&mut runner, &job);
        session.stop().unwrap();
        assert_eq!(
            collect_until_exit(&rx).last(),
            Some(&RunEvent::Exit {
                task: TASK.to_string(),
                ok: false,
                detail: "opencode was killed by a signal".to_string(),
            })
        );
        // A second stop after the child is gone is a no-op.
        session.stop().unwrap();
        drop(path);
        fs::remove_dir_all(dir).unwrap();
    }
}
