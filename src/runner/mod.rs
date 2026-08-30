//! The runner contract: a job in, run events out, a session for control.
//!
//! A [`Runner`] turns one [`Job`] into one live child and hands back a
//! [`Session`]. The child's activity travels to the caller as [`RunEvent`]s
//! on the sender the caller provides. The interactive methods of [`Session`]
//! steer a live session; a runner without a steering channel, such as the
//! one-shot opencode runner, keeps their default bodies, which refuse
//! steering with an error.

pub mod claude;
pub mod opencode;

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::anyhow;

use crate::model::Stage;

/// One unit of work a runner starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The task id the run works for, for example `borsuk/implement-i142`.
    pub task: String,
    /// The pipeline stage the run serves.
    pub stage: Stage,
    /// The repository alias the run works on.
    pub repo: String,
    /// The model the agent runs on, in provider form.
    pub model: String,
    /// The optional effort variant, for example `xhigh`.
    pub variant: Option<String>,
    /// The fully rendered prompt for the agent.
    pub prompt: String,
    /// The working directory for the agent: a worktree or a checkout.
    pub cwd: PathBuf,
    /// The task log the runner's raw output is teed into.
    pub log: PathBuf,
    /// The session id to resume, when the run continues a known session.
    pub resume: Option<String>,
    /// Whether tools are auto-approved without asking a human.
    pub yolo: bool,
}

/// One asynchronous report from a running agent.
///
/// Every event names its task, so one receiver can serve many runs. Each run
/// ends with exactly one [`RunEvent::Exit`], sent after all other events of
/// that run.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// The agent reported its first step and, when known, its session id.
    Started {
        /// The task id the run works for.
        task: String,
        /// The agent session id, when the output carried one.
        session_id: Option<String>,
    },
    /// Assistant text from the agent.
    Text {
        /// The task id the run works for.
        task: String,
        /// The assistant text, as one piece.
        text: String,
    },
    /// Tool activity from the agent.
    Tool {
        /// The task id the run works for.
        task: String,
        /// The tool name, for example `bash`.
        name: String,
        /// A one-line summary of what the tool did.
        summary: String,
    },
    /// A request that waits for a human answer.
    Ask {
        /// The task id the run works for.
        task: String,
        /// The id the answer must name.
        request_id: String,
        /// The tool that asks, for example `AskUserQuestion`.
        tool: String,
        /// The tool input, verbatim.
        input: serde_json::Value,
        /// Permission suggestions the agent attached, verbatim.
        suggestions: serde_json::Value,
        /// Whether a human, not a policy, must answer.
        needs_human: bool,
    },
    /// One agent turn finished.
    TurnEnd {
        /// The task id the run works for.
        task: String,
        /// Whether the turn ended without an agent-side error.
        ok: bool,
        /// A short summary of what the turn did.
        summary: String,
        /// The turn cost in US dollars, when the agent reported one.
        cost_usd: Option<f64>,
    },
    /// The run's process ended. The last event of every run.
    Exit {
        /// The task id the run works for.
        task: String,
        /// Whether the process reported success.
        ok: bool,
        /// A short detail of how the process ended.
        detail: String,
    },
}

/// The human's answer to one [`RunEvent::Ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Approve the request, optionally with edited input.
    Allow {
        /// The input the tool should run instead of the requested one, when
        /// the human edited it.
        updated_input: Option<serde_json::Value>,
    },
    /// Refuse the request with a reason.
    Deny {
        /// The reason the agent sees.
        message: String,
    },
}

/// A factory for one agent session.
pub trait Runner: Send {
    /// Start `job` and return its control handle.
    ///
    /// The runner reports [`RunEvent`]s on `tx` from its own threads and ends
    /// the stream with exactly one [`RunEvent::Exit`]. An error return means
    /// the child never started, so no exit event will arrive.
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>>;
}

/// The control handle for one live agent session.
pub trait Session: Send {
    /// Send an extra user message into a live session.
    ///
    /// The default body refuses: the runner has no steering channel.
    fn send_user(&mut self, _text: &str) -> anyhow::Result<()> {
        Err(unsupported_steering("send_user"))
    }

    /// Answer the [`RunEvent::Ask`] named by `request_id`.
    ///
    /// The default body refuses: the runner has no steering channel.
    fn answer(&mut self, _request_id: &str, _answer: Answer) -> anyhow::Result<()> {
        Err(unsupported_steering("answer"))
    }

    /// Stop the session's child process.
    fn stop(&mut self) -> anyhow::Result<()>;
}

/// Build the error for a runner that cannot steer its session.
fn unsupported_steering(method: &str) -> anyhow::Error {
    anyhow!("this runner does not support steering; {method} is not available")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with no steering channel; only `stop` is implemented.
    struct StopOnly;

    impl Session for StopOnly {
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_default_session_methods_refuse_steering() {
        let mut session = StopOnly;
        let send_error = session.send_user("one more turn").unwrap_err();
        assert!(send_error.to_string().contains("does not support steering"));

        let answer_error = session
            .answer(
                "req-1",
                Answer::Deny {
                    message: "not yet".to_string(),
                },
            )
            .unwrap_err();
        assert!(answer_error
            .to_string()
            .contains("does not support steering"));

        session.stop().unwrap();
    }
}
