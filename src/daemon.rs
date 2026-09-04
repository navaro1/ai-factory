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
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{
    self, Config, ExecutionRole, ReleasePolicy, RepoConfig, ResolvedRoleSettings, SettingsEdit,
};
use crate::decisions::{self, Decision, DecisionKind, Decisions, Response};
use crate::exec::{Exec, RealExec};
use crate::gates::{implement_ready, review_ready, GateTracker, ReadyWork};
use crate::gh::GhClient;
use crate::links::Links;
use crate::model::{ItemKind, RepoSnapshot, Snapshot, Stage};
use crate::poll::DaemonMsg;
use crate::prompts::{
    IMPLEMENT_PROMPT, REFINE_PROMPT, RELEASE_PROMPT, RESTART_NOTICE, REVIEW_PROMPT,
    TICKET_CHAT_PROMPT, TICKET_PROMPT,
};
#[cfg(test)]
use crate::runner::Runner;
use crate::runner::{
    capabilities, Answer, DefaultRunnerFactory, Job, RunEvent, RunnerFactory, Session,
};
use crate::sched::{self, Limits, Paused, Verdict};
use crate::sock::{
    Action, AskView, InputMode, PauseScope, Push, SettingsOperation, SettingsResult,
    SettingsResultStatus, StateInput, StateView, TicketAction, TicketDetails, TicketProposal,
};
use crate::state::{DaemonState, RuntimeState, TicketConversationState};
use crate::tasks::{self, Task, TaskPurpose, TaskState, TaskTable};
use crate::ticket::TicketController;
use crate::trains::{Train, STACKED_LABEL};
use crate::worktree::{WorktreeKind, WorktreeManager, TRAIN_DIR};

/// The label that asks a human to decide something on GitHub.
pub const NEEDS_HUMAN_LABEL: &str = "needs-human";

/// How long a parked session stays alive without activity before the reaper
/// stops its process.
///
/// The task of a reaped session stays `AwaitingUser` and a chat message
/// resumes it later with a fresh process.
pub const DEFAULT_IDLE_REAP_MS: u64 = 30 * 60_000;

/// How long the shutdown sequence waits for the agent sessions to report
/// their exit.
///
/// The value covers the full stop ladder of `src/proc.rs`: 10 s after the
/// protocol interrupt, 5 s after `SIGTERM`, and 5 s after `SIGKILL`.
pub const SHUTDOWN_GRACE_MS: u64 = 25_000;

/// The one message sent for each new ticket refinement interval.
pub const TICKET_REFINEMENT_MESSAGE: &str =
    "The issue now has the to-refine label. Continue the refinement analysis in this session.";

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
    Act(Box<Action>),
}

/// The assistant text data needed for one strict proposal parse.
#[derive(Debug, Default)]
struct TicketTurnText {
    /// The last complete assistant text event of the turn.
    last: String,
    /// True when an earlier event contained proposal marker text.
    earlier_marker: bool,
}

/// The working directory identity of one task.
///
/// `Shared` is the repository checkout. The refine stage, the ticket
/// session, and the ticket chat all work there, and none of them owns it.
/// `Exclusive` is a private git worktree. One task at a time may run in it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Workspace {
    Shared,
    Exclusive(WorktreeKey),
}

/// The key of one exclusive worktree of one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeKey {
    Issue(u64),
    Pr(u64),
    Train,
}

impl fmt::Display for WorktreeKey {
    /// The directory name of the worktree, as the manager writes it on
    /// disk. The names come from the worktree module, so the refusal text
    /// and the directory cannot drift apart.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeKey::Issue(number) => write!(f, "{}{number}", WorktreeKind::Issue.prefix()),
            WorktreeKey::Pr(number) => write!(f, "{}{number}", WorktreeKind::Pr.prefix()),
            WorktreeKey::Train => f.write_str(TRAIN_DIR),
        }
    }
}

/// The daemon: every module assembled into one event loop.
pub struct Daemon {
    /// The parsed factory configuration.
    config: Config,
    /// The absolute `factory.toml` path that this daemon owns.
    config_path: PathBuf,
    /// The content revision of the active factory configuration.
    settings_revision: String,
    /// A test hook that changes the destination after temporary-file preparation.
    #[cfg(test)]
    before_config_commit: Option<Box<dyn FnMut() + Send>>,
    /// The command runner; tests replace it with a scripted double.
    exec: Arc<dyn Exec>,
    /// The adapter factory for resolved execution roles.
    runner_factory: Arc<dyn RunnerFactory>,
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
    /// Repositories whose last poll must refresh a train's stacked cache.
    pending_stacked: BTreeSet<String>,
    /// All tasks, in insertion order.
    table: TaskTable,
    /// The immutable resolved role of each task that started at least once.
    role_bindings: BTreeMap<String, ResolvedRoleSettings>,
    /// The decisions that wait for a human.
    decisions: Decisions,
    /// One release train per repository alias.
    trains: BTreeMap<String, Train>,
    /// The pull request set of each release task, for prompt rendering.
    release_batches: BTreeMap<String, Vec<u64>>,
    /// The ticket-PR links of each repository, rebuilt on every poll.
    links: BTreeMap<String, Links>,
    /// The source issue worktree of each review task.
    /// The ticket set of each review task, pinned at admit time. The
    /// supersede check compares it against the fresh poll.
    review_tickets: BTreeMap<String, BTreeSet<u64>>,
    /// The controller for every issue review and mutation action.
    ticket_controller: TicketController,
    /// Active issue conversations, keyed by repository and issue number.
    ticket_conversations: BTreeMap<(String, u64), TicketConversationState>,
    /// The final-text candidate of each active ticket turn.
    ticket_turn_text: BTreeMap<String, TicketTurnText>,

    /// One live session per running or parked task.
    sessions: BTreeMap<String, Box<dyn Session>>,
    /// Tasks whose old process is stopping and still owes an exit event.
    /// The stage keeps each process inside its live-process limit.
    stopping_sessions: BTreeMap<String, Stage>,
    /// Chat messages that wait for a stopped process or a live-process slot.
    pending_chats: BTreeMap<String, Vec<String>>,
    /// The time of the last event of each task, for the idle reaper.
    last_event_ms: BTreeMap<String, u64>,
    /// The tasks the snapshot restored from state `running`. Their first
    /// dispatch carries the restart notice.
    interrupted: BTreeSet<String>,
    /// The task ids the snapshot restored. The first poll of a repository
    /// reconciles them against GitHub.
    restored_ids: BTreeSet<String>,
    /// The repositories that still owe their first restore reconcile.
    restore_repos: BTreeSet<String>,

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
    /// How long the shutdown sequence waits for the session exits. Tests
    /// lower the value; the daemon starts at [`SHUTDOWN_GRACE_MS`].
    shutdown_grace_ms: u64,
    /// The serialized state of the last write, so an unchanged drive writes
    /// nothing.
    saved: Option<String>,
    /// The pusher that receives the state views. The binary wires it to
    /// `Server::publish`, and tests wire it to a channel. None until
    /// `set_pusher` runs, so an early view goes nowhere.
    pusher: Option<Box<dyn Fn(StateView) + Send>>,
    /// The pusher for ticket detail, label, and result messages.
    ticket_pusher: Option<Box<dyn Fn(Push) + Send>>,

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
    /// Each execution role selects a configured runner. A repository can
    /// override the global role settings. A true `paused` starts the whole
    /// factory paused. The daemon polls and reports, but dispatches nothing
    /// until the operator resumes.
    // The revision must travel with the parsed bytes. The remaining values
    // are daemon-owned dependencies and cannot be derived from that pair.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        config_path: PathBuf,
        settings_revision: String,
        prompts_dir: PathBuf,
        poll_rx: Receiver<DaemonMsg>,
        wake: BTreeMap<String, Sender<()>>,
        action_rx: Receiver<Action>,
        paused: bool,
    ) -> Result<Self> {
        let state_dir = config::state_dir();
        let exec: Arc<dyn Exec> = Arc::new(RealExec);
        let worktrees = WorktreeManager::new(state_dir.clone());
        for repo in config.repos.values() {
            worktrees
                .prepare_checkout(&*exec, &repo.path)
                .with_context(|| format!("cannot prepare repository {}", repo.alias))?;
        }
        Ok(Self::with_runner_factory(
            config,
            config_path,
            settings_revision,
            exec,
            state_dir,
            prompts_dir,
            poll_rx,
            wake,
            action_rx,
            Arc::new(DefaultRunnerFactory),
            paused,
        ))
    }

    /// Build a daemon over injected runners and a scripted command runner.
    ///
    /// The constructor restores `last_fire_ms` from `state.json` and applies
    /// the stored overrides before the first drive, so an interval policy
    /// never releases again just because the daemon restarted. It also
    /// restores the `runtime` object before the first drive: the pause
    /// marks, the task table, the queued chats, the review ticket sets, the
    /// release batches, and the stuck rows. A task that the snapshot holds
    /// as `running` becomes `queued` again, keeps its attempt count and
    /// session id, and its first dispatch carries the restart notice. A
    /// true `paused` sets `Paused.global` on top of the restored marks, so
    /// a start-paused factory dispatches nothing until the operator
    /// resumes.
    // Each argument is one daemon-owned dependency. A bundle would hide the
    // ownership boundary without reducing it.
    #[allow(clippy::too_many_arguments)]
    pub fn with_runner_factory(
        config: Config,
        config_path: PathBuf,
        settings_revision: String,
        exec: Arc<dyn Exec>,
        state_dir: PathBuf,
        prompts_dir: PathBuf,
        poll_rx: Receiver<DaemonMsg>,
        wake: BTreeMap<String, Sender<()>>,
        action_rx: Receiver<Action>,
        runner_factory: Arc<dyn RunnerFactory>,
        paused: bool,
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
        let role_bindings = stored.role_bindings;
        let ticket_conversations = stored
            .ticket_conversations
            .into_iter()
            .filter(|conversation| config.repos.contains_key(&conversation.repo))
            .map(|conversation| {
                (
                    (conversation.repo.clone(), conversation.number),
                    conversation,
                )
            })
            .collect();

        // The restore contract: the runtime object lands in the daemon
        // before the first drive, so the restored marks, tasks, and rows
        // behave like live state. A task that ran at save time becomes
        // queued again and keeps its attempt count and session id; the
        // first accepted poll validates each restored task before dispatch.
        let restored_at = now_ms();
        let start_paused = paused;
        let runtime = stored.runtime;
        let mut restored_table = TaskTable::new();
        let mut interrupted = BTreeSet::new();
        let mut restored_ids: BTreeSet<String> = BTreeSet::new();
        let mut restore_repos: BTreeSet<String> = BTreeSet::new();
        for mut task in runtime.tasks {
            if !config.repos.contains_key(&task.repo) {
                continue;
            }
            if task.state == TaskState::Running {
                task.state = TaskState::Queued;
                task.updated_ms = restored_at;
                interrupted.insert(task.id.clone());
            }
            restored_ids.insert(task.id.clone());
            restore_repos.insert(task.repo.clone());
            let id = task.id.clone();
            restored_table.by_id.insert(id.clone(), task);
            restored_table.order.push(id);
        }
        let restored_task_ids: BTreeSet<&str> =
            restored_table.by_id.keys().map(String::as_str).collect();
        let pending_chats: BTreeMap<String, Vec<String>> = runtime
            .pending_chats
            .into_iter()
            .filter(|(id, _)| restored_task_ids.contains(id.as_str()))
            .collect();
        let review_tickets = runtime
            .review_tickets
            .into_iter()
            .filter(|(id, _)| restored_task_ids.contains(id.as_str()))
            .collect();
        let release_batches: BTreeMap<String, Vec<u64>> = runtime
            .release_batches
            .into_iter()
            .filter(|(id, _)| restored_task_ids.contains(id.as_str()))
            .collect();
        let mut decisions = Decisions::new();
        for row in runtime.stuck {
            let names_kept_task = match &row.kind {
                DecisionKind::Stuck { task, .. } => restored_task_ids.contains(task.as_str()),
                _ => false,
            };
            if names_kept_task {
                decisions.push(row);
            }
        }
        let mut paused = Paused {
            global: runtime.paused.global,
            stages: runtime.paused.stages,
            lanes: runtime
                .paused
                .lanes
                .into_iter()
                .filter(|entry| config.repos.contains_key(&entry.repo))
                .map(|entry| ((entry.stage, entry.repo), entry.paused))
                .collect(),
            tasks: runtime
                .paused
                .tasks
                .into_iter()
                .filter(|(id, _)| restored_task_ids.contains(id.as_str()))
                .collect(),
        };
        if start_paused {
            paused.set_global(true);
        }

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
        // A restored release task re-links to its train, so its batch
        // behaves like an active train: no second fire, and one exact
        // finish.
        for (repo, train) in trains.iter_mut() {
            let id = crate::tasks::scoped_id(repo, "release");
            if restored_task_ids.contains(id.as_str()) {
                if let Some(prs) = release_batches.get(&id) {
                    train.resume_in_flight(&id, prs);
                }
            }
        }

        let (run_tx, run_rx) = mpsc::channel();
        let ticket_controller = TicketController::new(exec.clone());
        let mut daemon = Daemon {
            config,
            config_path,
            settings_revision,
            #[cfg(test)]
            before_config_commit: None,
            exec,
            runner_factory,
            worktrees: WorktreeManager::new(state_dir.clone()),
            state_path: state_path.clone(),
            prompts_dir,
            state_dir,
            limits,
            paused,
            policies,
            snapshot: Snapshot::default(),
            gates: GateTracker::new(),
            pending_ready: Vec::new(),
            pending_stacked: BTreeSet::new(),
            table: restored_table,
            role_bindings,
            decisions,
            trains,
            release_batches,
            links: BTreeMap::new(),
            review_tickets,
            ticket_controller,
            ticket_conversations,
            ticket_turn_text: BTreeMap::new(),
            sessions: BTreeMap::new(),
            stopping_sessions: BTreeMap::new(),
            pending_chats,
            last_event_ms: BTreeMap::new(),
            interrupted,
            restored_ids,
            restore_repos,
            clock: Arc::new(now_ms),
            now_ms: 0,
            idle_reap_ms: DEFAULT_IDLE_REAP_MS,
            changed: false,
            // The first drive publishes the initial state, so a UI that
            // connects right after the start sees the state at once.
            dirty: true,
            shutdown: false,
            shutdown_grace_ms: SHUTDOWN_GRACE_MS,
            saved: None,
            pusher: None,
            ticket_pusher: None,
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
    /// with no message. After the loop breaks, the shutdown sequence stops
    /// every live agent session and writes the state file once.
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
            forwarder("aif-act", action_rx, in_tx, inbound_action)?,
        ];

        self.now_ms = (self.clock)();
        self.drive();
        loop {
            if self.shutdown {
                break;
            }
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
        self.shutdown_sequence(&in_rx);
        Ok(())
    }

    /// The one exit path after the loop breaks.
    ///
    /// The sequence stops every live session, then reads the inbound
    /// channel until `sessions` and `stopping_sessions` are both empty or
    /// [`SHUTDOWN_GRACE_MS`] passes. It reads run events only, so a poll or
    /// an operator action that arrives during the exit changes nothing, and
    /// it never dispatches new work. The session exits keep their tasks
    /// stable, because `stopping_sessions` gives that guarantee. The
    /// sequence ends with one forced write of `state.json`.
    fn shutdown_sequence(&mut self, in_rx: &Receiver<Inbound>) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        eprintln!(
            "aifd: the daemon stops; stopping {} live agent session(s)",
            ids.len()
        );
        for id in &ids {
            self.stop_session(id, "cannot stop the session during the daemon shutdown");
        }
        let deadline = Instant::now() + Duration::from_millis(self.shutdown_grace_ms);
        while !self.sessions.is_empty() || !self.stopping_sessions.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match in_rx.recv_timeout(remaining) {
                Ok(Inbound::Run(event)) => {
                    self.now_ms = (self.clock)();
                    self.on_run_event(event);
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let unreported: Vec<String> = self
            .stopping_sessions
            .keys()
            .chain(self.sessions.keys())
            .cloned()
            .collect();
        for id in &unreported {
            eprintln!("aifd: session {id} reported no exit before the deadline");
        }
        self.force_save_state();
    }

    /// Write the state file even when nothing changed.
    ///
    /// The shutdown and the operator expect the snapshot on disk whatever
    /// the write dedup says.
    fn force_save_state(&mut self) {
        let state = self.collect_state();
        match state
            .to_json()
            .and_then(|text| state.save(&self.state_path).map(|()| text))
        {
            Ok(text) => {
                self.saved = Some(text);
                eprintln!(
                    "aifd: the daemon state is saved to {}",
                    self.state_path.display()
                );
            }
            Err(error) => eprintln!("aifd: cannot save the daemon state: {error:#}"),
        }
    }

    /// Process one inbound message, then drive the factory.
    ///
    /// Tests call this directly and drive the daemon without threads.
    pub fn handle(&mut self, message: Inbound) {
        self.now_ms = (self.clock)();
        match message {
            Inbound::Poll(message) => self.on_poll(message),
            Inbound::Run(event) => self.on_run_event(event),
            Inbound::Act(action) => self.on_action(*action),
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
        self.resume_pending_chats();
        self.dispatch_queued();
        self.save_state();
        self.push_state();
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

    /// Read and clear the dirty flag. The end of every drive pass calls
    /// this before it builds and pushes the state view.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Attach the pusher that receives the state views.
    ///
    /// The binary wires the pusher to [`crate::sock::Server::publish`].
    /// The pusher never blocks and coalesces pushes itself, so the daemon
    /// hands it a view whenever the dirty flag asks and adds no throttling
    /// of its own.
    pub fn set_pusher(&mut self, pusher: Box<dyn Fn(StateView) + Send>) {
        self.pusher = Some(pusher);
    }

    /// Attach the pusher for ticket detail, label, and result messages.
    pub fn set_ticket_pusher(&mut self, pusher: Box<dyn Fn(Push) + Send>) {
        self.ticket_pusher = Some(pusher);
    }

    /// Build the state view from the live state and hand it to the pusher.
    ///
    /// The call runs at the end of every drive pass and pushes only when
    /// the pass changed something. Every field of [`StateInput`] reads one
    /// field of the daemon, so the view can only show what the loop truly
    /// holds.
    fn push_state(&mut self) {
        if !self.take_dirty() {
            return;
        }
        let input_modes: BTreeMap<String, InputMode> = self
            .table
            .by_id
            .values()
            .map(|task| (task.id.clone(), self.input_mode(task)))
            .collect();
        let input = StateInput {
            config: &self.config,
            settings_revision: &self.settings_revision,
            limits: &self.limits,
            paused: &self.paused,
            table: &self.table,
            decisions: &self.decisions,
            snapshot: &self.snapshot,
            links: &self.links,
            trains: &self.trains,
            policies: &self.policies,
            input_modes: &input_modes,
            now_ms: self.now_ms,
        };
        let mut view = match input.build() {
            Ok(view) => view,
            Err(error) => {
                eprintln!("cannot build the state view: {error:#}");
                return;
            }
        };
        for task in &mut view.tasks {
            task.queued_messages = self.pending_chats.get(&task.id).map_or(0, Vec::len);
        }
        if let Some(pusher) = self.pusher.as_ref() {
            pusher(view);
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
            DaemonMsg::Polled {
                started_ms,
                repo,
                snapshot,
            } => self.apply_poll(&repo, snapshot, started_ms),
        }
    }

    /// Store a fresh snapshot and derive everything GitHub drives.
    ///
    /// A poll that repeats the previous snapshot marks nothing changed, so
    /// it writes no state and raises no dirty flag.
    fn apply_poll(&mut self, repo: &str, fresh: RepoSnapshot, started_ms: u64) {
        if self
            .ticket_controller
            .last_mutation_ms(repo)
            .is_some_and(|confirmed_ms| started_ms <= confirmed_ms)
        {
            return;
        }
        if self.restore_repos.remove(repo) {
            self.cancel_absent_restored(repo, &fresh);
        }
        let old = self.snapshot.repos.get(repo).cloned();
        let unchanged = old.as_ref() == Some(&fresh);
        self.snapshot.apply(repo, fresh.clone());
        self.links
            .insert(repo.to_string(), Links::derive(repo, &fresh));
        self.pending_stacked.insert(repo.to_string());
        if let Some(old) = old.filter(|_| !unchanged) {
            self.reconcile_removed(repo, &old, &fresh);
        }
        self.retire_absent_items(repo, &fresh);
        self.complete_parked_refines(repo, &fresh);
        if !unchanged {
            self.reconcile_unready(repo, &fresh);
        }
        self.reconcile_ticket_conversations(repo, &fresh);
        let mut changed = self.observe_ready_work(repo, &fresh);
        changed |= self.derive_needs_human(repo, &fresh);
        if !unchanged {
            changed = true;
        }
        if changed {
            self.changed = true;
        }
    }

    /// Cancel restored active tasks whose item is absent from the first
    /// poll of their repository.
    ///
    /// GitHub is the source of truth. The restore happened without a
    /// snapshot, so a task whose issue or pull request closed while the
    /// daemon was down only meets its end here. Release tasks stay out:
    /// the train, not one pull request, is their unit. Ticket sessions
    /// carry item number 0 and stay out for the same reason. The cancel
    /// stops the live session; [`Daemon::retire_absent_items`] then drops
    /// the task in the same poll.
    fn cancel_absent_restored(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let ids: Vec<String> = self
            .table
            .active()
            .iter()
            .filter(|task| task.repo == repo && self.restored_ids.contains(&task.id))
            .filter(|task| task.stage != Stage::Release && task.number != TICKET_NUMBER)
            .filter(|task| match task.kind {
                ItemKind::Issue => !fresh.issues.contains_key(&task.number),
                ItemKind::Pr => !fresh.prs.contains_key(&task.number),
            })
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.cancel_task(&id, false);
        }
    }

    /// Observe pipeline gates and reserve issue refinement for ticket chat.
    fn observe_ready_work(&mut self, repo: &str, fresh: &RepoSnapshot) -> bool {
        let ready: Vec<ReadyWork> = self
            .gates
            .observe(repo, fresh)
            .into_iter()
            .filter(|work| {
                !(work.stage == Stage::Refine
                    && work.kind == ItemKind::Issue
                    && self
                        .ticket_conversations
                        .contains_key(&(work.repo.clone(), work.number)))
            })
            .collect();
        let changed = !ready.is_empty();
        self.pending_ready.extend(ready);
        changed
    }

    /// Retire work whose item closed, went back to draft, or vanished.
    ///
    /// GitHub is the source of truth: a gone or draft pull request leaves the
    /// train, and a gone or closed item cancels its active tasks and drops
    /// its terminal tasks, so the board keeps no stale row.
    fn reconcile_removed(&mut self, repo: &str, old: &RepoSnapshot, fresh: &RepoSnapshot) {
        for number in old.issues.keys() {
            let gone = fresh.issues.get(number).is_none_or(|issue| !issue.open);
            if gone {
                self.gates.forget(repo, ItemKind::Issue, *number);
                self.cancel_item_tasks(repo, ItemKind::Issue, *number);
                self.retire_item_tasks(repo, ItemKind::Issue, *number);
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
                self.retire_item_tasks(repo, ItemKind::Pr, *number);
            }
        }
    }

    /// Cancel every active task of one item and stop its live session.
    fn cancel_item_tasks(&mut self, repo: &str, kind: ItemKind, number: u64) {
        let ticket_conversation = self
            .ticket_conversations
            .contains_key(&(repo.to_string(), number));
        let ids: Vec<String> = self
            .table
            .active()
            .iter()
            .filter(|task| task.repo == repo && task.kind == kind && task.number == number)
            .filter(|task| task.purpose != TaskPurpose::TicketChat || !ticket_conversation)
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.cancel_task(&id, false);
        }
    }

    /// Drop one task and every per-task record it owns.
    ///
    /// The daemon removes the tasks of an item that left GitHub instead of
    /// keeping them for ever. A missing id is a no-operation.
    fn retire_task(&mut self, id: &str) {
        if self.table.remove(id).is_none() {
            return;
        }
        self.role_bindings.remove(id);
        self.review_tickets.remove(id);
        self.release_batches.remove(id);
        self.pending_chats.remove(id);
        self.ticket_turn_text.remove(id);
        self.paused.tasks.remove(id);
        self.decisions.drop_for_task(id);
        self.changed = true;
    }

    /// True when a release train still needs this task.
    ///
    /// A train in flight needs its task, so [`Daemon::reconcile_trains`]
    /// can close the train. A failed train needs its task too: the train
    /// saved the exact batch to retry, and [`Daemon::retry_task`] reads
    /// that batch through the task id. The release merges one pull request
    /// at a time, so a failed batch often leaves GitHub in part. Without
    /// this guard the retire drops the task the moment the lowest pull
    /// request of the batch merges, and a `Manual` train can never retry.
    /// The board applies the same rule: it draws the task inside the retry
    /// border while the batch is not empty.
    fn train_needs_task(&self, task: &Task) -> bool {
        self.trains.values().any(|train| {
            train.in_flight.as_deref() == Some(task.id.as_str())
                || (train.repo == task.repo
                    && task.stage == Stage::Release
                    && !train.batch().is_empty())
        })
    }

    /// Drop every pipeline task of one item that left GitHub.
    ///
    /// A ticket chat and a ticket-creation session serve the Tickets view,
    /// so their purpose keeps them. A release task that its train still
    /// needs stays too, by [`Daemon::train_needs_task`].
    fn retire_item_tasks(&mut self, repo: &str, kind: ItemKind, number: u64) {
        let ids: Vec<String> = self
            .table
            .by_id
            .values()
            .filter(|task| task.repo == repo && task.kind == kind && task.number == number)
            .filter(|task| task.purpose == TaskPurpose::Pipeline)
            .filter(|task| !self.train_needs_task(task))
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.retire_task(&id);
        }
    }

    /// Drop every terminal pipeline task whose item is no longer open.
    ///
    /// [`Daemon::reconcile_removed`] compares two snapshots, so it sees only
    /// the poll that watches an item leave. The daemon saves no snapshot, so
    /// a restart forgets every earlier item. A pull request merged while the
    /// daemon was down therefore never meets that path, and its done task
    /// keeps a stale board row for ever. This sweep needs no earlier
    /// snapshot: it reads the fresh one alone, so it also clears the rows a
    /// restart carried over. It runs on every poll and repeats without
    /// effect.
    ///
    /// The sweep drops no active task. A live session ends through
    /// [`Daemon::cancel_absent_restored`] or [`Daemon::cancel_item_tasks`],
    /// which run first and make the task terminal. The purpose keeps a
    /// ticket chat and a ticket-creation session, and
    /// [`Daemon::train_needs_task`] keeps the release task that a train
    /// still needs.
    fn retire_absent_items(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let ids: Vec<String> = self
            .table
            .by_id
            .values()
            .filter(|task| task.repo == repo && task.state.is_terminal())
            .filter(|task| task.purpose == TaskPurpose::Pipeline)
            .filter(|task| !self.train_needs_task(task))
            .filter(|task| match task.kind {
                ItemKind::Issue => fresh
                    .issues
                    .get(&task.number)
                    .is_none_or(|issue| !issue.open),
                ItemKind::Pr => fresh.prs.get(&task.number).is_none_or(|pr| !pr.open),
            })
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.retire_task(&id);
        }
    }

    /// Cancel active implement and review tasks whose gates closed.
    fn reconcile_unready(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let links = self.links.get(repo).cloned().unwrap_or_default();
        let ids: Vec<String> = self
            .table
            .active()
            .into_iter()
            .filter(|task| task.repo == repo)
            .filter(|task| match task.stage {
                Stage::Implement => {
                    fresh
                        .issues
                        .get(&task.number)
                        .is_none_or(|issue| !implement_ready(fresh, issue))
                        && !(task.state == TaskState::Running
                            && implementation_transitioned(fresh, &links, task.number))
                }
                Stage::Review => {
                    fresh
                        .prs
                        .get(&task.number)
                        .is_none_or(|pr| !review_ready(pr))
                        && !(task.state == TaskState::Running
                            && review_transitioned(fresh, task.number))
                }
                Stage::Refine | Stage::Release => false,
            })
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.cancel_task(&id, false);
        }
    }

    /// Complete parked refine tasks after GitHub reports their gate change.
    ///
    /// A poll can follow the turn end. In that order, GitHub confirms that the
    /// parked task finished its requested transition.
    fn complete_parked_refines(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let tasks: Vec<Task> = self
            .table
            .active()
            .into_iter()
            .filter(|task| task.repo == repo)
            .filter(|task| {
                task.stage == Stage::Refine
                    && task.purpose == TaskPurpose::Pipeline
                    && task.state == TaskState::AwaitingUser
                    && refine_transitioned(fresh, task.number)
            })
            .cloned()
            .collect();
        for task in tasks {
            self.stop_session(&task.id, "cannot stop the completed refine session");
            self.complete_task(&task);
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
            let review_tickets: BTreeSet<u64> = (work.stage == Stage::Review)
                .then(|| {
                    self.links
                        .get(&work.repo)
                        .map(|links| links.tickets_of(work.number).into_iter().collect())
                })
                .flatten()
                .unwrap_or_default();
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
                            && (task.head_sha != work.head_sha
                                || self
                                    .review_tickets
                                    .get(&task.id)
                                    .cloned()
                                    .unwrap_or_default()
                                    != review_tickets)
                    })
                    .map(|task| task.id.clone());
                if let Some(id) = superseded {
                    self.cancel_task(&id, false);
                }
            }
            // A task the restore already holds skips the gate quietly: the
            // restored task keeps its attempt count, and the first poll of
            // a closed item cancels it. The same quiet skip holds a failed
            // task whose stuck row waits for the operator's answer.
            let candidate_id = format!(
                "{}/{}-{}{}",
                work.repo,
                work.stage.as_str(),
                work.kind.as_str(),
                work.number
            );
            if let Some(existing) = self.table.by_id.get(&candidate_id) {
                let stuck_holds = matches!(existing.state, TaskState::Failed(_))
                    && self.decisions.open().iter().any(|row| {
                        matches!(&row.kind, DecisionKind::Stuck { task, .. } if task == &candidate_id)
                    });
                if !existing.state.is_terminal() || stuck_holds {
                    continue;
                }
            }
            let log = self.log_path(&work.repo, work.stage, work.kind, work.number);
            let replaces_task = self.table.by_id.values().any(|task| {
                task.repo == work.repo
                    && task.stage == work.stage
                    && task.kind == work.kind
                    && task.number == work.number
                    && task.state.is_terminal()
            });
            match self.table.upsert_queued(
                &work.repo,
                work.stage,
                work.kind,
                work.number,
                log,
                self.now_ms,
            ) {
                Ok(task) => {
                    if replaces_task {
                        self.role_bindings.remove(&task.id);
                    }
                    if work.stage == Stage::Review {
                        task.head_sha = work.head_sha.clone();
                        self.review_tickets.insert(task.id.clone(), review_tickets);
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
        let aliases = std::mem::take(&mut self.pending_stacked);
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
                self.fire_train(&alias, &prs, true);
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
                self.finish_train(&alias, true, false);
            } else if task.state.is_terminal() {
                let retain_batch = !matches!(
                    &task.state,
                    TaskState::Failed(reason) if reason == "cancelled"
                );
                self.finish_train(&alias, false, retain_batch);
            }
        }
    }

    /// Stop the processes of parked sessions that the factory cannot use.
    ///
    /// A parked session stops when it passed the idle limit, or when a
    /// pause blocks its task. The task stays `AwaitingUser` and a chat
    /// message resumes it later. Its live-process slot becomes free after
    /// the process reports `Exit`. A `Running` task keeps its process.
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
            let idle = self.now_ms >= last.saturating_add(self.idle_reap_ms);
            let paused = self.paused.blocks_task(task.stage, &task.repo, &task.id);
            if idle || paused {
                reaped.push(task.id.clone());
            }
        }
        for id in reaped {
            if self.sessions.contains_key(&id) {
                eprintln!("task {id}: the parked session is idle or paused; stopping it");
                self.stop_session(&id, "cannot stop the parked session");
                self.changed = true;
            }
        }
    }

    /// Stop one parked session to free a live-process slot of `stage`.
    ///
    /// The call selects the `AwaitingUser` tasks of `stage` that hold a
    /// live session, that hold no queued chat message, and that are not
    /// `waiting`. It stops the one with the oldest `last_event_ms` and
    /// reports true. The stopped task keeps `AwaitingUser` and a later
    /// chat message resumes it from its session id, exactly as after an
    /// idle reap.
    fn free_live_slot(&mut self, stage: Stage, waiting: &str) -> bool {
        let candidate = self
            .table
            .by_id
            .values()
            .filter(|task| {
                task.stage == stage
                    && task.state == TaskState::AwaitingUser
                    && task.id != waiting
                    && self.sessions.contains_key(&task.id)
                    && !self.pending_chats.contains_key(&task.id)
            })
            .min_by_key(|task| {
                self.last_event_ms
                    .get(&task.id)
                    .copied()
                    .unwrap_or(self.now_ms)
            })
            .map(|task| task.id.clone());
        let Some(id) = candidate else {
            return false;
        };
        eprintln!("task {id}: the stage needs the live slot; stopping its parked session");
        self.stop_session(&id, "cannot stop the yielding parked session");
        self.changed = true;
        true
    }

    /// Start the follow-up turns of the tasks that hold chat messages.
    ///
    /// The call owns every task with entries in `pending_chats`, and the
    /// dispatcher skips such a task. A task in `Running` keeps its messages
    /// and waits for the exit; the exit reopens the task when messages
    /// remain. A task in `Queued` or `AwaitingUser` gets one run: the first
    /// message is the prompt, and the session id continues the old
    /// conversation. The pipeline order and the scheduler decide whether the
    /// run may start, so prior stages, stage limits, lane reservations, and
    /// pauses all apply to a follow-up turn. A full live-process limit stops
    /// one yielding parked session first, and the limit check retries once.
    fn resume_pending_chats(&mut self) {
        let ids: Vec<String> = self.pending_chats.keys().cloned().collect();
        for id in ids {
            if self.stopping_sessions.contains_key(&id) || self.sessions.contains_key(&id) {
                continue;
            }
            let Some(task) = self.table.by_id.get(&id).cloned() else {
                self.pending_chats.remove(&id);
                eprintln!("the pending chat for {id}: no such task");
                continue;
            };
            if (self.restored_ids.contains(&id) && self.restore_repos.contains(&task.repo))
                || self.interrupted.contains(&id)
            {
                continue;
            }
            if task.state == TaskState::Running {
                continue;
            }
            if task.state.is_terminal() {
                self.pending_chats.remove(&id);
                eprintln!("the pending chat for {id}: the task is {}", task.state);
                continue;
            }
            if self.prior_stage_active(&task) {
                continue;
            }
            if !matches!(
                sched::can_start(
                    &self.limits,
                    &self.paused,
                    &self.table,
                    task.stage,
                    &task.repo,
                    &task.id,
                ),
                Verdict::Yes
            ) {
                continue;
            }
            let session_id = match self.followup_session_id(&task) {
                Ok(session_id) => session_id,
                Err(error) => {
                    eprintln!("the pending chat for {id}: {error:#}");
                    continue;
                }
            };
            let Some(session_id) = session_id else {
                self.pending_chats.remove(&id);
                eprintln!("the pending chat for {id}: no session id to resume");
                continue;
            };
            // Scheduler capacity counts running tasks. The separate live
            // process limit also counts parked live-input sessions. A
            // parked session that no queued message waits for yields its
            // slot here, and the check retries once.
            if self.live_sessions(task.stage) >= self.limits.limit(task.stage) {
                self.free_live_slot(task.stage, &task.id);
                if self.live_sessions(task.stage) >= self.limits.limit(task.stage) {
                    continue;
                }
            }
            let Some(messages) = self.pending_chats.remove(&id) else {
                continue;
            };
            let Some(first) = messages.first().cloned() else {
                continue;
            };
            if let Err(error) = self.launch_task(&task, first, Some(session_id)) {
                eprintln!("the pending chat for {id}: the runner could not resume: {error:#}");
                self.pending_chats.insert(id, messages);
                continue;
            }
            if !self.task_capabilities(&task).live_input {
                let leftover: Vec<String> = messages.into_iter().skip(1).collect();
                if !leftover.is_empty() {
                    self.pending_chats.insert(id, leftover);
                }
                continue;
            }
            let mut delivered = 1;
            while delivered < messages.len() {
                let result = self
                    .sessions
                    .get_mut(&id)
                    .ok_or_else(|| anyhow!("the resumed session vanished"))
                    .and_then(|session| session.send_user(&messages[delivered]));
                match result {
                    Ok(()) => delivered += 1,
                    Err(error) => {
                        eprintln!("the pending chat for {id}: {error:#}");
                        // The session refuses the rest of the turn. Keep the
                        // undelivered messages; the next turn carries them.
                        let leftover: Vec<String> = messages[delivered..].to_vec();
                        self.pending_chats
                            .entry(id.clone())
                            .or_default()
                            .extend(leftover);
                        delivered = messages.len();
                    }
                }
            }
        }
    }

    /// Dispatch queued tasks while the scheduler yields one.
    fn dispatch_queued(&mut self) {
        let mut saturated: BTreeSet<Stage> = BTreeSet::new();
        let mut failed: BTreeSet<String> = BTreeSet::new();
        loop {
            let Some(id) = self.next_eligible(&saturated, &failed) else {
                break;
            };
            match self.dispatch_one(&id) {
                Ok(true) => {}
                Ok(false) => {
                    if let Some(task) = self.table.by_id.get(&id) {
                        saturated.insert(task.stage);
                    }
                }
                Err(error) => {
                    eprintln!("task {id}: dispatch failed: {error:#}");
                    failed.insert(id);
                }
            }
        }
    }

    /// The next queued task the scheduler admits, ignoring saturated stages.
    ///
    /// This mirrors [`sched::next_dispatch`] with one daemon-side exception:
    /// a stage whose live-process slots are full yields to the later tasks of
    /// other stages until `dispatch_one` stops a yielding parked session,
    /// the reaper stops one, or an exit frees a slot. A task that
    /// holds queued chat messages is not eligible either;
    /// `resume_pending_chats` owns it.
    fn next_eligible(
        &self,
        saturated: &BTreeSet<Stage>,
        failed: &BTreeSet<String>,
    ) -> Option<String> {
        for id in &self.table.order {
            let Some(task) = self.table.by_id.get(id) else {
                continue;
            };
            if task.state != TaskState::Queued
                || saturated.contains(&task.stage)
                || failed.contains(&task.id)
                || self.stopping_sessions.contains_key(&task.id)
                || self.sessions.contains_key(&task.id)
                || (self.pending_chats.contains_key(&task.id)
                    && !self.interrupted.contains(&task.id))
                || (self.restored_ids.contains(&task.id) && self.restore_repos.contains(&task.repo))
                || self.prior_stage_active(task)
                || self.worktree_holder(task).is_some()
            {
                continue;
            }
            if matches!(
                sched::can_start(
                    &self.limits,
                    &self.paused,
                    &self.table,
                    task.stage,
                    &task.repo,
                    &task.id,
                ),
                Verdict::Yes
            ) {
                return Some(task.id.clone());
            }
        }
        None
    }

    /// True when the task before this one still owns the same work.
    fn prior_stage_active(&self, task: &Task) -> bool {
        self.prior_stage_blocker(task).is_some()
    }

    /// True when `prior` still owns the work that `task` waits for.
    ///
    /// An agent updates GitHub before its runner result arrives. The next
    /// gate can therefore open first. This guard keeps two stages off one
    /// issue at the same time. It also keeps a release behind its review.
    /// A failed prior task holds the gate too, because its stage never
    /// finished the work.
    fn holds_prior_stage(&self, task: &Task, prior: &Task) -> bool {
        prior.state != TaskState::Done
            && match task.stage {
                Stage::Refine => false,
                Stage::Implement => {
                    prior.repo == task.repo
                        && prior.stage == Stage::Refine
                        && prior.kind == ItemKind::Issue
                        && prior.number == task.number
                }
                Stage::Review => {
                    prior.repo == task.repo
                        && prior.stage == Stage::Implement
                        && self
                            .review_tickets
                            .get(&task.id)
                            .is_some_and(|tickets| tickets.contains(&prior.number))
                }
                Stage::Release => {
                    prior.repo == task.repo
                        && prior.stage == Stage::Review
                        && self
                            .release_batches
                            .get(&task.id)
                            .is_some_and(|prs| prs.contains(&prior.number))
                }
            }
    }

    /// The id of the task before this one that still owns the same work.
    ///
    /// A failed blocker wins over every other one. It never finishes on
    /// its own, so it is the task the human must act on. A batch can hold
    /// one running review and one failed review at the same time. The
    /// running one ends by itself. Only the failed one stops the release
    /// permanently. The table is a `BTreeMap`, so both choices are stable.
    fn prior_stage_blocker(&self, task: &Task) -> Option<String> {
        let mut active: Option<&str> = None;
        for prior in self.table.by_id.values() {
            if !self.holds_prior_stage(task, prior) {
                continue;
            }
            if matches!(prior.state, TaskState::Failed(_)) {
                return Some(prior.id.clone());
            }
            active.get_or_insert(prior.id.as_str());
        }
        active.map(str::to_string)
    }

    /// Start one queued task: ensure the worktree, render the prompt, start
    /// the runner, move the task to `Running`.
    ///
    /// `Ok(true)` means the task started, or that it holds queued chat
    /// messages and `resume_pending_chats` owns it. `Ok(false)` means the
    /// stage's live processes are at their limit and no parked session
    /// could yield a slot; the caller tries the next stage. An error means
    /// the dispatch failed, the task is handled, and the caller must stop
    /// this round.
    fn dispatch_one(&mut self, id: &str) -> Result<bool> {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return Ok(true);
        };
        // One owner per queued task: a task with queued chat messages
        // belongs to `resume_pending_chats`, whatever the drive order is.
        if self.pending_chats.contains_key(id) && !self.interrupted.contains(id) {
            return Ok(true);
        }
        // The second limit: live processes, not scheduler slots. A parked
        // chat holds a process between turns, and that process is the real
        // memory cost the stage limit exists to bound. A parked session
        // that no queued message waits for yields its slot to this task,
        // and the check retries once.
        if self.live_sessions(task.stage) >= self.limits.limit(task.stage) {
            self.free_live_slot(task.stage, &task.id);
            if self.live_sessions(task.stage) >= self.limits.limit(task.stage) {
                return Ok(false);
            }
        }
        let Some(repo_cfg) = self.config.repos.get(&task.repo).cloned() else {
            let reason = format!("repository {} left the config", task.repo);
            self.log_dispatch_failure(&task, &reason);
            self.fail_run(&task, &reason);
            return Err(anyhow!(reason));
        };
        let cwd = match self.workspace(&task) {
            Workspace::Shared => Ok(repo_cfg.path.clone()),
            Workspace::Exclusive(WorktreeKey::Issue(number)) => {
                self.worktrees.ensure_issue(&*self.exec, &repo_cfg, number)
            }
            Workspace::Exclusive(WorktreeKey::Pr(number)) => {
                self.worktrees.ensure_pr(&*self.exec, &repo_cfg, number)
            }
            Workspace::Exclusive(WorktreeKey::Train) => {
                self.worktrees.ensure_train(&*self.exec, &repo_cfg)
            }
        };
        let cwd = match cwd {
            Ok(cwd) => cwd,
            Err(e) => {
                let reason = format!("cannot prepare the worktree: {e:#}");
                self.log_dispatch_failure(&task, &reason);
                self.fail_run(&task, &reason);
                return Err(e);
            }
        };
        let prompt = match self.render_prompt(&task, &repo_cfg, &cwd) {
            Ok(prompt) => prompt,
            Err(e) => {
                let reason = format!("cannot render the prompt: {e:#}");
                self.log_dispatch_failure(&task, &reason);
                self.fail_run(&task, &reason);
                return Err(e);
            }
        };
        let resume = if self.task_capabilities(&task).resume {
            match task.session_id.clone().map_or_else(
                || self.worktrees.read_task_session(&cwd, &task.id),
                |id| Ok(Some(id)),
            ) {
                Ok(resume) => resume,
                Err(e) => {
                    let reason = format!("cannot read the session marker: {e:#}");
                    self.log_dispatch_failure(&task, &reason);
                    self.fail_run(&task, &reason);
                    return Err(e);
                }
            }
        } else {
            None
        };
        // A task the snapshot restored as running reads the restart notice
        // once, so it resumes the worktree instead of repeating the stage.
        let prompt = if resume.is_some() && self.interrupted.contains(id) {
            format!("{RESTART_NOTICE}\n\n{prompt}")
        } else {
            prompt
        };
        if let Err(e) = self.launch_task(&task, prompt, resume) {
            let reason = format!("the runner could not start: {e:#}");
            self.log_dispatch_failure(&task, &reason);
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
        let role = self.bind_task_role(task)?;
        let settings = &role.settings;
        let cwd = self
            .task_cwd(&task.id)
            .ok_or_else(|| anyhow!("repository {} left the config", task.repo))?;
        let job = Job {
            task: task.id.clone(),
            stage: task.stage,
            repo: task.repo.clone(),
            model: settings.model.clone(),
            variant: settings.effort.clone(),
            prompt,
            cwd,
            log: task.log_path.clone(),
            resume,
            yolo: settings.auto_approve == Some(true)
                || settings.permission_mode.as_deref() == Some("bypassPermissions"),
            allowed_tools: (!settings.tools.is_empty()).then(|| settings.tools.clone()),
        };
        let mut runner = self.runner_factory.build(&role);
        let session = runner.start(&job, self.run_tx.clone())?;
        self.sessions.insert(task.id.clone(), session);
        self.last_event_ms.insert(task.id.clone(), self.now_ms);
        // The run started, so the restart notice has done its job. A second
        // run of the same task carries no notice.
        self.interrupted.remove(&task.id);
        if let Err(e) = self
            .table
            .transition(&task.id, TaskState::Running, self.now_ms)
        {
            self.stop_session(&task.id, "cannot stop the rejected session");
            return Err(e);
        }
        self.changed = true;
        Ok(())
    }

    /// Fail a task, requeue it while attempts remain, or open a stuck row.
    ///
    /// This is the one failure path for dispatch errors and run exits. A
    /// task that still has attempts left goes back to `Queued`; the last
    /// failure opens a `Stuck` decision for the human. A live process
    /// belongs to a task that can use it, so the daemon stops the session
    /// of the failed task first. The task waits for the `Exit` event
    /// before a retry, like every replaced session.
    fn fail_task(&mut self, id: &str, reason: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        self.stop_session(id, "cannot stop the failed session");
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
    /// queue, still labelled. A final run failure retains the exact batch for
    /// an operator retry; cancellation and admission failure remove it.
    fn finish_train(&mut self, repo: &str, ok: bool, retain_failed_batch: bool) {
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
                    if ok || batch.is_empty() || !retain_failed_batch {
                        self.release_batches.remove(&task_id);
                    } else {
                        self.release_batches.insert(task_id, batch.clone());
                    }
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
        if self.stopping_sessions.contains_key(&task_id) && !matches!(event, RunEvent::Exit { .. })
        {
            return;
        }
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
                let ticket_key = (task.repo.clone(), task.number);
                let ticket_chat = task.purpose == TaskPurpose::TicketChat;
                let marker = self
                    .task_cwd(&task_id)
                    .ok_or_else(|| anyhow!("the task has no worktree"))
                    .and_then(|cwd| {
                        self.worktrees.write_session(&cwd, &session_id)?;
                        self.worktrees
                            .write_task_session(&cwd, &task_id, &session_id)
                    });
                if let Err(error) = marker {
                    let reason = format!("cannot write the session marker: {error:#}");
                    eprintln!("task {task_id}: {reason}");
                    self.stop_session(&task_id, "cannot stop the session");
                    if let Some(task) = self.table.by_id.get(&task_id).cloned() {
                        self.fail_run(&task, &reason);
                    }
                    return;
                }
                if ticket_chat {
                    if let Some(conversation) = self.ticket_conversations.get_mut(&ticket_key) {
                        conversation.session_id = Some(session_id);
                    }
                    if let Some(fresh) = self.snapshot.repos.get(&ticket_key.0).cloned() {
                        self.reconcile_ticket_conversations(&ticket_key.0, &fresh);
                    }
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
            RunEvent::Text { text, .. } => {
                if self
                    .table
                    .by_id
                    .get(&task_id)
                    .is_some_and(Self::is_ticket_chat)
                {
                    let turn = self.ticket_turn_text.entry(task_id).or_default();
                    if proposal_marker_text(&turn.last) {
                        turn.earlier_marker = true;
                    }
                    turn.last = text;
                }
                // The runner also tees this into the task log.
            }
            RunEvent::Tool { .. } => {
                // The runner tees this into the task log; the interfaces read
                // the log file, so the daemon stores nothing here.
            }
            RunEvent::TurnEnd { ok, summary, .. } => {
                if ok {
                    self.finish_ticket_proposal_turn(&task_id);
                } else {
                    self.ticket_turn_text.remove(&task_id);
                }
                self.on_turn_end(&task_id, ok, &summary);
            }
            RunEvent::Exit { ok, detail, .. } => {
                self.ticket_turn_text.remove(&task_id);
                self.on_exit_event(&task_id, ok, &detail);
            }
        }
    }

    /// Apply one turn end.
    ///
    /// A live-input refine task waits for a user. Another live-input task
    /// completes or fails from the turn result. A one-shot turn is only a
    /// step boundary.
    fn on_turn_end(&mut self, id: &str, ok: bool, summary: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state != TaskState::Running || !self.task_capabilities(&task).live_input {
            return;
        }
        if task.stage == Stage::Refine {
            let transitioned = self
                .snapshot
                .repos
                .get(&task.repo)
                .is_some_and(|fresh| refine_transitioned(fresh, task.number));
            if ok && transitioned {
                self.stop_session(id, "cannot stop the completed refine session");
                self.complete_task(&task);
                return;
            }
            if let Err(error) = self
                .table
                .transition(id, TaskState::AwaitingUser, self.now_ms)
            {
                eprintln!("task {id}: {error:#}");
                return;
            }
            self.changed = true;
            self.reconcile(Some(&task.repo));
        } else if ok {
            self.complete_task(&task);
        } else {
            let reason = if summary.is_empty() {
                "the agent turn failed"
            } else {
                summary
            };
            self.fail_run(&task, reason);
        }
    }

    /// Apply one run exit.
    ///
    /// A terminal task ignores the exit. A parked task stays resumable.
    /// A one-shot exit supplies the task result.
    /// A live-input exit without a prior result fails the active task.
    /// After the terminal state is set, a queued chat message reopens the
    /// task for its follow-up turn.
    fn on_exit_event(&mut self, id: &str, ok: bool, detail: &str) {
        self.last_event_ms.remove(id);
        if self.stopping_sessions.remove(id).is_some() {
            return;
        }
        self.sessions.remove(id);
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        if task.state == TaskState::AwaitingUser {
            return;
        }
        if self.task_capabilities(&task).live_input {
            if task.state == TaskState::Queued {
                return;
            }
            let reason = if ok {
                format!("the live-input runner exited before a turn result: {detail}")
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
        // A live-input task without a saved message loses its restart data
        // at `Done`. A saved message keeps the marker until its next turn.
        // A resumable one-shot task keeps the marker for later follow-ups.
        if self.task_capabilities(task).live_input && !self.pending_chats.contains_key(&task.id) {
            self.remove_task_session_marker(task);
        }
        self.decisions.drop_for_task(&task.id);
        match task.stage {
            Stage::Release => self.finish_train(&task.repo, true, false),
            Stage::Refine | Stage::Implement | Stage::Review => {}
        }
        self.reopen_for_pending_chat(&task.id);
    }

    /// Fail one run and return a final release batch to its train.
    fn fail_run(&mut self, task: &Task, reason: &str) {
        if task.stage == Stage::Release && task.attempt >= tasks::MAX_ATTEMPTS {
            self.finish_train(&task.repo, false, true);
        }
        self.fail_task(&task.id, reason);
        self.reopen_for_pending_chat(&task.id);
    }

    /// Append one dispatch-failure line to the log of the task.
    ///
    /// A dispatch failure happens before the runner starts, so no process
    /// ever writes to the log, and the session view would show no output at
    /// all. This line is the reason the operator sees there. A write failure
    /// goes to standard error and never masks the dispatch failure.
    fn log_dispatch_failure(&self, task: &Task, reason: &str) {
        if let Some(parent) = task.log_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent) {
                    eprintln!("cannot create the log directory {}: {e}", parent.display());
                    return;
                }
            }
        }
        let line = format!("aif: dispatch failed: {reason}\n");
        let opened = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&task.log_path);
        match opened {
            Ok(mut file) => {
                use std::io::Write as _;
                if let Err(e) = file.write_all(line.as_bytes()) {
                    eprintln!("cannot write to {}: {e}", task.log_path.display());
                }
            }
            Err(e) => eprintln!("cannot open the task log {}: {e}", task.log_path.display()),
        }
    }

    /// Write the `.aif/reviewed-sha` marker of a finished review.
    ///
    /// The marker records the head sha the gate admitted, and it is written
    /// only here, after the review task reported success. A task without a
    /// stored sha falls back to the head of the same pull request in the last
    /// poll, so a bookkeeping gap cannot discard a finished review.
    fn write_review_marker(&self, task: &Task) -> Result<()> {
        let Some(sha) = task.head_sha.clone().or_else(|| self.polled_head_sha(task)) else {
            bail!("review task {} has no head sha", task.id);
        };
        let Some(cwd) = self.task_cwd(&task.id) else {
            bail!("review task {} has no worktree", task.id);
        };
        self.worktrees.write_reviewed_sha(&cwd, &sha)
    }

    /// The head sha of the pull request of `task` in the last poll.
    fn polled_head_sha(&self, task: &Task) -> Option<String> {
        if task.kind != ItemKind::Pr {
            return None;
        }
        self.snapshot
            .repos
            .get(&task.repo)?
            .prs
            .get(&task.number)
            .map(|pr| pr.head_sha.clone())
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
                let replaces_task = self.table.by_id.values().any(|task| {
                    task.repo == repo
                        && task.stage == Stage::Refine
                        && task.kind == kind
                        && task.number == number
                        && task.state.is_terminal()
                });
                match self
                    .table
                    .upsert_queued(&repo, Stage::Refine, kind, number, log, self.now_ms)
                {
                    Ok(task) => {
                        if replaces_task {
                            self.role_bindings.remove(&task.id);
                        }
                        self.changed = true;
                    }
                    Err(e) => eprintln!(
                        "the refine request for {repo} {} {number}: {e:#}",
                        kind.as_str()
                    ),
                }
            }
            Action::Ask { repo, kind, number } => self.fetch_ask(&repo, kind, number),
            Action::Chat { task, text } => {
                self.chat(&task, &text);
            }
            Action::Answer {
                decision_id,
                response,
            } => self.answer_decision(&decision_id, response),
            Action::Abort { task } => self.cancel_task(&task, true),
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
                self.fire_train(&repo, &prs, true);
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
                    PauseScope::Global => self.paused.set_global(paused),
                    PauseScope::Stage { stage } => self.paused.set_stage(stage, paused),
                    PauseScope::Lane { stage, repo } => {
                        if !self.config.repos.contains_key(&repo) {
                            eprintln!("the pause change for {repo}: no such repository");
                            return;
                        }
                        self.paused.set_lane(stage, repo, paused);
                    }
                    PauseScope::Task { task } => {
                        if !self.table.by_id.contains_key(&task) {
                            eprintln!("the pause change for {task}: no such task");
                            return;
                        }
                        self.paused.set_task(task, paused);
                    }
                }
                self.changed = true;
            }
            Action::TicketCreate { repo } => self.ticket_create(&repo),
            Action::Ticket(action) => {
                let chat = match &action {
                    TicketAction::Chat { repo, number, .. } => Some((repo.clone(), *number)),
                    _ => None,
                };
                let mentions_followup = match &action {
                    TicketAction::Details { repo, number, .. } => {
                        Some((repo.clone(), *number, false, true))
                    }
                    TicketAction::Mentions { repo, number, .. } => {
                        Some((repo.clone(), *number, false, false))
                    }
                    TicketAction::PrMentions { repo, number, .. } => {
                        Some((repo.clone(), *number, true, false))
                    }
                    _ => None,
                };
                let proposal_apply = match &action {
                    TicketAction::UpdateContent {
                        request,
                        repo,
                        number,
                        source: crate::sock::TicketContentSource::Proposal { proposal_id },
                        ..
                    } => Some((request.clone(), repo.clone(), *number, proposal_id.clone())),
                    _ => None,
                };
                let mut effects = self.ticket_controller.handle(
                    action,
                    &self.snapshot,
                    &self.config,
                    self.now_ms,
                );
                if let Some((repo, _, confirmed_ms)) = effects.confirmed.as_mut() {
                    let completed_ms = (self.clock)().max(self.now_ms);
                    self.now_ms = completed_ms;
                    *confirmed_ms = completed_ms;
                    self.ticket_controller
                        .record_confirmed_mutation(repo, completed_ms);
                }
                for push in &mut effects.pushes {
                    if let Push::TicketDetails(details) = push {
                        details.proposal = self
                            .ticket_conversations
                            .get(&(details.repo.clone(), details.issue.number))
                            .and_then(|conversation| conversation.proposal.clone());
                    }
                }
                let start_chat = chat.filter(|_| effects.pushes.is_empty());
                let proposal_succeeded = proposal_apply.as_ref().is_some_and(|_| {
                    effects.pushes.iter().any(|push| {
                        matches!(
                            push,
                            Push::TicketResult(result)
                                if result.kind == crate::sock::TicketResultKind::Success
                        )
                    })
                });
                let observed_conflict = effects.pushes.iter().find_map(|push| match push {
                    Push::TicketResult(result)
                        if result.kind == crate::sock::TicketResultKind::Conflict =>
                    {
                        result
                            .conflict
                            .as_ref()
                            .map(|conflict| (result.repo.clone(), conflict.remote.clone()))
                    }
                    _ => None,
                });
                if let Some((repo, issue)) = observed_conflict {
                    if let Some(items) = self.snapshot.repos.get_mut(&repo) {
                        items.issues.insert(issue.number, issue);
                        self.changed = true;
                    }
                    if let Some(fresh) = self.snapshot.repos.get(&repo).cloned() {
                        self.complete_parked_refines(&repo, &fresh);
                        self.reconcile_unready(&repo, &fresh);
                        self.reconcile_ticket_conversations(&repo, &fresh);
                        let ready = self.observe_ready_work(&repo, &fresh);
                        let decisions = self.derive_needs_human(&repo, &fresh);
                        self.changed |= ready || decisions;
                    }
                }
                if let Some((repo, issue, _confirmed_ms)) = effects.confirmed {
                    self.snapshot
                        .repos
                        .entry(repo.clone())
                        .or_default()
                        .issues
                        .insert(issue.number, issue);
                    self.changed = true;
                    if let Some(fresh) = self.snapshot.repos.get(&repo).cloned() {
                        self.complete_parked_refines(&repo, &fresh);
                        self.reconcile_unready(&repo, &fresh);
                        self.reconcile_ticket_conversations(&repo, &fresh);
                        let ready = self.observe_ready_work(&repo, &fresh);
                        let decisions = self.derive_needs_human(&repo, &fresh);
                        self.changed |= ready || decisions;
                    }
                }
                if let Some(pusher) = self.ticket_pusher.as_ref() {
                    for push in effects.pushes {
                        pusher(push);
                    }
                }
                if let Some((repo, number, subject_is_pr, force)) = mentions_followup {
                    let push = self.ticket_controller.mentions_push(
                        &self.snapshot,
                        &self.config,
                        &repo,
                        number,
                        subject_is_pr,
                        self.now_ms,
                        force,
                    );
                    if let (Some(push), Some(pusher)) = (push, self.ticket_pusher.as_ref()) {
                        pusher(push);
                    }
                }
                if let Some((repo, number)) = start_chat {
                    self.ticket_chat(&repo, number);
                }
                if proposal_succeeded {
                    if let Some((request, repo, number, proposal_id)) = proposal_apply {
                        let mut cleared = false;
                        if let Some(conversation) =
                            self.ticket_conversations.get_mut(&(repo.clone(), number))
                        {
                            if conversation
                                .proposal
                                .as_ref()
                                .is_some_and(|proposal| proposal.id == proposal_id)
                            {
                                conversation.proposal = None;
                                self.changed = true;
                                cleared = true;
                            }
                        }
                        if cleared {
                            if let Some(issue) = self
                                .snapshot
                                .repos
                                .get(&repo)
                                .and_then(|items| items.issues.get(&number))
                                .cloned()
                            {
                                if let Some(pusher) = self.ticket_pusher.as_ref() {
                                    pusher(Push::TicketDetails(TicketDetails {
                                        request,
                                        repo: repo.clone(),
                                        issue,
                                        proposal: None,
                                        chat_error: self.config.ticket_chat_model().err(),
                                    }));
                                }
                            }
                        }
                        let id = tasks::ticket_chat_id(&repo, number);
                        self.chat(
                            &id,
                            "AIF applied the shown proposal to the GitHub ticket. Continue with the confirmed content.",
                        );
                    }
                }
            }
            Action::SaveSettings {
                request,
                base_revision,
                edit,
            } => self.save_settings(request, base_revision, edit),
            Action::ReloadSettings { request } => self.reload_settings(request),
            Action::Reconcile { repo } => self.reconcile(repo.as_deref()),
            Action::Stop => self.shutdown = true,
        }
    }

    /// Fetch the comments of one ask row and push the question view back.
    ///
    /// The walk goes from the newest comment to the oldest one and picks
    /// the first comment that holds a valid ask block. Without a valid
    /// block the newest comment body becomes the question. A failed fetch
    /// ships an error view, so the daemon never crashes and never closes
    /// the inbox row.
    fn fetch_ask(&mut self, repo: &str, kind: ItemKind, number: u64) {
        let Some(repo_cfg) = self.config.repos.get(repo).cloned() else {
            eprintln!("the question request for {repo}: no such repository");
            return;
        };
        let view =
            match GhClient::new(&*self.exec).fetch_issue_comments(&repo_cfg.owner_repo, number) {
                Ok(comments) => comments
                    .iter()
                    .rev()
                    .find_map(|comment| {
                        let ask = crate::ask::parse_ask_block(&comment.body)?;
                        Some(AskView {
                            repo: repo.to_string(),
                            kind,
                            number,
                            question: ask.question,
                            options: ask.options,
                            author: Some(comment.author.clone()),
                            created_at: Some(comment.created_at.clone()),
                            error: None,
                        })
                    })
                    .unwrap_or_else(|| match comments.last() {
                        Some(comment) => AskView {
                            repo: repo.to_string(),
                            kind,
                            number,
                            question: comment.body.clone(),
                            options: Vec::new(),
                            author: Some(comment.author.clone()),
                            created_at: Some(comment.created_at.clone()),
                            error: None,
                        },
                        None => AskView {
                            repo: repo.to_string(),
                            kind,
                            number,
                            question: String::new(),
                            options: Vec::new(),
                            author: None,
                            created_at: None,
                            error: None,
                        },
                    }),
                Err(error) => AskView {
                    repo: repo.to_string(),
                    kind,
                    number,
                    question: String::new(),
                    options: Vec::new(),
                    author: None,
                    created_at: None,
                    error: Some(format!("{error:#}")),
                },
            };
        if let Some(pusher) = self.ticket_pusher.as_ref() {
            pusher(Push::Ask(view));
        }
    }

    /// Save one comment-preserving role edit and activate the valid result.
    fn save_settings(&mut self, request: String, base_revision: String, edit: SettingsEdit) {
        let (current, current_revision) = match self.read_factory_file() {
            Ok(value) => value,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Failed,
                    self.settings_revision.clone(),
                    Some(format!("cannot read factory.toml: {error:#}")),
                );
                return;
            }
        };
        if base_revision != current_revision {
            self.push_settings_result(
                request,
                SettingsOperation::Save,
                SettingsResultStatus::Stale,
                current_revision,
                Some("the factory.toml file changed after this edit started".to_string()),
            );
            return;
        }
        let candidate_text = match config::edit_config_text(&current, &edit) {
            Ok(text) => text,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Invalid,
                    current_revision,
                    Some(format!("the settings edit is invalid: {error:#}")),
                );
                return;
            }
        };
        let candidate = match Config::parse_resolved(&candidate_text, &*self.exec) {
            Ok(config) => config,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Invalid,
                    current_revision,
                    Some(format!("the settings edit is invalid: {error:#}")),
                );
                return;
            }
        };
        if !self.config.has_same_topology(&candidate) {
            self.push_settings_result(
                request,
                SettingsOperation::Save,
                SettingsResultStatus::RestartRequired,
                current_revision,
                Some("repository topology changes require a daemon restart".to_string()),
            );
            return;
        }
        let prepared = match config::prepare_config_atomic(&self.config_path, &candidate_text) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Failed,
                    current_revision,
                    Some(format!("cannot prepare factory.toml: {error:#}")),
                );
                return;
            }
        };
        #[cfg(test)]
        if let Some(mut hook) = self.before_config_commit.take() {
            hook();
        }
        match config::commit_config_atomic_checked(prepared, &base_revision) {
            Ok(config::AtomicWrite::Written) => {}
            Ok(config::AtomicWrite::Stale { revision }) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Stale,
                    revision,
                    Some("the factory.toml file changed while this edit was validated".to_string()),
                );
                return;
            }
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Save,
                    SettingsResultStatus::Failed,
                    current_revision,
                    Some(format!("cannot save factory.toml: {error:#}")),
                );
                return;
            }
        }
        let revision = config::file_revision(&candidate_text);
        self.activate_config(candidate, revision.clone());
        self.push_settings_result(
            request,
            SettingsOperation::Save,
            SettingsResultStatus::Saved,
            revision,
            None,
        );
    }

    /// Reload the factory file without changing it on disk.
    fn reload_settings(&mut self, request: String) {
        let (text, revision) = match self.read_factory_file() {
            Ok(value) => value,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Reload,
                    SettingsResultStatus::Failed,
                    self.settings_revision.clone(),
                    Some(format!("cannot read factory.toml: {error:#}")),
                );
                return;
            }
        };
        let candidate = match Config::parse_resolved(&text, &*self.exec) {
            Ok(config) => config,
            Err(error) => {
                self.push_settings_result(
                    request,
                    SettingsOperation::Reload,
                    SettingsResultStatus::Invalid,
                    revision,
                    Some(format!("the factory configuration is invalid: {error:#}")),
                );
                return;
            }
        };
        if !self.config.has_same_topology(&candidate) {
            self.push_settings_result(
                request,
                SettingsOperation::Reload,
                SettingsResultStatus::RestartRequired,
                revision,
                Some("repository topology changes require a daemon restart".to_string()),
            );
            return;
        }
        self.activate_config(candidate, revision.clone());
        self.push_settings_result(
            request,
            SettingsOperation::Reload,
            SettingsResultStatus::Reloaded,
            revision,
            None,
        );
    }

    /// Read the complete config and compute its compare-and-save revision.
    fn read_factory_file(&self) -> Result<(String, String)> {
        let text = fs::read_to_string(&self.config_path)
            .with_context(|| format!("cannot read {}", self.config_path.display()))?;
        let revision = config::file_revision(&text);
        Ok((text, revision))
    }

    /// Install a validated non-topology configuration in the running daemon.
    fn activate_config(&mut self, config: Config, revision: String) {
        for stage in Stage::ALL {
            let old_limit = self.config.stage(stage).limit;
            if self.limits.limit(stage) == old_limit {
                self.limits.stage.insert(stage, config.stage(stage).limit);
            }
        }
        self.config = config;
        self.settings_revision = revision;
        self.changed = true;
    }

    /// Send one non-state settings result to each connected client.
    fn push_settings_result(
        &self,
        request: String,
        operation: SettingsOperation,
        status: SettingsResultStatus,
        revision: String,
        message: Option<String>,
    ) {
        if let Some(pusher) = self.ticket_pusher.as_ref() {
            pusher(Push::SettingsResult(SettingsResult {
                request,
                operation,
                status,
                revision,
                message,
            }));
        }
    }

    /// Send a chat message to one task.
    ///
    /// A live-input session receives the message at once. Every other case
    /// queues the message as the next turn: the daemon reopens a terminal
    /// task, and `resume_pending_chats` starts a run whose prompt is the
    /// message and whose session continues the old one. A failed live send
    /// also keeps the message for a resumed turn. The call refuses any
    /// message while another task that owns the same exclusive worktree is
    /// active. It also refuses a task with no session id and no session
    /// marker.
    /// The result is true when the daemon delivered or queued the message.
    fn chat(&mut self, id: &str, text: &str) -> bool {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            eprintln!("the chat message for {id}: no such task");
            return false;
        };
        if let Some(refusal) = self.sibling_refusal(&task) {
            eprintln!("{refusal}");
            return false;
        }
        // Only a runner with live-input support receives a steering message.
        if self.task_capabilities(&task).live_input {
            if let Some(session) = self.sessions.get_mut(id) {
                match session.send_user(text) {
                    Ok(()) => {
                        self.last_event_ms.insert(id.to_string(), self.now_ms);
                        if task.state == TaskState::AwaitingUser {
                            if let Err(error) =
                                self.table.transition(id, TaskState::Running, self.now_ms)
                            {
                                eprintln!("the chat message for {id}: {error:#}");
                            } else {
                                self.changed = true;
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("the chat message for {id}: {error:#}");
                        // The process can exit before its event reaches the
                        // daemon. Keep the message for the resumed turn.
                        self.pending_chats
                            .entry(id.to_string())
                            .or_default()
                            .push(text.to_string());
                        self.changed = true;
                        if task.state.is_terminal() {
                            self.reopen_for_pending_chat(id);
                        }
                    }
                }
                return true;
            }
        }
        let session_id = match self.followup_session_id(&task) {
            Ok(session_id) => session_id,
            Err(error) => {
                eprintln!("the chat message for {id}: {error:#}");
                return false;
            }
        };
        if session_id.is_none() {
            eprintln!(
                "the chat message for {id}: no session id and no session marker; \
                 there is no agent session to continue"
            );
            return false;
        }
        // The wire and the daemon use the same policy. A saved session does
        // not make an unsupported task state accept a message.
        if let InputMode::Closed { reason } = self.input_mode(&task) {
            eprintln!("the chat message for {id}: {reason}");
            return false;
        }
        self.pending_chats
            .entry(id.to_string())
            .or_default()
            .push(text.to_string());
        // The queued count rides on the state view, so the queueing marks
        // the state dirty even when the task state stays as it was.
        self.changed = true;
        if task.state.is_terminal() {
            if let Err(error) = self.table.reopen(id, self.now_ms) {
                eprintln!("the chat message for {id}: {error:#}");
                return true;
            }
            self.changed = true;
            self.decisions.drop_for_task(id);
        }
        true
    }

    /// The item whose workspace a review task runs in.
    ///
    /// One linked ticket runs the review in that ticket's worktree. Zero or
    /// several links run it in the PR worktree. Every consumer of the
    /// review workspace goes through this one helper, so no site drifts.
    fn review_item(&self, task: &Task) -> (ItemKind, u64) {
        let tickets = self.review_tickets.get(&task.id);
        match tickets
            .filter(|tickets| tickets.len() == 1)
            .and_then(|tickets| tickets.iter().next())
        {
            Some(&ticket) => (ItemKind::Issue, ticket),
            None => (ItemKind::Pr, task.number),
        }
    }

    /// The working directory identity of one task.
    ///
    /// The refine stage, the ticket session, and the ticket chat share the
    /// repository checkout, so a `Shared` task owns no worktree. Every other
    /// stage runs in one private git worktree and owns it. The guard and the
    /// directory readers both read this helper, so they cannot disagree.
    fn workspace(&self, task: &Task) -> Workspace {
        match task.stage {
            Stage::Refine => Workspace::Shared,
            Stage::Implement => Workspace::Exclusive(WorktreeKey::Issue(task.number)),
            Stage::Review => {
                let (kind, number) = self.review_item(task);
                match kind {
                    ItemKind::Issue => Workspace::Exclusive(WorktreeKey::Issue(number)),
                    ItemKind::Pr => Workspace::Exclusive(WorktreeKey::Pr(number)),
                }
            }
            Stage::Release => Workspace::Exclusive(WorktreeKey::Train),
        }
    }

    /// The running or awaiting task that holds the worktree of `task`.
    ///
    /// A running or awaiting task owns its worktree process; a queued task
    /// does not. The dispatch loop skips a task whose worktree is held, so
    /// two agents never share one worktree. A `Shared` task holds nothing
    /// and nothing holds it.
    fn worktree_holder(&self, task: &Task) -> Option<String> {
        let workspace = self.workspace(task);
        if !matches!(workspace, Workspace::Exclusive(_)) {
            return None;
        }
        self.table
            .by_id
            .values()
            .find(|other| {
                other.id != task.id
                    && other.repo == task.repo
                    && matches!(other.state, TaskState::Running | TaskState::AwaitingUser)
                    && self.workspace(other) == workspace
            })
            .map(|other| other.id.clone())
    }

    /// The active task that blocks a follow-up to `task`, if one exists.
    ///
    /// Two agents must never run in one worktree. A follow-up waits while
    /// another task of the same repository and exclusive worktree is active.
    /// A `Shared` task never blocks and is never blocked.
    fn sibling_blocker(&self, task: &Task) -> Option<String> {
        let workspace = self.workspace(task);
        if !matches!(workspace, Workspace::Exclusive(_)) {
            return None;
        }
        self.table
            .by_id
            .values()
            .find(|other| {
                other.id != task.id
                    && other.repo == task.repo
                    && !other.state.is_terminal()
                    && self.workspace(other) == workspace
            })
            .map(|other| other.id.clone())
    }

    /// The clear refusal for a follow-up whose sibling is not terminal.
    ///
    /// The refusal names the blocker, its state, and the worktree, so the
    /// operator can check the claim.
    fn sibling_refusal(&self, task: &Task) -> Option<String> {
        let Workspace::Exclusive(key) = self.workspace(task) else {
            return None;
        };
        self.sibling_blocker(task).map(|blocker| {
            let state = &self.table.by_id[&blocker].state;
            format!(
                "the chat message for \"{}\" cannot start. Task \"{blocker}\" ({state}) uses \
                 the worktree \"{key}\". Wait until that task is terminal.",
                task.id
            )
        })
    }

    /// The session id that a follow-up turn continues.
    ///
    /// The id comes from the task, and else from the session marker in the
    /// task's workspace. The marker lets a human continue a resumable task
    /// after a daemon restart. `None` means no session exists.
    fn followup_session_id(&self, task: &Task) -> Result<Option<String>> {
        if let Some(session_id) = task.session_id.as_deref() {
            return Ok(Some(session_id.to_string()));
        }
        let Some(cwd) = self.task_cwd(&task.id) else {
            return Ok(None);
        };
        self.worktrees.read_task_session(&cwd, &task.id)
    }

    /// Decide what the session view's input bar does for one task.
    ///
    /// The sibling guard decides first: a blocked task is closed whatever
    /// else is true. A live-input session takes a steering message at once.
    /// A parked task relaunches its session on the next message. A running
    /// one-shot task with a recorded session uses the message for its next
    /// turn. A terminal one-shot task queues a follow-up turn. Every other
    /// task takes no message. The close reason says why.
    fn input_mode(&self, task: &Task) -> InputMode {
        if let Some(reason) = self.sibling_refusal(task) {
            return InputMode::Closed { reason };
        }
        let live_input = self.task_capabilities(task).live_input;
        if self.sessions.contains_key(&task.id) && live_input {
            return InputMode::Live;
        }
        let session = match self.followup_session_id(task) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("the input mode of {}: {error:#}", task.id);
                return InputMode::Closed {
                    reason: format!(
                        "The daemon cannot read the session for task \"{}\". \
                         Check its session marker and try again.",
                        task.id
                    ),
                };
            }
        };
        if live_input {
            if task.state == TaskState::AwaitingUser && session.is_some() {
                return InputMode::Resume;
            }
        } else if task.state == TaskState::Running && session.is_some() {
            return InputMode::NextTurn;
        } else if task.state.is_terminal() && session.is_some() {
            return InputMode::Follow;
        }
        InputMode::Closed {
            reason: self.closed_reason(task, session.is_some()),
        }
    }

    /// Build the sentence that says why one task takes no message.
    ///
    /// A task with no session id and no marker needs a new session. A task
    /// with a spent session needs an action that fits its current state.
    ///
    /// A queued task that a prior stage holds names that blocker. Without
    /// the name the human sees a task that never starts and no cause. This
    /// is the one place the session view can carry the cause, so it does.
    ///
    /// A failed blocker never finishes on its own. It holds the queued task
    /// forever. Only that case names an action, because only a failed task
    /// takes a retry. A blocker that still runs needs no action.
    ///
    /// The action names the board, not the inbox. A failed task keeps an
    /// inbox row only when it spent every attempt. `cancel_task` also drops
    /// the row of the task it cancels. An aborted task keeps no row. A task
    /// whose stuck row the human cancelled keeps no row either. Both stay
    /// `Failed`. The `R` key of the board retries any failed task, and it
    /// needs no row.
    ///
    /// The sentence says "This task" in place of the task id. The chat bar
    /// centers this text and clips both ends. Every character therefore
    /// costs a character of the cause. The header row above the bar already
    /// names the task, so the id here buys nothing.
    fn closed_reason(&self, task: &Task, has_session: bool) -> String {
        if task.state == TaskState::Queued {
            if let Some(blocker) = self.prior_stage_blocker(task) {
                let stuck = self
                    .table
                    .by_id
                    .get(&blocker)
                    .is_some_and(|prior| matches!(prior.state, TaskState::Failed(_)));
                let action = if stuck {
                    " That task failed. Press R on its pipeline row to retry it."
                } else {
                    ""
                };
                return format!("This task waits for \"{blocker}\" to finish.{action}");
            }
        }
        if !has_session {
            let action = match task.state {
                TaskState::Queued => "Send a message after the task runs once.",
                TaskState::Running => "Wait until the task records a session.",
                TaskState::AwaitingUser | TaskState::Done => {
                    "Start a new task before you send another message."
                }
                TaskState::Failed(_) => "Retry the task before you send a message.",
            };
            return format!(
                "The task \"{}\" has no session to continue. {action}",
                task.id,
            );
        }
        let (state, action) = match task.state {
            TaskState::Queued => ("queued", "Wait for it to start."),
            TaskState::Running => ("running", "Wait for its session to start."),
            TaskState::AwaitingUser => {
                ("awaiting the user", "Send a message to resume its session.")
            }
            TaskState::Done => ("done", "Start a new task before you send another message."),
            TaskState::Failed(_) => ("failed", "Retry the task before you send a message."),
        };
        format!("The task \"{}\" is {state}. {action}", task.id)
    }

    /// Reopen a terminal task whose chat messages still wait.
    ///
    /// complete_task and fail_run set the terminal state first. A human
    /// abort can also call this after `table.cancel`. A queued message asks
    /// for one more turn, so the task goes back to `Queued`, and
    /// `resume_pending_chats` starts it. The reopen ignores the attempt
    /// limit and never raises the attempt count. It drops the stuck row of
    /// the finished run.
    fn reopen_for_pending_chat(&mut self, id: &str) {
        if !self.pending_chats.contains_key(id) {
            return;
        }
        let Some(task) = self.table.by_id.get(id) else {
            return;
        };
        if !task.state.is_terminal() {
            return;
        }
        if let Err(error) = self.table.reopen(id, self.now_ms) {
            eprintln!("the pending chat for {id}: {error:#}");
            return;
        }
        self.changed = true;
        self.decisions.drop_for_task(id);
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
            (DecisionKind::Stuck { task, .. }, Response::Cancel) => self.cancel_task(task, false),
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
                self.fire_train(&decision.repo, prs, true);
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
        let task_value = self
            .table
            .by_id
            .get(task)
            .ok_or_else(|| anyhow!("no task holds this answer"))?;
        if !self.task_capabilities(task_value).permission_responses {
            bail!("the task runner does not support permission responses");
        }
        let session = self
            .sessions
            .get_mut(task)
            .ok_or_else(|| anyhow!("no live session holds it"))?;
        session.answer(request_id, answer)
    }

    /// Forward a chat line to the live session of one task.
    fn send_to_session(&mut self, task: &str, text: &str) -> Result<()> {
        let task_value = self
            .table
            .by_id
            .get(task)
            .ok_or_else(|| anyhow!("no task holds this message"))?;
        if !self.task_capabilities(task_value).live_input {
            bail!("the task runner does not support live input");
        }
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
        if !matches!(task.state, TaskState::Failed(_)) {
            eprintln!("the retry of {id}: the task is {}, not failed", task.state);
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
            self.fire_train(&task.repo, &prs, false);
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
            Ok(fresh) => {
                // The fresh task starts empty, but the retry reviews the same
                // head as the failed task. Without this the review completes
                // and then fails on the missing sha, and the gate treats the
                // retry as superseded work.
                fresh.head_sha = task.head_sha.clone();
                self.decisions.drop_for_task(id);
                self.changed = true;
            }
            Err(e) => eprintln!("the retry of {id}: {e:#}"),
        }
    }

    /// Cancel one task: stop its process, cancel it, and drop its decisions.
    ///
    /// Only a human abort can deliver a queued chat after the cancel. Every
    /// gate cancel and a stuck-task cancel drop queued chats and restart data.
    /// A human abort with queued chat keeps the messages and session marker.
    /// It returns the task to `Queued` for `resume_pending_chats`.
    fn cancel_task(&mut self, id: &str, deliver_pending_chat: bool) {
        let task = self.table.by_id.get(id).cloned();
        self.stop_session(id, "cannot stop the session during the abort");
        let carries_chat = deliver_pending_chat && self.pending_chats.contains_key(id);
        if !carries_chat {
            self.pending_chats.remove(id);
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
        if let Some(task) = task.as_ref() {
            if carries_chat {
                self.reopen_for_pending_chat(id);
            } else {
                self.remove_task_session_marker(task);
            }
        }
    }

    /// Queue an interactive ticket-creation task for one repository.
    fn ticket_create(&mut self, repo: &str) {
        if !self.config.repos.contains_key(repo) {
            eprintln!("the ticket session for {repo}: no such repository");
            return;
        }
        let log = self.log_path(repo, Stage::Refine, ItemKind::Issue, TICKET_NUMBER);
        let replaces_task = self.table.by_id.values().any(|task| {
            task.repo == repo
                && task.stage == Stage::Refine
                && task.kind == ItemKind::Issue
                && task.number == TICKET_NUMBER
                && task.state.is_terminal()
        });
        match self.table.upsert_queued(
            repo,
            Stage::Refine,
            ItemKind::Issue,
            TICKET_NUMBER,
            log,
            self.now_ms,
        ) {
            Ok(task) => {
                task.purpose = TaskPurpose::TicketCreate;
                if replaces_task {
                    self.role_bindings.remove(&task.id);
                }
                self.changed = true;
            }
            Err(e) => eprintln!("the ticket session for {repo}: {e:#}"),
        }
    }

    /// Queue or reuse one issue conversation.
    fn ticket_chat(&mut self, repo: &str, number: u64) {
        let handoff_active = self
            .snapshot
            .repos
            .get(repo)
            .and_then(|snapshot| snapshot.issues.get(&number))
            .is_some_and(|issue| issue.labels.iter().any(|label| label == "to-refine"));
        self.ticket_conversations
            .entry((repo.to_string(), number))
            .or_insert_with(|| TicketConversationState {
                repo: repo.to_string(),
                number,
                session_id: None,
                handoff_active,
                proposal: None,
            });
        let log = self
            .state_dir
            .join("logs")
            .join(format!("{repo}__ticket-i{number}.jsonl"));
        match self
            .table
            .upsert_ticket_chat(repo, number, log, self.now_ms)
        {
            Ok(_) => self.changed = true,
            Err(error) => eprintln!("the ticket chat for {repo}#{number}: {error:#}"),
        }
    }

    /// Restore, hand off, or end each conversation after GitHub changes.
    fn reconcile_ticket_conversations(&mut self, repo: &str, fresh: &RepoSnapshot) {
        let keys: Vec<(String, u64)> = self
            .ticket_conversations
            .keys()
            .filter(|(conversation_repo, _)| conversation_repo == repo)
            .cloned()
            .collect();
        for key in keys {
            let number = key.1;
            let issue = fresh.issues.get(&number).filter(|issue| issue.open);
            let ended = issue.is_none()
                || issue.is_some_and(|issue| issue.labels.iter().any(|label| label == "refined"));
            if ended {
                self.end_ticket_conversation(&key);
                continue;
            }

            let id = tasks::ticket_chat_id(repo, number);
            if !self.table.by_id.contains_key(&id) {
                let log = self
                    .state_dir
                    .join("logs")
                    .join(format!("{repo}__ticket-i{number}.jsonl"));
                let session_id = self
                    .ticket_conversations
                    .get(&key)
                    .and_then(|conversation| conversation.session_id.clone());
                match self
                    .table
                    .upsert_ticket_chat(repo, number, log, self.now_ms)
                {
                    Ok(task) => task.session_id = session_id,
                    Err(error) => {
                        eprintln!("the ticket chat restore for {repo}#{number}: {error:#}");
                        continue;
                    }
                }
                self.changed = true;
            }

            let has_label =
                issue.is_some_and(|issue| issue.labels.iter().any(|label| label == "to-refine"));
            let (was_active, has_session) = self
                .ticket_conversations
                .get(&key)
                .map(|conversation| {
                    (
                        conversation.handoff_active,
                        conversation.session_id.is_some(),
                    )
                })
                .unwrap_or((false, false));
            if was_active && !has_label {
                if let Some(conversation) = self.ticket_conversations.get_mut(&key) {
                    conversation.handoff_active = false;
                }
                self.changed = true;
            } else if !was_active
                && has_label
                && has_session
                && self.chat(&id, TICKET_REFINEMENT_MESSAGE)
            {
                if let Some(conversation) = self.ticket_conversations.get_mut(&key) {
                    conversation.handoff_active = true;
                }
                self.changed = true;
            }
        }
    }

    /// Stop one issue conversation and remove its private state.
    fn end_ticket_conversation(&mut self, key: &(String, u64)) {
        let id = tasks::ticket_chat_id(&key.0, key.1);
        self.ticket_turn_text.remove(&id);
        if self.table.by_id.contains_key(&id) {
            self.cancel_task(&id, false);
            self.table.remove(&id);
        }
        self.role_bindings.remove(&id);
        if self.ticket_conversations.remove(key).is_some() {
            self.changed = true;
        }
    }

    /// Accept one strict final proposal block and refresh the focus data.
    fn finish_ticket_proposal_turn(&mut self, id: &str) {
        let Some(turn) = self.ticket_turn_text.remove(id) else {
            return;
        };
        if turn.earlier_marker {
            return;
        }
        let Some(content) = crate::ticket::parse_ticket_proposal(&turn.last) else {
            return;
        };
        let Some(task) = self
            .table
            .by_id
            .get(id)
            .filter(|task| Self::is_ticket_chat(task))
        else {
            return;
        };
        let key = (task.repo.clone(), task.number);
        let Some(issue) = self
            .snapshot
            .repos
            .get(&task.repo)
            .and_then(|snapshot| snapshot.issues.get(&task.number))
            .cloned()
        else {
            return;
        };
        let proposal = TicketProposal {
            id: uuid::Uuid::new_v4().to_string(),
            title: content.title,
            body: content.body,
            original_title: issue.title.clone(),
            original_body: issue.body.clone(),
        };
        let Some(conversation) = self.ticket_conversations.get_mut(&key) else {
            return;
        };
        conversation.proposal = Some(proposal.clone());
        self.changed = true;
        if let Some(pusher) = self.ticket_pusher.as_ref() {
            pusher(Push::TicketDetails(TicketDetails {
                request: proposal.id.clone(),
                repo: task.repo.clone(),
                issue,
                proposal: Some(proposal),
                chat_error: self.config.ticket_chat_model().err(),
            }));
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
    fn fire_train(&mut self, repo: &str, prs: &[u64], replace_binding: bool) {
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
        if let Err(e) = self.table.upsert_with_id(
            crate::tasks::ScopedTask {
                id: &id,
                repo,
                stage: Stage::Release,
                kind: ItemKind::Pr,
                number: first,
            },
            log,
            self.now_ms,
        ) {
            eprintln!("the release task {id}: {e:#}");
            self.finish_train(repo, false, false);
            return;
        }
        if replace_binding {
            self.role_bindings.remove(&id);
        }
        self.changed = true;
    }

    /// The number of live sessions of one stage.
    ///
    /// This counts each session until its process reports `Exit`. The daemon
    /// stops the session of a failed task and the parked session that
    /// blocks, yields, or passed the idle limit, so the count stays a true
    /// bound on the stage's live processes.
    fn live_sessions(&self, stage: Stage) -> usize {
        let active = self
            .sessions
            .keys()
            .filter(|id| {
                self.table
                    .by_id
                    .get(*id)
                    .is_some_and(|task| task.stage == stage)
            })
            .count();
        let stopping = self
            .stopping_sessions
            .values()
            .filter(|stopping_stage| **stopping_stage == stage)
            .count();
        active + stopping
    }

    /// Stop one live session and wait for its exit before a replacement.
    fn stop_session(&mut self, id: &str, context: &str) {
        let Some(mut session) = self.sessions.remove(id) else {
            return;
        };
        if let Some(stage) = self.table.by_id.get(id).map(|task| task.stage) {
            self.stopping_sessions.insert(id.to_string(), stage);
        }
        if let Err(error) = session.stop() {
            eprintln!("task {id}: {context}: {error:#}");
        }
    }

    /// Remove restart data after one task becomes terminal.
    fn remove_task_session_marker(&self, task: &Task) {
        let Some(cwd) = self.task_cwd(&task.id) else {
            return;
        };
        if let Err(error) = self.worktrees.remove_task_session(&cwd, &task.id) {
            eprintln!(
                "task {}: cannot remove the session marker: {error:#}",
                task.id
            );
        }
    }

    /// The working directory of a task's run.
    fn task_cwd(&self, id: &str) -> Option<PathBuf> {
        let task = self.table.by_id.get(id)?;
        let repo = self.config.repos.get(&task.repo)?;
        Some(match self.workspace(task) {
            Workspace::Shared => repo.path.clone(),
            Workspace::Exclusive(WorktreeKey::Issue(number)) => {
                self.worktrees.issue_path(repo, number)
            }
            Workspace::Exclusive(WorktreeKey::Pr(number)) => self.worktrees.pr_path(repo, number),
            Workspace::Exclusive(WorktreeKey::Train) => self.worktrees.train_path(repo),
        })
    }

    /// True when the task is an issue-creation session.
    fn is_ticket_creation(task: &Task) -> bool {
        task.purpose == TaskPurpose::TicketCreate
            || (task.stage == Stage::Refine
                && task.kind == ItemKind::Issue
                && task.number == TICKET_NUMBER)
    }

    /// True when the task is an issue conversation.
    fn is_ticket_chat(task: &Task) -> bool {
        task.purpose == TaskPurpose::TicketChat
    }

    /// Select the execution role for one task purpose.
    fn execution_role(task: &Task) -> ExecutionRole {
        if Self::is_ticket_creation(task) {
            ExecutionRole::TicketCreate
        } else if Self::is_ticket_chat(task) {
            ExecutionRole::TicketChat
        } else {
            match task.stage {
                Stage::Refine => ExecutionRole::Refine,
                Stage::Implement => ExecutionRole::Implement,
                Stage::Review => ExecutionRole::Review,
                Stage::Release => ExecutionRole::Release,
            }
        }
    }

    /// Resolve the current typed settings for one task.
    fn resolved_task_role(&self, task: &Task) -> Result<ResolvedRoleSettings> {
        self.role_bindings.get(&task.id).cloned().map_or_else(
            || {
                self.config
                    .resolved_role(Some(&task.repo), Self::execution_role(task).table_name())
            },
            Ok,
        )
    }

    /// Resolve and persist the immutable settings before one first run.
    fn bind_task_role(&mut self, task: &Task) -> Result<ResolvedRoleSettings> {
        if let Some(binding) = self.role_bindings.get(&task.id) {
            return Ok(binding.clone());
        }
        let binding = self
            .config
            .resolved_role(Some(&task.repo), Self::execution_role(task).table_name())?;
        self.role_bindings.insert(task.id.clone(), binding.clone());
        let state = self.collect_state();
        let text = state.to_json()?;
        if let Err(error) = state.save(&self.state_path) {
            self.role_bindings.remove(&task.id);
            return Err(error.context("cannot persist the task role binding"));
        }
        self.saved = Some(text);
        self.changed = true;
        Ok(binding)
    }

    /// Return the runtime actions of the task's resolved harness.
    fn task_capabilities(&self, task: &Task) -> crate::runner::Capabilities {
        self.resolved_task_role(task)
            .map(|role| capabilities(role.settings.harness))
            .unwrap_or(crate::runner::Capabilities {
                live_input: false,
                resume: false,
                permission_responses: false,
            })
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
        if Self::is_ticket_creation(task) {
            return fill_template(
                TICKET_PROMPT,
                &[
                    ("repo", task.repo.clone()),
                    ("owner_repo", repo_cfg.owner_repo.clone()),
                    ("worktree", worktree.display().to_string()),
                ],
            );
        }
        if Self::is_ticket_chat(task) {
            let template = self.ticket_chat_prompt_template()?;
            let issue = self
                .snapshot
                .repos
                .get(&task.repo)
                .and_then(|snapshot| snapshot.issues.get(&task.number))
                .ok_or_else(|| anyhow!("the issue is absent from the current snapshot"))?;
            return fill_template(
                &template,
                &[
                    ("repo", task.repo.clone()),
                    ("owner_repo", repo_cfg.owner_repo.clone()),
                    ("number", task.number.to_string()),
                    ("title", issue.title.clone()),
                    ("body", issue.body.clone()),
                    ("labels", issue.labels.join(", ")),
                    ("author", issue.author.clone()),
                    ("assignees", issue.assignees.join(", ")),
                    ("updated_at", issue.updated_at.clone()),
                    ("github_url", issue.github_url.clone()),
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
        // The linked ticket list of the review prompt: `#4, #9`, or `none`
        // when no ticket links.
        let tickets = match task.stage {
            Stage::Review => self
                .links
                .get(&task.repo)
                .map(|links| links.tickets_of(task.number))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let tickets_text = if tickets.is_empty() {
            "none".to_string()
        } else {
            tickets
                .iter()
                .map(|ticket| format!("#{ticket}"))
                .collect::<Vec<_>>()
                .join(", ")
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
                ("tickets", tickets_text),
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

    /// Read the ticket chat prompt template or use the built-in text.
    fn ticket_chat_prompt_template(&self) -> Result<String> {
        let path = self.prompts_dir.join("ticket-chat.md");
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(TICKET_CHAT_PROMPT.to_string())
            }
            Err(error) => Err(anyhow!("cannot read {}: {error}", path.display())),
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
        let runtime = RuntimeState {
            paused: crate::state::PausedState {
                global: self.paused.global,
                stages: self.paused.stages.clone(),
                lanes: self
                    .paused
                    .lanes
                    .iter()
                    .map(|((stage, repo), paused)| crate::state::LanePauseEntry {
                        stage: *stage,
                        repo: repo.clone(),
                        paused: *paused,
                    })
                    .collect(),
                tasks: self.paused.tasks.clone(),
            },
            tasks: self
                .table
                .order
                .iter()
                .filter_map(|id| self.table.by_id.get(id).cloned())
                .collect(),
            pending_chats: self.pending_chats.clone(),
            review_tickets: self.review_tickets.clone(),
            release_batches: self.release_batches.clone(),
            stuck: self
                .decisions
                .open()
                .iter()
                .filter(|row| matches!(row.kind, DecisionKind::Stuck { .. }))
                .cloned()
                .collect(),
        };
        DaemonState {
            stage_limits,
            lanes,
            policies: self.policies.clone(),
            last_fire_ms,
            ticket_conversations: self.ticket_conversations.values().cloned().collect(),
            role_bindings: self
                .role_bindings
                .iter()
                .filter(|(id, _)| {
                    self.table
                        .by_id
                        .get(*id)
                        .is_none_or(|task| task.state != TaskState::Done)
                })
                .map(|(id, binding)| (id.clone(), binding.clone()))
                .collect(),
            runtime,
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

/// True when assistant text contains a full or partial proposal marker.
fn proposal_marker_text(text: &str) -> bool {
    text.contains("<aif") || text.contains("</aif") || text.contains("aif-ticket")
}

/// True when GitHub shows the completed refine transition.
fn refine_transitioned(fresh: &RepoSnapshot, number: u64) -> bool {
    fresh.issues.get(&number).is_some_and(|issue| {
        issue.open
            && issue.labels.iter().any(|label| label == "refined")
            && !issue.labels.iter().any(|label| label == "to-refine")
    })
}

/// True when GitHub shows a pull request for the implemented ticket.
///
/// The links table holds the open pull requests of the last poll, so the
/// branch rule and the body rule both count here.
fn implementation_transitioned(fresh: &RepoSnapshot, links: &Links, number: u64) -> bool {
    fresh.issues.get(&number).is_some_and(|issue| {
        issue.open
            && !issue.labels.iter().any(|label| label == "refined")
            && !issue.labels.iter().any(|label| label == "to-refine")
    }) && !links.prs_of(number).is_empty()
}

/// True when GitHub shows the completed review transition.
fn review_transitioned(fresh: &RepoSnapshot, number: u64) -> bool {
    fresh
        .prs
        .get(&number)
        .is_some_and(|pr| pr.open && !pr.draft)
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

/// Pack one control action for the shared inbound queue.
fn inbound_action(action: Action) -> Inbound {
    Inbound::Act(Box::new(action))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionRole, Harness, RoleSettings, StageConfig};
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, Pr, RepoSnapshot};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    /// The five git calls of a fresh issue worktree: the create path first
    /// prunes broken registrations, then cuts the branch from `HEAD`.
    fn fresh_issue_steps(repo: &Path, worktree: &Path, number: u64, gitdir: &Path) -> Vec<Step> {
        let reference = format!("refs/heads/aif/borsuk/issue-{number}");
        let branch = format!("aif/borsuk/issue-{number}");
        let wt_text = worktree.to_string_lossy().into_owned();
        vec![
            git_step(repo, &["worktree", "prune"], CmdOut::ok("")),
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

    /// The git calls of a first dispatch into a PR worktree: prune broken
    /// registrations, cut the branch from `HEAD`, prepare the markers, then
    /// fetch the GitHub pull ref in the worktree and reset the branch hard
    /// to it.
    fn fresh_pr_steps(repo: &Path, worktree: &Path, number: u64, gitdir: &Path) -> Vec<Step> {
        let reference = format!("refs/heads/aif/borsuk/pr-{number}");
        let branch = format!("aif/borsuk/pr-{number}");
        let pull_ref = format!("pull/{number}/head");
        let wt_text = worktree.to_string_lossy().into_owned();
        vec![
            git_step(repo, &["worktree", "prune"], CmdOut::ok("")),
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
            git_step(
                worktree,
                &["fetch", "origin", pull_ref.as_str()],
                CmdOut::ok(""),
            ),
            git_step(worktree, &["reset", "--hard", "FETCH_HEAD"], CmdOut::ok("")),
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

    /// The five git calls of a fresh train worktree: the create path prunes
    /// broken registrations before it checks the branch.
    fn fresh_train_steps(repo: &Path, worktree: &Path, gitdir: &Path) -> Vec<Step> {
        let wt_text = worktree.to_string_lossy().into_owned();
        vec![
            git_step(
                repo,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                refused(),
            ),
            git_step(repo, &["worktree", "prune"], CmdOut::ok("")),
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
        answers: Arc<Mutex<Vec<(String, Answer)>>>,
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
            self.handle
                .answers
                .lock()
                .unwrap()
                .push((request_id.to_string(), answer));
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

    /// A factory that records resolved roles and returns fake sessions.
    struct FakeRunnerFactory {
        jobs: Arc<Mutex<Vec<Job>>>,
        sessions: Arc<Mutex<Vec<SessionHandle>>>,
        roles: Arc<Mutex<Vec<ResolvedRoleSettings>>>,
    }

    impl RunnerFactory for FakeRunnerFactory {
        fn build(&self, role: &ResolvedRoleSettings) -> Box<dyn Runner> {
            self.roles.lock().unwrap().push(role.clone());
            Box::new(FakeRunner::new(self.jobs.clone(), self.sessions.clone()))
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

    /// The worktree path of one PR inside a rig root.
    fn pr_wt(dir: &Path, number: u64) -> PathBuf {
        dir.join("state")
            .join("worktrees")
            .join("borsuk")
            .join(format!("pr-{number}"))
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
        let role_settings = RoleSettings {
            harness: Harness::Claude,
            program: "claude".to_string(),
            model: "m".to_string(),
            effort: None,
            extra_args: Vec::new(),
            agent: None,
            profile: None,
            permission_mode: Some("bypassPermissions".to_string()),
            permission_handler: None,
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            strict_mcp: None,
            auto_approve: None,
            approval_policy: None,
            sandbox: None,
        };
        let roles = ExecutionRole::ALL
            .into_iter()
            .map(|role| {
                let mut settings = role_settings.clone();
                if role == ExecutionRole::TicketChat {
                    settings.permission_mode = Some("manual".to_string());
                    settings.permission_handler = Some("inbox".to_string());
                    settings.tools =
                        vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
                }
                (role, settings)
            })
            .collect();
        let mut repos = BTreeMap::new();
        repos.insert(
            "borsuk".to_string(),
            RepoConfig {
                alias: "borsuk".to_string(),
                path: dir.join("repo"),
                owner_repo: "acme/borsuk".to_string(),
                lanes: BTreeMap::new(),
                release: ReleasePolicy::Manual,
                theory: crate::config::TheoryConfig::default(),
                role_overrides: BTreeMap::new(),
            },
        );
        Config {
            schema_version: 1,
            roles,
            stages,
            repos,
            ticket_chat: crate::config::TicketChatConfig {
                model: Some("m".to_string()),
            },
        }
    }

    /// A valid editable config with the same repository layout as a rig.
    fn settings_config_text(repo: &Path, model: &str) -> String {
        format!(
            "schema_version = 1\n\
             \n[stage.refine]\nharness = \"claude\"\nmodel = \"{model}\"\nlimit = 2\n\
             \n[stage.implement]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 1\n\
             \n[stage.review]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 2\n\
             \n[stage.release]\nharness = \"claude\"\nmodel = \"m\"\nlimit = 1\n\
             \n[ticket.create]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[ticket.chat]\nharness = \"claude\"\nmodel = \"m\"\npermission_mode = \"manual\"\npermission_handler = \"inbox\"\ntools = [\"Read\", \"Glob\", \"Grep\"]\n\
             \n[repo.borsuk]\npath = \"{}\"\n",
            repo.display()
        )
    }

    fn set_role_harness(config: &mut Config, role: ExecutionRole, harness: Harness) {
        let settings = config.roles.get_mut(&role).unwrap();
        settings.harness = harness;
        settings.program = harness.program().to_string();
        settings.agent = (harness == Harness::Opencode).then(|| "build".to_string());
        settings.profile = None;
        settings.permission_mode = None;
        settings.permission_handler = None;
        settings.tools.clear();
        settings.disallowed_tools.clear();
        settings.strict_mcp = None;
        settings.auto_approve = (harness == Harness::Opencode).then_some(true);
        settings.approval_policy = None;
        settings.sandbox = None;
    }

    /// One open issue.
    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: format!("issue {number}"),
            body: format!("body {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "author".to_string(),
            assignees: Vec::new(),
            updated_at: format!("2026-08-{number:02}T12:00:00Z"),
            github_url: format!("https://github.com/acme/borsuk/issues/{number}"),
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
            head_ref: format!("aif/borsuk/issue-{number}"),
        }
    }

    /// A daemon over fake runners, a scripted command runner, and a pinned
    /// clock.
    struct Rig {
        daemon: Daemon,
        exec: Arc<ScriptExec>,
        jobs: Arc<Mutex<Vec<Job>>>,
        sessions: Arc<Mutex<Vec<SessionHandle>>>,
        roles: Arc<Mutex<Vec<ResolvedRoleSettings>>>,
        wake_rx: Receiver<()>,
        t: Arc<Mutex<u64>>,
        repo: PathBuf,
        prompts: PathBuf,
    }

    impl Rig {
        fn make(steps: Vec<Step>) -> Rig {
            Self::build(temp_root(), steps, |_| {}, false)
        }

        fn make_with(steps: Vec<Step>, tweak: impl FnOnce(&mut Config)) -> Rig {
            Self::build(temp_root(), steps, tweak, false)
        }

        /// A daemon that starts with the whole factory paused.
        fn make_paused(steps: Vec<Step>) -> Rig {
            Self::build(temp_root(), steps, |_| {}, true)
        }

        fn make_in(dir: PathBuf, steps: Vec<Step>, tweak: impl FnOnce(&mut Config)) -> Rig {
            Self::build(dir, steps, tweak, false)
        }

        /// A daemon in `dir` that starts with the whole factory paused.
        fn make_in_paused(dir: PathBuf, steps: Vec<Step>) -> Rig {
            Self::build(dir, steps, |_| {}, true)
        }

        fn build(
            dir: PathBuf,
            steps: Vec<Step>,
            tweak: impl FnOnce(&mut Config),
            paused: bool,
        ) -> Rig {
            fs::create_dir_all(dir.join("repo")).unwrap();
            let state = dir.join("state");
            let prompts = dir.join("prompts");
            let mut config = test_config(&dir);
            tweak(&mut config);
            let exec = scripted(steps);
            let jobs = Arc::new(Mutex::new(Vec::new()));
            let sessions = Arc::new(Mutex::new(Vec::new()));
            let roles = Arc::new(Mutex::new(Vec::new()));
            let (_poll_tx, poll_rx) = mpsc::channel::<DaemonMsg>();
            let (wake_tx, wake_rx) = mpsc::channel::<()>();
            let mut wake = BTreeMap::new();
            wake.insert("borsuk".to_string(), wake_tx);
            let (_action_tx, action_rx) = mpsc::channel();
            let t = Arc::new(Mutex::new(T0));
            let runner_factory = Arc::new(FakeRunnerFactory {
                jobs: jobs.clone(),
                sessions: sessions.clone(),
                roles: roles.clone(),
            });
            let mut daemon = Daemon::with_runner_factory(
                config,
                dir.join("factory.toml"),
                String::new(),
                exec.clone(),
                state.clone(),
                prompts.clone(),
                poll_rx,
                wake,
                action_rx,
                runner_factory,
                paused,
            );
            let clock_t = t.clone();
            daemon.clock = Arc::new(move || *clock_t.lock().unwrap());
            Rig {
                daemon,
                exec,
                jobs,
                sessions,
                roles,
                wake_rx,
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
            let started_ms = *self.t.lock().unwrap();
            self.poll_started(issues, prs, started_ms);
        }

        /// Apply one poll with an explicit start time.
        fn poll_started(&mut self, issues: Vec<Issue>, prs: Vec<Pr>, started_ms: u64) {
            let mut issue_map = BTreeMap::new();
            for one in issues {
                issue_map.insert(one.number, one);
            }
            let mut pr_map = BTreeMap::new();
            for one in prs {
                pr_map.insert(one.number, one);
            }
            self.daemon.handle(Inbound::Poll(DaemonMsg::Polled {
                started_ms,
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
            self.daemon.handle(Inbound::Act(Box::new(action)));
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
    fn a_paused_daemon_polls_but_dispatches_nothing() {
        let mut rig = Rig::make_paused(vec![]);
        assert!(rig.daemon.paused.global);

        // The first poll fires the gate; the pause blocks the dispatch.
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(rig.job_count(), 0);
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Queued);

        // A further drive with no new message still dispatches nothing.
        rig.drive();
        assert_eq!(rig.job_count(), 0);
    }

    #[test]
    fn a_paused_daemon_dispatches_when_the_operator_resumes() {
        let mut rig = Rig::make_paused(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(rig.job_count(), 0);

        rig.act(Action::Pause {
            scope: PauseScope::Global,
            paused: false,
        });
        assert!(!rig.daemon.paused.global);
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/refine-i142");
    }

    #[test]
    fn a_task_resume_override_dispatches_only_that_task() {
        let mut rig = Rig::make_paused(vec![]);
        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        assert_eq!(rig.job_count(), 0);

        rig.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/refine-i143".to_string(),
            },
            paused: false,
        });

        assert!(rig.daemon.paused.global);
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/refine-i143");
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Queued);
    }

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
        // The ask block of the refine prompt holds literal braces, so the
        // pin checks for unfilled placeholders, not for bare braces.
        assert!(scan_placeholders(&job.prompt).is_empty());

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
        assert!(rig.daemon.table.by_id.contains_key("borsuk/refine-i142"));
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
        assert_eq!(
            rig.job_count(),
            1,
            "the replacement waits for the stopped process exit"
        );

        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the stopped process exited",
        ));

        assert_eq!(rig.job_count(), 2);
    }

    #[test]
    fn one_dispatch_error_does_not_block_another_stage() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 143),
            143,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("refine.md"), "bad {unknown}").unwrap();

        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["refined"])],
            vec![],
        );

        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/implement-i143");
        assert_eq!(rig.task("borsuk/refine-i142").attempt, 2);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Queued);
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
    fn a_dispatch_failure_appends_its_reason_to_the_task_log() {
        let dir = temp_root();
        let steps = vec![git_step(
            &rig_repo(&dir),
            &["worktree", "prune"],
            CmdOut {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: not a git repository\n".to_string(),
            },
        )];
        let mut rig = Rig::make_in(dir, steps, |_| {});

        rig.poll(vec![issue(142, &["refined"])], vec![]);

        assert_eq!(rig.job_count(), 0, "the runner never started");
        let task = rig.task("borsuk/implement-i142");
        let log =
            fs::read_to_string(&task.log_path).expect("the dispatch failure must write the log");
        assert!(
            log.contains("aif: dispatch failed: cannot prepare the worktree"),
            "log: {log}"
        );
        assert!(
            log.contains("git worktree prune failed: fatal: not a git repository"),
            "the git error text must reach the log: {log}"
        );
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
    fn a_review_uses_the_issue_worktree_named_by_the_pull_head() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        let mut pull = pr(5, true, &[]);
        pull.head_ref = "aif/borsuk/issue-142".to_string();

        rig.poll(vec![], vec![pull]);

        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).cwd, issue_wt(&dir, 142));
    }

    #[test]
    fn a_review_prompt_names_the_tickets_the_pr_closes() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps = fresh_pr_steps(&rig_repo(&dir), &pr_worktree, 7, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #4 fixes #9".to_string();

        rig.poll(vec![], vec![pull]);

        assert_eq!(rig.job_count(), 1);
        assert!(
            rig.job(0).prompt.contains("Tickets this PR closes: #4, #9"),
            "prompt:\n{}",
            rig.job(0).prompt
        );
    }

    #[test]
    fn an_unlinked_review_prompt_says_none() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps = fresh_pr_steps(&rig_repo(&dir), &pr_worktree, 7, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = String::new();

        rig.poll(vec![], vec![pull]);

        assert_eq!(rig.job_count(), 1);
        assert!(
            rig.job(0).prompt.contains("Tickets this PR closes: none"),
            "prompt:\n{}",
            rig.job(0).prompt
        );
    }

    #[test]
    fn a_review_of_a_multi_ticket_pr_runs_in_the_pr_worktree() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps = fresh_pr_steps(&rig_repo(&dir), &pr_worktree, 7, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #4 and fixes #9".to_string();

        rig.poll(vec![], vec![pull]);

        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/review-p7");
        assert_eq!(rig.job(0).cwd, pr_worktree);
    }

    #[test]
    fn a_review_of_a_foreign_branch_pr_starts_without_a_refusal() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps = fresh_pr_steps(&rig_repo(&dir), &pr_worktree, 7, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = String::new();

        rig.poll(vec![], vec![pull]);

        // The old daemon refused this PR for its branch. The task starts now.
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/review-p7");
        assert_eq!(rig.task("borsuk/review-p7").state, TaskState::Running);
        assert_eq!(rig.job(0).cwd, pr_worktree);
    }

    #[test]
    fn a_review_waits_while_a_linked_ticket_still_implements() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "aif/borsuk/issue-142".to_string();
        pull.body = "Closes #142".to_string();

        rig.poll(vec![issue(142, &["refined"])], vec![pull]);

        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.job(0).task, "borsuk/implement-i142");
        assert_eq!(rig.task("borsuk/review-p7").state, TaskState::Queued);
    }

    #[test]
    fn a_review_without_links_never_waits_on_a_prior_stage() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps: Vec<Step> = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        )
        .into_iter()
        .chain(fresh_pr_steps(
            &rig_repo(&dir),
            &pr_worktree,
            7,
            &rig_gitdir(&dir),
        ))
        .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = String::new();

        rig.poll(vec![issue(142, &["refined"])], vec![pull]);

        assert_eq!(
            rig.job_count(),
            2,
            "the zero-link review starts beside the implement"
        );
        assert_eq!(rig.job(0).task, "borsuk/implement-i142");
        assert_eq!(rig.job(1).task, "borsuk/review-p7");
        assert_eq!(rig.job(1).cwd, pr_worktree);
    }

    #[test]
    fn two_reviews_of_prs_of_one_ticket_share_one_worktree_serially() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut first = pr(7, true, &[]);
        first.head_ref = "aif/borsuk/issue-5".to_string();
        first.body = "Closes #5".to_string();
        let mut second = pr(9, true, &[]);
        second.head_ref = "aif/borsuk/issue-5".to_string();
        second.body = "Closes #5".to_string();

        rig.poll(vec![], vec![first, second]);

        // Both PRs link only ticket 5, so both reviews map to the ticket
        // worktree. One runs; the other waits for the worktree.
        assert_eq!(rig.job_count(), 1);
        let running = rig.job(0).task.clone();
        let waiting = if running == "borsuk/review-p7" {
            "borsuk/review-p9"
        } else {
            "borsuk/review-p7"
        };
        assert_eq!(rig.task(waiting).state, TaskState::Queued);
    }

    #[test]
    fn a_pr_worktree_review_writes_its_session_marker_in_place() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 7);
        let steps = fresh_pr_steps(&rig_repo(&dir), &pr_worktree, 7, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #4 fixes #9".to_string();

        rig.poll(vec![], vec![pull]);
        rig.event(started("borsuk/review-p7", "sid-1"));

        // The Started event writes the marker through task_cwd, so the
        // marker proves the daemon resolved the PR worktree for the review.
        assert_eq!(
            rig.daemon
                .worktrees
                .read_task_session(&pr_worktree, "borsuk/review-p7")
                .unwrap()
                .as_deref(),
            Some("sid-1")
        );
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
    fn a_retry_keeps_the_review_head_sha() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        for _ in 0..3 {
            rig.event(exited("borsuk/review-p5", false, "boom"));
        }
        assert!(rig.decision("stuck:borsuk/review-p5:3").is_some());

        rig.act(Action::Retry {
            task: "borsuk/review-p5".to_string(),
        });

        assert_eq!(
            rig.task("borsuk/review-p5").head_sha.as_deref(),
            Some("sha5"),
            "the retry reviews the same head as the failed task"
        );

        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));

        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = worktree.join(".aif").join("reviewed-sha");
        assert_eq!(fs::read_to_string(marker).unwrap().trim_end(), "sha5");
    }

    #[test]
    fn a_review_without_a_stored_sha_takes_the_polled_head() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/review-p5")
            .unwrap()
            .head_sha = None;

        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));

        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Done,
            "a missing sha must not discard a finished review"
        );
        let marker = worktree.join(".aif").join("reviewed-sha");
        assert_eq!(fs::read_to_string(marker).unwrap().trim_end(), "sha5");
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
        assert_eq!(
            answers,
            vec![
                (
                    "req-1".to_string(),
                    Answer::Allow {
                        updated_input: None,
                    },
                ),
                (
                    "req-2".to_string(),
                    Answer::Deny {
                        message: "not now".to_string(),
                    },
                ),
            ]
        );
        assert!(rig.decision("perm:borsuk/refine-i142:req-1").is_none());
    }

    #[test]
    fn a_question_answer_reaches_the_runner_verbatim() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "q-answers".to_string(),
            tool: "AskUserQuestion".to_string(),
            input: json!({"questions": [{"question": "Which database?"}]}),
            suggestions: serde_json::Value::Null,
            needs_human: true,
        });
        let updated_input = json!({
            "questions": [{
                "question": "Which database?",
                "header": "Database",
                "multiSelect": false,
                "options": [{"label": "Postgres", "description": "Use Postgres."}],
                "answer": "Postgres"
            }],
            "keep_exactly": [1, "two", {"three": true}]
        });

        rig.act(Action::Answer {
            decision_id: "perm:borsuk/refine-i142:q-answers".to_string(),
            response: Response::Answers {
                updated_input: updated_input.clone(),
            },
        });

        assert_eq!(
            rig.session(0).answers.lock().unwrap().as_slice(),
            &[(
                "q-answers".to_string(),
                Answer::Allow {
                    updated_input: Some(updated_input),
                },
            )]
        );
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
    fn turn_end_parks_a_session_and_the_queued_task_starts_after_exit() {
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
        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the parked session receives a stop request"
        );
        assert_eq!(
            rig.job_count(),
            1,
            "the old process holds the slot until its exit"
        );
        assert_eq!(rig.task("borsuk/refine-i143").state, TaskState::Queued);
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);

        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the yielding process exited",
        ));
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser,
            "the task stays parked and resumable"
        );
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/refine-i143");
        assert_eq!(rig.task("borsuk/refine-i143").state, TaskState::Running);
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_failed_turn_returns_the_live_slot() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);

        rig.event(turn_finished(
            "borsuk/implement-i142",
            false,
            "the turn failed",
        ));

        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the failed task loses its live process"
        );
        assert_eq!(
            rig.daemon.live_sessions(Stage::Implement),
            1,
            "the stopping process still holds the live slot"
        );
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
    }

    #[test]
    fn the_last_failed_attempt_returns_the_live_slot() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir))
                .into_iter()
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &worktree,
                    &rig_gitdir(&dir),
                ))
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &worktree,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);

        for attempt in 0..tasks::MAX_ATTEMPTS {
            let last = attempt + 1 == tasks::MAX_ATTEMPTS;
            rig.event(turn_finished(
                "borsuk/implement-i142",
                false,
                "the turn failed",
            ));
            assert!(
                rig.session(attempt as usize).stopped.load(Ordering::SeqCst),
                "attempt {} stops its live process",
                attempt + 1
            );
            assert_eq!(rig.daemon.live_sessions(Stage::Implement), 1);
            if last {
                assert_eq!(
                    rig.task("borsuk/implement-i142").state,
                    TaskState::Failed("the turn failed".to_string())
                );
            } else {
                assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
                rig.event(exited(
                    "borsuk/implement-i142",
                    false,
                    "the stopped process exited",
                ));
                assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
            }
        }
        assert!(
            rig.decision("stuck:borsuk/implement-i142:3").is_some(),
            "the last failure opens a stuck row"
        );
        rig.event(exited(
            "borsuk/implement-i142",
            false,
            "the final stopped process exited",
        ));
        assert_eq!(rig.daemon.live_sessions(Stage::Implement), 0);
    }

    #[test]
    fn the_retried_task_starts_after_the_old_process_exits() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir))
                .into_iter()
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &worktree,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);

        rig.event(turn_finished(
            "borsuk/implement-i142",
            false,
            "the turn failed",
        ));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        rig.drive();
        assert_eq!(
            rig.job_count(),
            1,
            "the exit of the stopped process holds the retry back"
        );

        rig.event(exited(
            "borsuk/implement-i142",
            false,
            "the stopped process exited",
        ));

        assert_eq!(rig.job_count(), 2, "the exit frees the retry");
        assert_eq!(rig.job(1).task, "borsuk/implement-i142");
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
    }

    #[test]
    fn a_parked_refine_turn_requests_an_immediate_github_poll() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);

        rig.event(turn_ended("borsuk/refine-i142"));

        rig.wake_rx
            .try_recv()
            .expect("the turn end must wake its repository poller");
    }

    #[test]
    fn a_refined_poll_completes_the_parked_refine_task() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: true,
        });
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));

        rig.poll(vec![issue(142, &["refined"])], vec![]);

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the completed task must release its live process"
        );
    }

    #[test]
    fn a_turn_end_completes_a_refine_when_the_poll_won_the_race() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);

        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(rig.job_count(), 1, "implement waits for the refine result");
        rig.event(turn_ended("borsuk/refine-i142"));

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/implement-i142");
    }

    #[test]
    fn a_draft_pull_poll_keeps_the_implementation_until_runner_success() {
        let dir = temp_root();
        let steps: Vec<Step> = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        )
        .into_iter()
        .chain(reuse_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            &rig_gitdir(&dir),
        ))
        .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        let session = rig.session(0);
        let mut pull = pr(5, true, &[]);
        pull.head_ref = "aif/borsuk/issue-142".to_string();

        rig.poll(vec![issue(142, &[])], vec![pull]);

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
        assert!(!session.stopped.load(Ordering::SeqCst));
        assert_eq!(rig.job_count(), 1, "review waits for the implement result");
        rig.event(turn_finished("borsuk/implement-i142", true, "done"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/review-p5");
    }

    #[test]
    fn a_live_chat_marks_the_parked_task_running() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "continue with Postgres".to_string(),
        });

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(
            rig.session(0).sends.lock().unwrap().as_slice(),
            &["continue with Postgres".to_string()]
        );
        assert_eq!(rig.daemon.next_deadline(), None);
    }

    #[test]
    fn a_reaped_chat_waits_for_a_live_process_slot() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().limit = 1;
        });
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        rig.event(turn_ended("borsuk/refine-i142"));
        rig.set_now(T0 + 31 * 60_000);
        rig.drive();
        assert_eq!(rig.job_count(), 1);
        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the reaped process exited",
        ));
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 0);

        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.task("borsuk/refine-i143").state, TaskState::Running);

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "resume this exact message".to_string(),
        });

        assert_eq!(rig.job_count(), 2, "the running task keeps the only slot");
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );

        rig.event(exited(
            "borsuk/refine-i143",
            false,
            "the active process exited",
        ));

        assert_eq!(rig.job_count(), 3);
        assert_eq!(rig.job(2).task, "borsuk/refine-i142");
        assert_eq!(rig.job(2).prompt, "resume this exact message");
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_parked_session_yields_its_slot_to_a_chat_resume() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().limit = 1;
        });
        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        rig.event(started("borsuk/refine-i142", "sid-142"));
        // The park of 142 frees its slot, and the queued 143 takes it.
        rig.event(turn_ended("borsuk/refine-i142"));
        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the yielding process exited",
        ));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert_eq!(rig.task("borsuk/refine-i143").state, TaskState::Running);
        rig.event(started("borsuk/refine-i143", "sid-143"));
        rig.event(turn_ended("borsuk/refine-i143"));
        assert_eq!(
            rig.task("borsuk/refine-i143").state,
            TaskState::AwaitingUser
        );
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "resume after the parked session".to_string(),
        });

        assert!(
            rig.session(1).stopped.load(Ordering::SeqCst),
            "the parked session of 143 yields its slot to the chat resume"
        );
        assert_eq!(rig.job_count(), 2, "the stopping process holds its slot");
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert_eq!(
            rig.task("borsuk/refine-i143").state,
            TaskState::AwaitingUser,
            "the yielding task stays parked and resumable"
        );
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);

        rig.event(exited(
            "borsuk/refine-i143",
            false,
            "the yielding process exited",
        ));

        assert_eq!(rig.job_count(), 3);
        assert_eq!(rig.job(2).task, "borsuk/refine-i142");
        assert_eq!(rig.job(2).prompt, "resume after the parked session");
        assert_eq!(rig.job(2).resume.as_deref(), Some("sid-142"));
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_paused_chat_does_not_stop_an_unrelated_parked_session() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().limit = 1;
        });
        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        rig.event(started("borsuk/refine-i142", "sid-142"));
        rig.event(turn_ended("borsuk/refine-i142"));
        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the yielding process exited",
        ));
        rig.event(started("borsuk/refine-i143", "sid-143"));
        rig.event(turn_ended("borsuk/refine-i143"));

        rig.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/refine-i142".to_string(),
            },
            paused: true,
        });
        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "wait until the pause ends".to_string(),
        });

        assert!(
            !rig.session(1).stopped.load(Ordering::SeqCst),
            "the blocked chat cannot take another task's live slot"
        );
        assert_eq!(rig.job_count(), 2);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["wait until the pause ends".to_string()][..])
        );
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_parked_session_with_a_queued_chat_keeps_its_process() {
        let mut rig = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().limit = 1;
        });
        rig.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        assert_eq!(rig.job_count(), 1);
        // The session refuses the message, so the chat queues for the
        // resumed turn while the live session stays parked.
        rig.session(0).fail_send.store(true, Ordering::SeqCst);
        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "queued for the parked turn".to_string(),
        });
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);

        rig.event(turn_ended("borsuk/refine-i142"));

        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert!(
            !rig.session(0).stopped.load(Ordering::SeqCst),
            "the queued chat message keeps the process alive"
        );
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["queued for the parked turn".to_string()][..])
        );
        assert_eq!(rig.job_count(), 1, "the queued task stays back");
        assert_eq!(rig.task("borsuk/refine-i143").state, TaskState::Queued);
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 1);
    }

    #[test]
    fn a_pause_stops_the_parked_process_it_blocks() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );
        assert!(!rig.session(0).stopped.load(Ordering::SeqCst));

        rig.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/refine-i142".to_string(),
            },
            paused: true,
        });

        assert!(
            rig.session(0).stopped.load(Ordering::SeqCst),
            "the pause stops the parked process it blocks"
        );
        assert_eq!(
            rig.daemon.live_sessions(Stage::Refine),
            1,
            "the stopping process keeps the slot until its exit"
        );
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser,
            "the task stays parked and resumable"
        );
        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the paused process exited",
        ));
        assert_eq!(rig.daemon.live_sessions(Stage::Refine), 0);
    }

    #[test]
    fn a_pause_keeps_a_running_process() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);

        rig.act(Action::Pause {
            scope: PauseScope::Global,
            paused: true,
        });

        assert!(
            !rig.session(0).stopped.load(Ordering::SeqCst),
            "the running process stays alive under a pause"
        );
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
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
    fn a_restart_resumes_the_claude_session_of_the_same_task() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir));
        let mut first = Rig::make_in(dir.clone(), steps, |_| {});
        first.poll(vec![issue(142, &["refined"])], vec![]);
        first.event(started("borsuk/implement-i142", "session-142"));
        drop(first);

        let steps = reuse_issue_steps(&rig_repo(&dir), &worktree, &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |_| {});
        second.poll(vec![issue(142, &["refined"])], vec![]);

        assert_eq!(second.job_count(), 1);
        assert_eq!(second.job(0).resume.as_deref(), Some("session-142"));
        second.event(turn_finished("borsuk/implement-i142", true, "done"));
        assert_eq!(
            second
                .daemon
                .worktrees
                .read_task_session(&worktree, "borsuk/implement-i142")
                .unwrap(),
            None
        );
    }

    #[test]
    fn concurrent_refine_tasks_resume_their_own_sessions() {
        let dir = temp_root();
        let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
        first.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );
        first.event(started("borsuk/refine-i142", "session-142"));
        first.event(started("borsuk/refine-i143", "session-143"));
        drop(first);

        let mut second = Rig::make_in(dir, vec![], |_| {});
        second.poll(
            vec![issue(142, &["to-refine"]), issue(143, &["to-refine"])],
            vec![],
        );

        let resumes: BTreeMap<String, Option<String>> = (0..second.job_count())
            .map(|index| {
                let job = second.job(index);
                (job.task, job.resume)
            })
            .collect();
        assert_eq!(
            resumes["borsuk/refine-i142"].as_deref(),
            Some("session-142")
        );
        assert_eq!(
            resumes["borsuk/refine-i143"].as_deref(),
            Some("session-143")
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
        assert_eq!(first.daemon.trains["borsuk"].last_fire_ms, Some(T0));
        let state_text = fs::read_to_string(dir.join("state").join("state.json")).unwrap();
        assert!(state_text.contains("last_fire_ms"));
        drop(first);

        let steps = reuse_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir))
            .into_iter()
            .chain(vec![gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/2/labels/release-stacked",
                ],
                gh_ok(),
            )])
            .collect();
        let mut second = Rig::make_in(dir, steps, |_| {});
        assert_eq!(
            second.daemon.trains["borsuk"].last_fire_ms,
            Some(T0),
            "the restart restores last_fire_ms before the first drive"
        );
        assert_eq!(
            second.job_count(),
            0,
            "the restore dispatches nothing before the first message"
        );
        second.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        assert_eq!(
            second.job_count(),
            1,
            "the interrupted release task resumes once"
        );
        assert_eq!(second.job(0).task, "borsuk/release");
        assert!(
            second.job(0).prompt.contains("#2"),
            "the restored batch feeds the release prompt:\n{}",
            second.job(0).prompt
        );
        assert_eq!(
            second.daemon.trains["borsuk"].last_fire_ms,
            Some(T0),
            "the restart does not re-fire the train"
        );
        assert!(
            second.exec.calls().iter().all(|call| call.program == "git"),
            "the resume touches no GitHub call"
        );

        second.event(turn_finished("borsuk/release", true, "released"));
        assert_eq!(second.task("borsuk/release").state, TaskState::Done);

        second.set_now(T0 + 61 * 60_000);
        second.poll(vec![], vec![pr(2, false, &[])]);
        assert_eq!(
            second.job_count(),
            1,
            "the finished batch never releases again"
        );
        assert_eq!(
            second.daemon.trains["borsuk"].last_fire_ms,
            Some(T0),
            "an empty queue produces no fire, whatever the interval"
        );
    }

    #[test]
    fn a_paused_factory_starts_paused_after_a_restart() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.act(Action::Pause {
                scope: PauseScope::Global,
                paused: true,
            });
            assert!(first.daemon.paused.global);
        }

        let mut second = Rig::make_in(dir, vec![], |_| {});
        assert!(
            second.daemon.paused.global,
            "the saved pause mark survives the restart"
        );
        second.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(second.job_count(), 0, "the restored pause holds the work");

        second.act(Action::Pause {
            scope: PauseScope::Global,
            paused: false,
        });
        assert_eq!(
            second.job_count(),
            1,
            "the lift dispatches the admitted task"
        );
    }

    #[test]
    fn a_restored_stage_pause_survives_a_flagless_restart() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.act(Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Implement,
                },
                paused: true,
            });
            first.act(Action::Pause {
                scope: PauseScope::Lane {
                    stage: Stage::Review,
                    repo: "borsuk".to_string(),
                },
                paused: true,
            });
        }

        let second = Rig::make_in(dir, vec![], |_| {});
        assert!(!second.daemon.paused.global);
        assert_eq!(
            second.daemon.paused.stages.get(&Stage::Implement),
            Some(&true)
        );
        assert_eq!(
            second
                .daemon
                .paused
                .lanes
                .get(&(Stage::Review, "borsuk".to_string())),
            Some(&true)
        );
    }

    #[test]
    fn a_paused_flag_forces_the_global_mark_over_the_restored_marks() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.act(Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Implement,
                },
                paused: true,
            });
            assert!(!first.daemon.paused.global);
        }

        let second = Rig::make_in_paused(dir, vec![]);
        assert!(
            second.daemon.paused.global,
            "the flag sets the global mark on top"
        );
        assert!(
            second.daemon.paused.stages.is_empty(),
            "the global mark clears the narrower restored marks"
        );
    }

    #[test]
    fn the_restore_drops_tasks_of_repositories_that_left_the_config() {
        let dir = temp_root();
        let path = dir.join("state").join("state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let kept = Task::new(
            "borsuk",
            Stage::Refine,
            ItemKind::Issue,
            1,
            PathBuf::from("logs/kept.jsonl"),
            1_000,
        );
        let gone = Task::new(
            "gone",
            Stage::Refine,
            ItemKind::Issue,
            5,
            PathBuf::from("logs/gone.jsonl"),
            1_000,
        );
        let mut state = DaemonState::default();
        state.runtime.tasks = vec![kept, gone];
        state
            .runtime
            .pending_chats
            .insert("gone/refine-i5".to_string(), vec!["hello".to_string()]);
        state
            .runtime
            .paused
            .tasks
            .insert("gone/refine-i5".to_string(), true);
        state.runtime.paused.lanes.extend([
            crate::state::LanePauseEntry {
                stage: Stage::Review,
                repo: "borsuk".to_string(),
                paused: false,
            },
            crate::state::LanePauseEntry {
                stage: Stage::Review,
                repo: "gone".to_string(),
                paused: true,
            },
        ]);
        state.save(&path).unwrap();

        let rig = Rig::make_in(dir, vec![], |_| {});

        assert!(rig.daemon.table.by_id.contains_key("borsuk/refine-i1"));
        assert!(
            !rig.daemon.table.by_id.contains_key("gone/refine-i5"),
            "the repository left the config, so its task goes"
        );
        assert!(!rig.daemon.pending_chats.contains_key("gone/refine-i5"));
        assert!(!rig.daemon.paused.tasks.contains_key("gone/refine-i5"));
        assert_eq!(
            rig.daemon
                .paused
                .lanes
                .get(&(Stage::Review, "borsuk".to_string())),
            Some(&false)
        );
        assert!(!rig
            .daemon
            .paused
            .lanes
            .contains_key(&(Stage::Review, "gone".to_string())));
    }

    #[test]
    fn a_task_restarts_on_its_attempt_count_not_on_attempt_one() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(142, &["to-refine"])], vec![]);
            first.event(exited("borsuk/refine-i142", false, "boom"));
            assert_eq!(first.task("borsuk/refine-i142").attempt, 2);
        }

        let mut second = Rig::make_in(dir, vec![], |_| {});
        second.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(
            second.task("borsuk/refine-i142").attempt,
            2,
            "the restore keeps the attempt count"
        );
        second.event(exited("borsuk/refine-i142", false, "boom"));
        assert_eq!(
            second.task("borsuk/refine-i142").attempt,
            3,
            "the count continues after the restart"
        );
        second.event(exited("borsuk/refine-i142", false, "boom"));
        assert!(
            matches!(
                second.task("borsuk/refine-i142").state,
                TaskState::Failed(_)
            ),
            "the task gives up on the restored count"
        );
        assert!(second.decision("stuck:borsuk/refine-i142:3").is_some());
    }

    #[test]
    fn a_running_task_restarts_as_queued_and_its_first_prompt_carries_the_notice() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(142, &["to-refine"])], vec![]);
            first.event(started("borsuk/refine-i142", "session-142"));
            assert_eq!(first.task("borsuk/refine-i142").state, TaskState::Running);
        }

        let mut second = Rig::make_in(dir, vec![], |_| {});
        let task = second.task("borsuk/refine-i142");
        assert_eq!(
            task.state,
            TaskState::Queued,
            "the interrupted run restarts as queued"
        );
        assert_eq!(task.attempt, 1);
        assert_eq!(task.session_id.as_deref(), Some("session-142"));
        second.poll(vec![issue(142, &["to-refine"])], vec![]);
        assert_eq!(second.job_count(), 1);
        assert_eq!(second.job(0).resume.as_deref(), Some("session-142"));
        assert!(
            second.job(0).prompt.starts_with(RESTART_NOTICE),
            "the first prompt carries the restart notice:\n{}",
            second.job(0).prompt
        );

        second.event(exited("borsuk/refine-i142", false, "boom"));
        assert_eq!(second.job_count(), 2, "the retry starts at once");
        assert!(
            !second.job(1).prompt.contains(RESTART_NOTICE),
            "the second prompt carries no notice:\n{}",
            second.job(1).prompt
        );
    }

    #[test]
    fn a_queued_chat_message_survives_a_restart() {
        let dir = temp_root();
        {
            let mut first = opencode_rig(&dir, 0);
            first.poll(vec![issue(142, &["refined"])], vec![]);
            first.event(started("borsuk/implement-i142", "ses-142"));
            first
                .daemon
                .chat("borsuk/implement-i142", "add a regression test");
            assert!(first
                .daemon
                .pending_chats
                .contains_key("borsuk/implement-i142"));
            first.drive();
        }

        let steps = reuse_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 142), &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, Harness::Opencode);
        });
        second.drive();
        assert_eq!(
            second.job_count(),
            0,
            "the initial drive waits for the first repository poll"
        );
        second.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(second.job_count(), 1, "the interrupted turn starts first");
        let restart = second.job(0);
        assert_eq!(restart.task, "borsuk/implement-i142");
        assert!(restart.prompt.starts_with(RESTART_NOTICE));
        assert_eq!(restart.resume.as_deref(), Some("ses-142"));
        assert_eq!(
            second.daemon.pending_chats["borsuk/implement-i142"],
            vec!["add a regression test".to_string()]
        );

        second.event(exited("borsuk/implement-i142", true, "done"));

        assert_eq!(second.job_count(), 2, "the saved follow-up starts second");
        let follow_up = second.job(1);
        assert_eq!(follow_up.task, "borsuk/implement-i142");
        assert_eq!(follow_up.prompt, "add a regression test");
        assert_eq!(follow_up.resume.as_deref(), Some("ses-142"));
    }

    #[test]
    fn a_restored_task_of_a_closed_issue_is_retired_at_the_first_poll() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        {
            let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir));
            let mut first = Rig::make_in(dir.clone(), steps, |_| {});
            first.poll(vec![issue(142, &["refined"])], vec![]);
            first.event(started("borsuk/implement-i142", "ses-142"));
        }

        let steps = reuse_issue_steps(&rig_repo(&dir), &worktree, &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |_| {});
        second.drive();
        assert_eq!(
            second.job_count(),
            0,
            "the production initial drive cannot start restored work"
        );
        second.poll(vec![], vec![]);

        assert!(
            !second
                .daemon
                .table
                .by_id
                .contains_key("borsuk/implement-i142"),
            "the first poll cancels and then retires the restored task of the closed issue"
        );
        assert_eq!(second.job_count(), 0);
    }

    #[test]
    fn a_restored_done_review_of_a_merged_pr_is_retired_at_the_first_poll() {
        let dir = temp_root();
        {
            let steps =
                fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
            let mut first = Rig::make_in(dir.clone(), steps, |_| {});
            first.poll(vec![], vec![pr(5, true, &[])]);
            first.poll(vec![], vec![pr(5, false, &[])]);
            first.event(turn_finished("borsuk/review-p5", true, "approved"));
            assert_eq!(first.task("borsuk/review-p5").state, TaskState::Done);
        }

        let mut second = Rig::make_in(dir, vec![], |_| {});
        second.drive();
        assert!(
            second.daemon.table.by_id.contains_key("borsuk/review-p5"),
            "the restore carries the done review task over"
        );

        second.poll(vec![], vec![]);

        assert!(
            !second.daemon.table.by_id.contains_key("borsuk/review-p5"),
            "the first poll retires the done review of the merged pull request, \
             although no earlier snapshot names it"
        );
    }

    #[test]
    fn the_shutdown_sequence_stops_sessions_reads_exits_and_forces_the_write() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let session = rig.session(0);
        let (in_tx, in_rx) = mpsc::channel::<Inbound>();
        in_tx
            .send(Inbound::Run(exited(
                "borsuk/refine-i142",
                true,
                "the stop ended the run",
            )))
            .unwrap();

        rig.daemon.shutdown_sequence(&in_rx);
        drop(in_tx);

        assert!(
            session.stopped.load(Ordering::SeqCst),
            "the sequence stops the live session"
        );
        assert!(
            rig.daemon.sessions.is_empty() && rig.daemon.stopping_sessions.is_empty(),
            "the reported exit drains the stop bookkeeping"
        );
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::Running,
            "the session exit keeps its task stable for the next start"
        );
        let state = fs::read_to_string(dir.join("state").join("state.json")).unwrap();
        assert!(
            state.contains("borsuk/refine-i142"),
            "the forced write lands on disk: {state}"
        );
    }

    #[test]
    fn a_stop_runs_the_shutdown_sequence_and_returns() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let session = rig.session(0);
        rig.daemon.shutdown_grace_ms = 200;
        let (action_tx, action_rx) = mpsc::channel();
        rig.daemon.action_rx = Some(action_rx);
        let daemon = rig.daemon;
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = done_tx.send(daemon.run());
        });

        action_tx.send(Action::Stop).unwrap();
        drop(action_tx);
        let result = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the stop must let the event loop return");
        assert!(result.is_ok(), "the event loop returned {result:?}");
        handle.join().unwrap();

        assert!(
            session.stopped.load(Ordering::SeqCst),
            "the shutdown stops the live session"
        );
        let state = fs::read_to_string(dir.join("state").join("state.json")).unwrap();
        assert!(
            state.contains("borsuk/refine-i142"),
            "the shutdown ends with the state file on disk"
        );
    }

    #[test]
    fn a_paused_release_lane_fires_its_train_but_cannot_start_the_release() {
        let mut rig = Rig::make(vec![]);
        rig.act(Action::Policy {
            repo: "borsuk".to_string(),
            policy: ReleasePolicy::Interval { minutes: 60 },
        });
        rig.act(Action::Pause {
            scope: PauseScope::Lane {
                stage: Stage::Release,
                repo: "borsuk".to_string(),
            },
            paused: true,
        });

        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);

        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release")
        );
        assert_eq!(rig.task("borsuk/release").state, TaskState::Queued);
        assert_eq!(rig.job_count(), 0);
        assert!(
            rig.exec.calls().is_empty(),
            "firing a paused train must not call GitHub or prepare a worktree"
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
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| push_tx.send(view).unwrap()));
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
        let initial = push_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the loop must publish its initial state before it blocks");
        assert_eq!(initial.repos.len(), 1);
        assert!(initial.tasks.is_empty());
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
        rig.set_now(T0 + 1);
        rig.act(Action::Stack {
            repo: "borsuk".to_string(),
            pr: 3,
            on: true,
        });
        let second = rig
            .decision("gate:borsuk")
            .expect("the row refreshes after the label call");
        assert_eq!(sorted(&second), vec![2, 3]);
        assert_eq!(
            second.opened_ms,
            T0 + 1,
            "a changed batch replaces the stale approval row"
        );

        // The next poll confirms the label and keeps the same row.
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

    /// One GitHub comment object for the ask fixtures.
    fn comment(author: &str, created_at: &str, body: &str) -> String {
        format!(
            r#"{{"user":{{"login":"{author}"}},"created_at":"{created_at}","body":{}}}"#,
            serde_json::to_string(body).unwrap()
        )
    }

    /// One ask request against the scripted comments answer of one page.
    fn ask_steps(comments: &str) -> Vec<Step> {
        vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=1",
            ],
            CmdOut::ok(format!("HTTP/2 200\r\n\r\n{comments}")),
        )]
    }

    #[test]
    fn an_ask_request_fetches_comments_and_pushes_the_parsed_block() {
        let block = format!(
            "<aif-ask-v1>\n{}\n</aif-ask-v1>\n",
            serde_json::to_string(&json!({
                "question": "Which workload mode ships first?",
                "options": [
                    {"label": "Fast", "description": "deterministic only"},
                    {"label": "Full"}
                ]
            }))
            .unwrap()
        );
        let comments = format!(
            "[{},{}]",
            comment("agent", "2026-09-01T10:00:00Z", &block),
            comment("human", "2026-09-01T10:05:00Z", "plain prose")
        );
        let mut rig = Rig::make(ask_steps(&comments));
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));
        rig.poll(vec![issue(9, &["needs-human"])], vec![]);

        rig.act(Action::Ask {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 9,
        });

        let calls = rig.exec.calls();
        assert_eq!(calls.len(), 1, "the poll itself ran no gh call");
        assert_eq!(
            calls[0].argv(),
            [
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=1"
            ]
        );
        let Push::Ask(view) = push_rx.try_recv().unwrap() else {
            panic!("the ask request must push a Push::Ask");
        };
        assert_eq!(view.repo, "borsuk");
        assert_eq!(view.kind, ItemKind::Issue);
        assert_eq!(view.number, 9);
        assert_eq!(view.question, "Which workload mode ships first?");
        assert_eq!(
            view.options,
            vec![
                crate::ask::AskOption {
                    label: "Fast".to_string(),
                    description: "deterministic only".to_string(),
                },
                crate::ask::AskOption {
                    label: "Full".to_string(),
                    description: String::new(),
                },
            ]
        );
        assert_eq!(view.author.as_deref(), Some("agent"));
        assert_eq!(view.created_at.as_deref(), Some("2026-09-01T10:00:00Z"));
        assert_eq!(view.error, None);
    }

    #[test]
    fn a_poll_of_a_needs_human_item_runs_no_comment_call() {
        let mut rig = Rig::make(vec![]);

        rig.poll(vec![issue(9, &["needs-human"])], vec![]);

        assert!(rig.decision("human:borsuk:i9").is_some());
        assert!(rig.exec.calls().is_empty());
    }

    #[test]
    fn an_ask_without_a_block_ships_the_newest_comment_body() {
        let comments = format!(
            "[{},{}]",
            comment("agent", "2026-09-01T10:00:00Z", "first prose"),
            comment("agent", "2026-09-01T10:05:00Z", "second prose")
        );
        let mut rig = Rig::make(ask_steps(&comments));
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));
        rig.poll(vec![issue(9, &["needs-human"])], vec![]);

        rig.act(Action::Ask {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 9,
        });

        let Push::Ask(view) = push_rx.try_recv().unwrap() else {
            panic!("the ask request must push a Push::Ask");
        };
        assert_eq!(view.question, "second prose");
        assert!(view.options.is_empty());
        assert_eq!(view.author.as_deref(), Some("agent"));
        assert_eq!(view.created_at.as_deref(), Some("2026-09-01T10:05:00Z"));
        assert_eq!(view.error, None);
    }

    #[test]
    fn an_ask_without_comments_ships_an_empty_question() {
        let mut rig = Rig::make(ask_steps("[]"));
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));
        rig.poll(vec![issue(9, &["needs-human"])], vec![]);

        rig.act(Action::Ask {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 9,
        });

        let Push::Ask(view) = push_rx.try_recv().unwrap() else {
            panic!("the ask request must push a Push::Ask");
        };
        assert_eq!(view.question, "");
        assert!(view.options.is_empty());
        assert_eq!(view.author, None);
        assert_eq!(view.created_at, None);
        assert_eq!(view.error, None);
    }

    #[test]
    fn a_failed_comment_fetch_ships_the_error() {
        let mut rig = Rig::make(vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=1",
            ],
            refused(),
        )]);
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));
        rig.poll(vec![issue(9, &["needs-human"])], vec![]);

        rig.act(Action::Ask {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 9,
        });

        let Push::Ask(view) = push_rx.try_recv().unwrap() else {
            panic!("the ask request must push a Push::Ask");
        };
        assert_eq!(view.repo, "borsuk");
        assert_eq!(view.number, 9);
        assert!(view.error.is_some_and(|error| !error.is_empty()));
        assert!(view.question.is_empty());
        assert!(view.options.is_empty());
        assert!(
            rig.decision("human:borsuk:i9").is_some(),
            "a failed fetch never closes the row"
        );
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
    fn a_failed_live_chat_waits_for_resume_without_extending_the_idle_deadline() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
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
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["hello".to_string()][..]),
            "a failed live send must not lose the message"
        );

        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the live session closed its input",
        ));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).prompt, "hello");
        assert_eq!(rig.job(1).resume.as_deref(), Some("sid-142"));
    }

    #[test]
    fn a_failed_live_chat_on_a_terminal_task_reopens_for_resume() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let session = rig.session(0);
        session.fail_send.store(true, Ordering::SeqCst);
        let task = rig.task("borsuk/refine-i142");
        rig.daemon.complete_task(&task);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "continue after this process exits".to_string(),
        });

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Queued);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["continue after this process exits".to_string()][..])
        );

        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the completed process closed its input",
        ));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).prompt, "continue after this process exits");
        assert_eq!(rig.job(1).resume.as_deref(), Some("sid-142"));
    }

    #[test]
    fn a_completed_claude_turn_keeps_a_message_that_the_live_send_refused() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = issue_wt(&dir, 142);
        let gitdir = rig_gitdir(&dir);
        let steps: Vec<Step> = fresh_issue_steps(&repo, &worktree, 142, &gitdir)
            .into_iter()
            .chain(reuse_issue_steps(&repo, &worktree, &gitdir))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, Harness::Claude);
        });
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "sid-142"));
        let session = rig.session(0);
        session.fail_send.store(true, Ordering::SeqCst);

        rig.act(Action::Chat {
            task: "borsuk/implement-i142".to_string(),
            text: "carry this message into the next turn".to_string(),
        });
        rig.event(turn_finished("borsuk/implement-i142", true, "done"));

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["carry this message into the next turn".to_string()][..])
        );

        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).prompt, "carry this message into the next turn");
        assert_eq!(rig.job(1).resume.as_deref(), Some("sid-142"));
    }

    #[test]
    fn a_poll_completion_keeps_the_message_of_a_parked_claude_task() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        rig.event(turn_ended("borsuk/refine-i142"));
        rig.set_now(T0 + 31 * 60_000);
        rig.drive();
        rig.event(exited(
            "borsuk/refine-i142",
            false,
            "the reaped process exited",
        ));
        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Refine,
            },
            paused: true,
        });
        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "keep this accepted message".to_string(),
        });

        rig.poll(vec![issue(142, &["refined"])], vec![]);

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Queued);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["keep this accepted message".to_string()][..])
        );

        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Refine,
            },
            paused: false,
        });

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).prompt, "keep this accepted message");
        assert_eq!(rig.job(1).resume.as_deref(), Some("sid-142"));
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
    fn builtin_prompts_advance_the_github_gates() {
        assert!(
            REFINE_PROMPT.contains("--remove-label to-refine --add-label refined"),
            "the refine prompt must open the implement gate"
        );
        assert!(
            IMPLEMENT_PROMPT.contains("gh pr create --draft"),
            "the implement prompt must leave the pull request in the review gate"
        );
        assert!(
            IMPLEMENT_PROMPT.contains("--remove-label refined"),
            "the implement prompt must close its issue gate"
        );
        assert!(
            REVIEW_PROMPT.contains("gh pr ready {number}"),
            "the review prompt must open the release gate after approval"
        );
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
    fn ticket_create_uses_its_own_execution_role() {
        let mut rig = Rig::make_with(vec![], |config| {
            let settings = config.roles.get_mut(&ExecutionRole::Refine).unwrap();
            settings.harness = Harness::Opencode;
            settings.program = "opencode-refine".to_string();
            settings.auto_approve = Some(true);
        });

        rig.act(Action::TicketCreate {
            repo: "borsuk".to_string(),
        });

        let jobs = rig.jobs.lock().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].task, "borsuk/refine-i0");
        assert!(jobs[0].prompt.contains("gh issue create"));
        drop(jobs);
        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role, ExecutionRole::TicketCreate);
        assert_eq!(roles[0].settings.harness, Harness::Claude);
        drop(roles);
        let task = rig.task("borsuk/refine-i0");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Live,
            "a ticket task uses the live-input capability of its own role"
        );

        rig.event(turn_ended("borsuk/refine-i0"));

        assert_eq!(rig.task("borsuk/refine-i0").state, TaskState::AwaitingUser);
    }

    #[test]
    fn repository_role_override_selects_the_runner_and_all_settings() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |config| {
            config
                .repos
                .get_mut("borsuk")
                .unwrap()
                .role_overrides
                .insert(
                    ExecutionRole::Implement,
                    crate::config::RoleOverride {
                        harness: Some(Harness::Codex),
                        program: Some("codex-local".to_string()),
                        model: Some("gpt-local".to_string()),
                        effort: Some("high".to_string()),
                        extra_args: Some(vec!["--notice".to_string(), "local".to_string()]),
                        profile: Some("repository".to_string()),
                        approval_policy: Some("never".to_string()),
                        sandbox: Some("workspace-write".to_string()),
                        ..crate::config::RoleOverride::default()
                    },
                );
        });

        rig.poll(vec![issue(142, &["refined"])], vec![]);

        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role, ExecutionRole::Implement);
        assert_eq!(
            roles[0].source,
            crate::config::SettingsSource::Repository {
                alias: "borsuk".to_string(),
            }
        );
        assert_eq!(roles[0].settings.harness, Harness::Codex);
        assert_eq!(roles[0].settings.program, "codex-local");
        assert_eq!(roles[0].settings.model, "gpt-local");
        assert_eq!(roles[0].settings.effort.as_deref(), Some("high"));
        assert_eq!(roles[0].settings.extra_args, ["--notice", "local"]);
        assert_eq!(roles[0].settings.profile.as_deref(), Some("repository"));
        assert_eq!(roles[0].settings.approval_policy.as_deref(), Some("never"));
        assert_eq!(
            roles[0].settings.sandbox.as_deref(),
            Some("workspace-write")
        );
    }

    #[test]
    fn an_automatic_retry_keeps_the_first_role_binding() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.daemon
            .config
            .roles
            .get_mut(&ExecutionRole::Refine)
            .unwrap()
            .model = "changed-after-start".to_string();

        rig.event(exited("borsuk/refine-i142", false, "failed"));

        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0], roles[1]);
        assert_eq!(roles[1].settings.model, "m");
    }

    #[test]
    fn a_parked_follow_up_keeps_the_first_role_binding() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "session-142"));
        rig.event(turn_ended("borsuk/refine-i142"));
        rig.event(exited("borsuk/refine-i142", true, "parked"));
        rig.daemon
            .config
            .roles
            .get_mut(&ExecutionRole::Refine)
            .unwrap()
            .model = "changed-after-park".to_string();

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "continue".to_string(),
        });

        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0], roles[1]);
        assert_eq!(roles[1].settings.model, "m");
    }

    #[test]
    fn a_daemon_restart_keeps_the_first_role_binding() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(142, &["to-refine"])], vec![]);
            assert_eq!(first.roles.lock().unwrap()[0].settings.model, "m");
        }

        let mut second = Rig::make_in(dir, vec![], |config| {
            config.roles.get_mut(&ExecutionRole::Refine).unwrap().model =
                "changed-after-restart".to_string();
        });
        second.poll(vec![issue(142, &["to-refine"])], vec![]);

        let roles = second.roles.lock().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].settings.model, "m");
    }

    #[test]
    fn a_restart_drops_a_completed_binding_before_a_new_logical_task() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(142, &["to-refine"])], vec![]);
            first.daemon.sessions.remove("borsuk/refine-i142");
            first
                .daemon
                .table
                .by_id
                .get_mut("borsuk/refine-i142")
                .unwrap()
                .state = TaskState::Done;
            first.daemon.changed = true;
            first.daemon.save_state();
        }

        let mut second = Rig::make_in(dir, vec![], |config| {
            config.roles.get_mut(&ExecutionRole::Refine).unwrap().model =
                "new-task-after-restart".to_string();
        });
        second.poll(vec![issue(142, &["to-refine"])], vec![]);

        let roles = second.roles.lock().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].settings.model, "new-task-after-restart");
    }

    #[test]
    fn a_daemon_restart_keeps_a_failed_task_binding_for_retry() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(142, &["to-refine"])], vec![]);
            for attempt in 0..tasks::MAX_ATTEMPTS {
                first.event(exited(
                    "borsuk/refine-i142",
                    false,
                    &format!("failed attempt {attempt}"),
                ));
            }
            assert!(matches!(
                first.task("borsuk/refine-i142").state,
                TaskState::Failed(_)
            ));
        }

        let mut second = Rig::make_in(dir, vec![], |config| {
            config.roles.get_mut(&ExecutionRole::Refine).unwrap().model =
                "changed-after-failure".to_string();
        });
        assert!(
            second.decision("stuck:borsuk/refine-i142:3").is_some(),
            "the stuck row survives the restart"
        );
        second.poll(vec![issue(142, &["to-refine"])], vec![]);

        assert_eq!(
            second.job_count(),
            0,
            "the open stuck row holds the gate; the task does not run again on its own"
        );
        assert_eq!(second.roles.lock().unwrap().len(), 0);

        second.act(Action::Retry {
            task: "borsuk/refine-i142".to_string(),
        });

        let roles = second.roles.lock().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(
            roles[0].settings.model, "m",
            "the operator retry reuses the restored binding"
        );
    }

    #[test]
    fn a_logically_new_task_replaces_a_stale_role_binding() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.daemon.sessions.remove("borsuk/refine-i142");
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/refine-i142")
            .unwrap()
            .state = TaskState::Done;
        rig.daemon
            .config
            .roles
            .get_mut(&ExecutionRole::Refine)
            .unwrap()
            .model = "new-task-model".to_string();

        rig.act(Action::Refine {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 142,
        });

        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[1].settings.model, "new-task-model");
    }

    #[test]
    fn a_settings_save_updates_the_file_live_config_and_result_push() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let mut rig = Rig::make_in(
            dir,
            vec![git_step(
                &repo,
                &["remote", "get-url", "origin"],
                CmdOut::ok("git@github.com:acme/borsuk.git\n"),
            )],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::create_dir_all(rig.repo.join(".git")).unwrap();
        fs::write(&config_path, &original).unwrap();
        let mut settings = Config::parse(&original).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "after-save".to_string();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "save-42".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: crate::sock::SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the save must push a settings result");
        };
        assert_eq!(result.request, "save-42");
        assert_eq!(result.status, crate::sock::SettingsResultStatus::Saved);
        assert_eq!(
            rig.daemon.config.roles[&ExecutionRole::Refine].model,
            "after-save"
        );
        assert!(fs::read_to_string(config_path)
            .unwrap()
            .contains("model = \"after-save\""));
    }

    #[test]
    fn a_stale_settings_save_changes_neither_the_file_nor_live_config() {
        let mut rig = Rig::make(vec![]);
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        fs::create_dir_all(rig.repo.join(".git")).unwrap();
        let original = settings_config_text(&rig.repo, "m");
        let newer = settings_config_text(&rig.repo, "changed-on-disk");
        fs::write(&config_path, &newer).unwrap();
        let mut settings = Config::parse(&original).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "operator-edit".to_string();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "save-stale".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: crate::sock::SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the stale save must push a settings result");
        };
        assert_eq!(result.request, "save-stale");
        assert_eq!(result.status, crate::sock::SettingsResultStatus::Stale);
        assert_eq!(result.revision, crate::config::file_revision(&newer));
        assert_eq!(fs::read_to_string(config_path).unwrap(), newer);
        assert_eq!(rig.daemon.config.roles[&ExecutionRole::Refine].model, "m");
    }

    #[test]
    fn a_save_detects_a_file_change_during_candidate_resolution() {
        struct MutatingExec {
            path: PathBuf,
            replacement: String,
            changed: AtomicBool,
        }
        impl Exec for MutatingExec {
            fn run(&self, program: &str, args: &[&str], _cwd: Option<&Path>) -> Result<CmdOut> {
                if program == "git" && args.ends_with(&["remote", "get-url", "origin"]) {
                    if !self.changed.swap(true, Ordering::SeqCst) {
                        fs::write(&self.path, &self.replacement)?;
                    }
                    return Ok(CmdOut::ok("git@github.com:acme/borsuk.git\n"));
                }
                bail!("unexpected command")
            }
        }

        let dir = temp_root();
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("repo/.git")).unwrap();
        let path = dir.join("factory.toml");
        let original = settings_config_text(&dir.join("repo"), "m");
        let external = settings_config_text(&dir.join("repo"), "external-change");
        fs::write(&path, &original).unwrap();
        let exec: Arc<dyn Exec> = Arc::new(MutatingExec {
            path: path.clone(),
            replacement: external.clone(),
            changed: AtomicBool::new(false),
        });
        let (_poll_tx, poll_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let jobs = Arc::new(Mutex::new(Vec::new()));
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let roles = Arc::new(Mutex::new(Vec::new()));
        let mut daemon = Daemon::with_runner_factory(
            test_config(&dir),
            path.clone(),
            config::file_revision(&original),
            exec,
            dir.join("state"),
            dir.join("prompts"),
            poll_rx,
            BTreeMap::new(),
            action_rx,
            Arc::new(FakeRunnerFactory {
                jobs,
                sessions,
                roles,
            }),
            false,
        );
        let (tx, rx) = mpsc::channel();
        daemon.set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        let mut settings = Config::parse(&original).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "operator-edit".to_string();
        daemon.handle(Inbound::Act(Box::new(Action::SaveSettings {
            request: "race".to_string(),
            base_revision: config::file_revision(&original),
            edit: SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        })));
        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(
            result.status,
            SettingsResultStatus::Stale,
            "{:?}",
            result.message
        );
        assert_eq!(fs::read_to_string(path).unwrap(), external);
        assert_eq!(daemon.config.roles[&ExecutionRole::Refine].model, "m");
    }

    #[test]
    fn a_save_detects_a_change_after_temporary_file_preparation() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let mut rig = Rig::make_in(
            dir,
            vec![git_step(
                &repo,
                &["remote", "get-url", "origin"],
                CmdOut::ok("git@github.com:acme/borsuk.git\n"),
            )],
            |_| {},
        );
        fs::create_dir_all(rig.repo.join(".git")).unwrap();
        let path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        let external = settings_config_text(&rig.repo, "external-after-prepare");
        fs::write(&path, &original).unwrap();
        let hook_path = path.clone();
        let hook_external = external.clone();
        rig.daemon.before_config_commit = Some(Box::new(move || {
            fs::write(&hook_path, &hook_external).unwrap();
        }));
        let mut settings = Config::parse(&original).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "operator-edit".to_string();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "after-prepare".to_string(),
            base_revision: config::file_revision(&original),
            edit: SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(result.status, SettingsResultStatus::Stale);
        assert_eq!(fs::read_to_string(&path).unwrap(), external);
        assert_eq!(rig.daemon.config.roles[&ExecutionRole::Refine].model, "m");
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn an_invalid_reload_keeps_the_live_config() {
        let mut rig = Rig::make(vec![]);
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let invalid = "schema_version = 1\n[stage.refine]\nharness = \"claude\"\n";
        fs::write(&config_path, invalid).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::ReloadSettings {
            request: "reload-invalid".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the invalid reload must push a settings result");
        };
        assert_eq!(result.request, "reload-invalid");
        assert_eq!(result.status, crate::sock::SettingsResultStatus::Invalid);
        assert_eq!(fs::read_to_string(config_path).unwrap(), invalid);
        assert_eq!(rig.daemon.config.roles[&ExecutionRole::Refine].model, "m");
    }

    #[test]
    fn a_topology_reload_keeps_the_old_live_topology() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let added = dir.join("second-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(added.join(".git")).unwrap();
        let mut rig = Rig::make_in(
            dir,
            vec![
                git_step(
                    &repo,
                    &["remote", "get-url", "origin"],
                    CmdOut::ok("git@github.com:acme/borsuk.git\n"),
                ),
                git_step(
                    &added,
                    &["remote", "get-url", "origin"],
                    CmdOut::ok("git@github.com:acme/second.git\n"),
                ),
            ],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let topology = format!(
            "{}\n[repo.second]\npath = \"{}\"\n",
            settings_config_text(&rig.repo, "m"),
            added.display()
        );
        fs::write(&config_path, &topology).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::ReloadSettings {
            request: "reload-topology".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the topology reload must push a settings result");
        };
        assert_eq!(
            result.status,
            crate::sock::SettingsResultStatus::RestartRequired
        );
        assert_eq!(result.revision, crate::config::file_revision(&topology));
        assert_eq!(rig.daemon.config.repos.len(), 1);
        assert!(rig.daemon.config.repos.contains_key("borsuk"));
        assert_eq!(fs::read_to_string(config_path).unwrap(), topology);
    }

    #[test]
    fn a_reloaded_role_applies_to_a_logically_new_task_only() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let mut rig = Rig::make_in(
            dir,
            vec![git_step(
                &repo,
                &["remote", "get-url", "origin"],
                CmdOut::ok("git@github.com:acme/borsuk.git\n"),
            )],
            |_| {},
        );
        fs::create_dir_all(rig.repo.join(".git")).unwrap();
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        fs::write(
            &config_path,
            settings_config_text(&rig.repo, "after-reload"),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::ReloadSettings {
            request: "reload-new-task".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the valid reload must push a settings result");
        };
        assert_eq!(result.status, crate::sock::SettingsResultStatus::Reloaded);
        assert_eq!(rig.roles.lock().unwrap()[0].settings.model, "m");
        rig.daemon.sessions.remove("borsuk/refine-i142");
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/refine-i142")
            .unwrap()
            .state = TaskState::Done;

        rig.act(Action::Refine {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 142,
        });

        let roles = rig.roles.lock().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].settings.model, "m");
        assert_eq!(roles[1].settings.model, "after-reload");
    }

    #[test]
    fn ticket_chat_starts_once_with_issue_context_and_read_only_tools() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);

        rig.act(Action::Ticket(crate::sock::TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));

        assert_eq!(rig.job_count(), 1);
        let job = rig.job(0);
        assert_eq!(job.task, "borsuk/ticket-i7");
        assert_eq!(job.cwd, rig.repo);
        assert_eq!(job.model, "m");
        assert_eq!(
            job.allowed_tools,
            Some(vec![
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string()
            ])
        );
        assert!(job.prompt.contains("issue 7"));
        assert!(job.prompt.contains("body 7"));
        assert!(job.prompt.contains("Start with analysis"));
        assert!(!job.prompt.contains("Do not edit files"));
        assert!(job.prompt.contains("<aif-ticket-proposal-v1>"));
        assert_eq!(
            rig.task("borsuk/ticket-i7").purpose,
            crate::tasks::TaskPurpose::TicketChat
        );

        rig.act(Action::Ticket(crate::sock::TicketAction::Chat {
            request: "chat-again-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        assert_eq!(
            rig.job_count(),
            1,
            "the second request must reuse the session"
        );
    }

    #[test]
    fn a_restart_resumes_the_same_ticket_chat_after_the_first_poll() {
        let dir = temp_root();
        let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
        first.poll(vec![issue(7, &[])], vec![]);
        first.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        first.event(started("borsuk/ticket-i7", "session-ticket-7"));
        drop(first);

        let mut second = Rig::make_in(dir, vec![], |_| {});
        assert_eq!(
            second.job_count(),
            0,
            "restore must wait for current GitHub state"
        );
        second.poll(vec![issue(7, &[])], vec![]);

        assert_eq!(second.job_count(), 1);
        let resumed = second.job(0);
        assert_eq!(resumed.task, "borsuk/ticket-i7");
        assert_eq!(resumed.resume.as_deref(), Some("session-ticket-7"));
    }

    #[test]
    fn a_restart_keeps_a_queued_ticket_chat_and_its_task_pause() {
        let dir = temp_root();
        {
            let mut first = Rig::make_in(dir.clone(), vec![], |_| {});
            first.poll(vec![issue(7, &[])], vec![]);
            first.act(Action::Ticket(TicketAction::Chat {
                request: "chat-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
            }));
            first.event(started("borsuk/ticket-i7", "session-ticket-7"));
            first.event(turn_ended("borsuk/ticket-i7"));
            first.daemon.sessions.remove("borsuk/ticket-i7");
            first.act(Action::Pause {
                scope: PauseScope::Task {
                    task: "borsuk/ticket-i7".to_string(),
                },
                paused: true,
            });
            first.act(Action::Chat {
                task: "borsuk/ticket-i7".to_string(),
                text: "check the acceptance criteria".to_string(),
            });
            assert_eq!(
                first.daemon.pending_chats["borsuk/ticket-i7"],
                vec!["check the acceptance criteria".to_string()]
            );
        }

        let mut second = Rig::make_in(dir, vec![], |_| {});
        assert_eq!(
            second.daemon.pending_chats["borsuk/ticket-i7"],
            vec!["check the acceptance criteria".to_string()]
        );
        assert_eq!(
            second.daemon.paused.tasks.get("borsuk/ticket-i7"),
            Some(&true)
        );
        second.drive();
        assert_eq!(second.job_count(), 0);

        second.poll(vec![issue(7, &[])], vec![]);
        assert_eq!(
            second.job_count(),
            0,
            "the restored task pause still blocks"
        );
        second.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/ticket-i7".to_string(),
            },
            paused: false,
        });

        assert_eq!(second.job_count(), 1);
        let resumed = second.job(0);
        assert_eq!(resumed.task, "borsuk/ticket-i7");
        assert_eq!(resumed.prompt, "check the acceptance criteria");
        assert_eq!(resumed.resume.as_deref(), Some("session-ticket-7"));
    }

    #[test]
    fn each_to_refine_label_interval_sends_one_handoff_to_the_same_session() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        rig.event(started("borsuk/ticket-i7", "session-ticket-7"));
        rig.event(turn_ended("borsuk/ticket-i7"));
        let session = rig.session(0);

        rig.poll(vec![issue(7, &["to-refine"])], vec![]);
        rig.poll(vec![issue(7, &["to-refine"])], vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.poll(vec![issue(7, &["to-refine"])], vec![]);

        let sends = session.sends.lock().unwrap();
        assert_eq!(sends.len(), 2);
        assert!(sends.iter().all(|text| text.contains("refinement")));
        assert!(!rig.daemon.table.by_id.contains_key("borsuk/refine-i7"));
    }

    #[test]
    fn a_label_transition_before_session_start_sends_the_handoff_after_start() {
        let steps = vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=to-refine",
            ],
            CmdOut::ok("HTTP/1.1 200 OK\r\n\r\n[{\"name\":\"to-refine\"}]"),
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let session = rig.session(0);

        rig.act(Action::Ticket(TicketAction::ToggleLabel {
            request: "label-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            label: "to-refine".to_string(),
            on: true,
        }));
        assert!(session.sends.lock().unwrap().is_empty());

        rig.event(started("borsuk/ticket-i7", "session-ticket-7"));

        assert_eq!(session.sends.lock().unwrap().len(), 1);
        assert!(rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)].handoff_active);
    }

    #[test]
    fn the_handoff_runs_while_a_refine_task_of_the_ticket_is_active() {
        let steps = vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=to-refine",
            ],
            CmdOut::ok("HTTP/1.1 200 OK\r\n\r\n[{\"name\":\"to-refine\"}]"),
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        rig.event(started("borsuk/ticket-i7", "session-ticket-7"));
        rig.event(turn_ended("borsuk/ticket-i7"));
        let session = rig.session(0);
        // The refine task and the ticket chat share the repository checkout,
        // so the active refine never blocks the conversation handoff.
        let blocker = rig
            .daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                7,
                rig.daemon.state_dir.join("logs/blocker.jsonl"),
                T0,
            )
            .unwrap()
            .id
            .clone();
        rig.daemon
            .table
            .transition(&blocker, TaskState::Running, T0)
            .unwrap();

        rig.act(Action::Ticket(TicketAction::ToggleLabel {
            request: "label-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            label: "to-refine".to_string(),
            on: true,
        }));
        assert_eq!(
            session.sends.lock().unwrap().len(),
            1,
            "the active refine never blocks the handoff"
        );
        assert!(rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)].handoff_active);
    }

    #[test]
    fn refined_issue_uses_one_ticket_chat_cleanup_path() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let session = rig.session(0);

        rig.poll(vec![issue(7, &["refined"])], vec![]);
        rig.poll(vec![issue(7, &["refined"])], vec![]);

        assert!(session.stopped.load(Ordering::SeqCst));
        assert!(!rig.daemon.table.by_id.contains_key("borsuk/ticket-i7"));
        assert!(rig.daemon.ticket_conversations.is_empty());
    }

    #[test]
    fn a_direct_refined_label_change_admits_implementation_without_a_poll() {
        let steps = vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=refined",
            ],
            CmdOut::ok("HTTP/1.1 200 OK\r\n\r\n[{\"name\":\"refined\"}]"),
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));

        rig.act(Action::Ticket(TicketAction::ToggleLabel {
            request: "label-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            label: "refined".to_string(),
            on: true,
        }));

        assert!(rig.daemon.ticket_conversations.is_empty());
        assert!(rig.daemon.table.by_id.contains_key("borsuk/implement-i7"));
    }

    #[test]
    fn closed_issue_uses_the_same_ticket_chat_cleanup_path() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let session = rig.session(0);

        rig.poll(vec![], vec![]);
        rig.poll(vec![], vec![]);

        assert!(session.stopped.load(Ordering::SeqCst));
        assert!(!rig.daemon.table.by_id.contains_key("borsuk/ticket-i7"));
        assert!(rig.daemon.ticket_conversations.is_empty());
    }

    #[test]
    fn only_the_final_complete_ticket_block_becomes_a_persisted_proposal() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));

        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: "Analysis first.".to_string(),
        });
        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: concat!(
                "<aif-ticket-proposal-v1>\n",
                "{\"title\":\"Proposed title\",\"body\":\"Proposed body\"}\n",
                "</aif-ticket-proposal-v1>"
            )
            .to_string(),
        });
        rig.event(turn_ended("borsuk/ticket-i7"));

        let proposal = rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)]
            .proposal
            .as_ref()
            .unwrap();
        assert_eq!(proposal.title, "Proposed title");
        assert_eq!(proposal.body, "Proposed body");
        assert_eq!(proposal.original_title, "issue 7");
        assert_eq!(proposal.original_body, "body 7");
        let Push::TicketDetails(details) = push_rx.try_recv().unwrap() else {
            panic!("the proposal must refresh the focused details");
        };
        assert_eq!(details.proposal, Some(proposal.clone()));

        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: "<aif-ticket-proposal-".to_string(),
        });
        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: "v1>\n{\"title\":\"Bad\",\"body\":\"Split\"}\n</aif-ticket-proposal-v1>"
                .to_string(),
        });
        rig.event(turn_ended("borsuk/ticket-i7"));
        assert_eq!(
            rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)]
                .proposal
                .as_ref()
                .unwrap()
                .title,
            "Proposed title"
        );

        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: "</aif-".to_string(),
        });
        rig.event(RunEvent::Text {
            task: "borsuk/ticket-i7".to_string(),
            text: concat!(
                "<aif-ticket-proposal-v1>\n",
                "{\"title\":\"Duplicate\",\"body\":\"Marker\"}\n",
                "</aif-ticket-proposal-v1>"
            )
            .to_string(),
        });
        rig.event(turn_ended("borsuk/ticket-i7"));
        assert_eq!(
            rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)]
                .proposal
                .as_ref()
                .unwrap()
                .title,
            "Proposed title"
        );
    }

    #[test]
    fn proposal_application_updates_only_content_and_notifies_the_same_session() {
        let response = |title: &str, body: &str| {
            CmdOut::ok(format!(
                "HTTP/1.1 200 OK\r\n\r\n{}",
                json!({
                    "number": 7,
                    "node_id": "node-7",
                    "title": title,
                    "body": body,
                    "state": "open",
                    "labels": [{"name": "ui"}],
                    "user": {"login": "author"},
                    "assignees": [],
                    "updated_at": "2026-08-07T12:00:00Z",
                    "html_url": "https://github.com/acme/borsuk/issues/7"
                })
            ))
        };
        let steps = vec![
            gh_step(
                &["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"],
                response("issue 7", "body 7"),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "PATCH",
                    "repos/acme/borsuk/issues/7",
                    "-f",
                    "title=Proposed title",
                    "-f",
                    "body=Proposed body",
                ],
                response("Proposed title", "Proposed body"),
            ),
        ];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &["ui"])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let session = rig.session(0);
        rig.daemon
            .ticket_conversations
            .get_mut(&("borsuk".to_string(), 7))
            .unwrap()
            .proposal = Some(TicketProposal {
            id: "proposal-7".to_string(),
            title: "Proposed title".to_string(),
            body: "Proposed body".to_string(),
            original_title: "issue 7".to_string(),
            original_body: "body 7".to_string(),
        });

        rig.act(Action::Ticket(TicketAction::UpdateContent {
            request: "apply-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            expected: crate::sock::TicketContent {
                title: "issue 7".to_string(),
                body: "body 7".to_string(),
            },
            desired: crate::sock::TicketContent {
                title: "Proposed title".to_string(),
                body: "Proposed body".to_string(),
            },
            source: crate::sock::TicketContentSource::Proposal {
                proposal_id: "proposal-7".to_string(),
            },
        }));

        let confirmed = &rig.daemon.snapshot.repos["borsuk"].issues[&7];
        assert_eq!(confirmed.title, "Proposed title");
        assert_eq!(confirmed.labels, vec!["ui".to_string()]);
        assert!(rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)]
            .proposal
            .is_none());
        let sends = session.sends.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert!(sends[0].contains("proposal"));
        assert!(sends[0].contains("applied"));
    }

    #[test]
    fn a_content_conflict_refreshes_the_confirmed_daemon_snapshot() {
        let remote = CmdOut::ok(format!(
            "HTTP/1.1 200 OK\r\n\r\n{}",
            json!({
                "number": 7,
                "node_id": "node-7",
                "title": "Remote title",
                "body": "Remote body",
                "state": "open",
                "labels": [{"name": "ui"}],
                "user": {"login": "author"},
                "assignees": [],
                "updated_at": "2026-08-07T12:00:00Z",
                "html_url": "https://github.com/acme/borsuk/issues/7"
            })
        ));
        let steps = vec![gh_step(
            &["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"],
            remote,
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &["ui"])], vec![]);

        rig.act(Action::Ticket(TicketAction::UpdateContent {
            request: "save-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            expected: crate::sock::TicketContent {
                title: "issue 7".to_string(),
                body: "body 7".to_string(),
            },
            desired: crate::sock::TicketContent {
                title: "Pending title".to_string(),
                body: "Pending body".to_string(),
            },
            source: crate::sock::TicketContentSource::Direct,
        }));

        let confirmed = &rig.daemon.snapshot.repos["borsuk"].issues[&7];
        assert_eq!(confirmed.title, "Remote title");
        assert_eq!(confirmed.body, "Remote body");
    }

    #[test]
    fn a_proposal_notice_failure_does_not_undo_the_github_update() {
        let response = |title: &str, body: &str| {
            CmdOut::ok(format!(
                "HTTP/1.1 200 OK\r\n\r\n{}",
                json!({
                    "number": 7,
                    "node_id": "node-7",
                    "title": title,
                    "body": body,
                    "state": "open",
                    "labels": [{"name": "ui"}],
                    "user": {"login": "author"},
                    "assignees": [],
                    "updated_at": "2026-08-07T12:00:00Z",
                    "html_url": "https://github.com/acme/borsuk/issues/7"
                })
            ))
        };
        let steps = vec![
            gh_step(
                &["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"],
                response("issue 7", "body 7"),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "PATCH",
                    "repos/acme/borsuk/issues/7",
                    "-f",
                    "title=Proposed title",
                    "-f",
                    "body=Proposed body",
                ],
                response("Proposed title", "Proposed body"),
            ),
        ];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &["ui"])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        let session = rig.session(0);
        session.fail_send.store(true, Ordering::SeqCst);
        rig.daemon
            .ticket_conversations
            .get_mut(&("borsuk".to_string(), 7))
            .unwrap()
            .proposal = Some(TicketProposal {
            id: "proposal-7".to_string(),
            title: "Proposed title".to_string(),
            body: "Proposed body".to_string(),
            original_title: "issue 7".to_string(),
            original_body: "body 7".to_string(),
        });
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));

        rig.act(Action::Ticket(TicketAction::UpdateContent {
            request: "apply-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            expected: crate::sock::TicketContent {
                title: "issue 7".to_string(),
                body: "body 7".to_string(),
            },
            desired: crate::sock::TicketContent {
                title: "Proposed title".to_string(),
                body: "Proposed body".to_string(),
            },
            source: crate::sock::TicketContentSource::Proposal {
                proposal_id: "proposal-7".to_string(),
            },
        }));

        let confirmed = &rig.daemon.snapshot.repos["borsuk"].issues[&7];
        assert_eq!(confirmed.title, "Proposed title");
        assert!(rig.daemon.ticket_conversations[&("borsuk".to_string(), 7)]
            .proposal
            .is_none());
        let pushes: Vec<Push> = push_rx.try_iter().collect();
        assert!(pushes.iter().any(|push| {
            matches!(
                push,
                Push::TicketDetails(details)
                    if details.repo == "borsuk"
                        && details.issue.number == 7
                        && details.proposal.is_none()
            )
        }));
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
            scope: PauseScope::Lane {
                stage: Stage::Implement,
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
        assert!(!rig
            .daemon
            .paused
            .lanes
            .contains_key(&(Stage::Implement, "missing".to_string())));
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
    fn retry_refuses_a_completed_task() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir))
                .into_iter()
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &worktree,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(turn_finished("borsuk/implement-i142", true, "done"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);

        rig.act(Action::Retry {
            task: "borsuk/implement-i142".to_string(),
        });

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 1);
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

        rig.event(exited("borsuk/release", false, "first"));
        assert_eq!(rig.task("borsuk/release").attempt, 2);
        assert!(rig.job(1).prompt.contains("#2"));
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release")
        );
        assert!(rig.decision("gate:borsuk").is_none());

        rig.event(exited("borsuk/release", false, "second"));
        assert_eq!(rig.task("borsuk/release").attempt, 3);
        assert!(rig.job(2).prompt.contains("#2"));

        rig.event(exited("borsuk/release", false, "third"));
        assert_eq!(rig.job_count(), 3);
        assert!(rig.decision("stuck:borsuk/release:3").is_some());
        assert!(rig.decision("gate:borsuk").is_none());

        rig.act(Action::Answer {
            decision_id: "stuck:borsuk/release:3".to_string(),
            response: Response::Retry,
        });
        assert_eq!(rig.job_count(), 4);
        assert!(rig.job(3).prompt.contains("#2"));
        assert_eq!(rig.task("borsuk/release").attempt, 1);
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release")
        );
    }

    #[test]
    fn a_manual_release_retry_keeps_its_unstacked_batch_after_a_restart() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = train_wt(&dir);
        let gitdir = rig_gitdir(&dir);
        {
            let steps: Vec<Step> = fresh_train_steps(&repo, &worktree, &gitdir)
                .into_iter()
                .chain(reuse_train_steps(&repo, &worktree, &gitdir))
                .chain(reuse_train_steps(&repo, &worktree, &gitdir))
                .collect();
            let mut first = Rig::make_in(dir.clone(), steps, |_| {});
            first.poll(vec![], vec![pr(2, false, &[])]);
            first.act(Action::Go {
                repo: "borsuk".to_string(),
                prs: vec![2],
            });
            for detail in ["first", "second", "third"] {
                first.event(exited("borsuk/release", false, detail));
            }
            assert!(first.decision("stuck:borsuk/release:3").is_some());
            assert_eq!(first.daemon.release_batches["borsuk/release"], vec![2]);
        }

        let steps = reuse_train_steps(&repo, &worktree, &gitdir);
        let mut second = Rig::make_in(dir, steps, |_| {});
        second.drive();
        second.poll(vec![], vec![pr(2, false, &[]), pr(3, false, &[])]);
        second.act(Action::Answer {
            decision_id: "stuck:borsuk/release:3".to_string(),
            response: Response::Retry,
        });

        assert_eq!(second.job_count(), 1);
        let retry = second.job(0);
        assert!(retry.prompt.contains("#2"));
        assert!(!retry.prompt.contains("#3"));
        assert_eq!(second.daemon.trains["borsuk"].batch(), &[2]);
        assert_eq!(second.daemon.trains["borsuk"].queue, vec![3]);
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
            Some("borsuk/release")
        );
    }

    #[test]
    fn a_fired_train_reads_the_scoped_id_in_the_view() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let (mut rig, rx) = pushed_rig(steps);
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);

        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });

        let view = last_view(&rx);
        assert!(
            view.tasks.iter().any(|task| task.id == "borsuk/release"),
            "task ids: {:?}",
            view.tasks
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<_>>()
        );
        assert!(view
            .trains
            .iter()
            .any(|train| train.in_flight.as_deref() == Some("borsuk/release")));
    }

    #[test]
    fn a_second_batch_writes_its_own_log_file() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir))
                .into_iter()
                .chain(vec![gh_step(
                    &[
                        "api",
                        "-i",
                        "-X",
                        "DELETE",
                        "repos/acme/borsuk/issues/2/labels/release-stacked",
                    ],
                    gh_ok(),
                )])
                .chain(reuse_train_steps(
                    &rig_repo(&dir),
                    &train_wt(&dir),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(
            vec![],
            vec![
                pr(2, false, &["release-stacked"]),
                pr(5, false, &["release-stacked"]),
            ],
        );

        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        assert!(rig
            .task("borsuk/release")
            .log_path
            .ends_with("borsuk__release-p2.jsonl"));

        rig.event(turn_finished("borsuk/release", true, "released"));
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![5],
        });
        assert!(
            rig.task("borsuk/release")
                .log_path
                .ends_with("borsuk__release-p5.jsonl"),
            "the log keeps the batch number"
        );
    }

    #[test]
    fn reverse_stack_actions_keep_the_release_prompt_in_queue_order() {
        let dir = temp_root();
        let steps: Vec<Step> = vec![
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "POST",
                    "repos/acme/borsuk/issues/9/labels",
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
                    "POST",
                    "repos/acme/borsuk/issues/7/labels",
                    "-f",
                    "labels[]=release-stacked",
                ],
                gh_ok(),
            ),
        ]
        .into_iter()
        .chain(fresh_train_steps(
            &rig_repo(&dir),
            &train_wt(&dir),
            &rig_gitdir(&dir),
        ))
        .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(7, false, &[]), pr(9, false, &[])]);

        rig.act(Action::Stack {
            repo: "borsuk".to_string(),
            pr: 9,
            on: true,
        });
        rig.act(Action::Stack {
            repo: "borsuk".to_string(),
            pr: 7,
            on: true,
        });
        let batch = match rig.decision("gate:borsuk").unwrap().kind {
            DecisionKind::ReleaseGate { prs } => prs,
            other => panic!("expected a release gate, got {other:?}"),
        };
        assert_eq!(batch, vec![7, 9]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: batch,
        });

        let prompt = rig.job(0).prompt;
        assert!(prompt.contains("- #7 pr 7\n- #9 pr 9"), "prompt:\n{prompt}");
        assert!(prompt.contains("Merge order is 7, 9."), "prompt:\n{prompt}");
    }

    #[test]
    fn aborting_a_release_returns_its_batch_to_the_train() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        assert_eq!(rig.job_count(), 1);

        rig.act(Action::Abort {
            task: "borsuk/release".to_string(),
        });

        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Failed("cancelled".to_string())
        );
        assert_eq!(rig.daemon.trains["borsuk"].in_flight, None);
        assert_eq!(rig.daemon.trains["borsuk"].queue, vec![2]);
        assert!(!rig.daemon.release_batches.contains_key("borsuk/release"));
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
    fn a_dropped_issue_stops_its_session_and_retires_its_tasks() {
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
        let session = rig.session(0);

        rig.poll(vec![], vec![]);

        assert!(
            session.stopped.load(Ordering::SeqCst),
            "the vanished issue stops the live session"
        );
        assert!(
            !rig.daemon
                .table
                .by_id
                .values()
                .any(|task| task.repo == "borsuk"
                    && task.kind == ItemKind::Issue
                    && task.number == 142),
            "the dropped issue leaves no task in the table"
        );
    }

    #[test]
    fn a_dropped_pr_retires_its_done_review_task() {
        let dir = temp_root();
        let steps = fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        rig.event(turn_finished("borsuk/review-p5", true, "approved"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);

        rig.poll(vec![], vec![]);

        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/review-p5"),
            "the dropped pull request leaves no review task in the table"
        );
    }

    #[test]
    fn a_dropped_issue_ends_its_open_ticket_conversation() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        assert!(rig
            .daemon
            .ticket_conversations
            .contains_key(&("borsuk".to_string(), 7)));

        rig.poll(vec![], vec![]);

        assert!(
            rig.daemon.ticket_conversations.is_empty(),
            "the dropped issue ends the conversation"
        );
        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/ticket-i7"),
            "the conversation reconciler removes the chat task"
        );
    }

    #[test]
    fn a_dropped_pr_keeps_the_in_flight_release_and_closes_the_train() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(fresh_train_steps(
                    &rig_repo(&dir),
                    &train_wt(&dir),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        rig.event(turn_finished("borsuk/review-p5", true, "approved"));
        rig.event(started("borsuk/release", "session-release"));
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release")
        );

        rig.poll(vec![], vec![]);

        assert!(
            rig.daemon.table.by_id.contains_key("borsuk/release"),
            "the in-flight release task survives the retire"
        );
        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/review-p5"),
            "the dropped review retires"
        );
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight, None,
            "reconcile_trains closes the train after the release task ends"
        );
    }

    #[test]
    fn a_failed_release_keeps_its_task_after_one_batch_pr_merges() {
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
        rig.poll(vec![], vec![pr(2, false, &[]), pr(3, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2, 3],
        });
        for detail in ["first", "second", "third"] {
            rig.event(exited("borsuk/release", false, detail));
        }
        assert_eq!(rig.daemon.trains["borsuk"].batch(), &[2, 3]);
        assert_eq!(rig.daemon.trains["borsuk"].in_flight, None);

        // The failed batch merged pr 2 before it stopped, so pr 2 leaves
        // GitHub while the saved retry batch still holds pr 3.
        rig.poll(vec![], vec![pr(3, false, &[])]);

        assert_eq!(
            rig.daemon.trains["borsuk"].batch(),
            &[3],
            "the merged pull request leaves the retry batch"
        );
        assert!(
            rig.daemon.table.by_id.contains_key("borsuk/release"),
            "the release task outlives the pull request that names it, \
             because the train still holds a batch to retry"
        );

        rig.act(Action::Answer {
            decision_id: "stuck:borsuk/release:3".to_string(),
            response: Response::Retry,
        });

        assert_eq!(rig.job_count(), 4, "the retry starts a fresh run");
        assert!(rig.job(3).prompt.contains("#3"));
        assert!(!rig.job(3).prompt.contains("#2"));
    }

    #[test]
    fn a_finished_release_retires_after_its_batch_merges() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = train_wt(&dir);
        let gitdir = rig_gitdir(&dir);
        let steps = fresh_train_steps(&repo, &worktree, &gitdir);
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        rig.event(turn_finished("borsuk/release", true, "released"));
        assert_eq!(rig.task("borsuk/release").state, TaskState::Done);
        assert!(rig.daemon.trains["borsuk"].batch().is_empty());

        rig.poll(vec![], vec![]);

        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/release"),
            "an idle train keeps no release task"
        );
    }

    #[test]
    fn a_retire_drops_the_binding_the_pause_and_the_decisions() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "session-impl"));
        assert!(rig
            .daemon
            .role_bindings
            .contains_key("borsuk/implement-i142"));
        rig.event(RunEvent::Ask {
            task: "borsuk/implement-i142".to_string(),
            request_id: "q-1".to_string(),
            tool: "AskUserQuestion".to_string(),
            input: json!([{"question": "which database?"}]),
            suggestions: serde_json::Value::Null,
            needs_human: true,
        });
        assert!(rig.decision("perm:borsuk/implement-i142:q-1").is_some());
        rig.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/implement-i142".to_string(),
            },
            paused: true,
        });
        assert!(rig
            .daemon
            .paused
            .tasks
            .contains_key("borsuk/implement-i142"));

        rig.poll(vec![], vec![]);

        assert!(!rig.daemon.table.by_id.contains_key("borsuk/implement-i142"));
        assert!(!rig
            .daemon
            .role_bindings
            .contains_key("borsuk/implement-i142"));
        assert!(!rig
            .daemon
            .paused
            .tasks
            .contains_key("borsuk/implement-i142"));
        assert!(rig.decision("perm:borsuk/implement-i142:q-1").is_none());
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
    fn a_ready_pull_poll_keeps_the_review_until_runner_success() {
        let dir = temp_root();
        let steps = fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let session = rig.session(0);

        rig.poll(vec![], vec![pr(5, false, &[])]);

        assert!(!session.stopped.load(Ordering::SeqCst));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Running);
        rig.event(turn_finished("borsuk/review-p5", true, "approved"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        assert_eq!(
            fs::read_to_string(issue_wt(&dir, 5).join(".aif/reviewed-sha"))
                .unwrap()
                .trim(),
            "sha5"
        );
    }

    #[test]
    fn an_automatic_release_waits_for_the_running_review() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(fresh_train_steps(
                    &rig_repo(&dir),
                    &train_wt(&dir),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);

        rig.poll(vec![], vec![pr(5, false, &[])]);

        assert_eq!(rig.job_count(), 1, "release waits for the review result");
        let release = rig.task("borsuk/release");
        assert_eq!(
            rig.daemon.input_mode(&release),
            InputMode::Closed {
                reason: "This task waits for \"borsuk/review-p5\" to finish.".to_string()
            },
            "a running blocker needs no action, so the reason names none"
        );
        rig.event(turn_finished("borsuk/review-p5", true, "approved"));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/release");
    }

    #[test]
    fn an_automatic_release_stays_blocked_after_a_failed_review() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(fresh_train_steps(
                &rig_repo(&dir),
                &train_wt(&dir),
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        assert_eq!(rig.task("borsuk/release").state, TaskState::Queued);
        assert_eq!(
            rig.task("borsuk/release").attempt,
            1,
            "the running review holds the release back"
        );

        for attempt in 0..tasks::MAX_ATTEMPTS {
            rig.event(turn_finished("borsuk/review-p5", false, "review failed"));
            assert_eq!(
                rig.task("borsuk/release").attempt,
                1,
                "the review attempt {} still holds the release back",
                attempt + 1
            );
            if attempt + 1 < tasks::MAX_ATTEMPTS {
                assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Queued);
                rig.event(exited("borsuk/review-p5", false, "review failed"));
                assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Running);
            }
        }

        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Failed("review failed".to_string())
        );
        assert!(rig.decision("stuck:borsuk/review-p5:3").is_some());
        let release = rig.task("borsuk/release");
        assert_eq!(rig.job_count(), 3, "only review attempts can run");
        assert_eq!(release.state, TaskState::Queued);
        assert_eq!(release.attempt, 1, "the release never reached dispatch");
        assert_eq!(
            rig.daemon.input_mode(&release),
            InputMode::Closed {
                reason: "This task waits for \"borsuk/review-p5\" to finish. \
                         That task failed. Press R on its pipeline row to retry it."
                    .to_string()
            },
            "the session view must name the task that holds the release"
        );
    }

    /// The widest reason must fit one common terminal.
    ///
    /// The chat bar centers the reason and clips both ends, so a long
    /// sentence loses its subject and its action together. 118 characters
    /// is the inner width of a 120-column terminal. This test holds the
    /// sentence inside that width; it fails if a later edit adds words.
    #[test]
    fn the_blocker_reason_fits_a_120_column_chat_bar() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        for _ in 0..3 {
            rig.event(turn_finished("borsuk/review-p5", false, "review failed"));
            rig.event(exited("borsuk/review-p5", false, "review failed"));
        }

        let release = rig.task("borsuk/release");
        let InputMode::Closed { reason } = rig.daemon.input_mode(&release) else {
            panic!("a queued release takes no message");
        };

        // A real id is longer than this rig's. Measure the shape with the
        // longest pair the live factory carries: "ai-factory/review-p65".
        let widest = reason.replace("borsuk/review-p5", "ai-factory/review-p65");
        assert!(
            widest.chars().count() <= 118,
            "the reason is {} characters and clips a 120-column bar: {widest}",
            widest.chars().count(),
        );
    }

    #[test]
    fn a_cancelled_stuck_review_still_names_a_recovery_the_human_can_reach() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        for _ in 0..3 {
            rig.event(turn_finished("borsuk/review-p5", false, "review failed"));
            rig.event(exited("borsuk/review-p5", false, "review failed"));
        }

        // The human answers Cancel on the stuck row. The review stays failed,
        // so it still holds the release, but its inbox row is gone.
        rig.act(Action::Answer {
            decision_id: "stuck:borsuk/review-p5:3".to_string(),
            response: Response::Cancel,
        });

        assert!(
            rig.decision("stuck:borsuk/review-p5:3").is_none(),
            "the cancel drops the inbox row"
        );
        assert!(
            matches!(rig.task("borsuk/review-p5").state, TaskState::Failed(_)),
            "the cancelled review stays failed, so it still holds the release"
        );
        let release = rig.task("borsuk/release");
        assert_eq!(release.state, TaskState::Queued);
        let InputMode::Closed { reason } = rig.daemon.input_mode(&release) else {
            panic!("a queued release takes no message");
        };
        assert!(
            reason.contains("Press R on its pipeline row"),
            "the named recovery must not be the inbox, which now holds nothing: {reason}"
        );
    }

    #[test]
    fn a_blocked_release_names_the_failed_review_over_the_running_one() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .chain(reuse_issue_steps(
                &rig_repo(&dir),
                &worktree,
                &rig_gitdir(&dir),
            ))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            config.repos.get_mut("borsuk").unwrap().release = ReleasePolicy::Threshold { count: 1 };
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, false, &[])]);
        for _ in 0..3 {
            rig.event(turn_finished("borsuk/review-p5", false, "review failed"));
            rig.event(exited("borsuk/review-p5", false, "review failed"));
        }

        // A second review of the same batch still runs. Its id sorts before
        // the failed one, so a plain scan would name it and offer no action.
        let log = rig.task("borsuk/review-p5").log_path.clone();
        rig.daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                3,
                log,
                rig.daemon.now_ms,
            )
            .unwrap()
            .state = TaskState::Running;
        rig.daemon
            .release_batches
            .insert("borsuk/release".to_string(), vec![3, 5]);

        let release = rig.task("borsuk/release");
        // The test only discriminates while the running review comes first
        // in the scan. Assert that order, not the mere presence of the row:
        // a rename that reversed it would leave the assertion below green
        // without the preference.
        let order: Vec<&str> = rig
            .daemon
            .table
            .by_id
            .keys()
            .map(String::as_str)
            .filter(|id| *id == "borsuk/review-p3" || *id == "borsuk/review-p5")
            .collect();
        assert_eq!(
            order,
            vec!["borsuk/review-p3", "borsuk/review-p5"],
            "the running review must reach the scan before the failed one"
        );
        assert_eq!(
            rig.daemon.input_mode(&release),
            InputMode::Closed {
                reason: "This task waits for \"borsuk/review-p5\" to finish. \
                         That task failed. Press R on its pipeline row to retry it."
                    .to_string()
            },
            "a failed blocker never ends by itself, so it wins over a running one"
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
        assert_eq!(
            rig.job_count(),
            1,
            "the replacement waits for the superseded process exit"
        );

        rig.event(exited(
            "borsuk/review-p5",
            false,
            "the superseded process exited",
        ));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(
            rig.task("borsuk/review-p5").head_sha.as_deref(),
            Some("new-sha")
        );
    }

    #[test]
    fn a_new_head_branch_replaces_the_review_worktree() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(fresh_issue_steps(
                    &rig_repo(&dir),
                    &issue_wt(&dir, 142),
                    142,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let first = rig.session(0);
        let mut updated = pr(5, true, &[]);
        updated.head_ref = "aif/borsuk/issue-142".to_string();

        rig.poll(vec![], vec![updated]);

        assert!(first.stopped.load(Ordering::SeqCst));
        rig.event(exited(
            "borsuk/review-p5",
            false,
            "the superseded process exited",
        ));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).cwd, issue_wt(&dir, 142));
    }

    #[test]
    fn a_ticket_set_change_supersedes_the_review() {
        let dir = temp_root();
        let pr_worktree = pr_wt(&dir, 5);
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(fresh_pr_steps(
                    &rig_repo(&dir),
                    &pr_worktree,
                    5,
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(5, true, &[]);
        pull.body = "Closes #5".to_string();
        rig.poll(vec![], vec![pull]);
        let first = rig.session(0);

        // The PR leaves the draft state, so the gate closes. It returns to
        // draft with a body that now closes ticket 9 too, so the gate
        // re-opens with the same head and a changed ticket set.
        let mut updated = pr(5, false, &[]);
        updated.body = "Closes #5".to_string();
        rig.poll(vec![], vec![updated.clone()]);
        updated.draft = true;
        updated.body = "Closes #5, fixes #9".to_string();
        rig.poll(vec![], vec![updated]);

        assert!(
            first.stopped.load(Ordering::SeqCst),
            "the ticket set change must supersede the running review"
        );
        rig.event(exited(
            "borsuk/review-p5",
            false,
            "the superseded process exited",
        ));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/review-p5");
        assert_eq!(rig.job(1).cwd, pr_worktree);
    }

    #[test]
    fn an_unchanged_ticket_set_and_head_supersedes_nothing() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        let first = rig.session(0);

        // The PR leaves the draft state, so the gate closes; the running
        // review survives. The PR returns to draft with the same head, so
        // the gate re-opens and the daemon compares the ticket set.
        let mut ready = pr(5, false, &[]);
        rig.poll(vec![], vec![ready.clone()]);
        ready.draft = true;
        rig.poll(vec![], vec![ready]);

        assert!(
            !first.stopped.load(Ordering::SeqCst),
            "an unchanged ticket set must not supersede the review"
        );
        assert_eq!(rig.job_count(), 1);
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Running);
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
            set_role_harness(config, ExecutionRole::Implement, Harness::Opencode);
        });
        rig.poll(vec![issue(142, &["refined"])], vec![]);

        rig.event(turn_finished("borsuk/implement-i142", true, "one step"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
    }

    // ------------------------------------------------------------------
    // The opencode follow-up turn
    // ------------------------------------------------------------------

    /// A rig whose implement stage runs on the one-shot opencode runner.
    ///
    /// `reuses` adds that many reuse-step rounds for later launches of the
    /// same issue worktree.
    fn opencode_rig(dir: &Path, reuses: usize) -> Rig {
        let repo = rig_repo(dir);
        let worktree = issue_wt(dir, 142);
        let gitdir = rig_gitdir(dir);
        let mut steps = fresh_issue_steps(&repo, &worktree, 142, &gitdir);
        for _ in 0..reuses {
            steps.extend(reuse_issue_steps(&repo, &worktree, &gitdir));
        }
        Rig::make_in(dir.to_path_buf(), steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, Harness::Opencode);
        })
    }

    #[test]
    fn a_chat_on_a_done_opencode_task_queues_the_text_and_reopens_the_task() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        // A daemon restart loses the session id in the table. The marker in
        // the worktree keeps it.
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .session_id = None;

        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );
        assert_eq!(rig.job_count(), 1, "the chat alone starts no run");
    }

    #[test]
    fn the_follow_up_relaunch_continues_the_session_with_the_typed_prompt() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .session_id = None;
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        rig.drive();

        assert_eq!(rig.job_count(), 2);
        let job = rig.job(1);
        assert_eq!(job.task, "borsuk/implement-i142");
        assert_eq!(job.resume.as_deref(), Some("ses-142"));
        assert_eq!(job.prompt, "add a regression test");
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
    }

    #[test]
    fn a_chat_on_a_running_opencode_task_waits_for_the_exit() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        assert_eq!(rig.job_count(), 1, "the relaunch waits for the exit");
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        assert_eq!(rig.job_count(), 2, "the exit event frees the follow-up");
        let job = rig.job(1);
        assert_eq!(job.resume.as_deref(), Some("ses-142"));
        assert_eq!(job.prompt, "add a regression test");
    }

    #[test]
    fn a_chat_on_a_live_opencode_session_never_steers_it() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        // The fake session records every send. The empty record proves the
        // daemon never calls send_user on an opencode session, so the
        // unsupported-steering error can never appear.
        assert!(rig.session(0).sends.lock().unwrap().is_empty());
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );
    }

    #[test]
    fn queued_opencode_messages_each_start_a_turn_without_steering() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 2);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.daemon.chat("borsuk/implement-i142", "first follow-up");
        rig.daemon.chat("borsuk/implement-i142", "second follow-up");
        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).prompt, "first follow-up");
        assert!(
            rig.session(1).sends.lock().unwrap().is_empty(),
            "an opencode turn never accepts a steering call"
        );
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["second follow-up".to_string()][..])
        );

        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        assert_eq!(rig.job_count(), 3);
        assert_eq!(rig.job(2).prompt, "second follow-up");
        assert_eq!(rig.job(2).resume.as_deref(), Some("ses-142"));
    }

    #[test]
    fn a_queued_task_with_a_pending_chat_gets_exactly_one_launch() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 3);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("boom".to_string())
        );
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);
        assert_eq!(rig.job_count(), 3);
        let stuck = rig.decision("stuck:borsuk/implement-i142:3");
        assert!(stuck.is_some(), "the finished run left a stuck row");

        rig.daemon
            .chat("borsuk/implement-i142", "try one more typed turn");

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);
        assert!(rig.decision("stuck:borsuk/implement-i142:3").is_none());

        // The dispatcher must leave the task to resume_pending_chats,
        // whatever the order of the drive steps is.
        rig.daemon.dispatch_queued();
        assert_eq!(rig.job_count(), 3, "the dispatcher does not own the task");

        rig.drive();
        assert_eq!(rig.job_count(), 4, "the follow-up turn starts once");
        let job = rig.job(3);
        assert_eq!(job.resume.as_deref(), Some("ses-142"));
        assert_eq!(job.prompt, "try one more typed turn");
        rig.drive();
        assert_eq!(rig.job_count(), 4, "no second launch");
    }

    #[test]
    fn a_queued_later_stage_does_not_deadlock_a_pending_follow_up() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: true,
        });
        rig.act(Action::Chat {
            task: "borsuk/implement-i142".to_string(),
            text: "check the review feedback".to_string(),
        });

        let review_id = rig
            .daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                7,
                dir.join("review.jsonl"),
                rig.daemon.now_ms,
            )
            .unwrap()
            .id
            .clone();
        rig.daemon
            .review_tickets
            .insert(review_id.clone(), BTreeSet::from([142]));

        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: false,
        });

        assert_eq!(rig.job_count(), 2, "the follow-up owns the queued task");
        assert_eq!(rig.job(1).task, "borsuk/implement-i142");
        assert_eq!(rig.job(1).prompt, "check the review feedback");
        assert_eq!(rig.task(&review_id).state, TaskState::Queued);
    }

    #[test]
    fn an_opencode_retry_resumes_when_the_adapter_supports_resume() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job(0).resume, None);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.event(exited("borsuk/implement-i142", false, "boom"));

        assert_eq!(rig.task("borsuk/implement-i142").attempt, 2);
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).resume.as_deref(), Some("ses-142"));
        assert!(
            rig.job(1).prompt.contains("142"),
            "the retry reruns the stage prompt, not a chat message"
        );
    }

    #[test]
    fn a_follow_up_refuses_while_another_task_of_the_item_is_active() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &["refined"])], vec![pull]);
        assert_eq!(rig.job_count(), 1, "the implement runs first");
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(
            rig.job_count(),
            2,
            "the review gate opened after the implement"
        );
        rig.event(started("borsuk/review-p7", "sid-7"));

        let task = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.sibling_refusal(&task).as_deref(),
            Some(
                "the chat message for \"borsuk/implement-i142\" cannot start. Task \
                 \"borsuk/review-p7\" (running) uses the worktree \"issue-142\". Wait until \
                 that task is terminal."
            )
        );
        rig.daemon
            .chat("borsuk/implement-i142", "adjust the implementation");

        assert!(
            rig.daemon.pending_chats.is_empty(),
            "the sibling guard refuses the follow-up"
        );
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 2);
    }

    #[test]
    fn a_sibling_closure_blocks_a_live_claude_send() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &["refined"])], vec![pull]);
        rig.event(started("borsuk/implement-i142", "sid-142"));
        let session = rig.session(0);
        // The review of the linked ticket queues behind the implement, and
        // both tasks map to the ticket worktree.
        assert_eq!(rig.task("borsuk/review-p7").state, TaskState::Queued);

        let task = rig.task("borsuk/implement-i142");
        assert!(matches!(
            rig.daemon.input_mode(&task),
            InputMode::Closed { .. }
        ));

        rig.daemon
            .chat("borsuk/implement-i142", "do not steer this task");

        assert!(
            session.sends.lock().unwrap().is_empty(),
            "the daemon must enforce the sibling closure on live sends"
        );
        assert!(rig.daemon.pending_chats.is_empty());
    }

    #[test]
    fn the_session_marker_survives_a_done_opencode_task() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);

        let marker = rig
            .daemon
            .worktrees
            .read_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
            .unwrap();

        assert_eq!(marker.as_deref(), Some("ses-142"));
    }

    #[test]
    fn a_paused_stage_holds_a_queued_follow_up() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: true,
        });
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);

        rig.drive();
        assert_eq!(rig.job_count(), 1, "the pause holds the follow-up");
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );

        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: false,
        });
        assert_eq!(rig.job_count(), 2, "the lift starts the follow-up");
        assert_eq!(rig.job(1).prompt, "add a regression test");
        assert_eq!(rig.job(1).resume.as_deref(), Some("ses-142"));
    }

    // ------------------------------------------------------------------
    // The abort delivers the queued message
    // ------------------------------------------------------------------

    #[test]
    fn an_abort_of_a_task_without_chats_keeps_the_old_behaviour() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        assert!(
            rig.daemon
                .worktrees
                .read_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
                .unwrap()
                .is_some(),
            "the run wrote the session marker"
        );
        rig.event(RunEvent::Ask {
            task: "borsuk/implement-i142".to_string(),
            request_id: "req-1".to_string(),
            tool: "Bash".to_string(),
            input: json!({"command": "cargo test"}),
            suggestions: serde_json::Value::Null,
            needs_human: false,
        });
        assert!(rig.decision("perm:borsuk/implement-i142:req-1").is_some());

        rig.act(Action::Abort {
            task: "borsuk/implement-i142".to_string(),
        });

        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("cancelled".to_string())
        );
        assert!(
            rig.daemon
                .worktrees
                .read_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
                .unwrap()
                .is_none(),
            "the abort removes the restart data"
        );
        assert!(rig.daemon.pending_chats.is_empty());
        assert!(rig.decision("perm:borsuk/implement-i142:req-1").is_none());
        assert_eq!(rig.job_count(), 1);
    }

    #[test]
    fn an_abort_keeps_the_queued_message_and_reopens_the_task() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        rig.act(Action::Abort {
            task: "borsuk/implement-i142".to_string(),
        });

        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );
        let marker = rig
            .daemon
            .worktrees
            .read_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
            .unwrap();
        assert_eq!(marker.as_deref(), Some("ses-142"));
        assert_eq!(rig.job_count(), 1, "the abort alone starts no run");
    }

    #[test]
    fn the_turn_after_an_abort_carries_the_message_and_the_session() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");
        rig.act(Action::Abort {
            task: "borsuk/implement-i142".to_string(),
        });

        // The stopped process reports its exit and clears the stop gate.
        // The exit frees the follow-up, and the daemon launches it at once.
        rig.event(exited("borsuk/implement-i142", false, "killed"));

        assert_eq!(rig.job_count(), 2);
        let job = rig.job(1);
        assert_eq!(job.task, "borsuk/implement-i142");
        assert_eq!(job.prompt, "add a regression test");
        assert_eq!(job.resume.as_deref(), Some("ses-142"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
    }

    #[test]
    fn a_closed_gate_drops_a_queued_message_and_does_not_relaunch() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        rig.poll(vec![issue(142, &[])], vec![]);

        assert!(rig.session(0).stopped.load(Ordering::SeqCst));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("cancelled".to_string())
        );
        assert!(!rig
            .daemon
            .pending_chats
            .contains_key("borsuk/implement-i142"));
        assert!(rig
            .daemon
            .worktrees
            .read_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
            .unwrap()
            .is_none());

        rig.event(exited("borsuk/implement-i142", false, "killed"));

        assert_eq!(rig.job_count(), 1, "the closed gate starts no new run");
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("cancelled".to_string())
        );
    }

    #[test]
    fn a_paused_stage_holds_the_task_an_abort_reopened() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");
        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: true,
        });
        rig.act(Action::Abort {
            task: "borsuk/implement-i142".to_string(),
        });
        rig.event(exited("borsuk/implement-i142", false, "killed"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);

        rig.drive();
        assert_eq!(rig.job_count(), 1, "the pause holds the reopened task");
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );

        rig.act(Action::Pause {
            scope: PauseScope::Stage {
                stage: Stage::Implement,
            },
            paused: false,
        });
        assert_eq!(rig.job_count(), 2, "the lift starts the follow-up");
        assert_eq!(rig.job(1).prompt, "add a regression test");
        assert_eq!(rig.job(1).resume.as_deref(), Some("ses-142"));
    }

    #[test]
    fn a_follow_up_still_refuses_after_an_abort_of_the_sibling() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &["refined"])], vec![pull]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 2, "the review gate opened");
        rig.event(started("borsuk/review-p7", "sid-7"));
        // A message that the live review could not take waits as a queued
        // chat, so the abort requeues the review instead of closing it.
        rig.session(1).fail_send.store(true, Ordering::SeqCst);
        rig.daemon
            .chat("borsuk/review-p7", "check the failing test");
        assert!(rig.daemon.pending_chats.contains_key("borsuk/review-p7"));
        rig.act(Action::Abort {
            task: "borsuk/review-p7".to_string(),
        });
        assert_eq!(rig.task("borsuk/review-p7").state, TaskState::Queued);

        let task = rig.task("borsuk/implement-i142");
        assert!(rig.daemon.sibling_refusal(&task).is_some());
        rig.daemon
            .chat("borsuk/implement-i142", "adjust the implementation");

        assert!(
            !rig.daemon
                .pending_chats
                .contains_key("borsuk/implement-i142"),
            "the sibling guard refuses the follow-up after the abort"
        );
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
    }

    #[test]
    fn a_follow_up_to_the_implement_waits_while_the_refine_is_active() {
        for refine_state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
        ] {
            let dir = temp_root();
            let mut rig = opencode_rig(&dir, 0);
            rig.poll(vec![issue(142, &["refined"])], vec![]);
            rig.event(started("borsuk/implement-i142", "ses-142"));
            rig.event(exited("borsuk/implement-i142", true, "code 0"));
            assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
            // A refine task of the same ticket that is not terminal. The
            // refine works in the shared checkout, so it owns no worktree.
            let log = rig
                .daemon
                .log_path("borsuk", Stage::Refine, ItemKind::Issue, 142);
            rig.daemon
                .table
                .upsert_queued(
                    "borsuk",
                    Stage::Refine,
                    ItemKind::Issue,
                    142,
                    log,
                    rig.daemon.now_ms,
                )
                .unwrap();
            if refine_state != TaskState::Queued {
                rig.daemon
                    .table
                    .transition("borsuk/refine-i142", TaskState::Running, rig.daemon.now_ms)
                    .unwrap();
            }
            if refine_state == TaskState::AwaitingUser {
                rig.daemon
                    .table
                    .transition(
                        "borsuk/refine-i142",
                        TaskState::AwaitingUser,
                        rig.daemon.now_ms,
                    )
                    .unwrap();
            }
            assert_eq!(rig.task("borsuk/refine-i142").state, refine_state);

            let task = rig.task("borsuk/implement-i142");
            assert!(!matches!(
                rig.daemon.input_mode(&task),
                InputMode::Closed { .. }
            ));
            rig.act(Action::Chat {
                task: "borsuk/implement-i142".to_string(),
                text: "start from the specification".to_string(),
            });

            let implement_runs = (0..rig.job_count())
                .filter(|&index| rig.job(index).task == "borsuk/implement-i142")
                .count();
            assert_eq!(
                implement_runs, 1,
                "the implement follow-up waits for the refine state {refine_state:?}"
            );
            assert_eq!(
                rig.daemon
                    .pending_chats
                    .get("borsuk/implement-i142")
                    .map(Vec::as_slice),
                Some(&["start from the specification".to_string()][..]),
                "the refine state {refine_state:?} must not block the implement chat"
            );
            // The chat reopened the task, so the bar now names the cause of
            // the wait. The operator never faces a silent stall.
            let queued = rig.task("borsuk/implement-i142");
            assert_eq!(queued.state, TaskState::Queued);
            assert_eq!(
                rig.daemon.input_mode(&queued),
                InputMode::Closed {
                    reason: "This task waits for \"borsuk/refine-i142\" to finish.".to_string()
                },
                "the bar names the prior stage for the refine state {refine_state:?}"
            );
        }
    }

    #[test]
    fn a_follow_up_to_the_refine_starts_while_the_implement_runs() {
        let dir = temp_root();
        let checkout = rig_repo(&dir);
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Refine, Harness::Opencode);
        });
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        // The one-shot refine reports its result and completes.
        rig.event(exited("borsuk/refine-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job_count(), 2, "the implement gate opened");
        rig.event(started("borsuk/implement-i142", "ses-142"));

        let task = rig.task("borsuk/refine-i142");
        assert!(!matches!(
            rig.daemon.input_mode(&task),
            InputMode::Closed { .. }
        ));
        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "clarify the second requirement".to_string(),
        });

        // The shared checkout carries no guard, and no prior stage holds a
        // refine, so the follow-up turn runs while the implement runs.
        assert_eq!(rig.job_count(), 3, "the refine follow-up started");
        let followup = rig.job(2);
        assert_eq!(followup.task, "borsuk/refine-i142");
        assert_eq!(followup.cwd, checkout, "the refine keeps the checkout");
        assert_eq!(followup.prompt, "clarify the second requirement");
        assert!(rig.daemon.pending_chats.is_empty());
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
    }

    #[test]
    fn a_follow_up_to_the_implement_queues_while_the_ticket_chat_is_active() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        // An active conversation about the same ticket works in the shared
        // checkout, so it owns no worktree.
        let log = rig
            .daemon
            .log_path("borsuk", Stage::Refine, ItemKind::Issue, 142);
        rig.daemon
            .table
            .upsert_ticket_chat("borsuk", 142, log, rig.daemon.now_ms)
            .unwrap();

        let task = rig.task("borsuk/implement-i142");
        assert!(
            rig.daemon.sibling_refusal(&task).is_none(),
            "the ticket chat never blocks the implement"
        );
        rig.act(Action::Chat {
            task: "borsuk/implement-i142".to_string(),
            text: "extend the change".to_string(),
        });

        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["extend the change".to_string()][..])
        );
        // The worktree guard takes the message. The pipeline order still
        // holds the turn: the ticket chat carries the refine stage of the
        // same ticket, so it is a prior stage of the implement.
        let implement_runs = (0..rig.job_count())
            .filter(|&index| rig.job(index).task == "borsuk/implement-i142")
            .count();
        assert_eq!(implement_runs, 1, "the pipeline order holds the turn");
        let queued = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.input_mode(&queued),
            InputMode::Closed {
                reason: "This task waits for \"borsuk/ticket-i142\" to finish.".to_string()
            },
            "the bar names the ticket chat as the cause"
        );
    }

    #[test]
    fn workspace_and_task_cwd_agree_for_every_stage() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});
        let log = PathBuf::from("log");
        let now = rig.daemon.now_ms;
        rig.daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                142,
                log.clone(),
                now,
            )
            .unwrap();
        rig.daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Implement,
                ItemKind::Issue,
                142,
                log.clone(),
                now,
            )
            .unwrap();
        rig.daemon
            .table
            .upsert_queued("borsuk", Stage::Review, ItemKind::Pr, 7, log.clone(), now)
            .unwrap();
        rig.daemon
            .table
            .upsert_queued("borsuk", Stage::Review, ItemKind::Pr, 8, log.clone(), now)
            .unwrap();
        rig.daemon
            .table
            .upsert_with_id(
                crate::tasks::ScopedTask {
                    id: "borsuk/release",
                    repo: "borsuk",
                    stage: Stage::Release,
                    kind: ItemKind::Pr,
                    number: 7,
                },
                log.clone(),
                now,
            )
            .unwrap();
        rig.daemon
            .table
            .upsert_ticket_chat("borsuk", 142, log, now)
            .unwrap();
        rig.daemon
            .review_tickets
            .insert("borsuk/review-p7".to_string(), BTreeSet::from([142]));

        // The stage walk keeps this test complete when the pipeline grows.
        let stage_cases = Stage::ALL.map(|stage| match stage {
            Stage::Refine => ("borsuk/refine-i142", Workspace::Shared),
            Stage::Implement => (
                "borsuk/implement-i142",
                Workspace::Exclusive(WorktreeKey::Issue(142)),
            ),
            Stage::Review => (
                "borsuk/review-p7",
                Workspace::Exclusive(WorktreeKey::Issue(142)),
            ),
            Stage::Release => ("borsuk/release", Workspace::Exclusive(WorktreeKey::Train)),
        });
        let extra_cases = [
            ("borsuk/ticket-i142", Workspace::Shared),
            ("borsuk/review-p8", Workspace::Exclusive(WorktreeKey::Pr(8))),
        ];
        let checkout = rig_repo(&dir);
        for (id, expected) in stage_cases.into_iter().chain(extra_cases) {
            let task = rig.task(id);
            assert_eq!(rig.daemon.workspace(&task), expected, "workspace of {id}");
            let expected_cwd = match &expected {
                Workspace::Shared => checkout.clone(),
                Workspace::Exclusive(key) => dir
                    .join("state")
                    .join("worktrees")
                    .join("borsuk")
                    .join(key.to_string()),
            };
            assert_eq!(
                rig.daemon.task_cwd(id).unwrap(),
                expected_cwd,
                "task_cwd of {id}"
            );
        }
    }

    #[test]
    fn an_implement_waits_while_the_refine_of_its_ticket_is_not_done() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        // The label change queues the implement. The shared checkout carries
        // no worktree guard, so only the pipeline order holds it.
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        rig.drive();
        assert_eq!(rig.job_count(), 1, "the implement waits for the refine");

        rig.event(turn_ended("borsuk/refine-i142"));
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).task, "borsuk/implement-i142");
        assert_eq!(
            rig.job(1).cwd,
            issue_wt(&dir, 142),
            "the implement still runs in the issue worktree"
        );
    }

    // ------------------------------------------------------------------
    // The input mode
    // ------------------------------------------------------------------

    #[test]
    fn a_poll_with_closing_keywords_ships_the_links_in_the_view() {
        let mut rig = Rig::make(vec![]);
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| push_tx.send(view).unwrap()));
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &[])], vec![pull]);

        let view = last_view(&push_rx);
        assert!(
            view.links
                .iter()
                .any(|link| link.repo == "borsuk" && link.ticket == 142 && link.pr == 7),
            "links: {:?}",
            view.links
        );
    }

    /// Drain the push channel and return the last view it holds.
    fn last_view(rx: &mpsc::Receiver<StateView>) -> StateView {
        let mut last = rx.try_recv().expect("the daemon must have pushed a view");
        while let Ok(view) = rx.try_recv() {
            last = view;
        }
        last
    }

    /// The task view of one task id inside a pushed state view.
    fn pushed_task<'a>(view: &'a StateView, id: &str) -> &'a crate::sock::TaskView {
        view.tasks
            .iter()
            .find(|task| task.id == id)
            .unwrap_or_else(|| panic!("the view must carry the task {id}"))
    }

    #[test]
    fn the_input_mode_follows_a_live_then_parked_claude_task() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Live);

        rig.event(turn_ended("borsuk/refine-i142"));
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::AwaitingUser);
        // The live process still steers, so the parked task stays live.
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Live);

        // The idle reaper stops the process. The session id survives in
        // the table, so the next message relaunches the session.
        rig.set_now(T0 + DEFAULT_IDLE_REAP_MS);
        rig.drive();
        assert!(!rig.daemon.sessions.contains_key("borsuk/refine-i142"));
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Resume);
    }

    #[test]
    fn the_input_mode_closes_a_task_that_a_sibling_blocks() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &["refined"])], vec![pull]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 2, "the review gate opened");
        rig.event(started("borsuk/review-p7", "sid-7"));

        let task = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "the chat message for \"borsuk/implement-i142\" cannot start. Task \
                     \"borsuk/review-p7\" (running) uses the worktree \"issue-142\". Wait \
                     until that task is terminal."
                    .to_string()
            }
        );
    }

    #[test]
    fn the_input_mode_follows_an_opencode_task_through_its_life() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::NextTurn);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.state, TaskState::Done);
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Follow);

        // The worktree marker alone keeps the follow-up path open after a
        // daemon restart loses the table session id.
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .session_id = None;
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Follow);

        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .state = TaskState::Failed("the turn failed".to_string());
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::Follow);

        // A queued retry is not a terminal follow-up. It starts a fresh
        // attempt, so the input stays closed while it waits.
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .state = TaskState::Queued;
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The task \"borsuk/implement-i142\" is queued. Wait for it to start."
                    .to_string()
            }
        );

        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .state = TaskState::Done;
        rig.daemon
            .worktrees
            .remove_task_session(&issue_wt(&dir, 142), "borsuk/implement-i142")
            .unwrap();
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The task \"borsuk/implement-i142\" has no session to continue. \
                         Start a new task before you send another message."
                    .to_string()
            }
        );
    }

    #[test]
    fn the_input_mode_closes_a_task_with_no_session_to_continue() {
        let mut rig = Rig::make_paused(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The task \"borsuk/refine-i142\" has no session to continue. \
                         Send a message after the task runs once."
                    .to_string()
            }
        );
    }

    #[test]
    fn the_input_mode_closes_a_running_task_that_has_no_session_yet() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        // The run is live, but opencode prints its first NDJSON line only
        // one to three seconds after the start. No session id and no
        // marker exist yet, so the input stays closed.
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.session_id, None);
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The task \"borsuk/implement-i142\" has no session to continue. \
                         Wait until the task records a session."
                    .to_string()
            }
        );

        // The run records its session id, and the input opens.
        rig.event(started("borsuk/implement-i142", "ses-142"));
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::NextTurn);

        // A restart loses the in-memory id, but the worktree marker keeps
        // the next-turn path open while the run continues.
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .session_id = None;
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(rig.daemon.input_mode(&task), InputMode::NextTurn);
    }

    #[test]
    fn a_claude_task_with_a_spent_session_closes_the_input() {
        let mut rig = Rig::make_paused(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        // A done claude task drops its restart data by design. The table
        // session id stays, but the input must not offer it again.
        rig.daemon.sessions.remove("borsuk/refine-i142");
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/refine-i142")
            .unwrap()
            .state = TaskState::Done;

        let task = rig.task("borsuk/refine-i142");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The task \"borsuk/refine-i142\" is done. \
                         Start a new task before you send another message."
                    .to_string()
            }
        );

        rig.daemon
            .chat("borsuk/refine-i142", "do not accept this message");

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(
            !rig.daemon.pending_chats.contains_key("borsuk/refine-i142"),
            "the daemon must refuse the same task that the wire closes"
        );
    }

    #[test]
    fn a_session_marker_read_error_closes_the_input_with_an_action() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/implement-i142")
            .unwrap()
            .session_id = None;

        let marker_dir = issue_wt(&dir, 142).join(".aif");
        fs::remove_dir_all(&marker_dir).unwrap();
        fs::write(&marker_dir, "not a directory").unwrap();

        let task = rig.task("borsuk/implement-i142");
        assert_eq!(
            rig.daemon.input_mode(&task),
            InputMode::Closed {
                reason: "The daemon cannot read the session for task \
                         \"borsuk/implement-i142\". Check its session marker and try again."
                    .to_string()
            }
        );
    }

    #[test]
    fn the_queued_count_rises_after_a_chat_and_falls_after_the_relaunch() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| tx.send(view).unwrap()));

        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        // The running turn cannot take the message, so the count rises
        // until its exit frees the follow-up.
        rig.act(Action::Chat {
            task: "borsuk/implement-i142".to_string(),
            text: "add a regression test".to_string(),
        });
        let view = last_view(&rx);
        let task = pushed_task(&view, "borsuk/implement-i142");
        assert_eq!(task.queued_messages, 1, "the chat queues one message");
        assert_eq!(task.input, InputMode::NextTurn);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        let view = last_view(&rx);
        let task = pushed_task(&view, "borsuk/implement-i142");
        assert_eq!(task.queued_messages, 0, "the relaunch takes the message");
        assert_eq!(task.input, InputMode::NextTurn);
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);
        assert_eq!(rig.job(1).resume.as_deref(), Some("ses-142"));
        assert_eq!(rig.job(1).prompt, "add a regression test");
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

    #[test]
    fn fill_template_fills_the_tickets_placeholder() {
        let filled = fill_template(
            "Tickets this PR closes: {tickets}",
            &[("tickets", "#4, #9".to_string())],
        )
        .unwrap();
        assert_eq!(filled, "Tickets this PR closes: #4, #9");

        let error =
            fill_template("hi {tickets} {nope}", &[("tickets", "none".to_string())]).unwrap_err();
        assert!(error.to_string().contains("nope"));
    }

    // Path helpers that mirror the rig layout.
    fn rig_repo(dir: &Path) -> PathBuf {
        dir.join("repo")
    }

    fn rig_gitdir(dir: &Path) -> PathBuf {
        dir.join("git-common")
    }

    // ------------------------------------------------------------------
    // State push
    // ------------------------------------------------------------------

    /// A rig whose state views land on a channel.
    fn pushed_rig(steps: Vec<Step>) -> (Rig, Receiver<StateView>) {
        let mut rig = Rig::make(steps);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| tx.send(view).unwrap()));
        (rig, rx)
    }

    fn stage_view(view: &StateView, stage: Stage) -> &crate::sock::StageView {
        view.stages
            .iter()
            .find(|one| one.stage == stage)
            .unwrap_or_else(|| panic!("the view must carry the {stage:?} stage"))
    }

    #[test]
    fn the_first_drive_publishes_the_state_a_new_subscriber_needs() {
        let (mut rig, rx) = pushed_rig(vec![]);
        rig.drive();
        let view = rx.try_recv().expect("the first drive must publish a view");
        assert_eq!(view.repos.len(), 1);
        assert_eq!(view.repos[0].alias, "borsuk");
        assert_eq!(view.repos[0].owner_repo, "acme/borsuk");
        assert_eq!(view.stages.len(), Stage::ALL.len());
        // The config seeds every limit, so a fresh daemon shows no override.
        assert!(view.stages.iter().all(|stage| !stage.overridden));
        let refine = stage_view(&view, Stage::Refine);
        assert_eq!(refine.limit, 2);
        assert_eq!(refine.running, 0);
        assert_eq!(refine.queued, 0);
        assert!(view.tasks.is_empty());
        assert!(view.lanes.is_empty());
        assert!(view.decisions.is_empty());
        assert!(view.decision_items.is_empty());
        assert!(!view.paused.global);
        assert!(view.paused.overrides.is_empty());
        assert_eq!(view.trains.len(), 1);
        assert_eq!(view.trains[0].repo, "borsuk");
        assert_eq!(view.trains[0].queue, Vec::<u64>::new());
        assert_eq!(view.trains[0].stacked, Vec::<u64>::new());
        assert_eq!(view.trains[0].policy, ReleasePolicy::Manual);
        assert_eq!(view.trains[0].next_fire_ms, None);
        assert_eq!(view.trains[0].in_flight, None);

        // A quiet second drive publishes nothing.
        rig.drive();
        assert!(rx.try_recv().is_err(), "an unchanged drive must not push");
    }

    #[test]
    fn a_ticket_created_before_the_first_poll_reaches_the_next_state() {
        let created = json!({
            "number": 12,
            "node_id": "node-12",
            "title": "Direct title",
            "body": "Direct body",
            "state": "open",
            "labels": [],
            "user": {"login": "piotr"},
            "assignees": [],
            "updated_at": "2026-09-03T12:00:00Z",
            "html_url": "https://github.com/acme/borsuk/issues/12"
        });
        let (mut rig, rx) = pushed_rig(vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues",
                "-f",
                "title=Direct title",
                "-f",
                "body=Direct body",
            ],
            CmdOut::ok(format!("HTTP/2 201\r\n\r\n{created}")),
        )]);
        rig.drive();
        let initial = rx.try_recv().expect("the first drive must publish");
        assert!(initial.tickets.is_empty());

        rig.act(Action::Ticket(TicketAction::Create {
            request: "create-12".to_string(),
            repo: "borsuk".to_string(),
            title: "Direct title".to_string(),
            body: "Direct body".to_string(),
        }));

        assert_eq!(
            rig.daemon.snapshot.repos["borsuk"].issues[&12].title,
            "Direct title"
        );
        let view = rx
            .try_recv()
            .expect("ticket creation must publish a fresh state");
        assert_eq!(view.tickets.len(), 1);
        assert_eq!(view.tickets[0].repo, "borsuk");
        assert_eq!(view.tickets[0].number, 12);
    }

    #[test]
    fn a_ticket_details_action_pushes_the_confirmed_issue() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(7, &[])], vec![]);
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));

        rig.act(Action::Ticket(crate::sock::TicketAction::Details {
            request: "details-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));

        let crate::sock::Push::TicketDetails(details) = push_rx.try_recv().unwrap() else {
            panic!("the action must push ticket details");
        };
        assert_eq!(details.request, "details-7");
        assert_eq!(details.repo, "borsuk");
        assert_eq!(details.issue.number, 7);
        assert_eq!(details.issue.author, "author");
    }

    #[test]
    fn a_ticket_details_action_pushes_details_then_mention_statuses() {
        let mut focus = issue(7, &[]);
        focus.body = "Depends on #8 and tracks #9".to_string();
        let open = issue(9, &[]);
        let steps = vec![gh_step(
            &["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"],
            CmdOut::ok("HTTP/2 200\r\n\r\n{\"number\":8,\"state\":\"closed\"}"),
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![focus, open], vec![]);
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| push_tx.send(push).unwrap()));

        rig.act(Action::Ticket(crate::sock::TicketAction::Details {
            request: "details-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));

        let crate::sock::Push::TicketDetails(details) = push_rx.try_recv().unwrap() else {
            panic!("the details push must come first");
        };
        assert_eq!(details.issue.number, 7);
        let crate::sock::Push::TicketMentions(mentions) = push_rx.try_recv().unwrap() else {
            panic!("the mention statuses must follow the details");
        };
        assert_eq!(mentions.repo, "borsuk");
        assert_eq!(mentions.number, 7);
        let statuses: Vec<(u64, crate::sock::MentionStatus)> = mentions
            .statuses
            .iter()
            .map(|status| (status.number, status.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                (8, crate::sock::MentionStatus::ClosedIssue),
                (9, crate::sock::MentionStatus::OpenIssue),
            ]
        );
        assert_eq!(
            rig.exec.calls().len(),
            1,
            "the open ticket must resolve from the snapshot"
        );
    }

    #[test]
    fn a_poll_without_a_later_start_time_cannot_replace_a_label_mutation() {
        let steps = vec![gh_step(
            &[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=urgent",
            ],
            CmdOut::ok("HTTP/1.1 200 OK\r\n\r\n[{\"name\":\"ui\"},{\"name\":\"urgent\"}]"),
        )];
        let mut rig = Rig::make(steps);
        rig.poll(vec![issue(7, &["ui"])], vec![]);
        let clock_calls = Arc::new(AtomicUsize::new(0));
        let clock_calls_for_daemon = clock_calls.clone();
        rig.daemon.clock =
            Arc::new(
                move || match clock_calls_for_daemon.fetch_add(1, Ordering::SeqCst) {
                    0 => T0 + 100,
                    1 => T0 + 300,
                    _ => T0 + 400,
                },
            );
        rig.act(Action::Ticket(crate::sock::TicketAction::ToggleLabel {
            request: "label-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            label: "urgent".to_string(),
            on: true,
        }));
        assert!(rig.daemon.snapshot.repos["borsuk"].issues[&7]
            .labels
            .contains(&"urgent".to_string()));

        rig.poll_started(vec![issue(7, &["ui"])], vec![], T0 + 200);

        assert!(rig.daemon.snapshot.repos["borsuk"].issues[&7]
            .labels
            .contains(&"urgent".to_string()));
    }

    #[test]
    fn a_change_publishes_a_view_with_the_real_task_and_train_values() {
        let (mut rig, rx) = pushed_rig(vec![]);
        rig.poll(vec![], vec![pr(2, false, &["release-stacked"])]);
        let view = rx.try_recv().expect("the poll must publish a view");
        // The stacked pull request reaches the train queue and the gate row.
        assert_eq!(view.trains[0].queue, vec![2]);
        assert_eq!(view.trains[0].stacked, vec![2]);
        assert_eq!(view.decisions.len(), 1);
        assert_eq!(view.decisions[0].id, "gate:borsuk");
        assert_eq!(view.decision_items.len(), 1);
        assert_eq!(view.decision_items[0].repo, "borsuk");
        assert_eq!(view.decision_items[0].kind, ItemKind::Pr);
        assert_eq!(view.decision_items[0].number, 2);
        assert_eq!(view.decision_items[0].title, "pr 2");
        assert_eq!(view.decision_items[0].body, "body 2");

        rig.poll(
            vec![issue(142, &["to-refine"])],
            vec![pr(2, false, &["release-stacked"])],
        );
        let view = rx.try_recv().expect("the second poll must publish a view");
        assert_eq!(view.tasks.len(), 1);
        let task = &view.tasks[0];
        assert_eq!(task.id, "borsuk/refine-i142");
        assert_eq!(task.repo, "borsuk");
        assert_eq!(task.stage, Stage::Refine);
        assert_eq!(task.kind, ItemKind::Issue);
        assert_eq!(task.number, 142);
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.attempt, 1);
        assert!(task.log_path.ends_with("borsuk__refine-i142.jsonl"));
        let refine = stage_view(&view, Stage::Refine);
        assert_eq!(refine.running, 1);
        assert_eq!(refine.queued, 0);
        assert_eq!(refine.limit, 2);
    }

    #[test]
    fn a_view_marks_only_the_limits_that_differ_from_the_config() {
        let (mut rig, rx) = pushed_rig(vec![]);
        rig.act(Action::Limit {
            stage: Stage::Review,
            limit: 5,
        });
        let view = rx.try_recv().expect("the limit change must publish a view");
        let review = stage_view(&view, Stage::Review);
        assert_eq!(review.limit, 5);
        assert!(review.overridden);
        // Limits::from_config seeded every stage key, so a present key is
        // not an override: the untouched stage keeps the config value and
        // reports no override.
        let refine = stage_view(&view, Stage::Refine);
        assert_eq!(refine.limit, 2);
        assert!(!refine.overridden);

        rig.act(Action::Lane {
            stage: Stage::Implement,
            repo: "borsuk".to_string(),
            slots: 1,
        });
        let view = rx.try_recv().expect("the lane change must publish a view");
        assert_eq!(view.lanes.len(), 1);
        assert_eq!(view.lanes[0].stage, Stage::Implement);
        assert_eq!(view.lanes[0].repo, "borsuk");
        assert_eq!(view.lanes[0].slots, 1);

        rig.act(Action::Pause {
            scope: PauseScope::Global,
            paused: true,
        });
        let view = rx.try_recv().expect("the pause change must publish a view");
        assert!(view.paused.global);
        assert!(view.paused.overrides.is_empty());

        rig.act(Action::Policy {
            repo: "borsuk".to_string(),
            policy: ReleasePolicy::Interval { minutes: 5 },
        });
        let view = rx
            .try_recv()
            .expect("the policy change must publish a view");
        assert_eq!(
            view.trains[0].policy,
            ReleasePolicy::Interval { minutes: 5 }
        );
        // The real train state: an interval policy over an empty queue
        // gives no fire time.
        assert_eq!(view.trains[0].next_fire_ms, None);
    }
}
