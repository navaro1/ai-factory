//! The Codex runner for noninteractive JSON Lines tasks.
//!
//! `codex exec --json` prints one event per line: `thread.started`,
//! `turn.started`, `item.started`, `item.completed`, `turn.completed`,
//! `turn.failed`, and `error`. Headless codex never asks: its approval
//! policy is `never`, so a command the sandbox refuses ends as one
//! `item.completed` line of type `command_execution` with the status
//! `declined`. That line is the one approval request codex can carry, and
//! the runner turns it into a [`RunEvent::Ask`] permission row. An operator
//! grant reaches the next run as an allowed permission, and the runner then
//! starts codex with `--sandbox danger-full-access`, because headless codex
//! has no finer grant. Codex exec has no question channel at all: an agent
//! that needs a person uses the `needs-human` label and the ask block of
//! the prompts, which the daemon reads from GitHub.

use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use serde_json::Value;

use crate::config::RoleSettings;
use crate::proc::{self, ProcEvent, ProcHandle, RunSpec};
use crate::runner::{Job, RunEvent, Runner, Session};

/// The sandbox a granted permission runs under.
const GRANTED_SANDBOX: &str = "danger-full-access";

/// The item type of a shell command the agent ran.
const COMMAND_ITEM: &str = "command_execution";

/// The item status codex reports for a command its policy refused.
const DECLINED_STATUS: &str = "declined";

fn build_args(job: &Job, settings: &RoleSettings) -> Vec<String> {
    let mut args = vec!["exec".to_string()];
    args.push("--json".to_string());
    if let Some(profile) = settings.profile.as_ref() {
        args.push("--profile".to_string());
        args.push(profile.clone());
    }
    args.push("--model".to_string());
    args.push(settings.model.clone());
    if let Some(policy) = settings.approval_policy.as_ref() {
        args.push("-c".to_string());
        args.push(format!(
            "approval_policy={}",
            toml::Value::String(policy.clone())
        ));
    }
    // A grant from the inbox lifts the sandbox: headless codex cannot
    // approve one command, so the retry runs with full access instead.
    if !job.allowed_permissions.is_empty() {
        args.push("--sandbox".to_string());
        args.push(GRANTED_SANDBOX.to_string());
    } else if let Some(sandbox) = settings.sandbox.as_ref() {
        args.push("--sandbox".to_string());
        args.push(sandbox.clone());
    }
    if let Some(effort) = settings.effort.as_ref() {
        args.push("-c".to_string());
        args.push(format!(
            "model_reasoning_effort={}",
            toml::Value::String(effort.clone())
        ));
    }
    args.push("--cd".to_string());
    args.push(job.cwd.display().to_string());
    args.extend(settings.extra_args.iter().cloned());
    if let Some(session_id) = job.resume.as_ref() {
        args.push("resume".to_string());
        args.push(session_id.clone());
    }
    args.push(job.prompt.clone());
    args
}

/// One configured Codex command adapter.
pub struct CodexRunner {
    settings: RoleSettings,
}

impl CodexRunner {
    pub fn new(settings: RoleSettings) -> Self {
        Self { settings }
    }
}

impl Runner for CodexRunner {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
        let spec = RunSpec {
            task: job.task.clone(),
            cwd: job.cwd.clone(),
            program: self.settings.program.clone(),
            args: build_args(job, &self.settings),
            env: Vec::new(),
            log: job.log.clone(),
        };
        let (proc_tx, proc_rx) = channel::<ProcEvent>();
        let handle = proc::spawn(spec, proc_tx)?;
        handle.close_stdin();
        let task = job.task.clone();
        thread::spawn(move || forward_events(task, proc_rx, tx));
        Ok(Box::new(CodexSession {
            handle: Some(handle),
        }))
    }
}

struct CodexSession {
    handle: Option<ProcHandle>,
}

impl Session for CodexSession {
    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(handle) = self.handle.as_ref() {
            handle.kill()?;
        }
        self.handle = None;
        Ok(())
    }
}

fn forward_events(task: String, rx: Receiver<ProcEvent>, tx: Sender<RunEvent>) {
    let mut parser = JsonlParser::new(&task);
    let mut exited = false;
    for event in rx {
        match event {
            ProcEvent::Line(line) => {
                for run_event in parser.parse_line(&line) {
                    if tx.send(run_event).is_err() {
                        return;
                    }
                }
            }
            ProcEvent::Exit { code, ok } => {
                let failed_turn = parser.failed;
                let detail = match (code, ok, failed_turn) {
                    (Some(code), true, true) => {
                        format!("codex reported a failed turn before exit code {code}")
                    }
                    (Some(code), _, _) => format!("codex exited with code {code}"),
                    (None, _, _) => "codex was killed by a signal".to_string(),
                };
                exited = true;
                if tx
                    .send(RunEvent::Exit {
                        task: task.clone(),
                        ok: ok && !failed_turn,
                        detail,
                    })
                    .is_err()
                {
                    return;
                }
                break;
            }
            ProcEvent::StderrLine(_) => {
                // Codex speaks its protocol on stdout; the stderr tee already
                // reached the task log. A refused command arrives on stdout
                // as a declined item, never on stderr.
            }
            ProcEvent::Error(message) => eprintln!("task {task}: {message}"),
            ProcEvent::Stopped(outcome) => {
                eprintln!("task {task}: unexpected codex stop outcome: {outcome:?}")
            }
        }
    }
    if !exited {
        let _ = tx.send(RunEvent::Exit {
            task,
            ok: false,
            detail: "the codex event stream ended without an exit".to_string(),
        });
    }
}

#[derive(Debug)]
struct JsonlParser {
    task: String,
    active_items: HashSet<String>,
    turn_ended: bool,
    failed: bool,
}

impl JsonlParser {
    fn new(task: &str) -> Self {
        Self {
            task: task.to_string(),
            active_items: HashSet::new(),
            turn_ended: false,
            failed: false,
        }
    }

    fn parse_line(&mut self, line: &str) -> Vec<RunEvent> {
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "task {}: skipped one malformed codex line: {error}",
                    self.task
                );
                return Vec::new();
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("thread.started") => vec![RunEvent::Started {
                task: self.task.clone(),
                session_id: value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(String::from),
            }],
            Some("turn.started") => {
                self.turn_ended = false;
                self.active_items.clear();
                Vec::new()
            }
            Some("turn.completed") => self.turn_end(true, "turn completed"),
            Some("turn.failed") => {
                let summary = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed");
                self.turn_end(false, summary)
            }
            Some("error") => {
                let summary = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex reported an error");
                self.turn_end(false, summary)
            }
            Some("item.started") => self.item_event(&value, true),
            Some("item.completed") => self.item_event(&value, false),
            _ => Vec::new(),
        }
    }

    fn turn_end(&mut self, ok: bool, summary: &str) -> Vec<RunEvent> {
        if self.turn_ended {
            return Vec::new();
        }
        self.turn_ended = true;
        self.failed |= !ok;
        vec![RunEvent::TurnEnd {
            task: self.task.clone(),
            ok,
            summary: summary.to_string(),
            cost_usd: None,
        }]
    }

    fn item_event(&mut self, value: &Value, started: bool) -> Vec<RunEvent> {
        let Some(item) = value.get("item") else {
            return Vec::new();
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("item");
        if kind == "agent_message" {
            return if started {
                Vec::new()
            } else {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(|text| {
                        vec![RunEvent::Text {
                            task: self.task.clone(),
                            text: text.to_string(),
                        }]
                    })
                    .unwrap_or_default()
            };
        }
        if kind == "reasoning" {
            return Vec::new();
        }
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        if !started && kind == COMMAND_ITEM && self.declined(item) {
            self.active_items.remove(id);
            return vec![self.declined_ask(id, item)];
        }
        if !started && !id.is_empty() && self.active_items.remove(id) {
            return Vec::new();
        }
        if started && !id.is_empty() {
            self.active_items.insert(id.to_string());
        }
        vec![RunEvent::Tool {
            task: self.task.clone(),
            name: kind.to_string(),
            summary: item_summary(kind, item),
        }]
    }
}

impl JsonlParser {
    /// Whether a completed command item reports the `declined` status.
    fn declined(&self, item: &Value) -> bool {
        item.get("status").and_then(Value::as_str) == Some(DECLINED_STATUS)
    }

    /// The permission row of one declined command.
    ///
    /// The row names the command as its one pattern, so the grant that the
    /// daemon records reads like the opencode grants. The item id is the
    /// request id; codex numbers items per thread, so a retry that declines
    /// the same command refreshes the same row.
    fn declined_ask(&self, id: &str, item: &Value) -> RunEvent {
        let command = item
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        RunEvent::Ask {
            task: self.task.clone(),
            request_id: if id.is_empty() {
                "declined".to_string()
            } else {
                id.to_string()
            },
            tool: COMMAND_ITEM.to_string(),
            input: serde_json::json!({ "command": command, "patterns": [command] }),
            suggestions: Value::Null,
            needs_human: false,
        }
    }
}

fn item_summary(kind: &str, item: &Value) -> String {
    let key = match kind {
        "command_execution" => "command",
        "web_search" => "query",
        "mcp_tool_call" => "tool",
        "file_change" => "path",
        _ => "text",
    };
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Harness, RoleSettings};
    use crate::model::Stage;
    use crate::runner::{Job, RunEvent};
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

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
            sandbox: Some("workspace-write".to_string()),
        }
    }

    fn job(resume: Option<&str>) -> Job {
        Job {
            task: "borsuk/review-p142".to_string(),
            stage: Stage::Review,
            repo: "borsuk".to_string(),
            model: "unused".to_string(),
            variant: None,
            prompt: "Review pull request 142.".to_string(),
            cwd: Path::new("/state/worktrees/borsuk/issue-142").to_path_buf(),
            log: Path::new("/state/logs/review.jsonl").to_path_buf(),
            resume: resume.map(String::from),
            yolo: false,
            allowed_tools: None,
            allowed_permissions: Vec::new(),
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-codex-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let mut file = fs::File::create(path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        drop(file);
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn collect_until_exit(rx: &Receiver<RunEvent>) -> Vec<RunEvent> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the fake codex child did not exit");
            let exit = matches!(event, RunEvent::Exit { .. });
            events.push(event);
            if exit {
                return events;
            }
        }
    }

    #[test]
    fn fresh_arguments_match_the_official_exec_contract() {
        assert_eq!(
            build_args(&job(None), &settings()),
            vec![
                "exec",
                "--json",
                "--profile",
                "reviewer",
                "--model",
                "codex-review-model",
                "-c",
                "approval_policy=\"never\"",
                "--sandbox",
                "workspace-write",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "--cd",
                "/state/worktrees/borsuk/issue-142",
                "--notice",
                "review",
                "Review pull request 142.",
            ]
        );
    }

    #[test]
    fn resume_arguments_put_all_exec_options_before_the_nested_subcommand() {
        assert_eq!(
            build_args(
                &job(Some("019d1c0a-0137-73f3-bf4a-88c90739150c")),
                &settings()
            ),
            vec![
                "exec",
                "--json",
                "--profile",
                "reviewer",
                "--model",
                "codex-review-model",
                "-c",
                "approval_policy=\"never\"",
                "--sandbox",
                "workspace-write",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "--cd",
                "/state/worktrees/borsuk/issue-142",
                "--notice",
                "review",
                "resume",
                "019d1c0a-0137-73f3-bf4a-88c90739150c",
                "Review pull request 142.",
            ]
        );
    }

    #[test]
    fn fixture_replay_maps_every_official_event_type() {
        let mut parser = JsonlParser::new("borsuk/review-p142");
        let events: Vec<RunEvent> = include_str!("fixtures/codex-events.jsonl")
            .lines()
            .flat_map(|line| parser.parse_line(line))
            .collect();

        assert_eq!(
            events,
            vec![
                RunEvent::Started {
                    task: "borsuk/review-p142".to_string(),
                    session_id: Some("019d1c0a-0137-73f3-bf4a-88c90739150c".to_string()),
                },
                RunEvent::Tool {
                    task: "borsuk/review-p142".to_string(),
                    name: "command_execution".to_string(),
                    summary: "cargo test".to_string(),
                },
                RunEvent::Text {
                    task: "borsuk/review-p142".to_string(),
                    text: "The review passed.".to_string(),
                },
                RunEvent::TurnEnd {
                    task: "borsuk/review-p142".to_string(),
                    ok: true,
                    summary: "turn completed".to_string(),
                    cost_usd: None,
                },
                RunEvent::Tool {
                    task: "borsuk/review-p142".to_string(),
                    name: "web_search".to_string(),
                    summary: "Codex CLI contract".to_string(),
                },
                RunEvent::TurnEnd {
                    task: "borsuk/review-p142".to_string(),
                    ok: false,
                    summary: "the turn failed".to_string(),
                    cost_usd: None,
                },
                RunEvent::TurnEnd {
                    task: "borsuk/review-p142".to_string(),
                    ok: false,
                    summary: "stream error".to_string(),
                    cost_usd: None,
                },
            ]
        );
    }

    /// Headless codex refuses a command as one declined item. That item is
    /// the only approval request the harness can carry, so it must open a
    /// permission row that names the command.
    #[test]
    fn a_declined_command_opens_a_permission_row() {
        let mut parser = JsonlParser::new("borsuk/review-p142");
        assert!(parser
            .parse_line(r#"{"type":"item.started","item":{"id":"item_3","type":"command_execution","command":"curl https://example.test","status":"in_progress"}}"#)
            .iter()
            .any(|event| matches!(event, RunEvent::Tool { .. })));
        let events = parser.parse_line(
            r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","command":"curl https://example.test","aggregated_output":"","exit_code":null,"status":"declined"}}"#,
        );
        assert_eq!(
            events,
            vec![RunEvent::Ask {
                task: "borsuk/review-p142".to_string(),
                request_id: "item_3".to_string(),
                tool: "command_execution".to_string(),
                input: serde_json::json!({
                    "command": "curl https://example.test",
                    "patterns": ["curl https://example.test"],
                }),
                suggestions: Value::Null,
                needs_human: false,
            }]
        );
        assert!(
            parser.active_items.is_empty(),
            "the declined item is no longer active"
        );

        // A completed command stays a plain tool event.
        let events = parser.parse_line(
            r#"{"type":"item.completed","item":{"id":"item_4","type":"command_execution","command":"ls","aggregated_output":"","exit_code":0,"status":"completed"}}"#,
        );
        assert!(events
            .iter()
            .all(|event| matches!(event, RunEvent::Tool { .. })));
    }

    /// A grant from the inbox reaches codex as the full-access sandbox,
    /// because headless codex cannot approve one command on its own.
    #[test]
    fn a_granted_permission_lifts_the_sandbox() {
        let mut settings = settings();
        settings.sandbox = Some("workspace-write".to_string());
        let mut job = job(None);
        assert!(
            build_args(&job, &settings)
                .windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"]),
            "the configured sandbox holds without a grant"
        );

        job.allowed_permissions = vec![crate::runner::AllowedPermission {
            permission: "command_execution".to_string(),
            patterns: vec!["curl https://example.test".to_string()],
        }];
        let args = build_args(&job, &settings);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--sandbox", "danger-full-access"]));
        assert!(
            !args.contains(&"workspace-write".to_string()),
            "the grant replaces the configured sandbox"
        );
    }

    #[test]
    fn a_fake_executable_receives_the_exact_program_and_arguments() {
        let dir = temp_dir();
        let program = dir.join("custom-codex");
        let argv = dir.join("argv.txt");
        let fixture = dir.join("events.jsonl");
        fs::write(&fixture, include_str!("fixtures/codex-events.jsonl")).unwrap();
        script(
            &program,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                argv.display(),
                fixture.display()
            ),
        );
        let mut settings = settings();
        settings.program = program.display().to_string();
        let mut job = job(None);
        job.cwd = dir.clone();
        job.log = dir.join("task.jsonl");
        let expected_args = build_args(&job, &settings);
        let mut runner = CodexRunner::new(settings);
        let (tx, rx) = channel();

        let mut session = runner.start(&job, tx).unwrap();
        let events = collect_until_exit(&rx);
        session.stop().unwrap();

        let actual_args: Vec<String> = fs::read_to_string(&argv)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(actual_args, expected_args);
        assert!(matches!(
            events.first(),
            Some(RunEvent::Started {
                session_id: Some(id),
                ..
            }) if id == "019d1c0a-0137-73f3-bf4a-88c90739150c"
        ));
        assert_eq!(
            events.last(),
            Some(&RunEvent::Exit {
                task: "borsuk/review-p142".to_string(),
                ok: false,
                detail: "codex reported a failed turn before exit code 0".to_string(),
            })
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
