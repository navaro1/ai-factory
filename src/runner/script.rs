//! The script runner: one shell command as one supervised child.
//!
//! A measure task runs a command, not an agent. This runner starts
//! `sh -c <command>` in the task's working directory, tees its output into
//! the task log like every other runner, and reports the exit code in the
//! detail of the one [`RunEvent::Exit`] it sends.
//!
//! A timeout thread stops the child when the job's deadline passes. A
//! stopped child reports exit [`TIMEOUT_EXIT`], the code `timeout(1)` uses,
//! so the caller reads a timeout the same way a shell would.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crate::proc::{self, ProcEvent, ProcHandle, RunSpec};
use crate::runner::{Job, RunEvent, Runner, Session};

/// The exit code a run that passed its deadline reports.
pub const TIMEOUT_EXIT: i32 = 124;

/// The exit code a run that a signal ended reports.
pub const SIGNAL_EXIT: i32 = 128;

/// The seconds a job without an explicit deadline may run.
pub const DEFAULT_TIMEOUT_S: u64 = 120;

/// The shell the runner starts.
const SHELL: &str = "sh";

/// The child of one run, shared by the session and the timeout thread.
///
/// [`ProcHandle`] is not `Sync`, so the two owners share it through a
/// mutex instead of a plain reference.
type SharedChild = Arc<Mutex<Option<ProcHandle>>>;

/// Kill the child of one run, if it is still there.
fn kill(child: &SharedChild) -> anyhow::Result<()> {
    let guard = child.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref() {
        Some(handle) => handle.kill_group(),
        None => Ok(()),
    }
}

/// The `Exit` detail of one script run.
pub fn exit_detail(code: i32) -> String {
    format!("exit {code}")
}

/// The exit code of one script run, read back from its `Exit` detail.
pub fn exit_code(detail: &str) -> Option<i32> {
    detail.strip_prefix("exit ")?.trim().parse().ok()
}

/// The runner that starts one shell command per job.
#[derive(Debug, Default)]
pub struct ScriptRunner;

impl ScriptRunner {
    /// A runner with no configuration; every job carries its own command.
    pub fn new() -> Self {
        ScriptRunner
    }
}

impl Runner for ScriptRunner {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
        let spec = RunSpec {
            // The shell forks for the command, so the run needs its own
            // group; a timeout then reaches the command, not the shell
            // alone.
            own_group: true,
            task: job.task.clone(),
            cwd: job.cwd.clone(),
            program: SHELL.to_string(),
            args: vec!["-c".to_string(), job.prompt.clone()],
            env: Vec::new(),
            log: job.log.clone(),
        };
        let (proc_tx, proc_rx) = channel::<ProcEvent>();
        let handle = proc::spawn(spec, proc_tx)?;
        // The command reads no input, so the pipe closes at once and a
        // child that reads stdin sees end of file instead of hanging.
        handle.close_stdin();
        let child: SharedChild = Arc::new(Mutex::new(Some(handle)));
        let timeout = Duration::from_secs(job.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).max(1));
        let timed_out = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = channel::<()>();
        let killer = child.clone();
        let flag = timed_out.clone();
        let task = job.task.clone();
        thread::spawn(move || wait_for_deadline(&task, &killer, &flag, &done_rx, timeout));
        let task = job.task.clone();
        thread::spawn(move || {
            forward_events(&task, &proc_rx, &tx, &timed_out);
            drop(done_tx);
        });
        Ok(Box::new(ScriptSession { child }))
    }
}

/// Stop the child when the deadline passes before the run ends.
///
/// The forwarding thread drops its end of `done` when the child exits, so
/// a finished run wakes this thread at once and kills nothing.
fn wait_for_deadline(
    task: &str,
    child: &SharedChild,
    timed_out: &AtomicBool,
    done: &Receiver<()>,
    timeout: Duration,
) {
    if done.recv_timeout(timeout) != Err(RecvTimeoutError::Timeout) {
        return;
    }
    timed_out.store(true, Ordering::SeqCst);
    if let Err(error) = kill(child) {
        eprintln!("task {task}: cannot stop the timed-out command: {error:#}");
    }
}

/// Map the process events of one command onto run events.
///
/// The command's own output already reached the task log, so this thread
/// reports nothing until the child exits. The one exit event carries the
/// code; a stream that closes without an exit reports a signal.
fn forward_events(
    task: &str,
    rx: &Receiver<ProcEvent>,
    tx: &Sender<RunEvent>,
    timed_out: &AtomicBool,
) {
    for event in rx {
        match event {
            ProcEvent::Exit { code, .. } => {
                let code = if timed_out.load(Ordering::SeqCst) {
                    TIMEOUT_EXIT
                } else {
                    code.unwrap_or(SIGNAL_EXIT)
                };
                let _ = tx.send(RunEvent::Exit {
                    task: task.to_string(),
                    ok: code == 0,
                    detail: exit_detail(code),
                });
                return;
            }
            ProcEvent::Error(message) => eprintln!("task {task}: {message}"),
            ProcEvent::Line(_) | ProcEvent::StderrLine(_) | ProcEvent::Stopped(_) => {}
        }
    }
    let _ = tx.send(RunEvent::Exit {
        task: task.to_string(),
        ok: false,
        detail: exit_detail(SIGNAL_EXIT),
    });
}

/// The control handle for one command child.
///
/// A command has no steering channel, so `send_user` and `answer` keep the
/// trait defaults, which refuse steering.
struct ScriptSession {
    child: SharedChild,
}

impl Session for ScriptSession {
    /// Kill the command. A second stop is a no-op.
    fn stop(&mut self) -> anyhow::Result<()> {
        kill(&self.child)?;
        *self.child.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stage;
    use std::path::PathBuf;
    use std::time::Instant;

    /// One measure job over `command` with the given deadline.
    fn job(dir: &std::path::Path, command: &str, timeout_s: u64) -> Job {
        Job {
            task: "borsuk/fast-aabbccdd-checkout".to_string(),
            stage: Stage::Review,
            repo: "borsuk".to_string(),
            model: SHELL.to_string(),
            variant: None,
            prompt: command.to_string(),
            cwd: dir.to_path_buf(),
            log: dir.join("run.jsonl"),
            resume: None,
            yolo: false,
            allowed_tools: None,
            allowed_permissions: Vec::new(),
            timeout_s: Some(timeout_s),
        }
    }

    /// A fresh temporary working directory.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-script-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("the temporary directory must exist");
        dir
    }

    /// Run one command and return its exit event.
    fn run(dir: &std::path::Path, command: &str, timeout_s: u64) -> RunEvent {
        let (tx, rx) = channel::<RunEvent>();
        let mut runner = ScriptRunner::new();
        let _session = runner
            .start(&job(dir, command, timeout_s), tx)
            .expect("the shell must start");
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the run must report an exit")
    }

    #[test]
    fn a_command_reports_its_exit_code_and_tees_its_output() {
        let dir = temp_dir("exit");
        let event = run(&dir, "echo hello; exit 3", 10);

        assert_eq!(
            event,
            RunEvent::Exit {
                task: "borsuk/fast-aabbccdd-checkout".to_string(),
                ok: false,
                detail: "exit 3".to_string(),
            }
        );
        let log = std::fs::read_to_string(dir.join("run.jsonl")).expect("the log must exist");
        assert!(
            log.contains("hello"),
            "the log must carry the output: {log}"
        );
        assert_eq!(exit_code(&event_detail(&event)), Some(3));

        assert_eq!(
            run(&dir, "exit 0", 10),
            RunEvent::Exit {
                task: "borsuk/fast-aabbccdd-checkout".to_string(),
                ok: true,
                detail: "exit 0".to_string(),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_command_that_sleeps_past_its_deadline_reports_exit_124_within_three_seconds() {
        let dir = temp_dir("timeout");
        let started = Instant::now();

        let event = run(&dir, "sleep 30", 1);

        let elapsed = started.elapsed();
        assert_eq!(
            event,
            RunEvent::Exit {
                task: "borsuk/fast-aabbccdd-checkout".to_string(),
                ok: false,
                detail: "exit 124".to_string(),
            }
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the timeout must end the run inside three seconds, not {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_exit_detail_round_trips_through_the_code_reader() {
        for code in [0, 1, TIMEOUT_EXIT, SIGNAL_EXIT] {
            assert_eq!(exit_code(&exit_detail(code)), Some(code));
        }
        assert_eq!(exit_code("the agent stopped"), None);
        assert_eq!(exit_code("exit later"), None);
    }

    /// The detail of one exit event.
    fn event_detail(event: &RunEvent) -> String {
        match event {
            RunEvent::Exit { detail, .. } => detail.clone(),
            other => panic!("expected an exit event, got {other:?}"),
        }
    }
}
