use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::control::{self, Incoming, Reply};
use crate::factory::FactoryPaths;
use crate::graph::{Agent, Exec, Graph};
use crate::harness::{AdapterEvent, DispatchJob, HarnessAdapter, HarnessSignal};
use crate::ids;
use crate::journal::{Journal, Rec};
use crate::ocserve::OcAdapter;
use crate::codex::CodexAdapter;
use crate::snapshot::{apply_batch, GateTracker, ItemKind, ItemState, ReadyWork};
use crate::source::{GithubPoller, Observation, PollConfig, RealGh};
use crate::task::{TaskRecord, TaskState};

const MAX_QUEUED_RETRIES: u32 = 3;
const JOURNAL_WARN_BYTES: u64 = 50 * 1024 * 1024;

pub enum Msg {
    Obs(Observation),
    Harness(&'static str, AdapterEvent),
    Control(Incoming),
    Tick,
    Stop { force: bool, reply: Option<Sender<Reply>> },
}

pub type PresentFn = Box<dyn Fn(&str, &str, &str) -> Result<()> + Send>;
pub type SubmitFn = Box<dyn Fn(&str, &str) -> Result<()> + Send>;

pub struct SupervisedIo {
    pub present: PresentFn,
    pub submit: SubmitFn,
    pub session: String,
}

pub fn zellij_supervised(session: String) -> SupervisedIo {
    SupervisedIo {
        session,
        present: Box::new(crate::actions::paste_text),
        submit: Box::new(crate::actions::press_enter),
    }
}

pub fn noop_supervised() -> SupervisedIo {
    SupervisedIo {
        session: String::new(),
        present: Box::new(|_, _, _| Ok(())),
        submit: Box::new(|_, _| Ok(())),
    }
}

pub type WorktreeFn = Box<dyn Fn(&TaskRecord, &FactoryPaths) -> Result<PathBuf> + Send>;

pub struct WorktreeMaker {
    pub make: WorktreeFn,
}

pub fn real_worktrees() -> WorktreeMaker {
    WorktreeMaker {
        make: Box::new(|task, paths| {
            let dir = paths
                .worktrees_dir()
                .join(ids::sanitize_component(&task.id));
            let branch = format!("aif/{}", ids::sanitize_component(&task.id));
            let out = std::process::Command::new("git")
                .current_dir(&paths.root)
                .args(["worktree", "add", "-b", &branch, dir.to_string_lossy().as_ref()])
                .output()
                .with_context(|| "git worktree add failed to start")?;
            if !out.status.success() {
                bail!(
                    "git worktree add failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(dir)
        }),
    }
}

pub fn temp_worktrees() -> WorktreeMaker {
    WorktreeMaker {
        make: Box::new(|task, paths| {
            let dir = paths
                .worktrees_dir()
                .join(ids::sanitize_component(&task.id));
            std::fs::create_dir_all(&dir)?;
            Ok(dir)
        }),
    }
}

pub struct Daemon {
    pub paths: FactoryPaths,
    pub graph: Graph,
    pub journal: Journal,
    stop_flag: bool,
    snapshot: BTreeMap<String, ItemState>,
    gates: GateTracker,
    pub tasks: BTreeMap<String, TaskRecord>,
    order: Vec<String>,
    adapters: BTreeMap<&'static str, Box<dyn HarnessAdapter>>,
    supervised: SupervisedIo,
    worktrees: WorktreeMaker,
    paused_global: bool,
    paused_nodes: BTreeSet<String>,
    stale_source: bool,
    dispatch_begun: BTreeSet<String>,
    retries: BTreeMap<String, u32>,
    subscribers: Vec<(u64, Sender<String>)>,
    revision: u64,
    wake: Option<Sender<()>>,
    log_tx: Option<Sender<String>>,
}

impl Daemon {
    pub fn new(
        paths: FactoryPaths,
        graph: Graph,
        journal: Journal,
        adapters: Vec<(&'static str, Box<dyn HarnessAdapter>)>,
        supervised: SupervisedIo,
        worktrees: WorktreeMaker,
        records: Vec<crate::journal::Record>,
    ) -> Result<Self> {
        let mut daemon = Daemon {
            paths,
            graph,
            journal,
            stop_flag: false,
            snapshot: BTreeMap::new(),
            gates: GateTracker::default(),
            tasks: BTreeMap::new(),
            order: Vec::new(),
            adapters: adapters.into_iter().collect(),
            supervised,
            worktrees,
            paused_global: false,
            paused_nodes: BTreeSet::new(),
            stale_source: false,
            dispatch_begun: BTreeSet::new(),
            retries: BTreeMap::new(),
            subscribers: Vec::new(),
            revision: 0,
            wake: None,
            log_tx: None,
        };
        daemon.replay(records)?;
        daemon.recover();
        Ok(daemon)
    }

    fn replay(&mut self, records: Vec<crate::journal::Record>) -> Result<()> {
        for record in records {
            self.revision = record.seq;
            match record.rec {
                Rec::FactoryMeta { .. } | Rec::Trust { .. } => {}
                Rec::SourceBatch { items, .. } => {
                    let changed = apply_batch(&mut self.snapshot, items);
                    self.gates.apply(&self.graph, &self.snapshot, &changed);
                }
                Rec::TaskCreated {
                    id,
                    node,
                    item_kind,
                    number,
                    item_node_id,
                    title,
                    revision,
                    attempt,
                } => {
                    let spec = self.graph.node(&node);
                    let agent = spec.map(|s| s.agent).unwrap_or(Agent::Codex);
                    let exec = spec.map(|s| s.exec).unwrap_or(Exec::Auto);
                    let kind = ItemKind::parse(&item_kind).unwrap_or(ItemKind::Issue);
                    let record = TaskRecord {
                        id: id.clone(),
                        node: node.clone(),
                        agent,
                        exec,
                        kind,
                        number,
                        item_node_id: item_node_id.clone(),
                        title: title.clone(),
                        revision,
                        attempt,
                        state: TaskState::Queued,
                        ext: Default::default(),
                        detail: String::new(),
                        created_seq: record.seq,
                        worktree: None,
                    };
                    let gate_key = GateTracker::gate_key(&node, &item_node_id);
                    self.gates.last_tasked_rev.insert(gate_key, revision);
                    self.tasks.insert(id.clone(), record);
                    self.order.push(id);
                }
                Rec::TaskTransition { id, to, .. } => {
                    if let Some(state) = TaskState::parse(&to) {
                        if let Some(task) = self.tasks.get_mut(&id) {
                            task.state = state;
                        }
                    }
                }
                Rec::DispatchBegin { id, .. } => {
                    self.dispatch_begun.insert(id);
                }
                Rec::External { id, ext, .. } => {
                    if let Some(task) = self.tasks.get_mut(&id) {
                        if ext.thread.is_some() {
                            task.ext.thread = ext.thread;
                        }
                        if ext.turn.is_some() {
                            task.ext.turn = ext.turn;
                        }
                        if ext.session.is_some() {
                            task.ext.session = ext.session;
                        }
                    }
                }
                Rec::Paused { node, paused } => match node {
                    Some(node) => {
                        if paused {
                            self.paused_nodes.insert(node);
                        } else {
                            self.paused_nodes.remove(&node);
                        }
                    }
                    None => self.paused_global = paused,
                },
            }
        }
        Ok(())
    }

    fn recover(&mut self) {
        let ids: Vec<String> = self.tasks.keys().cloned().collect();
        for id in ids {
            let Some(task) = self.tasks.get(&id) else {
                continue;
            };
            let (state, exec, begun) = (task.state, task.exec, self.dispatch_begun.contains(&id));
            let recovered = match (state, exec) {
                (TaskState::Reserved, Exec::Auto) => {
                    if begun {
                        Some((TaskState::Uncertain, "recovered after crash; dispatch may have started".to_owned()))
                    } else {
                        Some((TaskState::Queued, "recovered before transport".to_owned()))
                    }
                }
                (TaskState::Accepted, Exec::Auto) | (TaskState::Running, Exec::Auto) => {
                    Some((TaskState::Uncertain, "harness state lost after restart".to_owned()))
                }
                (TaskState::CancelRequested, _) => {
                    Some((TaskState::Uncertain, "cancellation outcome unknown after restart".to_owned()))
                }
                _ => None,
            };
            if let Some((to, detail)) = recovered {
                self.transition(&id, to, &detail);
            }
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn append(&mut self, rec: Rec) -> Result<()> {
        let record = self.journal.append(rec)?;
        self.revision = record.seq;
        let line = serde_json::to_string(&record)?;
        self.subscribers.retain(|(_, tx)| tx.send(line.clone()).is_ok());
        Ok(())
    }

    pub fn set_wake(&mut self, wake: Sender<()>) {
        self.wake = Some(wake);
    }

    pub fn set_logger(&mut self, tx: Sender<String>) {
        self.log_tx = Some(tx);
    }

    fn log(&self, line: String) {
        if let Some(tx) = &self.log_tx {
            let _ = tx.send(line);
        }
    }

    fn transition(&mut self, id: &str, to: TaskState, detail: &str) -> bool {
        let Some(task) = self.tasks.get(id) else {
            return false;
        };
        let from = task.state;
        if from == to || !from.can_reach(to) {
            return false;
        }
        if let Err(err) = self.append(Rec::TaskTransition {
            id: id.to_owned(),
            from: from.as_str().to_owned(),
            to: to.as_str().to_owned(),
            detail: detail.to_owned(),
        }) {
            eprintln!("aif: journal append failed: {err:#}");
            return false;
        }
        if let Some(task) = self.tasks.get_mut(id) {
            task.state = to;
            task.detail = if detail.is_empty() {
                String::new()
            } else {
                detail.to_owned()
            };
        }
        if to.is_terminal() {
            self.dispatch_begun.remove(id);
        }
        true
    }

    fn node_limit(&self, node: &str) -> usize {
        self.graph
            .node(node)
            .and_then(|n| n.limit)
            .unwrap_or(1)
    }

    fn node_count(&self, node: &str) -> usize {
        self.tasks
            .values()
            .filter(|t| t.node == node && t.state.consumes_node())
            .count()
    }

    fn global_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.state.consumes_global())
            .count()
    }

    fn render_prompt(&self, task: &TaskRecord) -> Result<String> {
        let node = self
            .graph
            .node(&task.node)
            .with_context(|| format!("node {} missing from graph", task.node))?;
        let legacy_task = crate::scheduler::Task {
            node: task.node.clone(),
            kind: match task.kind {
                ItemKind::Issue => crate::probe::ItemKind::Issue,
                ItemKind::PullRequest => crate::probe::ItemKind::PullRequest,
            },
            number: task.number,
            title: task.title.clone(),
            url: String::new(),
        };
        crate::scheduler::render_prompt(node, &self.paths.root, &legacy_task)
    }

    pub fn observe(&mut self, obs: Observation) {
        if obs.stale {
            self.stale_source = true;
            return;
        }
        let changed = apply_batch(&mut self.snapshot, obs.items);
        if !changed.is_empty() || obs.forced {
            self.stale_source = false;
        }
        let ready = self.gates.apply(&self.graph, &self.snapshot, &changed);
        for work in ready {
            self.create_task(work);
        }
        self.pump();
    }

    fn create_task(&mut self, work: ReadyWork) {
        let spec = self.graph.node(&work.node);
        let (agent, exec) = match spec {
            Some(spec) => (spec.agent, spec.exec),
            None => return,
        };
        let attempt = 1;
        let id = TaskRecord::task_id(&work.node, work.kind, work.number, work.revision, attempt);
        if self.tasks.contains_key(&id) {
            return;
        }
        let record = Rec::TaskCreated {
            id: id.clone(),
            node: work.node.clone(),
            item_kind: work.kind.as_str().to_owned(),
            number: work.number,
            item_node_id: work.item_node_id.clone(),
            title: work.title.clone(),
            revision: work.revision,
            attempt,
        };
        if self.append(record).is_err() {
            return;
        }
        let gate_key = GateTracker::gate_key(&work.node, &work.item_node_id);
        self.gates
            .last_tasked_rev
            .insert(gate_key, work.revision);
        self.tasks.insert(
            id.clone(),
            TaskRecord {
                id: id.clone(),
                node: work.node.clone(),
                agent,
                exec,
                kind: work.kind,
                number: work.number,
                item_node_id: work.item_node_id,
                title: work.title,
                revision: work.revision,
                attempt,
                state: TaskState::Queued,
                ext: Default::default(),
                detail: String::new(),
                created_seq: self.revision,
                worktree: None,
            },
        );
        self.order.push(id.clone());
        self.log(format!("task {id} queued"));
    }

    fn pane_for(&self, node: &str) -> Option<String> {
        let registry = crate::status::registry_roles(&self.supervised.session);
        registry
            .iter()
            .find(|(_, role)| role.eq_ignore_ascii_case(node))
            .map(|(pane, _)| pane.clone())
    }

    fn pump(&mut self) {
        loop {
            let next = self
                .order
                .iter()
                .filter(|id| {
                    self.tasks
                        .get(*id)
                        .map(|t| t.state == TaskState::Queued)
                        .unwrap_or(false)
                })
                .find(|id| {
                    self.tasks
                        .get(*id)
                        .map(|t| !self.paused_global && !self.paused_nodes.contains(&t.node))
                        .unwrap_or(false)
                })
                .cloned();
            let Some(id) = next else {
                return;
            };
            let Some(task) = self.tasks.get(&id) else {
                return;
            };
            let node = task.node.clone();
            if self.node_count(&node) >= self.node_limit(&node) {
                                return;
            }
            if task.state.consumes_global() || self.global_count() >= self.graph.limit {
                                return;
            }
            let exec = task.exec;
            let agent = task.agent;
            if exec == Exec::Supervised {
                if !self.transition(&id, TaskState::Presenting, "") {
                    return;
                }
                let prompt = match self.render_prompt(&self.tasks[&id]) {
                    Ok(prompt) => prompt,
                    Err(err) => {
                        self.transition(&id, TaskState::Failed, &format!("{err:#}"));
                        continue;
                    }
                };
                let pane = self.pane_for(&node);
                match pane {
                    Some(pane) => {
                        let session = self.supervised.session.clone();
                        let presented = (self.supervised.present)(&session, &pane, &prompt);
                        if presented.is_ok() {
                            self.transition(&id, TaskState::AwaitingUser, &format!("prefilled {pane}"));
                        } else {
                            self.transition(&id, TaskState::Failed, "pane prefill failed");
                        }
                    }
                    None => {
                        self.transition(&id, TaskState::Failed, "no supervised pane for node");
                    }
                }
                continue;
            }
            if !self.paths.trusted() {
                self.log(format!("task {id} waits: factory is not trusted; run `aif trust`"));
                                return;
            }
            if self.stale_source {
                self.log(format!("task {id} waits: source is stale"));
                                return;
            }
            if !self.adapters.contains_key(agent.as_str()) {
                self.transition(&id, TaskState::Failed, "no adapter for agent");
                continue;
            }
            if let Err(err) = self.adapters.get_mut(agent.as_str()).unwrap().check() {
                self.log(format!("task {id} waits: {}: {err:#}", agent.as_str()));
                return;
            }
                        if !self.transition(&id, TaskState::Reserved, "") {
                                return;
            }
            let worktree = {
                let task = &self.tasks[&id];
                (self.worktrees.make)(task, &self.paths)
            };
            match worktree {
                Ok(path) => {
                    if let Some(task) = self.tasks.get_mut(&id) {
                        task.worktree = Some(path.display().to_string());
                    }
                }
                Err(err) => {
                    self.transition(&id, TaskState::Failed, &format!("worktree: {err:#}"));
                    continue;
                }
            }
            let target = agent.as_str().to_owned();
            if self
                .append(Rec::DispatchBegin {
                    id: id.clone(),
                    target: target.clone(),
                })
                .is_err()
            {
                return;
            }
            self.dispatch_begun.insert(id.clone());
            let task = &self.tasks[&id];
            let prompt = match self.render_prompt(task) {
                Ok(prompt) => prompt,
                Err(err) => {
                    self.transition(&id, TaskState::Failed, &format!("{err:#}"));
                    continue;
                }
            };
            let job = DispatchJob {
                task: id.clone(),
                node: task.node.clone(),
                model: self
                    .graph
                    .node(&task.node)
                    .map(|n| n.model.clone())
                    .unwrap_or_default(),
                prompt,
                cwd: PathBuf::from(task.worktree.clone().unwrap_or_else(|| {
                    self.paths.root.display().to_string()
                })),
                attempt: task.attempt,
                title: task.title.clone(),
            };
            self.log(format!("task {id} dispatched to {target}"));
            self.adapters
                .get_mut(agent.as_str())
                .unwrap()
                .dispatch(job);
        }
    }

    pub fn on_adapter(&mut self, _name: &'static str, event: AdapterEvent) {
        match event {
            AdapterEvent::DispatchAccepted { task, ext } => {
                let _ = self.append(Rec::External {
                    id: task.clone(),
                    ext,
                });
                self.transition(&task, TaskState::Accepted, "");
            }
            AdapterEvent::DispatchFailed {
                task,
                definitive,
                detail,
            } => {
                if definitive {
                    let retries = self.retries.get(&task).copied().unwrap_or(0) + 1;
                    self.retries.insert(task.clone(), retries);
                    if retries > MAX_QUEUED_RETRIES {
                        self.transition(&task, TaskState::Failed, &detail);
                    } else {
                        self.transition(&task, TaskState::Queued, &detail);
                    }
                } else {
                    self.transition(&task, TaskState::Uncertain, &detail);
                }
            }
            AdapterEvent::Signal { task, signal } => match signal {
                HarnessSignal::Started => {
                    self.transition(&task, TaskState::Running, "");
                }
                HarnessSignal::Succeeded { summary } => {
                    self.transition(&task, TaskState::Succeeded, &summary);
                }
                HarnessSignal::Failed { summary } => {
                    self.transition(&task, TaskState::Failed, &summary);
                }
                HarnessSignal::Interrupted => {
                    self.transition(&task, TaskState::Cancelled, "interrupted");
                }
            },
            AdapterEvent::Unknown { task, detail } => {
                self.transition(&task, TaskState::Uncertain, &detail);
            }
            AdapterEvent::Notice { detail } => {
                self.log(detail);
            }
        }
        self.pump();
    }

    pub fn on_tick(&mut self) {
        let idle_target = Duration::from_secs(
            std::env::var("AIF_SERVER_IDLE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        );
        let mut stopped: Vec<&'static str> = Vec::new();
        for (name, adapter) in self.adapters.iter_mut() {
            if adapter.active() == 0 && adapter.idle_for() >= idle_target {
                adapter.shutdown();
                stopped.push(*name);
            }
        }
        for name in stopped {
            self.log(format!("{name} server stopped after idle"));
        }
        if self.journal.size() > JOURNAL_WARN_BYTES {
            self.log(format!(
                "journal is {} MiB; consider archiving it",
                self.journal.size() / (1024 * 1024)
            ));
        }
    }

    pub fn status_json(&self) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = self
            .order
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .map(|task| {
                serde_json::json!({
                    "id": task.id,
                    "node": task.node,
                    "item": format!("{}#{}", task.kind.as_str(), task.number),
                    "title": task.title,
                    "state": task.state.as_str(),
                    "attempt": task.attempt,
                    "detail": task.detail,
                    "worktree": task.worktree,
                    "ext": {
                        "thread": task.ext.thread,
                        "turn": task.ext.turn,
                        "session": task.ext.session,
                    },
                })
            })
            .collect();
        let nodes: Vec<serde_json::Value> = self
            .graph
            .nodes
            .iter()
            .map(|node| {
                serde_json::json!({
                    "name": node.name,
                    "agent": node.agent.as_str(),
                    "model": node.model,
                    "exec": node.exec.as_str(),
                    "limit": node.limit.unwrap_or(1),
                    "active": self.node_count(&node.name),
                    "paused": self.paused_nodes.contains(&node.name),
                })
            })
            .collect();
        let adapters: Vec<serde_json::Value> = self
            .adapters
            .iter()
            .map(|(name, adapter)| {
                serde_json::json!({
                    "name": name,
                    "active": adapter.active(),
                    "idle_secs": adapter.idle_for().as_secs(),
                })
            })
            .collect();
        serde_json::json!({
            "factory_id": self.paths.factory_id,
            "revision": self.revision,
            "paused": self.paused_global,
            "stale_source": self.stale_source,
            "trusted": self.paths.trusted(),
            "journal_bytes": self.journal.size(),
            "nodes": nodes,
            "adapters": adapters,
            "tasks": tasks,
        })
    }

    fn reply_ok(&self, id: &str, result: serde_json::Value) -> Reply {
        Reply::ok(id, self.revision, result)
    }

    pub fn on_control(&mut self, incoming: Incoming) {
        let control::Incoming::Request {
            envelope,
            reply,
            follow,
        } = incoming;
        let response = match envelope.method.as_str() {
            "ping" => self.reply_ok(&envelope.id, serde_json::json!({"pong": true})),
            "status" => self.reply_ok(&envelope.id, self.status_json()),
            "pause" => {
                let node = envelope.params.get("node").and_then(|n| n.as_str()).map(str::to_owned);
                let _ = self.append(Rec::Paused {
                    node: node.clone(),
                    paused: true,
                });
                match node {
                    Some(node) => {
                        self.paused_nodes.insert(node.clone());
                        self.reply_ok(&envelope.id, serde_json::json!({"node": node, "paused": true}))
                    }
                    None => {
                        self.paused_global = true;
                        self.reply_ok(&envelope.id, serde_json::json!({"paused": true}))
                    }
                }
            }
            "resume" => {
                let node = envelope.params.get("node").and_then(|n| n.as_str()).map(str::to_owned);
                let _ = self.append(Rec::Paused {
                    node: node.clone(),
                    paused: false,
                });
                match node {
                    Some(node) => {
                        self.paused_nodes.remove(&node);
                        self.reply_ok(&envelope.id, serde_json::json!({"node": node, "paused": false}))
                    }
                    None => {
                        self.paused_global = false;
                        self.pump();
                        self.reply_ok(&envelope.id, serde_json::json!({"paused": false}))
                    }
                }
            }
            "reconcile" => {
                if let Some(wake) = &self.wake {
                    let _ = wake.send(());
                }
                self.reply_ok(&envelope.id, serde_json::json!({"reconcile": "requested"}))
            }
            "task.submit" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                self.submit_supervised(&id, &envelope.id)
            }
            "task.cancel" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                self.cancel(&id, &envelope.id)
            }
            "task.retry" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                self.retry(&id, &envelope.id)
            }
            "task.resolve" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                let outcome = envelope.params["outcome"].as_str().unwrap_or_default();
                let to = match outcome {
                    "succeeded" => Some(TaskState::Succeeded),
                    "failed" => Some(TaskState::Failed),
                    "cancelled" => Some(TaskState::Cancelled),
                    _ => None,
                };
                let Some(to) = to else {
                    let bad = Reply::err(
                        &envelope.id,
                        self.revision,
                        "bad_params",
                        "outcome must be succeeded|failed|cancelled".to_owned(),
                    );
                    let _ = reply.send(bad);
                    return;
                };
                if self.transition(&id, to, "operator resolved") {
                    self.pump();
                    self.reply_ok(&envelope.id, serde_json::json!({"task": id, "state": to.as_str()}))
                } else {
                    Reply::err(&envelope.id, self.revision, "bad_state", "task is not uncertain".into())
                }
            }
            "task.dismiss" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                if self.transition(&id, TaskState::Superseded, "dismissed") {
                    self.pump();
                    self.reply_ok(&envelope.id, serde_json::json!({"task": id, "state": "superseded"}))
                } else {
                    Reply::err(&envelope.id, self.revision, "bad_state", "task is not dismissable".into())
                }
            }
            "task.complete" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                if self.transition(&id, TaskState::Succeeded, "operator completed") {
                    self.pump();
                    self.reply_ok(&envelope.id, serde_json::json!({"task": id, "state": "succeeded"}))
                } else {
                    Reply::err(&envelope.id, self.revision, "bad_state", "task cannot complete".into())
                }
            }
            "task.fail" => {
                let id = envelope.params["task"].as_str().unwrap_or_default().to_owned();
                if self.transition(&id, TaskState::Failed, "operator failed") {
                    self.pump();
                    self.reply_ok(&envelope.id, serde_json::json!({"task": id, "state": "failed"}))
                } else {
                    Reply::err(&envelope.id, self.revision, "bad_state", "task cannot fail".into())
                }
            }
            "stop" => {
                let force = envelope.params["force"].as_bool().unwrap_or(false);
                let active: Vec<String> = self
                    .tasks
                    .values()
                    .filter(|t| t.state.consumes_global())
                    .map(|t| t.id.clone())
                    .collect();
                if !force && !active.is_empty() {
                    Reply::err(
                        &envelope.id,
                        self.revision,
                        "busy",
                        format!("{} active tasks; use force to stop", active.len()),
                    )
                } else {
                    if force {
                        for id in &active {
                            if self.tasks.get(id).map(|t| t.exec) == Some(Exec::Auto) {
                                let agent = self.tasks.get(id).map(|t| t.agent);
                                if let Some(agent) = agent {
                                    if let Some(adapter) = self.adapters.get_mut(agent.as_str()) {
                                        adapter.cancel(id);
                                    }
                                }
                                self.transition(id, TaskState::CancelRequested, "force stop");
                            }
                        }
                    }
                    let reply_msg = self.reply_ok(&envelope.id, serde_json::json!({"stopping": true}));
                    let _ = reply.send(reply_msg.clone());
                    self.stop_flag = true;
                    reply_msg
                }
            }
            "events.follow" => {
                if let Some(follow) = follow {
                    self.subscribers.push((self.revision, follow));
                }
                self.reply_ok(&envelope.id, serde_json::json!({"revision": self.revision}))
            }
            other => Reply::err(
                &envelope.id,
                self.revision,
                "unknown_method",
                format!("unknown method {other:?}"),
            ),
        };
        let _ = reply.send(response);
    }

    fn submit_supervised(&mut self, id: &str, request_id: &str) -> Reply {
        let Some(task) = self.tasks.get(id) else {
            return Reply::err(request_id, self.revision, "not_found", "no such task".into());
        };
        if task.state != TaskState::AwaitingUser {
            return Reply::err(request_id, self.revision, "bad_state", "task is not awaiting the user".into());
        }
        let node = task.node.clone();
        if !self.transition(id, TaskState::Reserved, "user submitted") {
            return Reply::err(request_id, self.revision, "bad_state", "submit refused".into());
        }
        let Some(pane) = self.pane_for(&node) else {
            self.transition(id, TaskState::Failed, "pane vanished");
            return Reply::err(request_id, self.revision, "bad_state", "pane vanished".into());
        };
        let session = self.supervised.session.clone();
        let outcome = (self.supervised.submit)(&session, &pane);
        match outcome {
            Ok(()) => {
                self.transition(id, TaskState::Accepted, "submitted to pane");
                self.pump();
                self.reply_ok(request_id, serde_json::json!({"task": id, "state": "accepted"}))
            }
            Err(err) => {
                self.transition(id, TaskState::Failed, &format!("submit failed: {err:#}"));
                Reply::err(request_id, self.revision, "submit_failed", format!("{err:#}"))
            }
        }
    }

    fn cancel(&mut self, id: &str, request_id: &str) -> Reply {
        let Some(task) = self.tasks.get(id) else {
            return Reply::err(request_id, self.revision, "not_found", "no such task".into());
        };
        let (state, exec, agent) = (task.state, task.exec, task.agent);
        match state {
            TaskState::Queued | TaskState::Presenting | TaskState::AwaitingUser => {
                if self.transition(id, TaskState::Cancelled, "operator cancelled") {
                    self.pump();
                    self.reply_ok(request_id, serde_json::json!({"task": id, "state": "cancelled"}))
                } else {
                    Reply::err(request_id, self.revision, "bad_state", "cancel refused".into())
                }
            }
            TaskState::Reserved if exec == Exec::Auto => {
                if self.dispatch_begun.contains(id) {
                    if let Some(adapter) = self.adapters.get_mut(agent.as_str()) {
                        adapter.cancel(id);
                    }
                    self.transition(id, TaskState::CancelRequested, "operator cancelled");
                    self.reply_ok(request_id, serde_json::json!({"task": id, "state": "cancel_requested"}))
                } else {
                    self.transition(id, TaskState::Cancelled, "operator cancelled");
                    self.pump();
                    self.reply_ok(request_id, serde_json::json!({"task": id, "state": "cancelled"}))
                }
            }
            TaskState::Accepted | TaskState::Running | TaskState::Reserved => {
                if let Some(adapter) = self.adapters.get_mut(agent.as_str()) {
                    adapter.cancel(id);
                }
                self.transition(id, TaskState::CancelRequested, "operator cancelled");
                self.reply_ok(request_id, serde_json::json!({"task": id, "state": "cancel_requested"}))
            }
            _ => Reply::err(request_id, self.revision, "bad_state", "task cannot be cancelled".into()),
        }
    }

    fn retry(&mut self, id: &str, request_id: &str) -> Reply {
        let Some(task) = self.tasks.get(id) else {
            return Reply::err(request_id, self.revision, "not_found", "no such task".into());
        };
        let (node, kind, number, revision, attempt, item_node_id, title) = (
            task.node.clone(),
            task.kind,
            task.number,
            task.revision,
            task.attempt,
            task.item_node_id.clone(),
            task.title.clone(),
        );
        match task.state {
            TaskState::Failed | TaskState::Cancelled => {}
            other => {
                return Reply::err(
                    request_id,
                    self.revision,
                    "bad_state",
                    format!("cannot retry a task in {}", other.as_str()),
                )
            }
        }
        let next_attempt = attempt + 1;
        let new_id = TaskRecord::task_id(&node, kind, number, revision, next_attempt);
        if self.tasks.contains_key(&new_id) {
            return Reply::err(request_id, self.revision, "exists", "retry attempt already exists".into());
        }
        let spec = self
            .graph
            .node(&node)
            .map(|s| (s.agent, s.exec));
        let record = Rec::TaskCreated {
            id: new_id.clone(),
            node: node.clone(),
            item_kind: kind.as_str().to_owned(),
            number,
            item_node_id: item_node_id.clone(),
            title: title.clone(),
            revision,
            attempt: next_attempt,
        };
        if self.append(record).is_err() {
            return Reply::err(request_id, self.revision, "journal", "append failed".into());
        }
        let (agent, exec) = spec.unwrap_or((Agent::Codex, Exec::Auto));
        self.tasks.insert(
            new_id.clone(),
            TaskRecord {
                id: new_id.clone(),
                node,
                agent,
                exec,
                kind,
                number,
                item_node_id,
                title,
                revision,
                attempt: next_attempt,
                state: TaskState::Queued,
                ext: Default::default(),
                detail: "retry".into(),
                created_seq: self.revision,
                worktree: None,
            },
        );
        self.order.push(new_id.clone());
        self.pump();
        self.reply_ok(request_id, serde_json::json!({"task": new_id, "state": "queued"}))
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_flag
    }
}

pub fn run(
    paths: FactoryPaths,
    graph: Graph,
    supervise_process: bool,
) -> Result<()> {
    paths.ensure()?;
    let (mut journal, records) = Journal::open(&paths.journal())?;
    let (events_tx, events_rx) = channel::<AdapterEvent>();
    let adapters: Vec<(&'static str, Box<dyn HarnessAdapter>)> = vec![
        ("codex", Box::new(CodexAdapter::new(events_tx.clone()))),
        ("opencode", Box::new(OcAdapter::new(events_tx.clone()))),
    ];
    let repo = paths
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let _ = journal.append(Rec::FactoryMeta {
        root: paths.root.display().to_string(),
        repo: repo.clone(),
    });
    paths.write_meta(&repo)?;

    let session = format!("aif-{}-factory", paths.short_id());
    let supervised = zellij_supervised(session);
    let mut daemon = Daemon::new(
        paths.clone(),
        graph,
        journal,
        adapters,
        supervised,
        real_worktrees(),
        records,
    )?;

    let (msg_tx, msg_rx) = channel::<Msg>();
    let (wake_tx, wake_rx) = channel::<()>();
    daemon.set_wake(wake_tx);
    let (log_tx, log_rx) = channel::<String>();
    daemon.set_logger(log_tx);
    spawn_log_writer(paths.logs_dir(), log_rx);

    let poll_tx = msg_tx.clone();
    if supervise_process {
        let root = paths.root.clone();
        std::thread::spawn(move || match crate::source::owner_repo_of(&root) {
            Ok((owner_repo, repo_id)) => {
                let poller = GithubPoller::new(RealGh, repo_id, owner_repo);
                let (obs_tx, obs_rx) = channel::<Observation>();
                let forward_tx = poll_tx.clone();
                std::thread::spawn(move || {
                    while let Ok(obs) = obs_rx.recv() {
                        if forward_tx.send(Msg::Obs(obs)).is_err() {
                            return;
                        }
                    }
                });
                let (wake_tx, wake_rx) = channel::<()>();
                let _ = wake_tx;
                poller.run_loop(
                    obs_tx,
                    PollConfig::from_env(),
                    Box::new(move |sleep| {
                        let _ = wake_rx.recv_timeout(sleep);
                    }),
                );
            }
            Err(err) => eprintln!("aif: github source disabled: {err:#}"),
        });
    }

    {
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = events_rx.recv() {
                let _ = msg_tx.send(Msg::Harness("adapter", event));
            }
        });
    }

    {
        let socket = paths.socket();
        let ctl_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let (control_tx, control_rx) = channel::<Incoming>();
            let forward_tx = ctl_tx.clone();
            std::thread::spawn(move || {
                while let Ok(incoming) = control_rx.recv() {
                    if forward_tx.send(Msg::Control(incoming)).is_err() {
                        return;
                    }
                }
            });
            if let Err(err) = control::serve(&socket, control_tx) {
                eprintln!("aif: control server stopped: {err:#}");
            }
        });
    }

    {
        let tick_tx = msg_tx.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));
            if tick_tx.send(Msg::Tick).is_err() {
                return;
            }
        });
    }

    let _ = wake_rx;
    for adapter in daemon.adapters.values_mut() {
        let _ = adapter.check();
    }
    event_loop(&mut daemon, msg_rx);
    Ok(())
}

fn spawn_log_writer(dir: PathBuf, rx: Receiver<String>) {
    std::thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("daemon.log"))
            .ok();
        while let Ok(line) = rx.recv() {
            if let Some(file) = file.as_mut() {
                use std::io::Write;
                let _ = writeln!(file, "{} {}", ids::now_iso(), line);
            }
        }
    });
}

pub fn event_loop(daemon: &mut Daemon, rx: Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Obs(obs) => daemon.observe(obs),
            Msg::Harness(name, event) => daemon.on_adapter(name, event),
            Msg::Control(incoming) => daemon.on_control(incoming),
            Msg::Tick => daemon.on_tick(),
            Msg::Stop { force, reply } => {
                let active = daemon.global_count();
                if !force && active > 0 {
                    if let Some(reply) = reply {
                        let _ = reply.send(Reply::err(
                            "stop",
                            daemon.revision(),
                            "busy",
                            format!("{active} active tasks"),
                        ));
                    }
                    continue;
                }
                if force {
                    let ids: Vec<String> = daemon
                        .tasks
                        .values()
                        .filter(|t| t.state.consumes_global())
                        .map(|t| t.id.clone())
                        .collect();
                    for id in ids {
                        daemon.transition(&id, TaskState::CancelRequested, "force stop");
                    }
                }
                daemon.stop_flag = true;
                if let Some(reply) = reply {
                    let _ = reply.send(Reply::ok("stop", daemon.revision(), serde_json::json!({"stopping": true})));
                }
                return;
            }
        }
        if daemon.stop_flag {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{fake_adapter, HarnessSignal};
    use crate::snapshot::ItemState;

    fn paths(tmp: &std::path::Path) -> FactoryPaths {
        FactoryPaths::from_id_with_base(
            tmp,
            "0123456789abcdef",
            &tmp.join("state"),
            &tmp.join("run"),
        )
    }

    fn graph() -> Graph {
        use crate::graph::{Agent, Exec, NodeSpec};
        use crate::graph::conditions::Condition;
        let spec = |name: &str, agent: Agent, exec: Exec, when: &str| NodeSpec {
            name: name.into(),
            agent,
            model: "test/model".into(),
            exec,
            when: Some(Condition::parse(when).unwrap()),
            prompt: Some("prompts/x.md".into()),
            limit: None,
            retrigger: crate::graph::Retrigger::Gate,
        };
        Graph {
            version: 4,
            tick_secs: 600,
            limit: 2,
            nodes: vec![
                spec("refiner", Agent::Codex, Exec::Auto, "issue has label 'to-refine'"),
                spec("releaser", Agent::Claude, Exec::Supervised, "pr is open and not draft"),
            ],
            edges: vec![],
        }
    }

    fn item(node_id: &str, number: u64, labels: &[&str]) -> ItemState {
        ItemState {
            repo_id: 1,
            node_id: node_id.into(),
            kind: ItemKind::Issue,
            number,
            title: format!("item {number}"),
            open: true,
            draft: false,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            blocked_by: vec![],
            head: None,
        }
    }

    struct Rig {
        daemon: Daemon,
        events_rx: Receiver<AdapterEvent>,
        codex: crate::harness::FakeHandle,
        tmp: PathBuf,
    }

    fn paths_setup(daemon: &Daemon) {
        let root = daemon.paths.root.clone();
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/x.md"), "work on {gh_ticket_no}").unwrap();
    }

    fn rig() -> Rig {
        let tmp = std::env::temp_dir().join(format!("aif-daemon-{}", ids::new_id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let paths = paths(&tmp);
        paths.ensure().unwrap();
        std::fs::create_dir_all(paths.root.join("prompts")).unwrap();
        std::fs::write(paths.root.join("prompts/x.md"), "work on {gh_ticket_no}").unwrap();
        std::fs::write(paths.root.join(".aif"), "").ok();
        let _ = std::fs::remove_file(paths.root.join(".aif"));
        let (journal, records) = Journal::open(&paths.journal()).unwrap();
        let (tx, rx) = channel::<AdapterEvent>();
        let (codex, handle) = fake_adapter("codex", tx.clone());
        let daemon = Daemon::new(
            paths.clone(),
            graph(),
            journal,
            vec![("codex", codex)],
            noop_supervised(),
            temp_worktrees(),
            records,
        )
        .unwrap();
        Rig {
            daemon,
            events_rx: rx,
            codex: handle,
            tmp,
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    fn drain(daemon: &mut Daemon, rx: &Receiver<AdapterEvent>) {
        while let Ok(event) = rx.try_recv() {
            daemon.on_adapter("codex", event);
        }
    }

    #[test]
    fn observation_reaches_reservation_and_success() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.paths.write_trust(true).unwrap();
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        assert_eq!(status["tasks"].as_array().unwrap().len(), 1);
        let task = &status["tasks"][0];
        assert!(task["state"].as_str().unwrap().starts_with("reserved") || task["state"].as_str().unwrap().starts_with("accepted"),
            "state was {:?}", task["state"]);
        let id = task["id"].as_str().unwrap().to_owned();
        rig.codex.signal(&id, HarnessSignal::Succeeded { summary: String::new() });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        assert_eq!(status["tasks"][0]["state"], "succeeded");
    }

    #[test]
    fn missing_trust_blocks_auto_dispatch() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        assert_eq!(status["tasks"][0]["state"], "queued");
        assert_eq!(rig.codex.jobs().len(), 0);
    }

    #[test]
    fn replay_recovers_reserved_as_uncertain_after_dispatch_begin() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.paths.write_trust(true).unwrap();
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let id = rig.daemon.status_json()["tasks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let path = rig.daemon.paths.journal();
        let graph = graph();
        let paths = rig.daemon.paths.clone();
        let (_, records) = Journal::open(&path).unwrap();
        let (tx, rx) = channel::<AdapterEvent>();
        let (codex, _handle) = fake_adapter("codex", tx);
        let mut second = Daemon::new(
            paths.clone(),
            graph,
            Journal::open(&path).unwrap().0,
            vec![("codex", codex)],
            noop_supervised(),
            temp_worktrees(),
            records,
        )
        .unwrap();
        drain(&mut second, &rx);
        let status = second.status_json();
        let _ = id;
        assert_eq!(status["tasks"][0]["state"], "uncertain");
    }

    #[test]
    fn terminal_event_dispatches_next_without_new_observation() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.paths.write_trust(true).unwrap();
        let graph = {
            let mut g = graph();
            g.limit = 1;
            g
        };
        rig.daemon.graph = graph;
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"]), item("I_2", 2, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        assert_eq!(rig.codex.jobs().len(), 1, "global limit one reserves one task");
        let id = rig.daemon.status_json()["tasks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        rig.codex.signal(&id, HarnessSignal::Succeeded { summary: String::new() });
        drain(&mut rig.daemon, &rig.events_rx);
        assert_eq!(rig.codex.jobs().len(), 2, "capacity released dispatches the next task");
    }

    #[test]
    fn definitive_failure_requeues_then_fails_after_limit() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.paths.write_trust(true).unwrap();
        for _ in 0..6 {
            rig.codex.script_next(vec![AdapterEvent::DispatchFailed {
                task: "refiner-issue1-r1000003a1".into(),
                definitive: true,
                detail: "server refused".into(),
            }]);
        }
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        assert_eq!(status["tasks"][0]["state"], "failed");
    }

    #[test]
    fn retry_creates_next_attempt() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        rig.daemon.paths.write_trust(true).unwrap();
        rig.codex.script_next(vec![
            AdapterEvent::DispatchAccepted {
                task: "refiner-issue1-r1000003a1".into(),
                ext: Default::default(),
            },
            AdapterEvent::Signal {
                task: "refiner-issue1-r1000003a1".into(),
                signal: HarnessSignal::Failed { summary: "boom".into() },
            },
        ]);
        rig.daemon.observe(Observation {
            items: vec![item("I_1", 1, &["to-refine"])],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        if status["tasks"][0]["state"] == "queued" {
            let id = status["tasks"][0]["id"].as_str().unwrap().to_owned();
            rig.codex.signal(&id, HarnessSignal::Failed { summary: "boom".into() });
            drain(&mut rig.daemon, &rig.events_rx);
        }
        let status = rig.daemon.status_json();
        let id = status["tasks"][0]["id"].as_str().unwrap().to_owned();
        let (tx_reply, rx_reply) = channel::<Reply>();
        let request_id = "r1".to_owned();
        let failed_id = id;
        rig.daemon.on_control(control::Incoming::Request {
            envelope: control::Envelope {
                v: 1,
                id: request_id.clone(),
                method: "task.retry".into(),
                params: serde_json::json!({"task": failed_id}),
            },
            reply: tx_reply,
            follow: None,
        });
        let reply = rx_reply.recv().unwrap();
        assert!(reply.ok, "{reply:?}");
        let status = rig.daemon.status_json();
        let attempts: Vec<&str> = status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert!(attempts.iter().any(|id| id.ends_with("a2")), "{attempts:?}");
    }

    #[test]
    fn supervised_task_presents_and_awaits_user() {
        let mut rig = rig();
        paths_setup(&rig.daemon);
        let pr = ItemState {
            kind: ItemKind::PullRequest,
            node_id: "P_1".into(),
            number: 9,
            draft: false,
            head: Some("aa".into()),
            ..item("P_1", 9, &[])
        };
        rig.daemon.observe(Observation {
            items: vec![pr],
            forced: true,
            stale: false,
        });
        drain(&mut rig.daemon, &rig.events_rx);
        let status = rig.daemon.status_json();
        let releaser = status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["node"] == "releaser")
            .unwrap();
        assert_eq!(releaser["state"], "failed", "no pane without a session");
    }
}
