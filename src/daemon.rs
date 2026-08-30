//! The daemon event loop: one thread that owns all mutable state.
//!
//! The loop blocks on one inbound channel until the next real deadline, so a
//! quiet factory costs nothing. Every message runs [`Daemon::drive`], which
//! admits gated work, fires due trains, refreshes release gates, reaps idle
//! sessions, and dispatches queued tasks. [`Daemon::drive`] is idempotent and
//! never recurses, so it is safe to run after every message.
//!
//! The loop never polls a clock. It sleeps until the earliest of the trains'
//! interval deadlines and the parked sessions' reaper expiries, or until a
//! message arrives.
//!
//! State falls into two kinds. GitHub holds the work state: issues, pull
//! requests, labels, and the stacked set. After a restart the first poll
//! rebuilds the gates, the trains, and the human decisions from GitHub, and
//! the gates re-open the tasks whose worktrees remain on disk, so work
//! resumes in place. `state.json` holds only what GitHub cannot: the
//! operator's limit, lane, and policy overrides, and each train's
//! `last_fire_ms`. The daemon restores `last_fire_ms` before the first
//! drive, so an interval policy never releases again just because the
//! process restarted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};

use crate::config::{self, Config, ReleasePolicy, RepoConfig};
use crate::decisions::{self, Decision, DecisionKind, Decisions, Response};
use crate::exec::{Exec, RealExec};
use crate::gates::{implement_ready, review_ready, GateTracker, ReadyWork};
use crate::gh::GhClient;
use crate::model::{ItemKind, RepoSnapshot, Snapshot, Stage};
use crate::poll::DaemonMsg;
use crate::runner::claude::ClaudeRunner;
use crate::runner::opencode::OpenCodeRunner;
use crate::runner::{Answer, Job, RunEvent, Runner, Session};
use crate::sched::{self, Limits, Paused, Verdict};
use crate::sock::{Action, PauseScope, StateInput};
use crate::state::DaemonState;
use crate::tasks::{self, Task, TaskState, TaskTable};
use crate::trains::{Train, STACKED_LABEL};
use crate::worktree::WorktreeManager;

/// The label that asks a human to decide something on GitHub.
pub const NEEDS_HUMAN_LABEL: &str = "needs-human";

/// How long a parked session stays alive without activity before the reaper
/// stops its process.
///
/// The task of a reaped session stays `AwaitingUser` and a chat message
/// resumes it later with a fresh process.
pub const DEFAULT_IDLE_REAP_MS: u64 = 30 * 60_000;

/// The item number of a ticket-creation task. A real issue never carries
/// number 0, so the daemon uses it as the marker of a ticket session.
pub const TICKET_NUMBER: u64 = 0;

/// The release policy that never fires on its own.
static MANUAL_POLICY: ReleasePolicy = ReleasePolicy::Manual;

/// One message of the event loop.
///
/// Forwarder threads fold the three inbound sources of the daemon, the
/// pollers, the runners, and the control socket, into one channel, because
/// one thread can block on only one receiver.
#[derive(Debug)]
pub enum Inbound {
    /// One message from the pollers.
    Poll(DaemonMsg),
    /// One event from a runner.
    Run(RunEvent),
    /// One operator action from the control socket.
    Act(Action),
}

/// The daemon: every module assembled into one event loop.
pub struct Daemon {
    /// The parsed factory configuration.
    config: Config,
    /// The command runner; tests replace it with a scripted double.
    exec: Arc<dyn Exec>,
    /// One runner per stage, in stage order.
    runners: BTreeMap<Stage, Box<dyn Runner>>,
    /// The worktree manager; it owns the naming rules for worktrees.
    worktrees: WorktreeManager,
    /// The path of `state.json`.
    state_path: PathBuf,
    /// The directory of operator-provided prompt templates.
    prompts_dir: PathBuf,
    /// The state directory; the parent of the worktrees and the logs.
    state_dir: PathBuf,

    /// The stage limits and lane reservations, as the operator edited them.
    limits: Limits,
    /// What the operator paused.
    paused: Paused,
    /// The release policy overrides, by repository alias.
    policies: BTreeMap<String, ReleasePolicy>,

    /// The last snapshot of every polled repository.
    snapshot: Snapshot,
    /// The edge-triggered readiness gates.
    gates: GateTracker,
    /// Ready work the gates reported, until the next drive admits it.
    pending_ready: Vec<ReadyWork>,
    /// All tasks, in insertion order.
    table: TaskTable,
    /// The decisions that wait for a human.
    decisions: Decisions,
    /// One release train per repository alias.
    trains: BTreeMap<String, Train>,
    /// The pull request set of each release task, for prompt rendering.
    release_batches: BTreeMap<String, Vec<u64>>,

    /// One live session per running or parked task.
    sessions: BTreeMap<String, Box<dyn Session>>,
    /// The time of the last event of each task, for the idle reaper.
    last_event_ms: BTreeMap<String, u64>,

    /// The current time, in milliseconds. The loop refreshes it; tests pin
    /// the clock so a test can move time.
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The cached current time, in milliseconds.
    now_ms: u64,
    /// How long a parked session may stay idle before the reaper stops it.
    idle_reap_ms: u64,
    /// True when anything changed since the last drive.
    changed: bool,
    /// True when the last drive changed something and the push did not run.
    pub dirty: bool,
    /// True when the operator asked the daemon to stop.
    shutdown: bool,
    /// The serialized state of the last write, so an unchanged drive writes
    /// nothing.
    saved: Option<String>,
    /// Reserved for the socket module: it attaches its push subscribers here.
    /// The list stays empty until that chunk lands.
    pub subscribers: Vec<()>,

    /// The inbound end of the poller channel.
    poll_rx: Receiver<DaemonMsg>,
    /// The outbound end of the runner channel; runners get clones.
    run_tx: Sender<RunEvent>,
    /// The inbound end of the runner channel.
    run_rx: Receiver<RunEvent>,
    /// The inbound end of the control socket channel.
    action_rx: Option<Receiver<Action>>,
    /// The wake sender of each repository's poller. Dropping the map stops
    /// every poller, which is the shutdown path.
    wake: BTreeMap<String, Sender<()>>,
}

impl Daemon {
    /// Build a daemon with the real runners and the real command runner.
    ///
    /// The runner of each stage comes from the stage's config: `opencode`
    /// selects the one-shot opencode runner, and every other value selects
    /// the interactive claude runner.
    pub fn new(
        config: Config,
        poll_rx: Receiver<DaemonMsg>,
        wake: BTreeMap<String, Sender<()>>,
        action_rx: Receiver<Action>,
    ) -> Self {
        let mut runners: BTreeMap<Stage, Box<dyn Runner>> = BTreeMap::new();
        for stage in Stage::ALL {
            let runner: Box<dyn Runner> = if config.stage(stage).runner == "opencode" {
                Box::new(OpenCodeRunner::new())
            } else {
                let sink: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|_session_id: &str| {});
                Box::new(ClaudeRunner::new(sink))
            };
            runners.insert(stage, runner);
        }
        let state_dir = config::state_dir();
        let prompts_dir = config::config_dir().join("prompts");
        Self::with_runners(
            config,
            Arc::new(RealExec),
            state_dir,
            prompts_dir,
            poll_rx,
            wake,
            action_rx,
            runners,
        )
    }

    /// Build a daemon over injected runners and a scripted command runner.
    ///
    /// The constructor restores `last_fire_ms` from `state.json` and applies
    /// the stored overrides before the first drive, so an interval policy
    /// never releases again just because the daemon restarted.
    // Each argument is one daemon-owned dependency. A bundle would hide the
    // ownership boundary without reducing it.
    #[allow(clippy::too_many_arguments)]
    pub fn with_runners(
        config: Config,
        exec: Arc<dyn Exec>,
        state_dir: PathBuf,
        prompts_dir: PathBuf,
        poll_rx: Receiver<DaemonMsg>,
        wake: BTreeMap<String, Sender<()>>,
        action_rx: Receiver<Action>,
        runners: BTreeMap<Stage, Box<dyn Runner>>,
    ) -> Self {
        let state_path = state_dir.join("state.json");
        let stored = DaemonState::load(&state_path);

        let mut limits = Limits::from_config(&config);
        for (stage, limit) in &stored.stage_limits {
            limits.stage.insert(*stage, *limit);
        }
        for (stage, repo, slots) in &stored.lanes {
            let config_slots = config
                .repos
                .get(repo)
                .and_then(|repo_cfg| repo_cfg.lanes.get(stage))
                .copied()
                .unwrap_or(0);
            if *slots != 0 || config_slots != 0 {
                limits.lanes.insert((*stage, repo.clone()), *slots);
            }
        }
        let policies = stored.policies;

        let mut trains = BTreeMap::new();
        for alias in config.repos.keys() {
            trains.insert(alias.clone(), Train::new(alias));
        }
        // The train contract: restore last_fire_ms BEFORE the first drive, or
        // an interval policy sees no previous fire and releases at once.
        for (repo, stamp) in &stored.last_fire_ms {
            if let Some(train) = trains.get_mut(repo) {
                train.last_fire_ms = Some(*stamp);
            }
        }

        let (run_tx, run_rx) = mpsc::channel();
        let mut daemon = Daemon {
            config,
            exec,
            runners,
            worktrees: WorktreeManager::new(state_dir.clone()),
            state_path: state_path.clone(),
            prompts_dir,
            state_dir,
            limits,
            paused: Paused::default(),
            policies,
            snapshot: Snapshot::default(),
            gates: GateTracker::new(),
            pending_ready: Vec::new(),
            table: TaskTable::new(),
            decisions: Decisions::new(),
            trains,
            release_batches: BTreeMap::new(),
            sessions: BTreeMap::new(),
            last_event_ms: BTreeMap::new(),
            clock: Arc::new(now_ms),
            now_ms: 0,
            idle_reap_ms: DEFAULT_IDLE_REAP_MS,
            changed: false,
            dirty: false,
            shutdown: false,
            saved: None,
            subscribers: Vec::new(),
            poll_rx,
            run_tx,
            run_rx,
            action_rx: Some(action_rx),
            wake,
        };
        // Seed the write cache with the current state, so a daemon that
        // changes nothing writes nothing.
        match daemon.collect_state().to_json() {
            Ok(text) => daemon.saved = Some(text),
            Err(error) => eprintln!("cannot serialize the initial daemon state: {error:#}"),
        }
        daemon
    }

    /// Run the event loop until the operator stops the daemon.
    ///
    /// Three forwarder threads fold the poller, runner, and socket channels
    /// into one inbound channel. The loop blocks on that channel until the
    /// next deadline, so the daemon never wakes without a reason. Every
    /// message runs [`Daemon::drive`], and so does a deadline that arrives
    /// with no message.
    pub fn run(mut self) -> Result<()> {
        let (in_tx, in_rx) = mpsc::channel::<Inbound>();
        let dummy_rx: Receiver<DaemonMsg> = mpsc::channel().1;
        let poll_rx = std::mem::replace(&mut self.poll_rx, dummy_rx);
        let dummy_rx: Receiver<RunEvent> = mpsc::channel().1;
        let run_rx = std::mem::replace(&mut self.run_rx, dummy_rx);
        let action_rx = self.action_rx.take().unwrap_or_else(|| mpsc::channel().1);
        let _forwarders = [
            forwarder("aif-poll", poll_rx, in_tx.clone(), Inbound::Poll)?,
            forwarder("aif-run", run_rx, in_tx.clone(), Inbound::Run)?,
            forwarder("aif-act", action_rx, in_tx, Inbound::Act)?,
        ];

        loop {
            if self.shutdown {
                break;
            }
            self.now_ms = (self.clock)();
            let message = match self.next_deadline() {
                Some(timeout) => match in_rx.recv_timeout(timeout) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => {
                        self.now_ms = (self.clock)();
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match in_rx.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                },
            };
            match message {
                Some(message) => self.handle(message),
                None => self.drive(),
            }
        }
        Ok(())
    }

    /// Process one inbound message, then drive the factory.
    ///
    /// Tests call this directly and drive the daemon without threads.
    pub fn handle(&mut self, message: Inbound) {
        self.now_ms = (self.clock)();
        match message {
            Inbound::Poll(message) => self.on_poll(message),
            Inbound::Run(event) => self.on_run_event(event),
            Inbound::Act(action) => self.on_action(action),
        }
        self.drive();
    }

    /// One pass of the factory: admit, fire, gate, reap, dispatch, persist.
    ///
    /// The pass is idempotent: a second pass with no new message dispatches
    /// nothing, fires nothing, and writes nothing. The pass never recurses.
    pub fn drive(&mut self) {
        if self.shutdown {
            return;
        }
        self.admit_ready();
        self.rebuild_stacked();
        self.fire_due_trains();
        self.refresh_release_gates();
        self.reconcile_trains();
        self.reap_idle_sessions();
        self.dispatch_queued();
        self.save_state();
    }

    /// The moment the loop must wake next, as a duration from now.
    ///
    /// The answer is the earliest of each interval train's fire moment and
    /// each parked session's reaper expiry. `None` means the loop may block:
    /// nothing can become due without a message.
    pub fn next_deadline(&self) -> Option<Duration> {
        let mut earliest: Option<u64> = None;
        for (repo, train) in &self.trains {
            let policy = self.active_policy(repo);
            if let Some(at) = train.next_deadline_ms(policy, self.now_ms) {
                earliest = Some(match earliest {
                    Some(so_far) => so_far.min(at),
                    None => at,
                });
            }
        }
        for task in self.table.active() {
            if task.state != TaskState::AwaitingUser {
                continue;
            }
            if !self.sessions.contains_key(&task.id) {
                continue;
            }
            let last = self
                .last_event_ms
                .get(&task.id)
                .copied()
                .unwrap_or(self.now_ms);
            let at = last.saturating_add(self.idle_reap_ms);
            earliest = Some(match earliest {
                Some(so_far) => so_far.min(at),
                None => at,
            });
        }
        earliest.map(|at| Duration::from_millis(at.saturating_sub(self.now_ms)))
    }

    /// True when the operator asked the daemon to stop.
    pub fn stopping(&self) -> bool {
        self.shutdown
    }

    /// Read and clear the dirty flag. The socket pusher calls this after it
    /// published a state view.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Assemble the input the socket module builds its state view from.
    pub fn state_input(&self) -> StateInput<'_> {
        StateInput {
            config: &self.config,
            limits: &self.limits,
            paused: &self.paused,
            table: &self.table,
            decisions: &self.decisions,
            trains: &self.trains,
            policies: &self.policies,
            now_ms: self.now_ms,
        }
    }

    // ------------------------------------------------------------------
    // Poll handling
    // ------------------------------------------------------------------

    /// Apply one poller message.
    fn on_poll(&mut self, message: DaemonMsg) {
        match message {
            DaemonMsg::Shutdown => self.shutdown = true,
            DaemonMsg::PollFailed { repo, error } => {
                eprintln!("the poll of {repo} failed: {error}");
            }
            DaemonMsg::Polled { repo, snapshot } => self.apply_poll(&repo, snapshot),
        }
    }

    /// Store a fresh snapshot and derive everything GitHub drives.
    ///
    /// A poll that repeats the previous snapshot marks nothing changed, so
    /// it writes no state and raises no dirty flag.
    fn apply_poll(&mut self, repo: &str, fresh: RepoSnapshot) {
        let old = self.snapshot.repos.get(repo).cloned();
        let unchanged = old.as_ref() == Some(&fresh);
        self.snapshot.apply(repo, fresh.clone());
        if let Some(old) = old.filter(|_| !unchanged) {
            self.reconcile_removed(repo, &old, &fresh);
        }
        if !unchanged {
            self.reconcile_unready(repo, &fresh);
        }
        let ready = self.gates.observe(repo, &fresh);
        let mut changed = !ready.is_empty();
        self.pending_ready.extend(ready);
        changed |= self.derive_needs_human(repo, &fresh);
        if !unchanged {
            changed = true;
        }
        if changed {
            self.changed = true;
        }
    }

    /// Retire work whose item closed, went back to draft, or vanished.
    ///
    /// GitHub is the source of truth: a gone or draft pull request leaves the
    /// train, and a gone or closed item cancels its active tasks.
    fn reconcile_removed(&mut self, repo: &str, old: &RepoSnapshot, fresh: &RepoSnapshot) {
        for number in old.issues.keys() {
            let gone = fresh.issues.get(number).is_none_or(|issue| !issue.open);
            if gone {
                self.gates.forget(repo, ItemKind::Issue, *number);
                self.cancel_item_tasks(repo, ItemKind::Issue, *number);
            }
        }
        for number in old.prs.keys() {
            let current = fresh.prs.get(number);
            let gone = current.is_none_or(|pr| !pr.open);
            if gone || current.is_some_and(|pr| pr.draft) {
                if let Some(train) = self.trains.get_mut(repo) {
                    train.dequeue(*number);
                }
            }
            if gone {
                self.gates.forget(repo, ItemKind::Pr, *number);
                self.cancel_item_tasks(repo, ItemKind::Pr, *number);
            }
        }
    }

    /// Cancel every active task of one item and stop its live session.
    fn cancel_item_tasks(&mut self, repo: &str, kind: ItemKind, number: u64) {
        let ids: Vec<String> = self
            .table
            .active()
            .iter()
            .filter(|task| task.repo == repo && task.kind == kind && task.number == number)
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.cancel_task(&id);
        }
    }

    /// Cancel active implement and review tasks whose gates closed.
    fn reconcile_unready(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let ids: Vec<String> = self
            .table
            .active()
            .into_iter()
            .filter(|task| task.repo == repo)
            .filter(|task| match task.stage {
                Stage::Implement => fresh
                    .issues
                    .get(&task.number)
                    .is_none_or(|issue| !implement_ready(fresh, issue)),
                Stage::Review => fresh
                    .prs
                    .get(&task.number)
                    .is_none_or(|pr| !review_ready(pr)),
                Stage::Refine | Stage::Release => false,
            })
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.cancel_task(&id);
        }
    }

    /// Re-derive the `NeedsHuman` decisions from the labels of one poll.
    ///
    /// A labeled item opens or refreshes its row; an item that lost the label
    /// loses its row. Nothing is lost between polls: the labels are the
    /// truth. Returns true when any row opened, refreshed, or closed.
    fn derive_needs_human(&mut self, repo: &str, fresh: &RepoSnapshot) -> bool {
        let mut changed = false;
        let mut live: BTreeSet<String> = BTreeSet::new();
        for (number, issue) in &fresh.issues {
            if !issue.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL) {
                continue;
            }
            let row =
                Decision::needs_human(repo, ItemKind::Issue, *number, &issue.title, self.now_ms);
            live.insert(row.id.clone());
            changed |= self.decisions.push(row).is_some();
        }
        for (number, pr) in &fresh.prs {
            if !pr.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL) {
                continue;
            }
            let row = Decision::needs_human(repo, ItemKind::Pr, *number, &pr.title, self.now_ms);
            live.insert(row.id.clone());
            changed |= self.decisions.push(row).is_some();
        }
        let stale: Vec<String> = self
            .decisions
            .open()
            .iter()
            .filter(|row| matches!(row.kind, DecisionKind::NeedsHuman { .. }))
            .filter(|row| row.repo == repo && !live.contains(&row.id))
            .map(|row| row.id.clone())
            .collect();
        for id in stale {
            self.decisions.take(&id);
            changed = true;
        }
        changed
    }

    // ------------------------------------------------------------------
    // Drive steps
    // ------------------------------------------------------------------

    /// Move gated ready work into the task table and the train queues.
    fn admit_ready(&mut self) {
        let ready = std::mem::take(&mut self.pending_ready);
        for work in ready {
            if work.stage == Stage::Release {
                if let Some(train) = self.trains.get_mut(&work.repo) {
                    let before = train.queue.len();
                    train.enqueue(work.number);
                    if train.queue.len() != before {
                        self.changed = true;
                    }
                }
                continue;
            }
            if work.stage == Stage::Review {
                let superseded = self
                    .table
                    .active()
                    .into_iter()
                    .find(|task| {
                        task.repo == work.repo
                            && task.stage == Stage::Review
                            && task.kind == work.kind
                            && task.number == work.number
                            && task.head_sha != work.head_sha
                    })
                    .map(|task| task.id.clone());
                if let Some(id) = superseded {
                    self.cancel_task(&id);
                }
            }
            let log = self.log_path(&work.repo, work.stage, work.kind, work.number);
            match self.table.upsert_queued(
                &work.repo,
                work.stage,
                work.kind,
                work.number,
                log,
                self.now_ms,
            ) {
                Ok(task) => {
                    if work.stage == Stage::Review {
                        task.head_sha = work.head_sha.clone();
                    }
                    self.changed = true;
                }
                Err(e) => {
                    eprintln!(
                        "the gate reported {} {}/{}, but the task table refuses it: {e:#}",
                        work.repo, work.stage, work.number
                    );
                }
            }
        }
    }

    /// Rebuild each train's stacked cache from the `release-stacked` labels
    /// of the last poll.
    fn rebuild_stacked(&mut self) {
        let aliases: Vec<String> = self.trains.keys().cloned().collect();
        for alias in aliases {
            let Some(snapshot) = self.snapshot.repos.get(&alias) else {
                continue;
            };
            let labeled: Vec<u64> = snapshot
                .prs
                .values()
                .filter(|pr| pr.labels.iter().any(|label| label == STACKED_LABEL))
                .map(|pr| pr.number)
                .collect();
            let Some(train) = self.trains.get_mut(&alias) else {
                continue;
            };
            let before = (train.queue.clone(), train.stacked.clone());
            train.rebuild_stacked(&labeled);
            if (train.queue.clone(), train.stacked.clone()) != before {
                self.changed = true;
            }
        }
    }

    /// Fire every train whose policy says it is due.
    fn fire_due_trains(&mut self) {
        let aliases: Vec<String> = self.trains.keys().cloned().collect();
        for alias in aliases {
            let policy = self.active_policy(&alias).clone();
            let due = self.trains[&alias].should_fire(&policy, self.now_ms);
            if let Some(prs) = due {
                self.fire_train(&alias, &prs);
            }
        }
    }

    /// Open or close one release-gate row per repository.
    ///
    /// A manual policy with a stacked or retrying set waits for a human.
    /// A changed set replaces the old row with a new snapshot.
    fn refresh_release_gates(&mut self) {
        let aliases: Vec<String> = self.trains.keys().cloned().collect();
        for alias in aliases {
            let release_is_stuck = self.decisions.open().iter().any(|row| {
                row.repo == alias
                    && row.stage == Some(Stage::Release)
                    && matches!(row.kind, DecisionKind::Stuck { .. })
            });
            let waiting = {
                let train = &self.trains[&alias];
                let policy = self.active_policy(&alias);
                train.in_flight.is_none()
                    && matches!(policy, ReleasePolicy::Manual)
                    && !train.fired_set().is_empty()
                    && !release_is_stuck
            };
            let id = format!("gate:{alias}");
            if waiting {
                let prs = self.trains[&alias].fired_set();
                let same = self.decisions.open().iter().any(|row| {
                    row.id == id
                        && matches!(&row.kind, DecisionKind::ReleaseGate { prs: open } if open == &prs)
                });
                if !same {
                    self.decisions.take(&id);
                    self.decisions
                        .push(Decision::release_gate(&alias, prs, self.now_ms));
                    self.changed = true;
                }
            } else if self.decisions.take(&id).is_some() {
                self.changed = true;
            }
        }
    }

    /// Retry the label cleanup of trains whose release task already ended.
    ///
    /// A label error keeps a train in flight; this step retries `finish`
    /// until the train is closed. A terminal release task whose train is
    /// still in flight gets its batch back, so no batch is ever lost.
    fn reconcile_trains(&mut self) {
        let aliases: Vec<String> = self
            .trains
            .iter()
            .filter(|(_, train)| train.in_flight.is_some())
            .map(|(alias, _)| alias.clone())
            .collect();
        for alias in aliases {
            let Some(train) = self.trains.get(&alias) else {
                continue;
            };
            let Some(task_id) = train.in_flight.clone() else {
                continue;
            };
            let Some(task) = self.table.by_id.get(&task_id) else {
                continue;
            };
            if task.state == TaskState::Done {
                self.finish_train(&alias, true);
            } else if task.state.is_terminal() {
                self.finish_train(&alias, false);
            }
        }
    }

    /// Stop the processes of parked sessions that passed the idle limit.
    ///
    /// The task stays `AwaitingUser` and a chat message resumes it later, but
    /// its live-process slot is free at once.
    fn reap_idle_sessions(&mut self) {
        let mut reaped: Vec<String> = Vec::new();
        for task in self.table.active() {
            if task.state != TaskState::AwaitingUser {
                continue;
            }
            if !self.sessions.contains_key(&task.id) {
                continue;
            }
            let last = self
                .last_event_ms
                .get(&task.id)
                .copied()
                .unwrap_or(self.now_ms);
            if self.now_ms >= last.saturating_add(self.idle_reap_ms) {
                reaped.push(task.id.clone());
            }
        }
        for id in reaped {
            if let Some(mut session) = self.sessions.remove(&id) {
                eprintln!("task {id}: the parked session passed the idle limit; stopping it");
                if let Err(error) = session.stop() {
                    eprintln!("task {id}: cannot stop the parked session: {error:#}");
                }
                self.changed = true;
            }
        }
    }

    /// Dispatch queued tasks while the scheduler yields one.
    fn dispatch_queued(&mut self) {
        let mut saturated: BTreeSet<Stage> = BTreeSet::new();
        loop {
            let Some(id) = self.next_eligible(&saturated) else {
                break;
            };
            match self.dispatch_one(&id) {
                Ok(true) => {}
                Ok(false) => {
                    if let Some(task) = self.table.by_id.get(&id) {
                        saturated.insert(task.stage);
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// The next queued task the scheduler admits, ignoring saturated stages.
    ///
    /// This mirrors [`sched::next_dispatch`] with one daemon-side exception:
    /// a stage whose live-process slots are full yields to the later tasks of
    /// other stages until the reaper or an exit frees a slot.
    fn next_eligible(&self, saturated: &BTreeSet<Stage>) -> Option<String> {
        for id in &self.table.order {
            let Some(task) = self.table.by_id.get(id) else {
                continue;
            };
            if task.state != TaskState::Queued
                || saturated.contains(&task.stage)
                || self.sessions.contains_key(&task.id)
            {
                continue;
            }
            if matches!(
                sched::can_start(
                    &self.limits,
                    &self.paused,
                    &self.table,
                    task.stage,
                    &task.repo
                ),
                Verdict::Yes
            ) {
                return Some(task.id.clone());
            }
        }
        None
    }

    /// Start one queued task: ensure the worktree, render the prompt, start
    /// the runner, move the task to `Running`.
    ///
    /// `Ok(true)` means the task started. `Ok(false)` means the stage's live
    /// processes are at their limit; the caller tries the next stage. An
    /// error means the dispatch failed, the task is handled, and the caller
    /// must stop this round.
    fn dispatch_one(&mut self, id: &str) -> Result<bool> {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return Ok(true);
        };
        // The second limit: live processes, not scheduler slots. A parked
        // chat holds a process between turns, and that process is the real
        // memory cost the stage limit exists to bound.
        if self.live_sessions(task.stage) >= self.limits.limit(task.stage) {
            return Ok(false);
        }
        let Some(repo_cfg) = self.config.repos.get(&task.repo).cloned() else {
            let reason = format!("repository {} left the config", task.repo);
            self.fail_run(&task, &reason);
            return Err(anyhow!(reason));
        };
        let cwd = match task.stage {
            Stage::Refine => Ok(repo_cfg.path.clone()),
            Stage::Implement | Stage::Review => {
                self.worktrees
                    .ensure_issue(&*self.exec, &repo_cfg, task.number)
            }
            Stage::Release => self.worktrees.ensure_train(&*self.exec, &repo_cfg),
        };
        let cwd = match cwd {
            Ok(cwd) => cwd,
            Err(e) => {
                let reason = format!("cannot prepare the worktree: {e:#}");
                self.fail_run(&task, &reason);
                return Err(e);
            }
        };
        let prompt = match self.render_prompt(&task, &repo_cfg, &cwd) {
            Ok(prompt) => prompt,
            Err(e) => {
                let reason = format!("cannot render the prompt: {e:#}");
                self.fail_run(&task, &reason);
                return Err(e);
            }
        };
        if let Err(e) = self.launch_task(&task, prompt, None) {
            let reason = format!("the runner could not start: {e:#}");
            self.fail_run(&task, &reason);
            return Err(e);
        }
        Ok(true)
    }

    /// Start a run for `task` and move the task to `Running`.
    ///
    /// Both fresh dispatches and chat resumes of parked tasks come through
    /// here; a resume carries the session id to continue.
    fn launch_task(&mut self, task: &Task, prompt: String, resume: Option<String>) -> Result<()> {
        let stage_cfg = self.config.stage(task.stage);
        let cwd = self
            .task_cwd(&task.id)
            .ok_or_else(|| anyhow!("repository {} left the config", task.repo))?;
        let job = Job {
            task: task.id.clone(),
            stage: task.stage,
            repo: task.repo.clone(),
            model: stage_cfg.model.clone(),
            variant: stage_cfg.variant.clone(),
            prompt,
            cwd,
            log: task.log_path.clone(),
            resume,
            yolo: stage_cfg.yolo,
        };
        let Some(runner) = self.runners.get_mut(&task.stage) else {
            bail!("no runner for the {} stage", task.stage);
        };
        let session = runner.start(&job, self.run_tx.clone())?;
        self.sessions.insert(task.id.clone(), session);
        self.last_event_ms.insert(task.id.clone(), self.now_ms);
        if let Err(e) = self
            .table
            .transition(&task.id, TaskState::Running, self.now_ms)
        {
            if let Some(mut session) = self.sessions.remove(&task.id) {
                if let Err(stop_error) = session.stop() {
                    eprintln!(
                        "task {}: cannot stop the rejected session: {stop_error:#}",
                        task.id
                    );
                }
            }
            return Err(e);
        }
        self.changed = true;
        Ok(())
    }

    /// Fail a task, requeue it while attempts remain, or open a stuck row.
    ///
    /// This is the one failure path for dispatch errors and run exits. A
    /// task that still has attempts left goes back to `Queued`; the last
    /// failure opens a `Stuck` decision for the human.
    fn fail_task(&mut self, id: &str, reason: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        let final_attempt = task.attempt >= tasks::MAX_ATTEMPTS;
        if let Err(e) =
            self.table
                .transition(id, TaskState::Failed(reason.to_string()), self.now_ms)
        {
            eprintln!("task {id}: {e:#}");
            return;
        }
        self.changed = true;
        self.decisions.drop_for_task(id);
        if final_attempt {
            let failed = self.table.by_id.get(id).cloned().unwrap_or(task);
            eprintln!("task {id} is stuck on attempt {}: {reason}", failed.attempt);
            let row = Decision::stuck(&failed, reason, self.now_ms);
            self.decisions.push(row);
        } else if let Err(e) = self.table.transition(id, TaskState::Queued, self.now_ms) {
            eprintln!("task {id}: {e:#}");
        }
    }

    /// Close the release train of one repository after its release task.
    ///
    /// Success removes the stacked labels. Failure puts the batch back in the
    /// queue, still labelled, so a retry ships the identical set.
    fn finish_train(&mut self, repo: &str, ok: bool) {
        let Some(owner_repo) = self.config.repos.get(repo).map(|r| r.owner_repo.clone()) else {
            return;
        };
        let Some(train) = self.trains.get_mut(repo) else {
            return;
        };
        let task_id = train.in_flight.clone();
        let gh = GhClient::new(&*self.exec);
        match train.finish(ok, &owner_repo, &gh) {
            Ok(batch) => {
                if let Some(task_id) = task_id {
                    self.release_batches.remove(&task_id);
                }
                if !batch.is_empty() {
                    self.changed = true;
                }
            }
            Err(e) => {
                eprintln!("the release train of {repo} could not finish: {e:#}");
            }
        }
    }

    // ------------------------------------------------------------------
    // Run events
    // ------------------------------------------------------------------

    /// Map one runner event onto the task table, the decisions, and disk.
    fn on_run_event(&mut self, event: RunEvent) {
        let task_id = event_task(&event).to_string();
        self.last_event_ms.insert(task_id.clone(), self.now_ms);
        match event {
            RunEvent::Started {
                session_id: Some(session_id),
                ..
            } => {
                let Some(task) = self.table.by_id.get_mut(&task_id) else {
                    eprintln!("task {task_id} started, but it is not in the table");
                    return;
                };
                task.session_id = Some(session_id.clone());
                let marker = self
                    .task_cwd(&task_id)
                    .ok_or_else(|| anyhow!("the task has no worktree"))
                    .and_then(|cwd| self.worktrees.write_session(&cwd, &session_id));
                if let Err(error) = marker {
                    let reason = format!("cannot write the session marker: {error:#}");
                    eprintln!("task {task_id}: {reason}");
                    if let Some(mut session) = self.sessions.remove(&task_id) {
                        if let Err(stop_error) = session.stop() {
                            eprintln!("task {task_id}: cannot stop the session: {stop_error:#}");
                        }
                    }
                    if let Some(task) = self.table.by_id.get(&task_id).cloned() {
                        self.fail_run(&task, &reason);
                    }
                    return;
                }
                self.changed = true;
            }
            RunEvent::Started {
                session_id: None, ..
            } => {}
            RunEvent::Ask {
                request_id,
                tool,
                input,
                needs_human,
                ..
            } => {
                let Some(task) = self.table.by_id.get(&task_id).cloned() else {
                    eprintln!("task {task_id} asked, but it is not in the table");
                    return;
                };
                let decision = if needs_human {
                    Decision::question(&task, &request_id, input, self.now_ms)
                } else {
                    Decision::permission(&task, &request_id, &tool, input, self.now_ms)
                };
                if self.decisions.push(decision).is_some() {
                    self.changed = true;
                }
            }
            RunEvent::Text { .. } | RunEvent::Tool { .. } => {
                // The runner tees this into the task log; the interfaces read
                // the log file, so the daemon stores nothing here.
            }
            RunEvent::TurnEnd { ok, summary, .. } => self.on_turn_end(&task_id, ok, &summary),
            RunEvent::Exit { ok, detail, .. } => self.on_exit_event(&task_id, ok, &detail),
        }
    }

    /// Apply one turn end.
    ///
    /// Completion differs per runner. A claude refine task waits for a user.
    /// Another claude task completes or fails from the turn result.
    /// An opencode turn is only a step boundary.
    fn on_turn_end(&mut self, id: &str, ok: bool, summary: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state != TaskState::Running
            || self.config.stage(task.stage).runner.as_str() != "claude"
        {
            return;
        }
        if task.stage == Stage::Refine {
            if let Err(error) = self
                .table
                .transition(id, TaskState::AwaitingUser, self.now_ms)
            {
                eprintln!("task {id}: {error:#}");
                return;
            }
            self.changed = true;
        } else if ok {
            self.complete_task(&task);
        } else {
            let reason = if summary.is_empty() {
                "the claude turn failed"
            } else {
                summary
            };
            self.fail_run(&task, reason);
        }
    }

    /// Apply one run exit.
    ///
    /// A terminal task ignores the exit. A parked task stays resumable.
    /// An opencode exit supplies the task result.
    /// A claude exit without a prior result fails the active task.
    fn on_exit_event(&mut self, id: &str, ok: bool, detail: &str) {
        self.sessions.remove(id);
        self.last_event_ms.remove(id);
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        if task.state == TaskState::AwaitingUser {
            return;
        }
        if self.config.stage(task.stage).runner.as_str() == "claude" {
            if task.state == TaskState::Queued {
                return;
            }
            let reason = if ok {
                format!("claude exited before it reported a turn result: {detail}")
            } else {
                detail.to_string()
            };
            self.fail_run(&task, &reason);
        } else if ok {
            self.complete_task(&task);
        } else {
            self.fail_run(&task, detail);
        }
    }

    /// Complete one task and apply its stage-specific success action.
    fn complete_task(&mut self, task: &Task) {
        if task.stage == Stage::Review {
            if let Err(error) = self.write_review_marker(task) {
                let reason = format!("cannot write the reviewed-sha marker: {error:#}");
                eprintln!("task {}: {reason}", task.id);
                self.fail_run(task, &reason);
                return;
            }
        }
        if let Err(error) = self
            .table
            .transition(&task.id, TaskState::Done, self.now_ms)
        {
            eprintln!("task {}: {error:#}", task.id);
            return;
        }
        self.changed = true;
        self.decisions.drop_for_task(&task.id);
        match task.stage {
            Stage::Release => self.finish_train(&task.repo, true),
            Stage::Refine | Stage::Implement | Stage::Review => {}
        }
    }

    /// Fail one run and return a final release batch to its train.
    fn fail_run(&mut self, task: &Task, reason: &str) {
        if task.stage == Stage::Release && task.attempt >= tasks::MAX_ATTEMPTS {
            self.finish_train(&task.repo, false);
        }
        self.fail_task(&task.id, reason);
    }

    /// Write the `.aif/reviewed-sha` marker of a finished review.
    ///
    /// The marker records the head sha the gate admitted, and it is written
    /// only here, after the review task reported success.
    fn write_review_marker(&self, task: &Task) -> Result<()> {
        let Some(sha) = task.head_sha.clone() else {
            bail!("review task {} has no head sha", task.id);
        };
        let Some(cwd) = self.task_cwd(&task.id) else {
            bail!("review task {} has no worktree", task.id);
        };
        self.worktrees.write_reviewed_sha(&cwd, &sha)
    }

    // ------------------------------------------------------------------
    // Operator actions
    // ------------------------------------------------------------------

    /// Apply one operator action from the control socket.
    fn on_action(&mut self, action: Action) {
        match action {
            Action::Refine { repo, kind, number } => {
                if !self.config.repos.contains_key(&repo) {
                    eprintln!("the refine request for {repo}: no such repository");
                    return;
                }
                let log = self.log_path(&repo, Stage::Refine, kind, number);
                match self
                    .table
                    .upsert_queued(&repo, Stage::Refine, kind, number, log, self.now_ms)
                {
                    Ok(_) => self.changed = true,
                    Err(e) => eprintln!(
                        "the refine request for {repo} {} {number}: {e:#}",
                        kind.as_str()
                    ),
                }
            }
            Action::Chat { task, text } => self.chat(&task, &text),
            Action::Answer {
                decision_id,
                response,
            } => self.answer_decision(&decision_id, response),
            Action::Abort { task } => self.cancel_task(&task),
            Action::Retry { task } => self.retry_task(&task),
            Action::Stack { repo, pr, on } => {
                let Some(repo_cfg) = self.config.repos.get(&repo).cloned() else {
                    eprintln!("cannot stack {repo}#{pr}: no such repository");
                    return;
                };
                let Some(train) = self.trains.get_mut(&repo) else {
                    eprintln!("cannot stack {repo}#{pr}: no train");
                    return;
                };
                let gh = GhClient::new(&*self.exec);
                if let Err(e) = train.stack(pr, on, &repo_cfg.owner_repo, &gh) {
                    eprintln!("cannot stack {repo}#{pr}: {e:#}");
                    return;
                }
                self.changed = true;
            }
            Action::Go { repo, prs } => {
                self.decisions.take(&format!("gate:{repo}"));
                self.fire_train(&repo, &prs);
            }
            Action::Policy { repo, policy } => {
                if !self.config.repos.contains_key(&repo) {
                    eprintln!("the policy change for {repo}: no such repository");
                    return;
                }
                if matches!(policy, ReleasePolicy::Threshold { count: 0 })
                    || matches!(policy, ReleasePolicy::Interval { minutes: 0 })
                {
                    eprintln!("the policy change for {repo}: the value must be at least 1");
                    return;
                }
                let is_default = self.config.repos.get(&repo).map(|r| &r.release) == Some(&policy);
                if is_default {
                    self.policies.remove(&repo);
                } else {
                    self.policies.insert(repo, policy);
                }
                self.changed = true;
            }
            Action::Limit { stage, limit } => {
                if limit == 0 {
                    eprintln!("the limit change for {stage}: the limit must be at least 1");
                    return;
                }
                self.limits.stage.insert(stage, limit);
                self.changed = true;
            }
            Action::Lane { stage, repo, slots } => {
                if !self.config.repos.contains_key(&repo) {
                    eprintln!("the lane change for {repo}: no such repository");
                    return;
                }
                let config_slots = self.config.repos[&repo]
                    .lanes
                    .get(&stage)
                    .copied()
                    .unwrap_or(0);
                let key = (stage, repo);
                let changed = if slots == 0 && config_slots == 0 {
                    self.limits.lanes.remove(&key).is_some()
                } else {
                    self.limits.lanes.insert(key, slots) != Some(slots)
                };
                if changed {
                    self.changed = true;
                }
            }
            Action::Pause { scope, paused } => {
                match scope {
                    PauseScope::Global => self.paused.global = paused,
                    PauseScope::Stage { stage } => {
                        if paused {
                            self.paused.stages.insert(stage);
                        } else {
                            self.paused.stages.remove(&stage);
                        }
                    }
                    PauseScope::Repo { repo } => {
                        if !self.config.repos.contains_key(&repo) {
                            eprintln!("the pause change for {repo}: no such repository");
                            return;
                        }
                        if paused {
                            self.paused.repos.insert(repo);
                        } else {
                            self.paused.repos.remove(&repo);
                        }
                    }
                }
                self.changed = true;
            }
            Action::TicketCreate { repo } => self.ticket_create(&repo),
            Action::Reconcile { repo } => self.reconcile(repo.as_deref()),
            Action::Stop => self.shutdown = true,
        }
    }

    /// Send a chat message to a live session, or resume a parked task.
    fn chat(&mut self, id: &str, text: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            match session.send_user(text) {
                Ok(()) => {
                    self.last_event_ms.insert(id.to_string(), self.now_ms);
                }
                Err(error) => eprintln!("the chat message for {id}: {error:#}"),
            }
            return;
        }
        let Some(task) = self.table.by_id.get(id).cloned() else {
            eprintln!("the chat message for {id}: no such task");
            return;
        };
        if task.state != TaskState::AwaitingUser {
            eprintln!(
                "the chat message for {id}: the task is {}, not awaiting a user",
                task.state
            );
            return;
        }
        let Some(session_id) = task.session_id.clone() else {
            eprintln!("the chat message for {id}: no session id to resume");
            return;
        };
        if let Err(e) = self.launch_task(&task, text.to_string(), Some(session_id)) {
            eprintln!("the chat message for {id}: the runner could not resume: {e:#}");
        }
    }

    /// Route one answered decision to its sink.
    ///
    /// The four sinks are the runner's `answer`, the runner's `send_user`,
    /// the task table for stuck rows, and the release train. `NeedsHuman` is
    /// the one sink that touches GitHub instead of a process.
    fn answer_decision(&mut self, id: &str, response: Response) {
        let Some(decision) = self.decisions.take(id) else {
            eprintln!("the answer for {id}: no open decision carries it");
            return;
        };
        if let Err(e) = decisions::validate(&decision, &response) {
            eprintln!("the answer for {id}: {e:#}");
            self.decisions.push(decision);
            return;
        }
        match (&decision.kind, &response) {
            (
                DecisionKind::Permission {
                    task, request_id, ..
                },
                Response::Allow,
            ) => {
                if let Err(error) = self.answer_session(
                    task,
                    request_id,
                    Answer::Allow {
                        updated_input: None,
                    },
                ) {
                    eprintln!("the answer for {task}: {error:#}");
                    self.decisions.push(decision.clone());
                    return;
                }
            }
            (
                DecisionKind::Permission {
                    task, request_id, ..
                },
                Response::Deny { message },
            ) => {
                if let Err(error) = self.answer_session(
                    task,
                    request_id,
                    Answer::Deny {
                        message: message.clone(),
                    },
                ) {
                    eprintln!("the answer for {task}: {error:#}");
                    self.decisions.push(decision.clone());
                    return;
                }
            }
            (
                DecisionKind::Question {
                    task, request_id, ..
                },
                Response::Answers { updated_input },
            ) => {
                if let Err(error) = self.answer_session(
                    task,
                    request_id,
                    Answer::Allow {
                        updated_input: Some(updated_input.clone()),
                    },
                ) {
                    eprintln!("the answer for {task}: {error:#}");
                    self.decisions.push(decision.clone());
                    return;
                }
            }
            (DecisionKind::Question { task, .. }, Response::Text { text }) => {
                if let Err(error) = self.send_to_session(task, text) {
                    eprintln!("the chat message for {task}: {error:#}");
                    self.decisions.push(decision.clone());
                    return;
                }
            }
            (DecisionKind::Stuck { task, .. }, Response::Retry) => self.retry_task(task),
            (DecisionKind::Stuck { task, .. }, Response::Cancel) => self.cancel_task(task),
            (DecisionKind::NeedsHuman { .. }, Response::Text { text }) => {
                self.resolve_needs_human(decision, Some(text))
            }
            (DecisionKind::NeedsHuman { .. }, Response::Cancel) => {
                self.resolve_needs_human(decision, None)
            }
            (DecisionKind::ReleaseGate { prs: expected }, Response::Go { prs }) => {
                if expected != prs {
                    eprintln!(
                        "the answer for {}: the release batch changed from {expected:?} to {prs:?}",
                        decision.id
                    );
                    self.decisions.push(decision.clone());
                    return;
                }
                self.fire_train(&decision.repo, prs);
            }
            _ => eprintln!(
                "the answer for {}: the response does not fit the decision",
                decision.id
            ),
        }
        self.changed = true;
    }

    /// Forward an answer to the live session of one task.
    fn answer_session(&mut self, task: &str, request_id: &str, answer: Answer) -> Result<()> {
        let session = self
            .sessions
            .get_mut(task)
            .ok_or_else(|| anyhow!("no live session holds it"))?;
        session.answer(request_id, answer)
    }

    /// Forward a chat line to the live session of one task.
    fn send_to_session(&mut self, task: &str, text: &str) -> Result<()> {
        let session = self
            .sessions
            .get_mut(task)
            .ok_or_else(|| anyhow!("no live session holds it"))?;
        session.send_user(text)
    }

    /// Apply a `NeedsHuman` answer: comment on GitHub, then drop the label.
    ///
    /// A failed step re-pushes the row, so the human's answer is never
    /// silently lost.
    fn resolve_needs_human(&mut self, decision: Decision, comment: Option<&str>) {
        let (repo, kind, number) = match &decision.kind {
            DecisionKind::NeedsHuman { kind, number, .. } => {
                (decision.repo.clone(), *kind, *number)
            }
            _ => return,
        };
        let Some(repo_cfg) = self.config.repos.get(&repo).cloned() else {
            eprintln!(
                "cannot resolve {repo} {} {number}: no such repository",
                kind.as_str()
            );
            self.decisions.push(decision);
            return;
        };
        if let Some(text) = comment {
            if let Err(e) = self.post_issue_comment(&repo_cfg, number, text) {
                eprintln!("the comment on {repo} {} {number}: {e:#}", kind.as_str());
                self.decisions.push(decision);
                return;
            }
        }
        let gh = GhClient::new(&*self.exec);
        if let Err(e) = gh.remove_label(&repo_cfg.owner_repo, number, NEEDS_HUMAN_LABEL) {
            eprintln!(
                "cannot remove {NEEDS_HUMAN_LABEL} from {repo} {} {number}: {e:#}",
                kind.as_str()
            );
            self.decisions.push(decision);
            return;
        }
        self.changed = true;
    }

    /// Post one comment on an issue or pull request with `gh api`.
    ///
    /// [`GhClient`] has no comment call, so the daemon runs the api call
    /// itself through the command runner.
    fn post_issue_comment(&self, repo: &RepoConfig, number: u64, text: &str) -> Result<()> {
        let url = format!("repos/{}/issues/{number}/comments", repo.owner_repo);
        let field = format!("body={text}");
        let args = ["api", "-X", "POST", url.as_str(), "-f", field.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .map_err(|e| anyhow!("gh could not run: {e:#}"))?;
        if out.status != 0 {
            bail!(
                "gh exited with status {}: {}",
                out.status,
                out.stderr.trim()
            );
        }
        Ok(())
    }

    /// Queue a failed task again from attempt 1.
    fn retry_task(&mut self, id: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            eprintln!("the retry of {id}: no such task");
            return;
        };
        if !task.state.is_terminal() {
            eprintln!("the retry of {id}: the task is still active");
            return;
        }
        if task.stage == Stage::Release {
            let Some(prs) = self.trains.get(&task.repo).map(Train::fired_set) else {
                eprintln!("the retry of {id}: no release train for {}", task.repo);
                return;
            };
            if prs.is_empty() {
                eprintln!("the retry of {id}: the release batch is empty");
                return;
            }
            self.fire_train(&task.repo, &prs);
            let queued = self
                .table
                .by_id
                .get(id)
                .is_some_and(|task| task.state == TaskState::Queued)
                && self.trains[&task.repo].in_flight.as_deref() == Some(id);
            if queued {
                self.decisions.drop_for_task(id);
            }
            return;
        }
        let log = task.log_path.clone();
        match self.table.upsert_queued(
            &task.repo,
            task.stage,
            task.kind,
            task.number,
            log,
            self.now_ms,
        ) {
            Ok(_) => {
                self.decisions.drop_for_task(id);
                self.changed = true;
            }
            Err(e) => eprintln!("the retry of {id}: {e:#}"),
        }
    }

    /// Abort one task: stop its process, cancel it, and drop its decisions.
    fn cancel_task(&mut self, id: &str) {
        if let Some(mut session) = self.sessions.remove(id) {
            if let Err(error) = session.stop() {
                eprintln!("the abort of {id}: cannot stop the session: {error:#}");
            }
        }
        let active = self
            .table
            .by_id
            .get(id)
            .is_some_and(|task| !task.state.is_terminal());
        if active {
            if let Err(e) = self.table.cancel(id, self.now_ms) {
                eprintln!("the abort of {id}: {e:#}");
            }
        }
        let dropped = self.decisions.drop_for_task(id);
        if active || !dropped.is_empty() {
            self.changed = true;
        }
    }

    /// Queue an interactive ticket-creation task for one repository.
    fn ticket_create(&mut self, repo: &str) {
        if !self.config.repos.contains_key(repo) {
            eprintln!("the ticket session for {repo}: no such repository");
            return;
        }
        let log = self.log_path(repo, Stage::Refine, ItemKind::Issue, TICKET_NUMBER);
        match self.table.upsert_queued(
            repo,
            Stage::Refine,
            ItemKind::Issue,
            TICKET_NUMBER,
            log,
            self.now_ms,
        ) {
            Ok(_) => self.changed = true,
            Err(e) => eprintln!("the ticket session for {repo}: {e:#}"),
        }
    }

    /// Force an early poll of one repository, or of all of them.
    fn reconcile(&self, repo: Option<&str>) {
        match repo {
            Some(alias) => match self.wake.get(alias) {
                Some(sender) => {
                    if sender.send(()).is_err() {
                        eprintln!("reconcile: the poller for {alias} stopped");
                    }
                }
                None => eprintln!("reconcile: no poller for {alias}"),
            },
            None => {
                for sender in self.wake.values() {
                    if sender.send(()).is_err() {
                        eprintln!("reconcile: a repository poller stopped");
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Fire one train with an explicit batch and queue its release task.
    fn fire_train(&mut self, repo: &str, prs: &[u64]) {
        let Some(train) = self.trains.get_mut(repo) else {
            eprintln!("cannot fire the train of {repo}: no such repository");
            return;
        };
        let id = match train.fire(prs, self.now_ms) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("cannot fire the train of {repo}: {e:#}");
                return;
            }
        };
        self.changed = true;
        self.release_batches.insert(id.clone(), prs.to_vec());
        let Some(first) = prs.iter().min().copied() else {
            return;
        };
        let log = self.log_path(repo, Stage::Release, ItemKind::Pr, first);
        if let Err(e) =
            self.table
                .upsert_queued(repo, Stage::Release, ItemKind::Pr, first, log, self.now_ms)
        {
            eprintln!("the release task {id}: {e:#}");
            self.finish_train(repo, false);
            return;
        }
        self.changed = true;
    }

    /// The number of live sessions of one stage.
    ///
    /// This counts each session until its process reports `Exit`.
    fn live_sessions(&self, stage: Stage) -> usize {
        self.sessions
            .keys()
            .filter(|id| {
                self.table
                    .by_id
                    .get(*id)
                    .is_some_and(|task| task.stage == stage)
            })
            .count()
    }

    /// The working directory of a task's run.
    fn task_cwd(&self, id: &str) -> Option<PathBuf> {
        let task = self.table.by_id.get(id)?;
        let repo = self.config.repos.get(&task.repo)?;
        Some(match task.stage {
            Stage::Refine => repo.path.clone(),
            Stage::Implement | Stage::Review => self.worktrees.issue_path(repo, task.number),
            Stage::Release => self.worktrees.train_path(repo),
        })
    }

    /// True when the task is a ticket-creation session.
    fn is_ticket_task(task: &Task) -> bool {
        task.stage == Stage::Refine && task.kind == ItemKind::Issue && task.number == TICKET_NUMBER
    }

    /// The task log path: `<state_dir>/logs/<repo>__<stage>-<kind><n>.jsonl`.
    fn log_path(&self, repo: &str, stage: Stage, kind: ItemKind, number: u64) -> PathBuf {
        self.state_dir.join("logs").join(format!(
            "{repo}__{}-{}{number}.jsonl",
            stage.as_str(),
            kind.as_str()
        ))
    }

    /// The release policy of one repository: the override, else the config.
    fn active_policy(&self, repo: &str) -> &ReleasePolicy {
        match self.policies.get(repo) {
            Some(policy) => policy,
            None => self
                .config
                .repos
                .get(repo)
                .map(|repo_cfg| &repo_cfg.release)
                .unwrap_or(&MANUAL_POLICY),
        }
    }

    /// Render the prompt of one task.
    ///
    /// The template comes from `prompts/<stage>.md` in the config directory,
    /// or from the built-in default. Every placeholder must be known; an
    /// unknown one is an error that names it, never a silent literal.
    fn render_prompt(&self, task: &Task, repo_cfg: &RepoConfig, worktree: &Path) -> Result<String> {
        if Self::is_ticket_task(task) {
            return fill_template(
                TICKET_PROMPT,
                &[
                    ("repo", task.repo.clone()),
                    ("owner_repo", repo_cfg.owner_repo.clone()),
                    ("worktree", worktree.display().to_string()),
                ],
            );
        }
        let template = self.prompt_template(task.stage)?;
        let (pr_list, pr_numbers, pr_count) = match self.release_batches.get(&task.id) {
            Some(prs) => {
                let snapshot = self.snapshot.repos.get(&task.repo);
                let mut lines = Vec::new();
                let mut numbers = Vec::new();
                for pr in prs {
                    numbers.push(pr.to_string());
                    let title = snapshot
                        .and_then(|snap| snap.prs.get(pr))
                        .map(|entry| entry.title.as_str())
                        .unwrap_or("");
                    lines.push(format!("- #{pr} {title}"));
                }
                (lines.join("\n"), numbers.join(", "), prs.len().to_string())
            }
            None => (String::new(), String::new(), "0".to_string()),
        };
        let snapshot = self.snapshot.repos.get(&task.repo);
        let (title, body) = match (task.kind, snapshot) {
            (_, None) => (String::new(), String::new()),
            (ItemKind::Issue, Some(snap)) => snap
                .issues
                .get(&task.number)
                .map(|issue| (issue.title.clone(), issue.body.clone()))
                .unwrap_or_default(),
            (ItemKind::Pr, Some(snap)) => snap
                .prs
                .get(&task.number)
                .map(|pr| (pr.title.clone(), pr.body.clone()))
                .unwrap_or_default(),
        };
        fill_template(
            &template,
            &[
                ("repo", task.repo.clone()),
                ("owner_repo", repo_cfg.owner_repo.clone()),
                ("number", task.number.to_string()),
                ("title", title),
                ("body", body),
                ("worktree", worktree.display().to_string()),
                ("pr_list", pr_list),
                ("pr_numbers", pr_numbers),
                ("pr_count", pr_count),
            ],
        )
    }

    /// Read the prompt template of one stage, or fall back to the built-in.
    fn prompt_template(&self, stage: Stage) -> Result<String> {
        let path = self.prompts_dir.join(format!("{}.md", stage.as_str()));
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(builtin_prompt(stage).to_string()),
            Err(e) => Err(anyhow!("cannot read {}: {e}", path.display())),
        }
    }

    /// Assemble the persisted view of the current state.
    ///
    /// Only overrides go in: a value that matches the config stays out.
    fn collect_state(&self) -> DaemonState {
        let mut stage_limits = BTreeMap::new();
        for (stage, limit) in &self.limits.stage {
            if *limit != self.config.stage(*stage).limit {
                stage_limits.insert(*stage, *limit);
            }
        }
        let mut lanes = Vec::new();
        for ((stage, repo), slots) in &self.limits.lanes {
            let config_slots = self
                .config
                .repos
                .get(repo)
                .and_then(|repo_cfg| repo_cfg.lanes.get(stage))
                .copied()
                .unwrap_or(0);
            if config_slots != *slots {
                lanes.push((*stage, repo.clone(), *slots));
            }
        }
        let last_fire_ms = self
            .trains
            .iter()
            .filter_map(|(repo, train)| train.last_fire_ms.map(|stamp| (repo.clone(), stamp)))
            .collect();
        DaemonState {
            stage_limits,
            lanes,
            policies: self.policies.clone(),
            last_fire_ms,
        }
    }

    /// Persist the state when, and only when, a value changed.
    fn save_state(&mut self) {
        if self.changed {
            self.dirty = true;
        }
        if !self.changed {
            return;
        }
        self.changed = false;
        let state = self.collect_state();
        let text = match state.to_json() {
            Ok(text) => text,
            Err(error) => {
                eprintln!("cannot serialize the daemon state: {error:#}");
                self.changed = true;
                return;
            }
        };
        if self.saved.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Err(e) = state.save(&self.state_path) {
            eprintln!("cannot write {}: {e:#}", self.state_path.display());
            self.changed = true;
            return;
        }
        self.saved = Some(text);
    }
}

/// The task id of one run event.
fn event_task(event: &RunEvent) -> &str {
    match event {
        RunEvent::Started { task, .. }
        | RunEvent::Text { task, .. }
        | RunEvent::Tool { task, .. }
        | RunEvent::Ask { task, .. }
        | RunEvent::TurnEnd { task, .. }
        | RunEvent::Exit { task, .. } => task,
    }
}

/// Fill a prompt template.
fn fill_template(template: &str, values: &[(&str, String)]) -> Result<String> {
    for token in scan_placeholders(template) {
        if !values.iter().any(|(name, _)| *name == token) {
            bail!("the prompt template uses the unknown placeholder {{{token}}}");
        }
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &after[..end];
        if let Some((_, value)) = values.iter().find(|(name, _)| *name == token) {
            out.push_str(value);
        } else {
            out.push_str(&rest[start..start + end + 2]);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// List the `{placeholder}` tokens of a template, in first-seen order.
///
/// A token is placeholder-shaped when it holds only ASCII letters, digits,
/// underscores, and hyphens. Other brace content stays untouched.
fn scan_placeholders(template: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let token = &after[..end];
        if !token.is_empty()
            && !token.contains('{')
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !found.contains(&token)
        {
            found.push(token);
        }
        rest = &after[end + 1..];
    }
    found
}

/// Fold one inbound source into the loop's channel.
///
/// One thread per source blocks on its receiver, so the loop itself never
/// polls.
fn forwarder<T: Send + 'static>(
    name: &str,
    rx: Receiver<T>,
    tx: Sender<Inbound>,
    wrap: fn(T) -> Inbound,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            while let Ok(message) = rx.recv() {
                if tx.send(wrap(message)).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| anyhow!("cannot spawn the {name} forwarder: {e}"))
}

/// The current time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The built-in prompt of one stage.
fn builtin_prompt(stage: Stage) -> &'static str {
    match stage {
        Stage::Refine => REFINE_PROMPT,
        Stage::Implement => IMPLEMENT_PROMPT,
        Stage::Review => REVIEW_PROMPT,
        Stage::Release => RELEASE_PROMPT,
    }
}

/// The built-in prompt of a refine run.
///
/// It runs in the repository checkout and never creates a worktree.
pub const REFINE_PROMPT: &str = r#"You refine one GitHub issue in the repository {repo}
({owner_repo}). You work in {worktree}, the repository checkout. Never create
a git worktree; stay in this checkout.

Issue #{number}: {title}

{body}

Read the issue and the surrounding code. Edit the issue body until it is a
complete, testable specification: the problem, the agreed approach, the
acceptance criteria. Write comments on the issue with `gh` when you decide
something the body must record.

When you need a human decision, add the `needs-human` label to the issue with
`gh` and state the question in a comment. Stop after the label is on.

When the specification is complete, end your turn and report one line that
says the issue is refined.
"#;

/// The built-in prompt of an implement run.
pub const IMPLEMENT_PROMPT: &str = r#"You implement GitHub issue #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Issue #{number}: {title}

{body}

Implement the issue on the current branch. Follow its acceptance criteria.
Run the test suite and make it pass. Commit your work in small, complete
commits. Open a pull request with `gh pr create` when the work is done, and
mention `#{number}` in the body.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to issue #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess.

Report one line at the end: what you did, and the pull request number.
"#;

/// The built-in prompt of a review run.
pub const REVIEW_PROMPT: &str = r#"You review one pull request of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Pull request #{number}: {title}

{body}

Read the diff of the pull request with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Leave your findings as a
review with `gh pr review {number}`: approve it when it is correct, or
request changes with concrete findings.

If the change needs a human decision, add the `needs-human` label to the
pull request with `gh`, write the question into a comment, and stop. Do not
guess.

Report one line at the end: the review verdict.
"#;

/// The built-in prompt of a release run.
pub const RELEASE_PROMPT: &str = r#"You release the stacked pull requests of {repo}
({owner_repo}). You work in {worktree}, the release worktree. Never create
another git worktree; work only in this one.

The batch holds {pr_count} pull request(s), in merge order:

{pr_list}

Merge every pull request in the listed order with `gh pr merge`, one at a
time. Merge order is {pr_numbers}. After each merge, pull the base branch
into this worktree so the next merge sees the updated state. If a merge
conflicts, stop, and report the pull request number that failed.

When all merges are done, report one line: the released pull requests.
"#;

/// The built-in prompt of a ticket-creation session.
pub const TICKET_PROMPT: &str = r#"You help the operator create one GitHub issue in the
repository {repo} ({owner_repo}). You work in {worktree}, the repository
checkout. Never create a git worktree; stay in this checkout.

Ask the operator what the ticket should say, in short questions, one topic at
a time. When you know enough, draft the title and body, show them, and on
approval create the issue with `gh issue create`. Report the new issue
number.

If the operator asks for something you cannot decide alone, say so plainly
and ask again.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StageConfig;
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, Pr, RepoSnapshot};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// The fake wall-clock time every rig starts at.
    const T0: u64 = 1_000_000;

    // ------------------------------------------------------------------
    // Scripted command steps
    // ------------------------------------------------------------------

    type Step = (Box<dyn Fn(&Call) -> bool + Send + Sync>, CmdOut);

    /// Build a command runner over the scripted steps, in call order.
    fn scripted(steps: Vec<Step>) -> Arc<ScriptExec> {
        let mut exec = ScriptExec::new();
        for (matches, out) in steps {
            exec = exec.expect(matches, out);
        }
        Arc::new(exec)
    }

    /// A step for `git -C <dir> <args...>`.
    fn git_step(dir: &Path, args: &[&str], out: CmdOut) -> Step {
        let mut full: Vec<String> = Vec::with_capacity(args.len() + 2);
        full.push("-C".to_string());
        full.push(dir.to_string_lossy().into_owned());
        full.extend(args.iter().map(|a| a.to_string()));
        (
            Box::new(move |call: &Call| call.program == "git" && call.args == full),
            out,
        )
    }

    /// A step for `gh <args...>`.
    fn gh_step(args: &[&str], out: CmdOut) -> Step {
        let full: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        (
            Box::new(move |call: &Call| call.program == "gh" && call.args == full),
            out,
        )
    }

    /// A successful `gh api -i` body: one status line and an empty body.
    fn gh_ok() -> CmdOut {
        CmdOut::ok("HTTP/1.1 204 No Content\r\n\r\n")
    }

    /// A status-1 output, which git uses for "the reference is absent".
    fn refused() -> CmdOut {
        CmdOut {
            status: 1,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// The common-dir step of `prepare`.
    fn common_dir_step(worktree: &Path, gitdir: &Path) -> Step {
        git_step(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            CmdOut::ok(format!("{}\n", gitdir.display())),
        )
    }

    /// The four git calls of a fresh issue worktree.
    fn fresh_issue_steps(repo: &Path, worktree: &Path, number: u64, gitdir: &Path) -> Vec<Step> {
        let reference = format!("refs/heads/aif/borsuk/issue-{number}");
        let branch = format!("aif/borsuk/issue-{number}");
        let wt_text = worktree.to_string_lossy().into_owned();
        vec![
            git_step(
                repo,
                &["rev-parse", "--verify", "--quiet", reference.as_str()],
                refused(),
            ),
            git_step(
                repo,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                refused(),
            ),
            git_step(
                repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch.as_str(),
                    wt_text.as_str(),
                    "HEAD",
                ],
                CmdOut::ok(""),
            ),
            common_dir_step(worktree, gitdir),
        ]
    }

    /// The two git calls of a reused issue worktree.
    fn reuse_issue_steps(repo: &Path, worktree: &Path, gitdir: &Path) -> Vec<Step> {
        let listed = format!("worktree {}\n", worktree.display());
        vec![
            git_step(
                repo,
                &["worktree", "list", "--porcelain"],
                CmdOut::ok(listed),
            ),
            common_dir_step(worktree, gitdir),
        ]
    }

    /// The four git calls of a fresh train worktree.
    fn fresh_train_steps(repo: &Path, worktree: &Path, gitdir: &Path) -> Vec<Step> {
        let wt_text = worktree.to_string_lossy().into_owned();
        vec![
            git_step(
                repo,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                refused(),
            ),
            git_step(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/aif/borsuk/train",
                ],
                refused(),
            ),
            git_step(
                repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "aif/borsuk/train",
                    wt_text.as_str(),
                    "HEAD",
                ],
                CmdOut::ok(""),
            ),
            common_dir_step(worktree, gitdir),
        ]
    }

    /// The four git calls of a reused train worktree.
    fn reuse_train_steps(repo: &Path, worktree: &Path, gitdir: &Path) -> Vec<Step> {
        let listed = format!("worktree {}\n", worktree.display());
        vec![
            git_step(
                repo,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                refused(),
            ),
            git_step(
                repo,
                &["worktree", "list", "--porcelain"],
                CmdOut::ok(listed),
            ),
            git_step(worktree, &["reset", "--hard", "HEAD"], CmdOut::ok("")),
            common_dir_step(worktree, gitdir),
        ]
    }

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    /// The test-visible handles of one fake session.
    #[derive(Clone)]
    struct SessionHandle {
        stopped: Arc<AtomicBool>,
        answers: Arc<Mutex<Vec<String>>>,
        sends: Arc<Mutex<Vec<String>>>,
        fail_answer: Arc<AtomicBool>,
        fail_send: Arc<AtomicBool>,
    }

    impl SessionHandle {
        fn new() -> Self {
            SessionHandle {
                stopped: Arc::new(AtomicBool::new(false)),
                answers: Arc::new(Mutex::new(Vec::new())),
                sends: Arc::new(Mutex::new(Vec::new())),
                fail_answer: Arc::new(AtomicBool::new(false)),
                fail_send: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    /// A session that records steering calls and never runs a process.
    struct FakeSession {
        handle: SessionHandle,
    }

    impl Session for FakeSession {
        fn send_user(&mut self, text: &str) -> anyhow::Result<()> {
            if self.handle.fail_send.load(Ordering::SeqCst) {
                bail!("the fake session refuses the message");
            }
            self.handle.sends.lock().unwrap().push(text.to_string());
            Ok(())
        }

        fn answer(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()> {
            if self.handle.fail_answer.load(Ordering::SeqCst) {
                bail!("the fake session refuses the answer");
            }
            let tag = match answer {
                Answer::Allow { .. } => "allow",
                Answer::Deny { .. } => "deny",
            };
            self.handle
                .answers
                .lock()
                .unwrap()
                .push(format!("{request_id}:{tag}"));
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            self.handle.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A runner that records every job and hands back a fake session.
    struct FakeRunner {
        jobs: Arc<Mutex<Vec<Job>>>,
        sessions: Arc<Mutex<Vec<SessionHandle>>>,
    }

    impl FakeRunner {
        fn new(jobs: Arc<Mutex<Vec<Job>>>, sessions: Arc<Mutex<Vec<SessionHandle>>>) -> Self {
            FakeRunner { jobs, sessions }
        }
    }

    impl Runner for FakeRunner {
        fn start(&mut self, job: &Job, _tx: Sender<RunEvent>) -> anyhow::Result<Box<dyn Session>> {
            self.jobs.lock().unwrap().push(job.clone());
            let handle = SessionHandle::new();
            self.sessions.lock().unwrap().push(handle.clone());
            Ok(Box::new(FakeSession { handle }))
        }
    }

    // ------------------------------------------------------------------
    // The rig
    // ------------------------------------------------------------------

    /// A fresh temporary root with a fixed alias layout.
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "aif-daemon-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    /// The worktree path of one issue inside a rig root.
    fn issue_wt(dir: &Path, number: u64) -> PathBuf {
        dir.join("state")
            .join("worktrees")
            .join("borsuk")
            .join(format!("issue-{number}"))
    }

    /// The worktree path of the train inside a rig root.
    fn train_wt(dir: &Path) -> PathBuf {
        dir.join("state")
            .join("worktrees")
            .join("borsuk")
            .join("train")
    }

    /// A four-stage config over one repository with fixed limits.
    fn test_config(dir: &Path) -> Config {
        let stage = |limit: usize| StageConfig {
            model: "m".to_string(),
            runner: "claude".to_string(),
            variant: None,
            limit,
            yolo: true,
        };
        let mut stages = BTreeMap::new();
        stages.insert(Stage::Refine, stage(2));
        stages.insert(Stage::Implement, stage(1));
        stages.insert(Stage::Review, stage(2));
        stages.insert(Stage::Release, stage(1));
        let mut repos = BTreeMap::new();
        repos.insert(
            "borsuk".to_string(),
            RepoConfig {
                alias: "borsuk".to_string(),
                path: dir.join("repo"),
                owner_repo: "acme/borsuk".to_string(),
                lanes: BTreeMap::new(),
                release: ReleasePolicy::Manual,
            },
        );
        Config { stages, repos }
    }

    /// One open issue.
    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: format!("issue {number}"),
            body: format!("body {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            open: true,
        }
    }

    /// One open pull request.
    fn pr(number: u64, draft: bool, labels: &[&str]) -> Pr {
        Pr {
            number,
            node_id: format!("node-{number}"),
            title: format!("pr {number}"),
            body: format!("body {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            open: true,
            draft,
            head_sha: format!("sha{number}"),
        }
    }

    /// A daemon over fake runners, a scripted command runner, and a pinned
    /// clock.
    struct Rig {
        daemon: Daemon,
        exec: Arc<ScriptExec>,
        jobs: Arc<Mutex<Vec<Job>>>,
        sessions: Arc<Mutex<Vec<SessionHandle>>>,
        t: Arc<Mutex<u64>>,
        repo: PathBuf,
        prompts: PathBuf,
    }

    impl Rig {
        fn make(steps: Vec<Step>) -> Rig {
            Self::make_in(temp_root(), steps, |_| {})
        }

        fn make_with(steps: Vec<Step>, tweak: impl FnOnce(&mut Config)) -> Rig {
            Self::make_in(temp_root(), steps, tweak)
        }

        fn make_in(dir: PathBuf, steps: Vec<Step>, tweak: impl FnOnce(&mut Config)) -> Rig {
            fs::create_dir_all(dir.join("repo")).unwrap();
            let state = dir.join("state");
            let prompts = dir.join("prompts");
            let mut config = test_config(&dir);
            tweak(&mut config);
            let exec = scripted(steps);
            let jobs = Arc::new(Mutex::new(Vec::new()));
            let sessions = Arc::new(Mutex::new(Vec::new()));
            let mut runners: BTreeMap<Stage, Box<dyn Runner>> = BTreeMap::new();
            for stage in Stage::ALL {
                runners.insert(
                    stage,
                    Box::new(FakeRunner::new(jobs.clone(), sessions.clone())),
                );
            }
            let (_poll_tx, poll_rx) = mpsc::channel::<DaemonMsg>();
            let (wake_tx, _wake_rx) = mpsc::channel::<()>();
            let mut wake = BTreeMap::new();
            wake.insert("borsuk".to_string(), wake_tx);
            let (_action_tx, action_rx) = mpsc::channel();
            let t = Arc::new(Mutex::new(T0));
            let mut daemon = Daemon::with_runners(
                config,
                exec.clone(),
                state.clone(),
                prompts.clone(),
                poll_rx,
                wake,
                action_rx,
                runners,
            );
            let clock_t = t.clone();
            daemon.clock = Arc::new(move || *clock_t.lock().unwrap());
            Rig {
                daemon,
                exec,
                jobs,
                sessions,
                t,
                repo: dir.join("repo"),
                prompts,
            }
        }

        fn set_now(&self, ms: u64) {
            *self.t.lock().unwrap() = ms;
        }

        /// Apply one poll of the `borsuk` repository.
        fn poll(&mut self, issues: Vec<Issue>, prs: Vec<Pr>) {
            let mut issue_map = BTreeMap::new();
            for one in issues {
                issue_map.insert(one.number, one);
            }
            let mut pr_map = BTreeMap::new();
            for one in prs {
                pr_map.insert(one.number, one);
            }
            self.daemon.handle(Inbound::Poll(DaemonMsg::Polled {
                repo: "borsuk".to_string(),
                snapshot: RepoSnapshot {
                    issues: issue_map,
                    prs: pr_map,
                },
            }));
        }

        fn event(&mut self, event: RunEvent) {
            self.daemon.handle(Inbound::Run(event));
        }

        fn act(&mut self, action: Action) {
            self.daemon.handle(Inbound::Act(action));
        }

        fn drive(&mut self) {
            self.daemon.now_ms = *self.t.lock().unwrap();
            self.daemon.drive();
        }

        fn job_count(&self) -> usize {
            self.jobs.lock().unwrap().len()
        }

        fn job(&self, index: usize) -> Job {
            self.jobs.lock().unwrap()[index].clone()
        }

        fn session(&self, index: usize) -> SessionHandle {
            self.sessions.lock().unwrap()[index].clone()
        }

        fn task(&self, id: &str) -> Task {
            self.daemon.table.by_id[id].clone()
        }

        fn decision(&self, id: &str) -> Option<Decision> {
            self.daemon
                .decisions
                .open()
                .iter()
                .find(|row| row.id == id)
                .cloned()
        }
    }

    fn started(task: &str, session_id: &str) -> RunEvent {
        RunEvent::Started {
            task: task.to_string(),
            session_id: Some(session_id.to_string()),
        }
    }

    fn exited(task: &str, ok: bool, detail: &str) -> RunEvent {
        RunEvent::Exit {
            task: task.to_string(),
            ok,
            detail: detail.to_string(),
        }
    }

    fn turn_ended(task: &str) -> RunEvent {
        turn_finished(task, true, "")
    }

    fn turn_finished(task: &str, ok: bool, summary: &str) -> RunEvent {
        RunEvent::TurnEnd {
            task: task.to_string(),
            ok,
            summary: summary.to_string(),
            cost_usd: None,
        }
    }

    // ------------------------------------------------------------------
    // Acceptance tests
    // ------------------------------------------------------------------

    #[test]
    fn a_gate_admits_work_and_a_second_drive_dispatches_nothing() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(rig.job_count(), 1);
        let job = rig.job(0);
        assert_eq!(job.task, "borsuk/refine-i142");
        assert_eq!(job.stage, Stage::Refine);
        assert_eq!(job.cwd, rig.repo);
        assert_eq!(job.resume, None);
        assert!(job.yolo);
        assert!(job.log.ends_with("borsuk__refine-i142.jsonl"));
        assert!(job.prompt.contains("issue 142"));
        assert!(job.prompt.contains("body 142"));
        assert!(job.prompt.contains("borsuk"));
        assert!(!job.prompt.contains('{'));

        rig.event(started("borsuk/refine-i142", "sid-1"));
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.session_id.as_deref(), Some("sid-1"));
        assert!(rig.repo.join(".aif/session").exists());

        // A second drive with no new message dispatches nothing.
        rig.drive();
        assert_eq!(rig.job_count(), 1);
        // A poll that repeats the same snapshot opens no gate again.
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(rig.job_count(), 1);
        assert!(rig
            .daemon
            .state_input()
            .table
            .by_id
            .contains_key("borsuk/refine-i142"));
    }

    #[test]
    fn a_session_marker_error_stops_and_retries_the_task() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        let first = rig.session(0);
        fs::create_dir_all(rig.repo.join(".aif").join("session")).unwrap();

        rig.event(started("borsuk/refine-i142", "sid-1"));

        assert!(first.stopped.load(Ordering::SeqCst));
        assert_eq!(rig.task("borsuk/refine-i142").attempt, 2);
        assert_eq!(rig.job_count(), 2);
    }

    #[test]
    fn a_failing_task_requeues_then_the_third_failure_opens_stuck() {
        let dir = temp_root();
        let wt = issue_wt(&dir, 142);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &wt, 142, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 1);

        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 2);

        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.job_count(), 3);
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);

        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.job_count(), 3);
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.state, TaskState::Failed("boom".to_string()));
        let stuck = rig
            .daemon
            .decisions
            .open()
            .iter()
            .find(|row| matches!(row.kind, DecisionKind::Stuck { .. }))
            .expect("the third failure opens a stuck row");
        assert_eq!(stuck.id, "stuck:borsuk/implement-i142:3");
    }

    #[test]
    fn review_success_writes_reviewed_sha_and_failure_does_not() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(fresh_issue_steps(
                    &rig_repo(&dir),
                    &issue_wt(&dir, 6),
                    6,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        assert_eq!(rig.job_count(), 1);
        assert_eq!(
            rig.task("borsuk/review-p5").head_sha.as_deref(),
            Some("sha5")
        );

        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = issue_wt(&dir, 5).join(".aif").join("reviewed-sha");
        assert_eq!(fs::read_to_string(marker).unwrap().trim_end(), "sha5");

        rig.poll(vec![], vec![pr(5, true, &[]), pr(6, true, &[])]);
        rig.event(turn_finished("borsuk/review-p6", false, "lint"));
        assert_eq!(rig.task("borsuk/review-p6").attempt, 2);
        assert!(!issue_wt(&dir, 6).join(".aif").join("reviewed-sha").exists());
    }

    #[test]
    fn a_review_marker_error_requeues_the_review() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        fs::create_dir_all(worktree.join(".aif").join("reviewed-sha")).unwrap();

        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));

        let task = rig.task("borsuk/review-p5");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.attempt, 2);
    }

    #[test]
    fn permission_asks_route_to_the_live_session() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().yolo = false;
        });
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "req-1".to_string(),
            tool: "Bash".to_string(),
            input: json!({"command": "ls"}),
            suggestions: serde_json::Value::Null,
            needs_human: false,
        });
        let row = rig
            .decision("perm:borsuk/refine-i142:req-1")
            .expect("a permission ask opens a row");
        assert!(matches!(row.kind, DecisionKind::Permission { .. }));

        rig.act(Action::Answer {
            decision_id: "perm:borsuk/refine-i142:req-1".to_string(),
            response: Response::Allow,
        });
        rig.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "req-2".to_string(),
            tool: "Bash".to_string(),
            input: json!({"command": "rm -rf /"}),
            suggestions: serde_json::Value::Null,
            needs_human: false,
        });
        rig.act(Action::Answer {
            decision_id: "perm:borsuk/refine-i142:req-2".to_string(),
            response: Response::Deny {
                message: "not now".to_string(),
            },
        });
        let answers = rig.session(0).answers.lock().unwrap().clone();
        assert_eq!(answers, vec!["req-1:allow", "req-2:deny"]);
        assert!(rig.decision("perm:borsuk/refine-i142:req-1").is_none());
    }

    #[test]
    fn question_text_becomes_a_chat_line() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "q-1".to_string(),
            tool: "AskUserQuestion".to_string(),
            input: json!([{"question": "which database?"}]),
            suggestions: serde_json::Value::Null,
            needs_human: true,
        });
        let row = rig
            .decision("perm:borsuk/refine-i142:q-1")
            .expect("a needs-human ask opens a row");
        assert!(matches!(row.kind, DecisionKind::Question { .. }));

        rig.act(Action::Answer {
            decision_id: "perm:borsuk/refine-i142:q-1".to_string(),
            response: Response::Text {
                text: "use postgres".to_string(),
            },
        });
        let sends = rig.session(0).sends.lock().unwrap().clone();
        assert_eq!(sends, vec!["use postgres".to_string()]);
    }

    #[test]
    fn turn_end_parks_a_session_and_the_reaper_frees_the_slot() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().limit = 1;
        });
        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        assert_eq!(
            rig.job_count(),
            1,
            "the live limit holds the second task back"
        );
        rig.event(turn_ended("borsuk/refine-i142"));
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert!(!rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(30 * 60_000)),
            "the parked session sets the reaper deadline"
        );

        rig.set_now(T0 + 31 * 60_000);
        rig.drive();
        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the reaper stops the process"
        );
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser,
            "the task stays parked and resumable"
        );
        assert_eq!(rig.job_count(), 2, "the freed slot admits the second task");
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_restart_restores_trains_decisions_and_worktrees() {
        let dir = temp_root();
        let snapshot_issues = vec![issue(142, &["refined"]), issue(7, &["needs-human"])];
        let snapshot_prs = vec![pr(2, false, &["release-stacked"])];

        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut first = Rig::make_in(dir.clone(), steps, |_| {});
        first.poll(snapshot_issues.clone(), snapshot_prs.clone());
        assert_eq!(first.job_count(), 1);
        assert!(first.decision("gate:borsuk").is_some());
        assert!(first.decision("human:borsuk:i7").is_some());
        assert!(first.daemon.trains["borsuk"].queue.contains(&2));
        let old_cwd = first.job(0).cwd.clone();
        drop(first);

        let steps = reuse_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 142), &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |_| {});
        second.poll(snapshot_issues, snapshot_prs);
        assert_eq!(second.job_count(), 1, "the gate re-opens the lost task");
        assert_eq!(
            second.job(0).cwd,
            old_cwd,
            "the work resumes in the same worktree"
        );
        assert!(second.decision("gate:borsuk").is_some());
        assert!(second.decision("human:borsuk:i7").is_some());
        assert!(second.daemon.trains["borsuk"].queue.contains(&2));
        assert!(
            second.exec.calls().iter().all(|call| call.program == "git"),
            "the restart touches no GitHub call"
        );
    }

    #[test]
    fn an_interval_fire_survives_a_restart() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut first = Rig::make_in(dir.clone(), steps, |_| {});
        first.act(Action::Policy {
            repo: "borsuk".to_string(),
            policy: ReleasePolicy::Interval { minutes: 60 },
        });
        first.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        assert_eq!(
            first.job_count(),
            1,
            "a never-fired interval train fires at once"
        );
        assert_eq!(first.job(0).stage, Stage::Release);
        assert!(first.job(0).prompt.contains("#2"));
        assert_eq!(first.daemon.trains["borsuk"].last_fire_ms, Some(T0));
        let state_text = fs::read_to_string(dir.join("state").join("state.json")).unwrap();
        assert!(state_text.contains("last_fire_ms"));
        drop(first);

        let steps = reuse_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |_| {});
        assert_eq!(
            second.daemon.trains["borsuk"].last_fire_ms,
            Some(T0),
            "the restart restores last_fire_ms before the first drive"
        );
        second.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        assert_eq!(second.job_count(), 0, "a fresh restart never re-releases");
        assert!(second.exec.calls().is_empty());

        second.set_now(T0 + 61 * 60_000);
        second.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        assert_eq!(
            second.job_count(),
            1,
            "the train fires again once the interval passes"
        );
        assert_eq!(
            second.daemon.trains["borsuk"].last_fire_ms,
            Some(T0 + 61 * 60_000)
        );
    }

    #[test]
    fn next_deadline_picks_the_earliest_and_none_when_idle() {
        let mut rig = Rig::make(vec![]);
        assert_eq!(
            rig.daemon.next_deadline(),
            None,
            "an idle daemon never wakes"
        );

        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(30 * 60_000))
        );

        rig.daemon.policies.insert(
            "borsuk".to_string(),
            ReleasePolicy::Interval { minutes: 60 },
        );
        let train = rig.daemon.trains.get_mut("borsuk").unwrap();
        train.last_fire_ms = Some(T0 + 10 * 60_000);
        train.queue.push(5);
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(30 * 60_000)),
            "the reaper beats the train fire at 70 minutes"
        );

        rig.set_now(T0 + 31 * 60_000);
        rig.drive();
        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(39 * 60_000)),
            "only the train deadline remains"
        );
    }

    #[test]
    fn an_idle_loop_waits_for_a_message_and_stop_returns() {
        let mut rig = Rig::make(vec![]);
        let (action_tx, action_rx) = mpsc::channel();
        rig.daemon.action_rx = Some(action_rx);
        let (clock_tx, clock_rx) = mpsc::channel();
        rig.daemon.clock = Arc::new(move || {
            let _ = clock_tx.send(());
            T0
        });
        let daemon = rig.daemon;
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = done_tx.send(daemon.run());
        });

        clock_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the loop reads the clock before it blocks");
        assert_eq!(
            clock_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "an idle loop does not wake without a message or deadline"
        );

        action_tx.send(Action::Stop).unwrap();
        drop(action_tx);
        let result = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the stop action must let the event loop return");
        assert!(result.is_ok(), "the event loop returned {result:?}");
        handle.join().unwrap();
    }

    #[test]
    fn the_gate_row_tracks_the_stacked_set() {
        let steps = vec![
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "POST",
                    "repos/acme/borsuk/issues/3/labels",
                    "-f",
                    "labels[]=release-stacked",
                ],
                gh_ok(),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/2/labels/release-stacked",
                ],
                gh_ok(),
            ),
        ];
        let mut rig = Rig::make(steps);
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        let first = rig
            .decision("gate:borsuk")
            .expect("a manual train opens a gate row");
        let sorted = |row: &Decision| match &row.kind {
            DecisionKind::ReleaseGate { prs } => prs.clone(),
            _ => Vec::new(),
        };
        assert_eq!(sorted(&first), vec![2]);

        rig.poll(
            vec![],
            vec![pr(2, false, &["release-stacked"]), pr(3, false, &[])],
        );
        rig.act(Action::Stack {
            repo: "borsuk".to_string(),
            pr: 3,
            on: true,
        });
        // The next poll confirms the label, and the row refreshes to the new set.
        rig.set_now(T0 + 1);
        rig.poll(
            vec![],
            vec![
                pr(2, false, &["release-stacked"]),
                pr(3, false, &["release-stacked"]),
            ],
        );
        let second = rig.decision("gate:borsuk").expect("the row stays open");
        let mut prs = sorted(&second);
        prs.sort();
        assert_eq!(prs, vec![2, 3]);
        assert_eq!(
            second.opened_ms,
            T0 + 1,
            "a changed batch replaces the stale approval row"
        );

        rig.act(Action::Stack {
            repo: "borsuk".to_string(),
            pr: 2,
            on: false,
        });
        rig.poll(
            vec![],
            vec![pr(2, false, &[]), pr(3, false, &["release-stacked"])],
        );
        assert_eq!(sorted(&rig.decision("gate:borsuk").unwrap()), vec![3]);
        assert!(
            rig.decision("gate:borsuk").is_some(),
            "the queue still holds work"
        );

        rig.poll(vec![], vec![]);
        assert!(
            rig.decision("gate:borsuk").is_none(),
            "an empty train closes the row"
        );
        assert_eq!(rig.exec.calls().len(), 2);
    }

    #[test]
    fn a_release_answer_must_match_the_gate_snapshot() {
        let mut rig = Rig::make(vec![]);
        rig.poll(
            vec![],
            vec![
                pr(2, false, &["release-stacked"]),
                pr(3, false, &["release-stacked"]),
            ],
        );

        rig.act(Action::Answer {
            decision_id: "gate:borsuk".to_string(),
            response: Response::Go { prs: vec![2] },
        });

        assert!(rig.decision("gate:borsuk").is_some());
        assert_eq!(rig.daemon.trains["borsuk"].in_flight, None);
        assert_eq!(rig.job_count(), 0);
    }

    #[test]
    fn a_needs_human_answer_comments_then_unlabels() {
        let steps = vec![
            gh_step(
                &[
                    "api",
                    "-X",
                    "POST",
                    "repos/acme/borsuk/issues/9/comments",
                    "-f",
                    "body=done",
                ],
                CmdOut::ok(""),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/9/labels/needs-human",
                ],
                gh_ok(),
            ),
            gh_step(
                &[
                    "api",
                    "-X",
                    "POST",
                    "repos/acme/borsuk/issues/10/comments",
                    "-f",
                    "body=hmm",
                ],
                refused(),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/10/labels/needs-human",
                ],
                gh_ok(),
            ),
        ];
        let mut rig = Rig::make(steps);
        rig.poll(
            vec![issue(9, &["needs-human"]), issue(10, &["needs-human"])],
            vec![],
        );
        assert!(rig.decision("human:borsuk:i9").is_some());
        assert!(rig.decision("human:borsuk:i10").is_some());

        rig.act(Action::Answer {
            decision_id: "human:borsuk:i9".to_string(),
            response: Response::Text {
                text: "done".to_string(),
            },
        });
        assert!(
            rig.decision("human:borsuk:i9").is_none(),
            "the resolved row closes"
        );

        rig.act(Action::Answer {
            decision_id: "human:borsuk:i10".to_string(),
            response: Response::Text {
                text: "hmm".to_string(),
            },
        });
        assert!(
            rig.decision("human:borsuk:i10").is_some(),
            "a failed answer goes back to the human"
        );
        assert_eq!(
            rig.exec.calls().len(),
            3,
            "the failed resolve never removes the label"
        );

        rig.act(Action::Answer {
            decision_id: "human:borsuk:i10".to_string(),
            response: Response::Cancel,
        });
        assert_eq!(
            rig.exec.calls().len(),
            4,
            "cancel removes the label without a comment"
        );
        assert!(rig.decision("human:borsuk:i10").is_none());
    }

    #[test]
    fn a_failed_session_answer_keeps_the_decision_open() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().yolo = false;
        });
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "req-1".to_string(),
            tool: "Bash".to_string(),
            input: json!({"command": "ls"}),
            suggestions: serde_json::Value::Null,
            needs_human: false,
        });
        let session = rig.session(0);
        session.fail_answer.store(true, Ordering::SeqCst);

        rig.act(Action::Answer {
            decision_id: "perm:borsuk/refine-i142:req-1".to_string(),
            response: Response::Allow,
        });

        assert!(rig.decision("perm:borsuk/refine-i142:req-1").is_some());
        assert!(session.answers.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_chat_does_not_extend_the_idle_deadline() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));
        let session = rig.session(0);
        session.fail_send.store(true, Ordering::SeqCst);
        rig.set_now(T0 + 100);

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "hello".to_string(),
        });

        assert_eq!(
            rig.daemon.last_event_ms["borsuk/refine-i142"], T0,
            "a rejected message is not session activity"
        );
    }

    #[test]
    fn prompt_rendering_fills_every_placeholder_and_reports_an_unknown_one() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(
            rig.prompts.join("implement.md"),
            "r={repo} o={owner_repo} n={number} t={title} b={body} w={worktree} pl={pr_list} pn={pr_numbers} pc={pr_count}",
        )
        .unwrap();
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        let expected = format!(
            "r=borsuk o=acme/borsuk n=142 t=issue 142 b=body 142 w={} pl= pn= pc=0",
            issue_wt(&dir, 142).display()
        );
        assert_eq!(rig.job(0).prompt, expected);

        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 143),
            143,
            &rig_gitdir(&dir),
        );
        let mut second = Rig::make_in(dir, steps, |_| {});
        fs::create_dir_all(&second.prompts).unwrap();
        fs::write(second.prompts.join("implement.md"), "hello {frobnicate}").unwrap();
        second.poll(vec![issue(143, &["refined"])], vec![]);
        assert_eq!(
            second.job_count(),
            0,
            "an unknown placeholder blocks the dispatch"
        );
        let task = second.task("borsuk/implement-i143");
        assert_eq!(task.attempt, 2, "the failed dispatch requeues the task");
        assert_eq!(task.state, TaskState::Queued);
    }

    #[test]
    fn ticket_create_starts_one_ticket_session() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::TicketCreate {
            repo: "borsuk".to_string(),
        });
        assert_eq!(rig.job_count(), 1);
        let job = rig.job(0);
        assert_eq!(job.task, "borsuk/refine-i0");
        assert_eq!(job.cwd, rig.repo);
        assert!(job.prompt.contains("gh issue create"));
        assert!(job.prompt.contains("borsuk"));
        assert!(!job.prompt.contains('{'));

        rig.act(Action::TicketCreate {
            repo: "borsuk".to_string(),
        });
        rig.drive();
        assert_eq!(
            rig.job_count(),
            1,
            "a live ticket session is never duplicated"
        );
    }

    #[test]
    fn unknown_repository_actions_change_no_domain_state() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::Refine {
            repo: "missing".to_string(),
            kind: ItemKind::Issue,
            number: 1,
        });
        rig.act(Action::TicketCreate {
            repo: "missing".to_string(),
        });
        rig.act(Action::Policy {
            repo: "missing".to_string(),
            policy: ReleasePolicy::Threshold { count: 2 },
        });
        rig.act(Action::Lane {
            stage: Stage::Implement,
            repo: "missing".to_string(),
            slots: 1,
        });
        rig.act(Action::Pause {
            scope: PauseScope::Repo {
                repo: "missing".to_string(),
            },
            paused: true,
        });

        assert!(rig.daemon.table.by_id.is_empty());
        assert!(!rig.daemon.policies.contains_key("missing"));
        assert!(!rig
            .daemon
            .limits
            .lanes
            .contains_key(&(Stage::Implement, "missing".to_string())));
        assert!(!rig.daemon.paused.repos.contains("missing"));
    }

    #[test]
    fn invalid_runtime_limits_and_policies_are_refused() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::Limit {
            stage: Stage::Refine,
            limit: 0,
        });
        rig.act(Action::Policy {
            repo: "borsuk".to_string(),
            policy: ReleasePolicy::Threshold { count: 0 },
        });

        assert_eq!(rig.daemon.limits.limit(Stage::Refine), 2);
        assert_eq!(rig.daemon.active_policy("borsuk"), &ReleasePolicy::Manual);
    }

    #[test]
    fn zero_removes_an_absent_lane_without_persisting_an_override() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});

        rig.act(Action::Lane {
            stage: Stage::Implement,
            repo: "borsuk".to_string(),
            slots: 0,
        });

        assert!(!rig
            .daemon
            .limits
            .lanes
            .contains_key(&(Stage::Implement, "borsuk".to_string())));
        let stored = DaemonState::load(&dir.join("state").join("state.json"));
        assert!(stored.lanes.is_empty());
    }

    #[test]
    fn a_stuck_task_retries_from_attempt_one_and_an_abort_cancels() {
        let dir = temp_root();
        let wt = issue_wt(&dir, 144);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &wt, 144, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(144, &["refined"])], vec![]);
        for _ in 0..3 {
            rig.event(exited("borsuk/implement-i144", false, "boom"));
        }
        assert!(rig.decision("stuck:borsuk/implement-i144:3").is_some());

        rig.act(Action::Retry {
            task: "borsuk/implement-i144".to_string(),
        });
        let task = rig.task("borsuk/implement-i144");
        assert_eq!(task.attempt, 1, "a retry starts a fresh attempt count");
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(rig.job_count(), 4);
        assert!(rig.decision("stuck:borsuk/implement-i144:3").is_none());

        rig.act(Action::Abort {
            task: "borsuk/implement-i144".to_string(),
        });
        assert!(rig.session(3).stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/implement-i144").state,
            TaskState::Failed("cancelled".to_string())
        );
        assert!(rig.decision("stuck:borsuk/implement-i144:3").is_none());
    }

    #[test]
    fn a_manual_release_retry_keeps_the_exact_batch() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = train_wt(&dir);
        let gitdir = rig_gitdir(&dir);
        let steps: Vec<Step> = fresh_train_steps(&repo, &worktree, &gitdir)
            .into_iter()
            .chain(reuse_train_steps(&repo, &worktree, &gitdir))
            .chain(reuse_train_steps(&repo, &worktree, &gitdir))
            .chain(reuse_train_steps(&repo, &worktree, &gitdir))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        assert!(rig.job(0).prompt.contains("#2"));

        rig.event(exited("borsuk/release-p2", false, "first"));
        assert_eq!(rig.task("borsuk/release-p2").attempt, 2);
        assert!(rig.job(1).prompt.contains("#2"));
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release-p2")
        );
        assert!(rig.decision("gate:borsuk").is_none());

        rig.event(exited("borsuk/release-p2", false, "second"));
        assert_eq!(rig.task("borsuk/release-p2").attempt, 3);
        assert!(rig.job(2).prompt.contains("#2"));

        rig.event(exited("borsuk/release-p2", false, "third"));
        assert_eq!(rig.job_count(), 3);
        assert!(rig.decision("stuck:borsuk/release-p2:3").is_some());
        assert!(rig.decision("gate:borsuk").is_none());

        rig.act(Action::Answer {
            decision_id: "stuck:borsuk/release-p2:3".to_string(),
            response: Response::Retry,
        });
        assert_eq!(rig.job_count(), 4);
        assert!(rig.job(3).prompt.contains("#2"));
        assert_eq!(rig.task("borsuk/release-p2").attempt, 1);
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release-p2")
        );
    }

    #[test]
    fn stop_shuts_the_loop_down_and_drive_becomes_a_no_op() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::Stop);
        assert!(rig.daemon.stopping());
        rig.drive();
        assert_eq!(rig.job_count(), 0);
    }

    #[test]
    fn go_fires_the_train_and_takes_the_gate_row() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        assert_eq!(rig.job_count(), 0, "a manual policy never fires alone");
        assert!(rig.decision("gate:borsuk").is_some());

        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).stage, Stage::Release);
        assert!(rig.job(0).prompt.contains("#2"));
        assert!(
            rig.decision("gate:borsuk").is_none(),
            "the answered row is taken"
        );
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release-p2")
        );
    }

    #[test]
    fn overrides_persist_across_a_restart() {
        let dir = temp_root();
        let lane_config = |config: &mut Config| {
            config
                .repos
                .get_mut("borsuk")
                .unwrap()
                .lanes
                .insert(Stage::Implement, 1);
        };
        let mut first = Rig::make_in(dir.clone(), vec![], lane_config);
        first.act(Action::Limit {
            stage: Stage::Refine,
            limit: 4,
        });
        first.act(Action::Lane {
            stage: Stage::Implement,
            repo: "borsuk".to_string(),
            slots: 0,
        });
        first.act(Action::Policy {
            repo: "borsuk".to_string(),
            policy: ReleasePolicy::Interval { minutes: 45 },
        });

        let path = dir.join("state").join("state.json");
        let stored = DaemonState::load(&path);
        assert_eq!(stored.stage_limits.get(&Stage::Refine), Some(&4));
        assert!(stored
            .lanes
            .iter()
            .any(|(stage, repo, slots)| *stage == Stage::Implement
                && repo == "borsuk"
                && *slots == 0));
        assert_eq!(
            stored.policies.get("borsuk"),
            Some(&ReleasePolicy::Interval { minutes: 45 })
        );

        first.act(Action::Limit {
            stage: Stage::Refine,
            limit: 2,
        });
        assert_eq!(
            first.daemon.limits.limit(Stage::Refine),
            2,
            "the config value remains the effective limit"
        );
        let stored = DaemonState::load(&path);
        assert!(
            stored.stage_limits.is_empty(),
            "an override back at the config value is not stored"
        );
        drop(first);

        let second = Rig::make_in(dir, vec![], lane_config);
        // from_config seeds every config limit; the empty stored.stage_limits above
        // is the proof that the override itself did not survive as an override.
        assert_eq!(second.daemon.limits.stage.get(&Stage::Refine), Some(&2));
        assert_eq!(
            second
                .daemon
                .limits
                .lanes
                .get(&(Stage::Implement, "borsuk".to_string())),
            Some(&0),
            "a zero lane reservation survives a restart"
        );
        assert_eq!(
            second.daemon.policies.get("borsuk"),
            Some(&ReleasePolicy::Interval { minutes: 45 })
        );
    }

    #[test]
    fn a_failed_state_write_retries_after_the_path_recovers() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});
        let path = dir.join("state").join("state.json");
        fs::create_dir_all(&path).unwrap();

        rig.act(Action::Limit {
            stage: Stage::Refine,
            limit: 4,
        });
        assert!(path.is_dir(), "the first state write fails at rename");

        fs::remove_dir(&path).unwrap();
        rig.drive();

        assert!(path.is_file(), "a later drive retries the state write");
        assert_eq!(
            DaemonState::load(&path).stage_limits.get(&Stage::Refine),
            Some(&4)
        );
    }

    #[test]
    fn removing_an_issue_cancels_its_running_tasks() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job_count(), 1);

        rig.poll(vec![], vec![]);
        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the vanished issue stops the live session"
        );
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("cancelled".to_string())
        );
    }

    #[test]
    fn an_unrelated_poll_change_does_not_cancel_a_draft_review() {
        let dir = temp_root();
        let steps = fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let session = rig.session(0);

        rig.poll(vec![issue(9, &[])], vec![pr(5, true, &[])]);

        assert!(!session.stopped.load(Ordering::SeqCst));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Running);
        assert_eq!(rig.job_count(), 1);
    }

    #[test]
    fn a_closed_implement_gate_cancels_its_running_task() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        let session = rig.session(0);

        rig.poll(vec![issue(142, &[])], vec![]);

        assert!(session.stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("cancelled".to_string())
        );
    }

    #[test]
    fn making_a_pull_request_ready_cancels_its_draft_review() {
        let dir = temp_root();
        let steps = fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let session = rig.session(0);

        rig.poll(vec![], vec![pr(5, false, &[])]);

        assert!(session.stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Failed("cancelled".to_string())
        );
    }

    #[test]
    fn a_new_head_replaces_only_the_superseded_review() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let first = rig.session(0);
        let mut updated = pr(5, true, &[]);
        updated.head_sha = "new-sha".to_string();

        rig.poll(vec![], vec![updated]);

        assert!(first.stopped.load(Ordering::SeqCst));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(
            rig.task("borsuk/review-p5").head_sha.as_deref(),
            Some("new-sha")
        );
    }

    #[test]
    fn a_claude_turn_completes_a_one_shot_task_before_process_exit() {
        let dir = temp_root();
        let steps: Vec<Step> = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        )
        .into_iter()
        .chain(fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 143),
            143,
            &rig_gitdir(&dir),
        ))
        .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(
            vec![issue(142, &["refined"]), issue(143, &["refined"])],
            vec![],
        );
        assert_eq!(rig.job_count(), 1);

        rig.event(turn_finished("borsuk/implement-i142", true, "done"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(
            rig.job_count(),
            1,
            "the live process keeps its stage slot until Exit"
        );

        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.job_count(), 2);
    }

    #[test]
    fn an_opencode_step_does_not_complete_its_task() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.stages.get_mut(&Stage::Implement).unwrap().runner = "opencode".to_string();
        });
        rig.poll(vec![issue(142, &["refined"])], vec![]);

        rig.event(turn_finished("borsuk/implement-i142", true, "one step"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
    }

    // ------------------------------------------------------------------
    // Template helpers
    // ------------------------------------------------------------------

    #[test]
    fn scan_placeholders_finds_placeholder_shaped_tokens() {
        assert_eq!(scan_placeholders("{a} x {b_1} {a}"), vec!["a", "b_1"]);
        assert!(scan_placeholders("{not a} {} {unclosed {oops}").is_empty());
    }

    #[test]
    fn fill_template_rejects_an_unknown_placeholder_and_fills_known_ones() {
        let error = fill_template("hi {name} {other}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("other"));
        let filled = fill_template("hi {name}", &[("name", "x".to_string())]).unwrap();
        assert_eq!(filled, "hi x");

        let error = fill_template("hi {not-known}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("not-known"));

        let filled = fill_template(
            "title={title}; body={body}",
            &[
                ("title", "keep {body} literal".to_string()),
                ("body", "body text".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(filled, "title=keep {body} literal; body=body text");
    }

    // Path helpers that mirror the rig layout.
    fn rig_repo(dir: &Path) -> PathBuf {
        dir.join("repo")
    }

    fn rig_gitdir(dir: &Path) -> PathBuf {
        dir.join("git-common")
    }
}
