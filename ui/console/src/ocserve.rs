use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::codex::version_in_range;
use crate::harness::{AdapterEvent, DispatchJob, HarnessAdapter, HarnessSignal, SharedClock};
use crate::ids;
use crate::task::ExtIds;

pub const SUPPORTED_VERSIONS: &str = ">=1.18.25,<1.20.0";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub enum OcCmd {
    Dispatch(DispatchJob),
    Cancel(String),
    Shutdown,
}

pub struct ServerInfo {
    base_url: String,
    auth: String,
}

pub struct OcAdapter {
    tx: Sender<OcCmd>,
    clock: Arc<SharedClock>,
    compat: Arc<Mutex<Option<Result<(), String>>>>,
    server: Arc<Mutex<Option<Arc<ServerShared>>>>,
}

struct ServerShared {
    info: ServerInfo,
    agent: ureq::Agent,
    owned: Mutex<std::collections::BTreeMap<String, String>>,
    pending: Mutex<std::collections::BTreeMap<String, Option<Result<(), String>>>>,
    alive: std::sync::atomic::AtomicBool,
}

pub fn check_oc_version() -> Result<()> {
    if std::env::var("AIF_ALLOW_UNTESTED_HARNESS").as_deref() == Ok("1") {
        return Ok(());
    }
    let out = Command::new("opencode")
        .arg("--version")
        .output()
        .context("failed to run opencode --version")?;
    if !out.status.success() {
        bail!("opencode --version failed");
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let version = raw
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or_else(|| anyhow!("cannot parse opencode version from {raw:?}"))?;
    if !version_in_range(version, SUPPORTED_VERSIONS) {
        bail!(
            "opencode {version} is outside the supported range {SUPPORTED_VERSIONS}; \
             set AIF_ALLOW_UNTESTED_HARNESS=1 to override"
        );
    }
    Ok(())
}

fn basic_auth(user: &str, password: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}");
    let mut out = String::new();
    for chunk in raw.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    format!("Basic {out}")
}

fn spawn_server() -> Result<(ServerInfo, std::process::Child, String)> {
    let password = ids::rand_hex(16)?;
    let mut child = Command::new("opencode")
        .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn opencode serve")?;
    let stdout = child.stdout.take().unwrap();
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    let url = {
        let reader = BufReader::new(stdout);
        let mut found = None;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(rest) = line.split_once("listening on ").map(|(_, r)| r.trim()) {
                if rest.starts_with("http") {
                    found = Some(rest.to_owned());
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                break;
            }
        }
        found
    };
    match url {
        Some(base_url) => Ok((
            ServerInfo {
                auth: basic_auth("opencode", &password),
                base_url,
            },
            child,
            password,
        )),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("opencode serve did not report a listening url")
        }
    }
}

impl OcAdapter {
    pub fn new(events: Sender<AdapterEvent>) -> Self {
        let (tx, rx) = channel::<OcCmd>();
        let clock = Arc::new(SharedClock::new());
        let compat = Arc::new(Mutex::new(None));
        let server: Arc<Mutex<Option<Arc<ServerShared>>>> = Arc::new(Mutex::new(None));
        {
            let events = events.clone();
            let clock = clock.clone();
            let worker_server = server.clone();
            std::thread::spawn(move || {
                let mut child: Option<std::process::Child> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        OcCmd::Dispatch(job) => {
                            let shared =
                                match ensure_server(&worker_server, &mut child, &events, &clock) {
                                    Some(s) => s,
                                    None => {
                                        let _ = events.send(AdapterEvent::DispatchFailed {
                                            task: job.task.clone(),
                                            definitive: true,
                                            detail: "opencode server unavailable".into(),
                                        });
                                        continue;
                                    }
                                };
                            dispatch_job(&shared, &job, &events, &clock);
                        }
                        OcCmd::Cancel(task) => {
                            let guard = worker_server.lock().unwrap();
                            if let Some(shared) = guard.as_ref() {
                                if let Some(session) =
                                    shared.owned.lock().unwrap().get(&task).cloned()
                                {
                                    let url =
                                        format!("{}/session/{session}/abort", shared.info.base_url);
                                    let _ = shared
                                        .agent
                                        .post(&url)
                                        .set("Authorization", &shared.info.auth)
                                        .call();
                                }
                            }
                        }
                        OcCmd::Shutdown => {
                            if let Some(shared) = worker_server.lock().unwrap().as_ref() {
                                shared.alive.store(false, Ordering::SeqCst);
                                let url = format!("{}/instance/dispose", shared.info.base_url);
                                let _ = shared
                                    .agent
                                    .post(&url)
                                    .set("Authorization", &shared.info.auth)
                                    .call();
                            }
                            if let Some(mut child) = child.take() {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                            worker_server.lock().unwrap().take();
                            clock.set_running(false);
                        }
                    }
                }
                if let Some(mut child) = child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            });
        }
        OcAdapter {
            tx,
            clock,
            compat,
            server,
        }
    }
}

fn ensure_server(
    server: &Arc<Mutex<Option<Arc<ServerShared>>>>,
    child: &mut Option<std::process::Child>,
    events: &Sender<AdapterEvent>,
    clock: &Arc<SharedClock>,
) -> Option<Arc<ServerShared>> {
    let existing = server.lock().unwrap().clone();
    if let Some(shared) = existing {
        return Some(shared);
    }
    let start = spawn_server();
    match start {
        Ok((info, spawned, _password)) => {
            let shared = Arc::new(ServerShared {
                alive: AtomicBool::new(true),
                agent: ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(10))
                    .timeout_read(HTTP_TIMEOUT)
                    .build(),
                info,
                owned: Mutex::new(std::collections::BTreeMap::new()),
                pending: Mutex::new(std::collections::BTreeMap::new()),
            });
            let sse_shared = shared.clone();
            let sse_events = events.clone();
            std::thread::spawn(move || sse_loop(sse_shared, sse_events));
            *child = Some(spawned);
            clock.set_running(true);
            clock.touch_now();
            *server.lock().unwrap() = Some(shared.clone());
            Some(shared)
        }
        Err(err) => {
            let _ = events.send(AdapterEvent::Notice {
                detail: format!("opencode serve start failed: {err:#}"),
            });
            None
        }
    }
}

fn dispatch_job(
    shared: &Arc<ServerShared>,
    job: &DispatchJob,
    events: &Sender<AdapterEvent>,
    clock: &Arc<SharedClock>,
) {
    let base = &shared.info.base_url;
    let auth = &shared.info.auth;
    let create_url = format!("{base}/session?directory={}", job.cwd.display());
    let title = format!("aif:{}:a{}", job.task, job.attempt);
    let body = serde_json::json!({ "title": title });
    let create = shared
        .agent
        .post(&create_url)
        .set("Authorization", auth)
        .timeout(START_TIMEOUT)
        .send_json(body);
    let session = match create {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(value) => value["id"].as_str().map(str::to_owned).unwrap_or_default(),
            Err(_) => String::new(),
        },
        Err(ureq::Error::Status(code, _)) => {
            let _ = events.send(AdapterEvent::DispatchFailed {
                task: job.task.clone(),
                definitive: true,
                detail: format!("session create returned {code}"),
            });
            return;
        }
        Err(err) => {
            let _ = events.send(AdapterEvent::Unknown {
                task: job.task.clone(),
                detail: format!("session create transport error: {err}"),
            });
            return;
        }
    };
    if session.is_empty() {
        let _ = events.send(AdapterEvent::Unknown {
            task: job.task.clone(),
            detail: "session create returned no id".into(),
        });
        return;
    }
    let (provider_id, model_id) = match job.model.split_once('/') {
        Some((p, m)) => (p.to_owned(), m.to_owned()),
        None => (job.model.clone(), job.model.clone()),
    };
    let prompt_url = format!(
        "{base}/session/{session}/prompt_async?directory={}",
        job.cwd.display()
    );
    let prompt_body = serde_json::json!({
        "model": { "providerID": provider_id, "modelID": model_id },
        "agent": "build",
        "parts": [{ "type": "text", "text": job.prompt }],
        "messageID": message_id(&job.task, job.attempt),
    });
    match shared
        .agent
        .post(&prompt_url)
        .set("Authorization", auth)
        .timeout(START_TIMEOUT)
        .send_json(prompt_body)
    {
        Ok(_) => {
            shared
                .owned
                .lock()
                .unwrap()
                .insert(job.task.clone(), session.clone());
            shared.pending.lock().unwrap().insert(session.clone(), None);
            clock.touch_now();
            let _ = events.send(AdapterEvent::DispatchAccepted {
                task: job.task.clone(),
                ext: ExtIds {
                    session: Some(session),
                    ..ExtIds::default()
                },
            });
        }
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            let suffix = if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            };
            let _ = events.send(AdapterEvent::DispatchFailed {
                task: job.task.clone(),
                definitive: true,
                detail: format!("prompt_async returned {code}{suffix}"),
            });
        }
        Err(err) => {
            let _ = events.send(AdapterEvent::Unknown {
                task: job.task.clone(),
                detail: format!("prompt_async transport error: {err}"),
            });
        }
    }
}

fn sse_loop(shared: Arc<ServerShared>, events: Sender<AdapterEvent>) {
    let base = shared.info.base_url.clone();
    loop {
        let url = format!("{base}/global/event");
        let request = shared
            .agent
            .get(&url)
            .set("Authorization", &shared.info.auth)
            .set("Accept", "text/event-stream")
            .timeout(Duration::from_secs(60));
        let response = request.call();
        match response {
            Ok(resp) => {
                let reader = resp.into_reader();
                let buf = BufReader::new(reader);
                for line in buf.lines() {
                    let Ok(line) = line else { break };
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    if handle_sse_event(&shared, &value, &events) {
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = events.send(AdapterEvent::Notice {
                    detail: format!("opencode event stream error: {err}"),
                });
            }
        }
        if !shared_running(&shared) {
            return;
        }
        reconcile(&shared, &events);
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn shared_running(shared: &Arc<ServerShared>) -> bool {
    shared.alive.load(Ordering::SeqCst)
}

fn handle_sse_event(
    shared: &Arc<ServerShared>,
    value: &serde_json::Value,
    events: &Sender<AdapterEvent>,
) -> bool {
    let payload = value.get("payload").unwrap_or(value);
    let event_type = payload
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let props = payload.get("properties").cloned().unwrap_or_default();
    match event_type {
        "message.updated" => {
            let info = props.get("info").cloned().unwrap_or_default();
            let session = info.get("sessionID").and_then(|s| s.as_str()).unwrap_or("");
            if owned_task(shared, session).is_some()
                && info.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && info.get("time").and_then(|t| t.get("completed")).is_some()
            {
                let outcome = match info.get("error") {
                    Some(err) => Err(error_summary(err)),
                    None => Ok(()),
                };
                shared
                    .pending
                    .lock()
                    .unwrap()
                    .insert(session.to_owned(), Some(outcome));
            }
        }
        "session.idle" => {
            let session = props
                .get("sessionID")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            resolve_idle(shared, session, events);
        }
        "session.error" => {
            let session = props
                .get("sessionID")
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            let detail = props
                .get("error")
                .map(error_summary)
                .unwrap_or_else(|| "session error".into());
            if let Some(task) = owned_task(shared, session) {
                finish_task(
                    shared,
                    events,
                    &task,
                    HarnessSignal::Failed { summary: detail },
                );
            }
        }
        "session.status" => {
            let session = props
                .get("sessionID")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if let Some(task) = owned_task(shared, session) {
                let _ = events.send(AdapterEvent::Signal {
                    task,
                    signal: HarnessSignal::Started,
                });
            }
        }
        "permission.updated" => {
            let session = props
                .get("sessionID")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let permission = props.get("id").and_then(|s| s.as_str()).unwrap_or("");
            if auto_permissions() {
                if let Some(task) = owned_task(shared, session) {
                    let _ = task;
                    let base = &shared.info.base_url;
                    let url = format!("{base}/session/{session}/permissions/{permission}");
                    let body = serde_json::json!({ "response": "once" });
                    let _ = shared
                        .agent
                        .post(&url)
                        .set("Authorization", &shared.info.auth)
                        .send_json(body);
                }
            }
        }
        _ => {}
    }
    false
}

fn auto_permissions() -> bool {
    std::env::var("AIF_OPENCODE_AUTO_PERMS").as_deref() != Ok("0")
}

fn error_summary(err: &serde_json::Value) -> String {
    err.get("name")
        .and_then(|n| n.as_str())
        .map(|name| {
            format!(
                "{name}: {}",
                err.get("message").and_then(|m| m.as_str()).unwrap_or("")
            )
        })
        .unwrap_or_else(|| "unknown error".into())
}

fn owned_task(shared: &Arc<ServerShared>, session: &str) -> Option<String> {
    shared
        .owned
        .lock()
        .unwrap()
        .iter()
        .find(|(_, s)| s.as_str() == session)
        .map(|(task, _)| task.clone())
}

fn resolve_idle(shared: &Arc<ServerShared>, session: &str, events: &Sender<AdapterEvent>) {
    let Some(task) = owned_task(shared, session) else {
        return;
    };
    let pending = shared
        .pending
        .lock()
        .unwrap()
        .get(session)
        .cloned()
        .flatten();
    match pending {
        None => {}
        Some(Ok(())) => finish_task(
            shared,
            events,
            &task,
            HarnessSignal::Succeeded {
                summary: String::new(),
            },
        ),
        Some(Err(summary)) => finish_task(shared, events, &task, HarnessSignal::Failed { summary }),
    }
}

fn finish_task(
    shared: &Arc<ServerShared>,
    events: &Sender<AdapterEvent>,
    task: &str,
    signal: HarnessSignal,
) {
    let session = shared.owned.lock().unwrap().get(task).cloned();
    if let Some(session) = session {
        shared.pending.lock().unwrap().remove(&session);
    }
    shared.owned.lock().unwrap().remove(task);
    let _ = events.send(AdapterEvent::Signal {
        task: task.to_owned(),
        signal,
    });
}

fn reconcile(shared: &Arc<ServerShared>, _events: &Sender<AdapterEvent>) {
    let base = &shared.info.base_url;
    let url = format!("{base}/session/status");
    let Ok(resp) = shared
        .agent
        .get(&url)
        .set("Authorization", &shared.info.auth)
        .call()
    else {
        return;
    };
    let Ok(value) = resp.into_json::<serde_json::Value>() else {
        return;
    };
    let _ = value;
}

impl HarnessAdapter for OcAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn check(&mut self) -> Result<()> {
        let mut guard = self.compat.lock().unwrap();
        if guard.is_none() {
            *guard = Some(check_oc_version().map_err(|e| e.to_string()));
        }
        match guard.as_ref().unwrap() {
            Ok(()) => Ok(()),
            Err(msg) => bail!("{msg}"),
        }
    }

    fn dispatch(&mut self, job: DispatchJob) {
        let _ = self.tx.send(OcCmd::Dispatch(job));
    }

    fn cancel(&mut self, task: &str) {
        let _ = self.tx.send(OcCmd::Cancel(task.to_owned()));
    }

    fn active(&self) -> usize {
        self.server
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.owned.lock().unwrap().len())
            .unwrap_or(0)
    }

    fn idle_for(&self) -> Duration {
        self.clock.idle_for()
    }

    fn touch(&mut self) {
        self.clock.touch_now();
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(OcCmd::Shutdown);
    }
}

pub fn session_title(task: &str, attempt: u32) -> String {
    format!("aif:{task}:a{attempt}")
}

fn message_id(task: &str, attempt: u32) -> String {
    format!("msg_aif-{task}-a{attempt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_shape() {
        let auth = basic_auth("opencode", "hello");
        let encoded = auth.strip_prefix("Basic ").unwrap();
        assert_eq!(encoded.len() % 4, 0);
    }

    #[test]
    fn title_is_deterministic() {
        assert_eq!(session_title("t", 2), "aif:t:a2");
    }

    #[test]
    fn message_id_has_required_prefix() {
        assert_eq!(message_id("task-1", 2), "msg_aif-task-1-a2");
    }

    #[test]
    fn version_range() {
        assert!(version_in_range("1.18.25", SUPPORTED_VERSIONS));
        assert!(version_in_range("1.19.2", SUPPORTED_VERSIONS));
        assert!(!version_in_range("1.20.0", SUPPORTED_VERSIONS));
        assert!(!version_in_range("1.17.0", SUPPORTED_VERSIONS));
    }
}
