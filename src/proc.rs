//! Child process supervision and the log tee.
//!
//! [`spawn`] starts one child with piped stdin, stdout, and stderr, tees every
//! raw stdout line into the task log byte for byte, and reports lines and exit
//! through a channel, so the daemon event loop never blocks on a child. A
//! [`ProcHandle`] writes to the child's stdin and stops it: an optional
//! protocol interrupt, then `kill -TERM` through the [`Exec`] trait, then
//! [`std::process::Child::kill`] for SIGKILL.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context};

use crate::exec::{Exec, RealExec};

/// How long a graceful stop waits for the child after the protocol
/// interrupt.
pub const TERM_GRACE: Duration = Duration::from_secs(10);

/// How long a graceful stop waits for the child after SIGTERM and after
/// SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// Everything needed to start one supervised child.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The task id the child works for, for example `borsuk/implement-i142`.
    pub task: String,
    /// The working directory for the child.
    pub cwd: PathBuf,
    /// The program to run, resolved through `PATH`.
    pub program: String,
    /// The argument vector, passed without a shell.
    pub args: Vec<String>,
    /// Extra environment variables set on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// The log file every raw output line is teed into.
    pub log: PathBuf,
}

/// One asynchronous report from a supervised child.
///
/// The channel carries events from the reader threads, the exit waiter, and
/// any graceful-stop escalation. A dropped receiver is a shutdown in
/// progress; senders treat a send error as a reason to stop sending, not as
/// a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcEvent {
    /// One complete stdout line, newline stripped and decoded lossily. The
    /// exact raw bytes, including lines that are not valid JSON, reached the
    /// log file first.
    Line(String),
    /// The child exited. Sent exactly once, whatever stopped the child.
    Exit {
        /// The exit code, or `None` when a signal killed the child.
        code: Option<i32>,
        /// Whether the exit status reports success.
        ok: bool,
    },
    /// A [`stop_gracefully`] escalation finished.
    Stopped(StopOutcome),
    /// A log write or a pipe read failed. The stream that failed may be
    /// incomplete from this point on.
    LogError(String),
}

/// How a graceful-stop escalation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The child exited before any signal, after the optional protocol
    /// interrupt.
    Exited,
    /// The child died after SIGTERM.
    Terminated,
    /// The child ignored SIGTERM and died after SIGKILL.
    Killed,
    /// The escalation could not confirm the child's exit. The string holds
    /// every error seen on the way.
    Failed(String),
}

/// The closure a graceful stop calls as the protocol interrupt, before any
/// signal. A runner installs one with
/// [`ProcHandle::set_interrupt_hook`].
pub type InterruptHook = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

/// The append-only task log shared by the reader threads.
struct LogSink {
    file: Mutex<File>,
    broken: AtomicBool,
}

impl LogSink {
    /// Write `prefix` and then `bytes` to the log. When `complete` is true,
    /// the entry is made newline-terminated so two entries never share a
    /// line. On the first write failure the sink is marked broken, one
    /// [`ProcEvent::LogError`] is sent, and later writes are skipped.
    fn write_bytes(
        &self,
        prefix: &[u8],
        bytes: &[u8],
        complete: bool,
        context: &str,
        tx: &Sender<ProcEvent>,
    ) {
        if self.broken.load(Ordering::Acquire) {
            return;
        }
        let mut chunk = Vec::with_capacity(prefix.len() + bytes.len() + 1);
        chunk.extend_from_slice(prefix);
        chunk.extend_from_slice(bytes);
        if complete && chunk.last() != Some(&b'\n') {
            chunk.push(b'\n');
        }
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = file.write_all(&chunk) {
            self.broken.store(true, Ordering::Release);
            let _ = tx.send(ProcEvent::LogError(format!(
                "{context}: log write failed: {error}"
            )));
        }
    }
}

/// Decode one raw output line for a [`ProcEvent::Line`].
///
/// The log keeps the exact bytes; this strips one trailing newline, one
/// preceding carriage return, and replaces bytes that are not UTF-8.
fn decode_line(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Spawn one supervised child and start its readers.
///
/// The parent directory of `spec.log` is created if needed and the log is
/// opened for append. The child starts with piped stdin, stdout, and stderr.
/// Every raw stdout line reaches the log byte for byte; stderr lines are
/// appended to the same log with a `stderr ` prefix and produce no events.
/// SIGTERM runs `kill -TERM <pid>` through [`RealExec`].
pub fn spawn(spec: RunSpec, tx: Sender<ProcEvent>) -> anyhow::Result<ProcHandle> {
    spawn_with_exec(spec, tx, Arc::new(RealExec))
}

/// Like [`spawn`], but with an injected [`Exec`], so a test can script the
/// `kill -TERM` call.
pub fn spawn_with_exec(
    spec: RunSpec,
    tx: Sender<ProcEvent>,
    exec: Arc<dyn Exec>,
) -> anyhow::Result<ProcHandle> {
    if let Some(parent) = spec.log.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating log directory {}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log)
        .with_context(|| format!("opening task log {}", spec.log.display()))?;
    let sink = Arc::new(LogSink {
        file: Mutex::new(file),
        broken: AtomicBool::new(false),
    });

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(
            spec.env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = start_child(&mut command, &spec.task, &spec.program)?;
    let pid = child.id();

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("task {}: child stdin unavailable", spec.task))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("task {}: child stdout unavailable", spec.task))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("task {}: child stderr unavailable", spec.task))?;

    let (exit_tx, exit_rx) = channel::<()>();

    // The stdout reader tees every raw line into the log and sends events.
    // If the event receiver is gone, it keeps reading so the log stays
    // complete; a dropped receiver is a shutdown, not a fault.
    let stdout_reader = {
        let sink = Arc::clone(&sink);
        let tx = tx.clone();
        let task = spec.task.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            let mut events_on = true;
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        sink.write_bytes(b"", &buf, false, &task, &tx);
                        if events_on && tx.send(ProcEvent::Line(decode_line(&buf))).is_err() {
                            events_on = false;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(ProcEvent::LogError(format!(
                            "task {task}: stdout read failed: {error}"
                        )));
                        break;
                    }
                }
            }
        })
    };

    // The stderr reader appends to the same log with a prefix and sends no
    // events.
    let stderr_reader = {
        let sink = Arc::clone(&sink);
        let tx = tx.clone();
        let task = spec.task.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => sink.write_bytes(b"stderr ", &buf, true, &task, &tx),
                    Err(error) => {
                        let _ = tx.send(ProcEvent::LogError(format!(
                            "task {task}: stderr read failed: {error}"
                        )));
                        break;
                    }
                }
            }
        })
    };

    // The waiter joins the readers, so both pipes are at end of file before
    // it reaps, then reports the exit exactly once.
    let child_cell: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));
    {
        let child_cell = Arc::clone(&child_cell);
        let tx = tx.clone();
        let task = spec.task.clone();
        thread::spawn(move || {
            if let Err(error) = stdout_reader.join() {
                let _ = tx.send(ProcEvent::LogError(format!(
                    "task {task}: stdout reader stopped: {error:?}"
                )));
            }
            if let Err(error) = stderr_reader.join() {
                let _ = tx.send(ProcEvent::LogError(format!(
                    "task {task}: stderr reader stopped: {error:?}"
                )));
            }
            let taken = child_cell
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let Some(mut child) = taken else {
                return;
            };
            match child.wait() {
                Ok(status) => {
                    let code = status.code();
                    let ok = status.success();
                    let _ = exit_tx.send(());
                    let _ = tx.send(ProcEvent::Exit { code, ok });
                }
                Err(error) => {
                    let _ = tx.send(ProcEvent::LogError(format!(
                        "task {task}: waiting for the child failed: {error}"
                    )));
                }
            }
        })
    };

    Ok(ProcHandle {
        task: spec.task,
        pid,
        stdin: Arc::new(Mutex::new(Some(stdin))),
        child: child_cell,
        hook: Mutex::new(None),
        exit: exit_rx,
        exec,
        tx,
    })
}

/// Start the child command, retrying briefly on a transient `ETXTBSY`.
///
/// A program that was written and executed at once, as the tests do with
/// their script files, can fail to start for a few milliseconds while the
/// last write handle lingers. This is not a polling loop: it retries one
/// known transient error a bounded number of times.
fn start_child(command: &mut Command, task: &str, program: &str) -> anyhow::Result<Child> {
    const ATTEMPTS: u32 = 10;
    const RETRY_PAUSE: Duration = Duration::from_millis(10);
    let mut last_busy: Option<std::io::Error> = None;
    for attempt in 1..=ATTEMPTS {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last_busy = Some(error);
                if attempt < ATTEMPTS {
                    thread::sleep(RETRY_PAUSE);
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("task {task}: failed to start {program}"));
            }
        }
    }
    match last_busy {
        Some(error) => {
            Err(error).with_context(|| format!("task {task}: failed to start {program}"))
        }
        None => Err(anyhow!("task {task}: failed to start {program}")),
    }
}

/// A live handle to one supervised child.
///
/// The handle never panics: a write to a dead child's stdin returns an
/// error. Dropping the handle does not stop the child; the owner stops it
/// with [`ProcHandle::kill`], [`ProcHandle::terminate`], or
/// [`stop_gracefully`].
pub struct ProcHandle {
    task: String,
    pid: u32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    hook: Mutex<Option<InterruptHook>>,
    exit: Receiver<()>,
    exec: Arc<dyn Exec>,
    tx: Sender<ProcEvent>,
}

impl ProcHandle {
    /// The task id the child works for.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The process id the child was spawned with.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Install the closure a graceful stop calls as the protocol interrupt.
    ///
    /// A runner uses this to send its own protocol-level interrupt, for
    /// example the claude control-channel interrupt line, before any signal.
    pub fn set_interrupt_hook(&self, hook: InterruptHook) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Write one line to the child's stdin.
    ///
    /// A newline is appended. When the stdin pipe is closed or the child has
    /// exited, this returns an error; it never panics.
    pub fn write_line(&self, line: &str) -> anyhow::Result<()> {
        let mut guard = self.stdin.lock().unwrap_or_else(PoisonError::into_inner);
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow!("task {}: stdin is closed", self.task))?;
        let written = writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush());
        if let Err(error) = written {
            // A broken pipe stays broken; report later writes as closed too.
            *guard = None;
            return Err(anyhow::Error::from(error).context(format!(
                "task {}: write to the child stdin failed; the child may have exited",
                self.task
            )));
        }
        Ok(())
    }

    /// Close the child's stdin, so a child that reads to end of file
    /// finishes its turn.
    pub fn close_stdin(&self) {
        *self.stdin.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Send SIGTERM to the child.
    ///
    /// The signal runs as `kill -TERM <pid>` through the [`Exec`] trait, so
    /// no `libc` dependency is needed. A child that already exited returns
    /// `Ok`.
    pub fn terminate(&self) -> anyhow::Result<()> {
        if !self.is_alive()? {
            return Ok(());
        }
        let pid = self.pid.to_string();
        let out = self
            .exec
            .run("kill", &["-TERM", &pid], None)
            .with_context(|| format!("task {}: SIGTERM via kill failed", self.task))?;
        if out.status != 0 {
            return Err(anyhow!(
                "task {}: kill -TERM {pid} exited with {}: {}",
                self.task,
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(())
    }

    /// Send SIGKILL to the child through [`std::process::Child::kill`].
    ///
    /// A child that already exited returns `Ok`.
    pub fn kill(&self) -> anyhow::Result<()> {
        let mut guard = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(child) = guard.as_mut() else {
            return Ok(());
        };
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        child
            .kill()
            .with_context(|| format!("task {}: SIGKILL failed", self.task))
    }

    /// Whether the child is still running.
    fn is_alive(&self) -> anyhow::Result<bool> {
        let mut guard = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(child) => Ok(child.try_wait()?.is_none()),
            None => Ok(false),
        }
    }
}

/// Stop a child politely: protocol interrupt, wait 10 s, SIGTERM, wait 5 s,
/// SIGKILL.
///
/// The escalation runs on its own thread, so this never blocks the caller.
/// The outcome arrives as a [`ProcEvent::Stopped`] on the spawn channel.
pub fn stop_gracefully(handle: ProcHandle, protocol_interrupt: bool) {
    thread::spawn(move || escalate(handle, protocol_interrupt, TERM_GRACE, KILL_GRACE));
}

/// Like [`stop_gracefully`], but with injectable waits, so a test can run
/// the full escalation in milliseconds.
pub fn stop_gracefully_with_grace(
    handle: ProcHandle,
    protocol_interrupt: bool,
    interrupt_grace: Duration,
    term_grace: Duration,
) {
    thread::spawn(move || escalate(handle, protocol_interrupt, interrupt_grace, term_grace));
}

/// Run the escalation ladder and report the outcome.
///
/// The waits follow the brief: up to `interrupt_grace` after the protocol
/// interrupt, up to `term_grace` after SIGTERM, and up to `term_grace` again
/// after SIGKILL.
fn escalate(
    handle: ProcHandle,
    protocol_interrupt: bool,
    interrupt_grace: Duration,
    term_grace: Duration,
) {
    let mut notes: Vec<String> = Vec::new();
    let outcome = 'ladder: {
        if protocol_interrupt {
            let hook = handle
                .hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                if let Err(error) = hook() {
                    notes.push(format!("interrupt hook failed: {error}"));
                }
            }
            if wait_for_exit(&handle.exit, interrupt_grace) {
                break 'ladder StopOutcome::Exited;
            }
        }
        if let Err(error) = handle.terminate() {
            notes.push(format!("SIGTERM failed: {error}"));
        }
        if wait_for_exit(&handle.exit, term_grace) {
            break 'ladder StopOutcome::Terminated;
        }
        if let Err(error) = handle.kill() {
            notes.push(format!("SIGKILL failed: {error}"));
        }
        if wait_for_exit(&handle.exit, term_grace) {
            break 'ladder StopOutcome::Killed;
        }
        if notes.is_empty() {
            StopOutcome::Failed("child did not exit".to_string())
        } else {
            StopOutcome::Failed(notes.join("; "))
        }
    };
    let _ = handle.tx.send(ProcEvent::Stopped(outcome));
}

/// Wait up to `grace` for the waiter thread's exit report.
fn wait_for_exit(exit: &Receiver<()>, grace: Duration) -> bool {
    exit.recv_timeout(grace).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmdOut, ScriptExec};
    use std::fs;
    use std::path::Path;

    /// One 10 s timeout covers a whole test; every wait inside is shorter.
    fn recv_timeout(rx: &Receiver<ProcEvent>, millis: u64) -> Option<ProcEvent> {
        rx.recv_timeout(Duration::from_millis(millis)).ok()
    }

    /// Collect events until [`ProcEvent::Exit`] arrives, then drain briefly.
    fn collect_until_exit(rx: &Receiver<ProcEvent>) -> Vec<ProcEvent> {
        let mut events = Vec::new();
        while let Some(event) = recv_timeout(rx, 10_000) {
            let exit_seen = matches!(event, ProcEvent::Exit { .. });
            events.push(event);
            if exit_seen {
                break;
            }
        }
        while let Some(event) = recv_timeout(rx, 100) {
            events.push(event);
        }
        events
    }

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-proc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    ///
    /// The write goes to a temporary name and a rename puts it in place, so
    /// the exec never races a still-open write handle (`ETXTBSY`).
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let staging = dir.join(format!(".{name}.tmp"));
        fs::write(&staging, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&staging).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        fs::set_permissions(&staging, permissions).unwrap();
        fs::rename(&staging, &path).unwrap();
        path
    }

    /// A `RunSpec` for `program` with its log inside `dir`.
    fn spec(dir: &Path, task: &str, program: &Path, args: &[String]) -> RunSpec {
        RunSpec {
            task: task.to_string(),
            cwd: dir.to_path_buf(),
            program: program.display().to_string(),
            args: args.to_vec(),
            env: Vec::new(),
            log: dir.join("log.jsonl"),
        }
    }

    #[test]
    fn every_raw_stdout_line_reaches_the_log_byte_for_byte() {
        let dir = temp_dir("raw-lines");
        let program = script(
            &dir,
            "printer",
            "printf 'line one\\n{\"a\":1}\\nnot json !!\\n\\ntail without newline'",
        );
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/raw", &program, &[]), tx).unwrap();

        let events = collect_until_exit(&rx);
        drop(handle);

        let lines: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                ProcEvent::Line(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                &"line one".to_string(),
                &"{\"a\":1}".to_string(),
                &"not json !!".to_string(),
                &String::new(),
                &"tail without newline".to_string(),
            ]
        );
        let logged = fs::read(dir.join("log.jsonl")).unwrap();
        assert_eq!(
            String::from_utf8(logged).unwrap(),
            "line one\n{\"a\":1}\nnot json !!\n\ntail without newline"
        );
    }

    #[test]
    fn exit_code_and_success_flag_are_reported_exactly_once() {
        let dir = temp_dir("exit-once");
        let failing = script(&dir, "fail", "exit 3");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/fail", &failing, &[]), tx).unwrap();
        let events = collect_until_exit(&rx);
        drop(handle);
        let exits: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, ProcEvent::Exit { .. }))
            .collect();
        assert_eq!(
            exits,
            vec![&ProcEvent::Exit {
                code: Some(3),
                ok: false
            }]
        );

        let ok = script(&dir, "ok", "exit 0");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/ok", &ok, &[]), tx).unwrap();
        let events = collect_until_exit(&rx);
        drop(handle);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ProcEvent::Exit { .. }))
                .count(),
            1
        );
        assert!(events.contains(&ProcEvent::Exit {
            code: Some(0),
            ok: true
        }));
    }

    #[test]
    fn write_line_reaches_the_child_and_close_stdin_ends_it() {
        let dir = temp_dir("stdin-echo");
        let program = script(
            &dir,
            "echo-stdin",
            "while read -r line; do echo \"got:$line\"; done; echo done",
        );
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/echo", &program, &[]), tx).unwrap();
        handle.write_line("hello").unwrap();

        // The echo comes back while the child still waits for more input.
        let mut saw_echo = false;
        while !saw_echo {
            match recv_timeout(&rx, 10_000) {
                Some(ProcEvent::Line(line)) if line == "got:hello" => saw_echo = true,
                Some(_) => {}
                None => panic!("the child never echoed the written line"),
            }
        }

        // Closing stdin ends the child's read loop and its turn.
        handle.close_stdin();
        let mut events = Vec::new();
        while let Some(event) = recv_timeout(&rx, 10_000) {
            let exit_seen = matches!(event, ProcEvent::Exit { .. });
            events.push(event);
            if exit_seen {
                break;
            }
        }
        assert!(saw_echo);
        assert!(events.contains(&ProcEvent::Line("done".to_string())));
        assert!(events.contains(&ProcEvent::Exit {
            code: Some(0),
            ok: true
        }));

        // A write after close_stdin returns an error, never a panic.
        handle.close_stdin();
        assert!(handle.write_line("late").is_err());
    }

    #[test]
    fn write_to_a_dead_child_returns_an_error() {
        let dir = temp_dir("dead-stdin");
        let program = script(&dir, "instant", "exit 0");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/dead", &program, &[]), tx).unwrap();
        while !matches!(recv_timeout(&rx, 10_000), Some(ProcEvent::Exit { .. })) {}
        let error = handle
            .write_line("too late")
            .expect_err("write after exit must fail");
        assert!(error.to_string().contains("t/dead"));
    }

    #[test]
    fn stderr_lines_reach_the_log_with_a_prefix_and_no_events() {
        let dir = temp_dir("stderr-prefix");
        let program = script(
            &dir,
            "noisy",
            "echo out1; echo err1 >&2; echo err2 >&2; echo out2",
        );
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/noisy", &program, &[]), tx).unwrap();
        let events = collect_until_exit(&rx);
        drop(handle);

        let lines: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                ProcEvent::Line(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec![&"out1".to_string(), &"out2".to_string()]);

        let logged = fs::read_to_string(dir.join("log.jsonl")).unwrap();
        let mut logged_lines: Vec<&str> = logged.lines().collect();
        logged_lines.sort_unstable();
        assert_eq!(
            logged_lines,
            vec!["out1", "out2", "stderr err1", "stderr err2"]
        );
    }

    #[test]
    fn a_polite_child_dies_at_sigterm() {
        let dir = temp_dir("sigterm-ok");
        let program = script(&dir, "polite", "while :; do sleep 0.05; done");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/polite", &program, &[]), tx).unwrap();
        stop_gracefully_with_grace(
            handle,
            false,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );

        let mut stopped = None;
        while let Some(event) = recv_timeout(&rx, 10_000) {
            match event {
                ProcEvent::Stopped(outcome) => stopped = Some(outcome),
                ProcEvent::Exit { ok, .. } => assert!(!ok),
                _ => {}
            }
        }
        assert_eq!(stopped, Some(StopOutcome::Terminated));
    }

    #[test]
    fn a_child_that_ignores_sigterm_reaches_sigkill() {
        let dir = temp_dir("sigkill");
        let program = script(&dir, "stubborn", "trap '' TERM; while :; do :; done");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/stubborn", &program, &[]), tx).unwrap();
        stop_gracefully_with_grace(
            handle,
            false,
            Duration::from_millis(150),
            Duration::from_millis(150),
        );

        let mut stopped = None;
        while let Some(event) = recv_timeout(&rx, 10_000) {
            match event {
                ProcEvent::Stopped(outcome) => stopped = Some(outcome),
                ProcEvent::Exit { code, ok } => {
                    assert!(!ok);
                    assert_eq!(code, None, "SIGKILL leaves no exit code");
                }
                _ => {}
            }
        }
        assert_eq!(stopped, Some(StopOutcome::Killed));
    }

    #[test]
    fn the_protocol_interrupt_can_stop_the_child_before_any_signal() {
        let dir = temp_dir("interrupt");
        let flag = dir.join("stop-flag");
        let program = script(
            &dir,
            "waiter",
            "while [ ! -f \"$1\" ]; do sleep 0.02; done; exit 0",
        );
        let (tx, rx) = channel();
        let handle = spawn(
            spec(&dir, "t/interrupt", &program, &[flag.display().to_string()]),
            tx,
        )
        .unwrap();
        let flag_for_hook = flag.clone();
        handle.set_interrupt_hook(Arc::new(move || {
            fs::write(&flag_for_hook, b"go")?;
            Ok(())
        }));
        stop_gracefully_with_grace(
            handle,
            true,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );

        let mut stopped = None;
        while let Some(event) = recv_timeout(&rx, 10_000) {
            if let ProcEvent::Stopped(outcome) = event {
                stopped = Some(outcome);
            }
        }
        assert_eq!(stopped, Some(StopOutcome::Exited));
    }

    #[test]
    fn stop_gracefully_does_not_block_the_caller() {
        let dir = temp_dir("non-blocking");
        let program = script(&dir, "polite2", "while :; do sleep 0.05; done");
        let (tx, rx) = channel();
        let handle = spawn(spec(&dir, "t/nonblocking", &program, &[]), tx).unwrap();
        let started = std::time::Instant::now();
        stop_gracefully(handle, false);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "stop_gracefully blocked for {elapsed:?}"
        );
        let mut stopped = None;
        while let Some(event) = recv_timeout(&rx, 10_000) {
            if let ProcEvent::Stopped(outcome) = event {
                stopped = Some(outcome);
            }
        }
        assert_eq!(stopped, Some(StopOutcome::Terminated));
    }

    #[test]
    fn terminate_runs_kill_term_through_the_injected_exec() {
        let dir = temp_dir("exec-sigterm");
        let program = script(&dir, "sleeper", "while :; do sleep 0.05; done");
        let exec =
            Arc::new(ScriptExec::new().expect(|call| call.program == "kill", CmdOut::ok("")));
        let (tx, rx) = channel();
        let handle =
            spawn_with_exec(spec(&dir, "t/sigterm", &program, &[]), tx, exec.clone()).unwrap();

        handle.terminate().unwrap();
        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv(), ["-TERM", &handle.pid().to_string()]);

        // Clean up the child through the real SIGKILL path.
        handle.kill().unwrap();
        while !matches!(recv_timeout(&rx, 10_000), Some(ProcEvent::Exit { .. })) {}
    }
}
