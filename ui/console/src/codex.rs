use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::harness::{AdapterEvent, DispatchJob, HarnessAdapter, HarnessSignal, SharedClock};
use crate::task::ExtIds;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const SUPPORTED: &str = ">=0.150.1,<0.152.0";

pub enum CodexCmd {
    Dispatch(DispatchJob),
    Cancel(String),
    Shutdown,
}

pub struct LinkEnds {
    pub reader: Box<dyn Read + Send + 'static>,
    pub writer: Box<dyn Write + Send + 'static>,
    pub killer: Box<dyn FnOnce() + Send + 'static>,
}

pub type LinkFactory = Box<dyn FnMut() -> Result<LinkEnds> + Send>;

struct Registry {
    by_thread: HashMap<String, String>,
    by_task: HashMap<String, (String, String)>,
}

pub struct CodexAdapter {
    tx: Sender<CodexCmd>,
    clock: Arc<SharedClock>,
    compat: Arc<Mutex<Option<Result<(), String>>>>,
}

pub fn discover_native_codex() -> Option<std::path::PathBuf> {
    if let Ok(bin) = std::env::var("AIF_CODEX_BIN") {
        let path = std::path::PathBuf::from(bin);
        if path.is_file() {
            return Some(path);
        }
    }
    let cli = which_codex()?;
    let cli = std::fs::canonicalize(&cli).ok()?;
    let dir = cli.parent()?;
    for pattern in [
        "../node_modules/@openai/codex-linux-*/vendor/*/bin/codex",
        "../@openai/codex-linux-*/vendor/*/bin/codex",
        "../node_modules/@openai/codex-*/vendor/*/bin/codex",
    ] {
        let Ok(entries) = glob(dir.join(pattern)) else {
            continue;
        };
        if let Some(first) = entries.into_iter().next() {
            return Some(first);
        }
    }
    None
}

fn which_codex() -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = std::path::Path::new(dir).join("codex");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn glob(pattern: std::path::PathBuf) -> Result<Vec<std::path::PathBuf>> {
    let raw = pattern.to_string_lossy().into_owned();
    let Some(star) = raw.find('*') else {
        return Ok(vec![pattern]);
    };
    let prefix = std::path::PathBuf::from(&raw[..star]);
    let Some(parent) = prefix.parent() else {
        return Ok(vec![]);
    };
    let after = &raw[star..];
    let segments: Vec<&str> = after.split('/').collect();
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, segments: &[&str], out: &mut Vec<std::path::PathBuf>) {
        if segments.is_empty() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let head = segments[0];
            let matches = if head.contains('*') {
                let (before, after) = head.split_once('*').unwrap_or((head, ""));
                name.starts_with(before) && name.ends_with(after)
            } else {
                name == head
            };
            if !matches {
                continue;
            }
            let path = entry.path();
            if segments.len() == 1 {
                if path.is_file() {
                    out.push(path);
                }
            } else {
                walk(&path, &segments[1..], out);
            }
        }
    }
    walk(parent, &segments, &mut out);
    out.sort();
    Ok(out)
}

pub fn check_codex_version(bin: Option<&std::path::Path>) -> Result<()> {
    if std::env::var("AIF_ALLOW_UNTESTED_HARNESS").as_deref() == Ok("1") {
        return Ok(());
    }
    let mut cmd = Command::new(bin.unwrap_or_else(|| std::path::Path::new("codex")));
    cmd.arg("--version");
    let out = cmd.output().context("failed to run codex --version")?;
    if !out.status.success() {
        bail!("codex --version failed");
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let version = raw
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or_else(|| anyhow!("cannot parse codex version from {raw:?}"))?;
    if !version_in_range(version, SUPPORTED) {
        bail!(
            "codex-cli {version} is outside the supported range {SUPPORTED}; \
             set AIF_ALLOW_UNTESTED_HARNESS=1 to override"
        );
    }
    Ok(())
}

pub fn version_in_range(version: &str, range: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|p| p.trim_start_matches('v').parse::<u64>().unwrap_or(0))
            .collect()
    };
    let cmp = |a: &[u64], b: &[u64]| -> std::cmp::Ordering {
        for i in 0..a.len().max(b.len()) {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            if x != y {
                return x.cmp(&y);
            }
        }
        std::cmp::Ordering::Equal
    };
    let have = parse(version);
    for part in range.split(',') {
        let part = part.trim();
        if let Some(lo) = part.strip_prefix(">=") {
            if cmp(&have, &parse(lo)) == std::cmp::Ordering::Less {
                return false;
            }
        } else if let Some(hi) = part.strip_prefix('<') {
            if cmp(&have, &parse(hi)) != std::cmp::Ordering::Less {
                return false;
            }
        }
    }
    true
}

impl CodexAdapter {
    pub fn new(events: Sender<AdapterEvent>) -> Self {
        let (tx, rx) = channel::<CodexCmd>();
        let clock = Arc::new(SharedClock::new());
        let compat = Arc::new(Mutex::new(None));
        let bin = discover_native_codex();
        let link_factory: LinkFactory = Box::new(move || {
            let bin = bin
                .clone()
                .or_else(discover_native_codex)
                .ok_or_else(|| anyhow!("codex binary not found"))?;
            let mut child = Command::new(&bin)
                .arg("app-server")
                .arg("--stdio")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("failed to spawn {}", bin.display()))?;
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            Ok(LinkEnds {
                reader: Box::new(stdout),
                writer: Box::new(stdin),
                killer: Box::new(move || {
                    let _ = child.kill();
                    let _ = child.wait();
                }),
            })
        });
        spawn_worker(link_factory, rx, events.clone(), clock.clone(), compat.clone());
        CodexAdapter {
            tx,
            clock,
            compat,
        }
    }

    pub fn with_link(
        events: Sender<AdapterEvent>,
        mut link_factory: LinkFactory,
    ) -> Self {
        let (tx, rx) = channel::<CodexCmd>();
        let clock = Arc::new(SharedClock::new());
        let compat = Arc::new(Mutex::new(Some(Ok(()))));
        let worker_link = link_factory.as_mut();
        let _ = worker_link;
        spawn_worker(link_factory, rx, events.clone(), clock.clone(), compat.clone());
        CodexAdapter {
            tx,
            clock,
            compat,
        }
    }
}

impl HarnessAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn check(&mut self) -> Result<()> {
        let mut guard = self.compat.lock().unwrap();
        if guard.is_none() {
            *guard = Some(
                check_codex_version(discover_native_codex().as_deref())
                    .map_err(|e| e.to_string()),
            );
        }
        match guard.as_ref().unwrap() {
            Ok(()) => Ok(()),
            Err(msg) => bail!("{msg}"),
        }
    }

    fn dispatch(&mut self, job: DispatchJob) {
        let _ = self.tx.send(CodexCmd::Dispatch(job));
    }

    fn cancel(&mut self, task: &str) {
        let _ = self.tx.send(CodexCmd::Cancel(task.to_owned()));
    }

    fn active(&self) -> usize {
        self.clock.active_count()
    }

    fn idle_for(&self) -> Duration {
        self.clock.idle_for()
    }

    fn touch(&mut self) {
        self.clock.touch_now();
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(CodexCmd::Shutdown);
    }
}

fn spawn_worker(
    mut link_factory: LinkFactory,
    rx: Receiver<CodexCmd>,
    events: Sender<AdapterEvent>,
    clock: Arc<SharedClock>,
    _compat: Arc<Mutex<Option<Result<(), String>>>>,
) {
    std::thread::spawn(move || {
        let mut session: Option<Session> = None;
        while let Ok(cmd) = rx.recv() {
            match cmd {
                CodexCmd::Dispatch(job) => {
                    if session.is_none() && !start_session(&mut session, &mut link_factory, &events, &clock) {
                        continue;
                    }
                    let Some(sess) = session.as_mut() else {
                        continue;
                    };
                    match sess.dispatch(&job) {
                        Ok(()) => {}
                        Err(err) => {
                            if err.definitive {
                                let _ = events.send(AdapterEvent::DispatchFailed {
                                    task: job.task.clone(),
                                    definitive: true,
                                    detail: err.msg.clone(),
                                });
                                clock.set_active(clock.active_count().saturating_sub(1));
                            } else {
                                let _ = events.send(AdapterEvent::Unknown {
                                    task: job.task.clone(),
                                    detail: err.msg.clone(),
                                });
                            }
                        }
                    }
                }
                CodexCmd::Cancel(task) => {
                    if let Some(sess) = session.as_mut() {
                        let _ = sess.cancel(&task);
                    }
                }
                CodexCmd::Shutdown => {
                    if let Some(sess) = session.take() {
                        sess.kill();
                    }
                    clock.set_running(false);
                }
            }
        }
        if let Some(sess) = session.take() {
            sess.kill();
        }
    });
}

fn start_session(
    session: &mut Option<Session>,
    link_factory: &mut LinkFactory,
    events: &Sender<AdapterEvent>,
    clock: &Arc<SharedClock>,
) -> bool {
    match link_factory() {
        Ok(ends) => {
            let mut sess = Session::new(ends, events.clone(), clock.clone());
            if sess.initialize() {
                clock.set_running(true);
                clock.touch_now();
                *session = Some(sess);
                true
            } else {
                sess.kill();
                false
            }
        }
        Err(err) => {
            let _ = events.send(AdapterEvent::Notice {
                detail: format!("codex server start failed: {err:#}"),
            });
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReqErr {
    pub definitive: bool,
    pub msg: String,
}

fn definitive(msg: String) -> ReqErr {
    ReqErr {
        definitive: true,
        msg,
    }
}

fn transport(msg: String) -> ReqErr {
    ReqErr {
        definitive: false,
        msg,
    }
}

pub struct Session {
    writer: Box<dyn Write + Send>,
    pending: Arc<Mutex<HashMap<u64, Sender<serde_json::Value>>>>,
    ids: Arc<AtomicU64>,
    registry: Arc<Mutex<Registry>>,
    events: Sender<AdapterEvent>,
    clock: Arc<SharedClock>,
    killer: Option<Box<dyn FnOnce() + Send>>,
}

impl Session {
    fn new(
        ends: LinkEnds,
        events: Sender<AdapterEvent>,
        clock: Arc<SharedClock>,
    ) -> Self {
        let LinkEnds {
            reader,
            writer,
            killer,
        } = ends;
        let pending: Arc<Mutex<HashMap<u64, Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let registry = Arc::new(Mutex::new(Registry {
            by_thread: HashMap::new(),
            by_task: HashMap::new(),
        }));
        let reader_pending = pending.clone();
        let reader_registry = registry.clone();
        let reader_events = events.clone();
        let reader_clock = clock.clone();
        std::thread::spawn(move || {
            let buf = BufReader::new(reader);
            for line in buf.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(value);
                    }
                    continue;
                }
                handle_notification(&value, &reader_registry, &reader_events, &reader_clock);
            }
            let _ = reader_events.send(AdapterEvent::Notice {
                detail: "codex server stream closed".into(),
            });
        });
        Session {
            writer,
            pending,
            ids: Arc::new(AtomicU64::new(1)),
            registry,
            events,
            clock,
            killer: Some(killer),
        }
    }

    fn initialize(&mut self) -> bool {
        let params = serde_json::json!({
            "clientInfo": {
                "name": "aif",
                "title": "ai-factory",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        self.request("initialize", &params).is_ok()
            && self.notify("initialized", &serde_json::json!({})).is_ok()
    }

    fn request(&mut self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value, ReqErr> {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel::<serde_json::Value>();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = serde_json::json!({
            "method": method,
            "id": id,
            "params": params,
        });
        let line = serde_json::to_string(&msg)
            .map_err(|err| transport(format!("serialize failed: {err}")))?;
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|err| transport(format!("codex write failed: {err}")))?;
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(value) => {
                if let Some(err) = value.get("error") {
                    let detail = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    return Err(definitive(format!("codex {method} error: {detail}")));
                }
                Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null))
            }
            Err(_) => Err(transport(format!("codex {method} timed out"))),
        }
    }

    fn notify(&mut self, method: &str, params: &serde_json::Value) -> Result<(), ReqErr> {
        let msg = serde_json::json!({
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&msg)
            .map_err(|err| transport(format!("serialize failed: {err}")))?;
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|err| transport(format!("codex write failed: {err}")))?;
        Ok(())
    }

    fn dispatch(&mut self, job: &DispatchJob) -> Result<(), ReqErr> {
        let thread_params = serde_json::json!({
            "cwd": job.cwd.display().to_string(),
        });
        let thread_resp = self.request("thread/start", &thread_params)?;
        let thread_id = thread_resp
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|i| i.as_str())
            .ok_or_else(|| definitive("thread/start returned no thread id".into()))?
            .to_owned();

        let turn_params = serde_json::json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": job.prompt }],
            "model": job.model,
            "cwd": job.cwd.display().to_string(),
            "sandboxPolicy": { "type": "dangerFullAccess" },
            "approvalPolicy": "never",
            "clientUserMessageId": format!("aif-{}-a{}", job.task, job.attempt),
        });
        let turn_resp = self.request("turn/start", &turn_params)?;
        let turn_id = turn_resp
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_owned();

        self.registry
            .lock()
            .unwrap()
            .by_thread
            .insert(thread_id.clone(), job.task.clone());
        self.registry
            .lock()
            .unwrap()
            .by_task
            .insert(job.task.clone(), (thread_id.clone(), turn_id.clone()));
        self.clock.touch_now();
        let _ = self.events.send(AdapterEvent::DispatchAccepted {
            task: job.task.clone(),
            ext: ExtIds {
                thread: Some(thread_id),
                turn: Some(turn_id),
                session: None,
            },
        });
        Ok(())
    }

    fn cancel(&mut self, task: &str) -> Result<()> {
        let ids = self.registry.lock().unwrap().by_task.get(task).cloned();
        match ids {
            Some((thread_id, turn_id)) => {
                let params = serde_json::json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                });
                let _ = self.request("turn/interrupt", &params);
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn kill(mut self) {
        if let Some(killer) = self.killer.take() {
            killer();
        }
    }
}

fn handle_notification(
    value: &serde_json::Value,
    registry: &Arc<Mutex<Registry>>,
    events: &Sender<AdapterEvent>,
    clock: &Arc<SharedClock>,
) {
    let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = value.get("params").cloned().unwrap_or(serde_json::Value::Null);
    match method {
        "turn/started" => {
            if let Some(task) = task_of(&params, registry, "threadId") {
                let _ = events.send(AdapterEvent::Signal {
                    task,
                    signal: HarnessSignal::Started,
                });
            }
        }
        "turn/completed" => {
            let Some(task) = task_of(&params, registry, "threadId") else {
                return;
            };
            let status = params
                .get("turn")
                .and_then(|t| t.get("status"))
                .and_then(|s| s.as_str())
                .unwrap_or("completed")
                .to_owned();
            let signal = match status.as_str() {
                "failed" => HarnessSignal::Failed {
                    summary: "turn failed".into(),
                },
                "interrupted" => HarnessSignal::Interrupted,
                _ => HarnessSignal::Succeeded {
                    summary: String::new(),
                },
            };
            {
                let mut reg = registry.lock().unwrap();
                if let Some((thread_id, _)) = reg.by_task.remove(&task) {
                    reg.by_thread.remove(&thread_id);
                }
            }
            clock.set_active(clock.active_count().saturating_sub(1));
            clock.touch_now();
            let _ = events.send(AdapterEvent::Signal { task, signal });
        }
        _ => {}
    }
}

fn task_of(params: &serde_json::Value, registry: &Arc<Mutex<Registry>>, key: &str) -> Option<String> {
    let thread = params.get(key).and_then(|v| v.as_str())?;
    registry.lock().unwrap().by_thread.get(thread).cloned()
}

pub type BoxRead = Box<dyn Read + Send + 'static>;
pub type BoxWrite = Box<dyn Write + Send + 'static>;

pub fn mem_pipe() -> (BoxRead, BoxWrite, BoxRead, BoxWrite) {
    let (a_to_b_tx, a_to_b_rx) = channel::<Vec<u8>>();
    let (b_to_a_tx, b_to_a_rx) = channel::<Vec<u8>>();
    let a_read = MemRead::new(a_to_b_rx);
    let b_read = MemRead::new(b_to_a_rx);
    let a_write = MemWrite::new(b_to_a_tx);
    let b_write = MemWrite::new(a_to_b_tx);
    (
        Box::new(a_read),
        Box::new(a_write),
        Box::new(b_read),
        Box::new(b_write),
    )
}

struct MemRead {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
    closed: bool,
}

impl MemRead {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        MemRead {
            rx,
            buf: Vec::new(),
            pos: 0,
            closed: false,
        }
    }
}

impl Read for MemRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            if self.closed {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(chunk) => self.buf = chunk,
                Err(_) => {
                    self.closed = true;
                    return Ok(0);
                }
            }
            self.pos = 0;
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct MemWrite {
    tx: Sender<Vec<u8>>,
}

impl MemWrite {
    fn new(tx: Sender<Vec<u8>>) -> Self {
        MemWrite { tx }
    }
}

impl Write for MemWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::AdapterEvent;
    use std::io::BufReader;

    fn script_server(read: Box<dyn Read + Send>, mut write: Box<dyn Write + Send>) {
        std::thread::spawn(move || {
            let mut buf = BufReader::new(read);
            loop {
                let mut line = String::new();
                match buf.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let resp = match method {
                    "initialize" | "initialized" => serde_json::json!({"id": id, "result": {}}),
                    "thread/start" => serde_json::json!({
                        "id": id,
                        "result": {"thread": {"id": "thr_1"}}
                    }),
                    "turn/start" => serde_json::json!({
                        "id": id,
                        "result": {"turn": {"id": "turn_1"}}
                    }),
                    "turn/interrupt" => serde_json::json!({"id": id, "result": {}}),
                    _ => serde_json::json!({"id": id, "error": {"message": "nope"}}),
                };
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                if write.write_all(out.as_bytes()).is_err() {
                    break;
                }
                let _ = write.flush();
            }
        });
    }

    fn test_adapter(events_tx: Sender<AdapterEvent>) -> CodexAdapter {
        let (a_read, _a_write, b_read, b_write) = mem_pipe();
        script_server(b_read, b_write);
        let mut ends: Option<LinkEnds> = Some(LinkEnds {
            reader: a_read,
            writer: _a_write,
            killer: Box::new(|| {}),
        });
        let factory: LinkFactory = Box::new(move || {
            ends.take().ok_or_else(|| anyhow!("already started"))
        });
        CodexAdapter::with_link(events_tx, factory)
    }

    #[test]
    fn dispatch_handshake_records_ids() {
        let (events_tx, events_rx) = channel::<AdapterEvent>();
        let mut adapter = test_adapter(events_tx);
        adapter.check().unwrap();
        adapter.dispatch(DispatchJob {
            task: "refiner-issue1-r1000003a1".into(),
            node: "refiner".into(),
            model: "gpt-5.6-sol".into(),
            prompt: "do it".into(),
            cwd: "/tmp".into(),
            attempt: 1,
            title: "T".into(),
        });
        match events_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            AdapterEvent::DispatchAccepted { task, ext } => {
                assert_eq!(task, "refiner-issue1-r1000003a1");
                assert_eq!(ext.thread.as_deref(), Some("thr_1"));
                assert_eq!(ext.turn.as_deref(), Some("turn_1"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn version_range_check() {
        assert!(version_in_range("0.150.1", SUPPORTED));
        assert!(version_in_range("0.150.9", SUPPORTED));
        assert!(version_in_range("0.151.0", SUPPORTED));
        assert!(!version_in_range("0.152.0", SUPPORTED));
        assert!(!version_in_range("0.149.2", SUPPORTED));
    }
}
