//! The daemon event loop: one thread that owns all mutable state.
//!
//! The loop blocks on one inbound channel until the next real deadline, so a
//! quiet factory costs nothing. Every message runs [`Daemon::drive`], which
//! settles the finished runs that wait for GitHub, admits gated work, fires
//! due trains, refreshes release gates, reaps idle sessions, and dispatches
//! queued tasks. [`Daemon::drive`] is idempotent and
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
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{
    self, Config, ExecutionRole, Harness, ReleasePolicy, RepoConfig, ResolvedRoleSettings,
    SettingsEdit,
};
use crate::decisions::{self, Decision, DecisionKind, Decisions, Response};
use crate::exec::{Exec, RealExec};
use crate::gates::{
    self, implement_ready, review_ready, GateTracker, ReadyWork, NEEDS_HUMAN_LABEL,
};
use crate::gh::GhClient;
use crate::links::Links;
use crate::model::{ItemKind, RepoSnapshot, Snapshot, Stage};
use crate::poll::DaemonMsg;
use crate::prompts::{self, RESTART_NOTICE};
#[cfg(test)]
use crate::runner::Runner;
use crate::runner::{
    capabilities, AllowedPermission, Answer, DefaultRunnerFactory, Job, RunEvent, RunnerFactory,
    Session,
};
use crate::sched::{self, Limits, Paused, Verdict};
use crate::sock::{
    Action, AskView, InputMode, PauseScope, PromptSource, PromptView, Push, SettingsOperation,
    SettingsResult, SettingsResultStatus, StateInput, StateView, TicketAction, TicketDetails,
    TicketProposal,
};
use crate::state::{DaemonState, RuntimeState, TicketConversationState};
use crate::tasks::{self, Task, TaskPurpose, TaskState, TaskTable};
use crate::ticket::TicketController;
use crate::trains::{Train, STACKED_LABEL};
use crate::usage::{self, SpendTotals, UsageRecord, UsageView};
use crate::worktree::{WorktreeKind, WorktreeManager, TRAIN_DIR};

/// How long a parked session stays alive without activity before the reaper
/// stops its process.
///
/// The task of a reaped session stays `AwaitingUser` and a chat message
/// resumes it later with a fresh process.
pub const DEFAULT_IDLE_REAP_MS: u64 = 30 * 60_000;

/// The largest probe wait of one identity, in minutes.
///
/// A failed probe doubles its identity's wait; the doubling stops here so a
/// persistently failing provider still gets retried about once an hour.
pub const USAGE_WAIT_CAP_MINUTES: u64 = 60;

/// How long the shutdown sequence waits for the agent sessions to report
/// their exit.
///
/// The value covers the full stop ladder of `src/proc.rs`: 10 s after the
/// protocol interrupt, 5 s after `SIGTERM`, and 5 s after `SIGKILL`.
pub const SHUTDOWN_GRACE_MS: u64 = 25_000;

/// How long a finished run may wait for GitHub to show its stage
/// transition.
///
/// The value covers about two 20-second polls and a margin, so a run that
/// did the work never fails because the next poll was slow.
pub const CONFIRM_GRACE_MS: u64 = 50_000;

/// How long a running task may print nothing before the daemon stops its
/// process and retries the run.
///
/// Every harness prints a step, a tool, or a text line long before this,
/// so a silent process is a stalled one, not a slow model. The failure
/// counts as one attempt: the retry ladder and the stuck row apply as for
/// any other failure.
pub const RUN_SILENCE_MS: u64 = 30 * 60_000;

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
    /// One finished usage probe of one billed identity.
    Usage {
        /// The billed identity the probe ran for.
        identity: String,
        /// The probe result. An error carries the failure reason.
        result: Result<UsageRecord, String>,
    },
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
    /// The effective prompt template of every role, as the Settings view
    /// shows it. A dispatch, a prompt save, and a settings reload refresh it
    /// from the prompt files.
    prompts: Vec<PromptView>,
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
    /// The allowed permission rules of each one-shot task, armed for its
    /// next dispatch.
    allowed_permissions: BTreeMap<String, Vec<AllowedPermission>>,
    /// One release train per repository alias.
    trains: BTreeMap<String, Train>,
    /// The pull request set of each release task, for prompt rendering.
    release_batches: BTreeMap<String, Vec<u64>>,
    /// The ticket-PR links of each repository, rebuilt on every poll.
    links: BTreeMap<String, Links>,
    /// The finished pipeline runs that wait for GitHub to confirm their
    /// stage transition, with the moment each one gives up. The map is
    /// runtime only: a restart re-derives the work from the labels.
    confirming: BTreeMap<String, u64>,
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

    /// The last good usage record of each billed identity.
    usage_records: BTreeMap<String, UsageRecord>,
    /// The accumulated factory spend of each billed identity.
    usage_spend: BTreeMap<String, SpendTotals>,
    /// The identities with one probe still running.
    usage_in_flight: BTreeSet<String>,
    /// The next probe moment of each identity, in milliseconds.
    usage_next_probe_ms: BTreeMap<String, u64>,
    /// The current probe wait of each identity, in minutes. A failure
    /// doubles the wait up to the cap; a success resets it.
    usage_wait_minutes: BTreeMap<String, u64>,
    /// The home directory the probes read their credential files from.
    /// Production leaves this unset, so the probes read the operator home.
    usage_home: Option<PathBuf>,
    /// The outbound end of the usage probe channel; each probe thread gets
    /// a clone.
    usage_tx: Sender<(String, Result<UsageRecord, String>)>,
    /// The inbound end of the usage probe channel.
    usage_rx: Option<Receiver<(String, Result<UsageRecord, String>)>>,

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
    /// The outbound end of the poller channel. The poller of an added
    /// repository gets a clone, so it reports into the same channel.
    poll_tx: Sender<DaemonMsg>,
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
        poll_tx: Sender<DaemonMsg>,
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
            poll_tx,
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
        poll_tx: Sender<DaemonMsg>,
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
        let usage_records = runtime.usage.clone();
        let usage_spend = runtime.spend.clone();
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
        // The ask rows of the one-shot tasks survive a restart, because the
        // question they carry is the one thing a restart must not lose.
        for row in runtime.asks {
            let names_kept_task = match &row.kind {
                DecisionKind::Permission { task, .. } | DecisionKind::Question { task, .. } => {
                    restored_task_ids.contains(task.as_str())
                }
                _ => false,
            };
            if names_kept_task {
                decisions.push(row);
            }
        }
        let allowed_permissions: BTreeMap<String, Vec<AllowedPermission>> = runtime
            .allowed_permissions
            .into_iter()
            .filter(|(id, _)| restored_task_ids.contains(id.as_str()))
            .collect();
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
        let (usage_tx, usage_rx) = mpsc::channel();
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
            prompts: prompt_views(&prompts_dir),
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
            allowed_permissions,
            trains,
            release_batches,
            links: BTreeMap::new(),
            confirming: BTreeMap::new(),
            review_tickets,
            ticket_controller,
            ticket_conversations,
            ticket_turn_text: BTreeMap::new(),
            usage_records,
            usage_spend,
            usage_in_flight: BTreeSet::new(),
            usage_next_probe_ms: BTreeMap::new(),
            usage_wait_minutes: BTreeMap::new(),
            usage_home: None,
            usage_tx,
            usage_rx: Some(usage_rx),
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
            poll_tx,
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
        let usage_rx = self.usage_rx.take().unwrap_or_else(|| mpsc::channel().1);
        let action_rx = self.action_rx.take().unwrap_or_else(|| mpsc::channel().1);
        let _forwarders = [
            forwarder("aif-poll", poll_rx, in_tx.clone(), Inbound::Poll)?,
            forwarder("aif-run", run_rx, in_tx.clone(), Inbound::Run)?,
            forwarder("aif-usage", usage_rx, in_tx.clone(), inbound_usage)?,
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
            Inbound::Usage { identity, result } => self.on_usage_result(&identity, result),
        }
        self.drive();
    }

    /// One pass of the factory: settle, admit, fire, gate, reap, dispatch,
    /// and persist.
    ///
    /// The pass is idempotent: a second pass with no new message dispatches
    /// nothing, fires nothing, and writes nothing. The pass never recurses.
    pub fn drive(&mut self) {
        if self.shutdown {
            return;
        }
        self.settle_confirming(None);
        self.admit_ready();
        self.rebuild_stacked();
        self.fire_due_trains();
        self.refresh_release_gates();
        self.reconcile_trains();
        self.reap_idle_sessions();
        self.fail_silent_runs();
        self.poll_usage();
        self.resume_pending_chats();
        self.dispatch_queued();
        self.save_state();
        self.push_state();
    }

    /// The moment the loop must wake next, as a duration from now.
    ///
    /// The answer is the earliest of each interval train's fire moment, each
    /// parked session's reaper expiry, and each identity's next usage probe.
    /// `None` means the loop may block: nothing can become due without a
    /// message.
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
        for task in self.table.active() {
            if task.state != TaskState::Running || !self.sessions.contains_key(&task.id) {
                continue;
            }
            let last = self
                .last_event_ms
                .get(&task.id)
                .copied()
                .unwrap_or(self.now_ms);
            let at = last.saturating_add(RUN_SILENCE_MS);
            earliest = Some(match earliest {
                Some(so_far) => so_far.min(at),
                None => at,
            });
        }
        for at in self.confirming.values() {
            earliest = Some(match earliest {
                Some(so_far) => so_far.min(*at),
                None => *at,
            });
        }
        if self.config.usage.enabled {
            for (identity, at) in &self.usage_next_probe_ms {
                if self.usage_in_flight.contains(identity) {
                    continue;
                }
                earliest = Some(match earliest {
                    Some(so_far) => so_far.min(*at),
                    None => *at,
                });
            }
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

    /// Point the usage probes at one home directory.
    ///
    /// Production never calls this. Tests point the probes at a temporary
    /// home, so they never read the operator's real credentials.
    pub fn set_usage_home(&mut self, home: PathBuf) {
        self.usage_home = Some(home);
    }

    /// Take the inbound end of the usage probe channel.
    ///
    /// The event loop takes it when `run` starts, so a later call returns
    /// None. Tests use this end to apply probe results without threads.
    pub fn take_usage_receiver(
        &mut self,
    ) -> Option<Receiver<(String, Result<UsageRecord, String>)>> {
        self.usage_rx.take()
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
        let usage = self.usage_views();
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
            usage: &usage,
            prompts: &self.prompts,
            role_bindings: &self.role_bindings,
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
    // Usage probes
    // ------------------------------------------------------------------

    /// Spawn one probe for every identity whose next moment is due.
    ///
    /// At most one probe per identity runs at any time. The pass first
    /// drops the schedule of every retired identity. A disabled `[usage]`
    /// table spawns nothing, so the view stays empty and the pipeline draws
    /// no band.
    fn poll_usage(&mut self) {
        if !self.config.usage.enabled {
            return;
        }
        let identities = usage::identities(&self.config);
        // A role edit retires an identity. Its past-due moment would keep
        // the event loop awake with nothing to run, so the schedule maps
        // follow the live identity set.
        let live: BTreeSet<&str> = identities.iter().map(|one| one.id.as_str()).collect();
        self.usage_next_probe_ms
            .retain(|identity, _| live.contains(identity.as_str()));
        self.usage_wait_minutes
            .retain(|identity, _| live.contains(identity.as_str()));
        for identity in identities {
            if self.usage_in_flight.contains(&identity.id) {
                continue;
            }
            let due = self
                .usage_next_probe_ms
                .get(&identity.id)
                .copied()
                .unwrap_or(0);
            if due > self.now_ms {
                continue;
            }
            self.spawn_probe(identity);
        }
    }

    /// Start the probe of one identity on its own thread.
    ///
    /// The thread never touches daemon state; it reports through the usage
    /// channel, and [`Daemon::on_usage_result`] applies the answer.
    fn spawn_probe(&mut self, identity: usage::Identity) {
        self.usage_in_flight.insert(identity.id.clone());
        let exec = Arc::clone(&self.exec);
        let tx = self.usage_tx.clone();
        let now_ms = self.now_ms;
        let home = self.usage_home.clone().unwrap_or_else(usage::home_dir);
        let id = identity.id.clone();
        let thread_identity = identity.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("aif-usage-{id}"))
            .spawn(move || {
                let result = usage::run_probe(&*exec, &thread_identity, &home, now_ms)
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send((id, result));
            });
        if let Err(error) = spawned {
            self.usage_in_flight.remove(&identity.id);
            // Without a new moment the identity stays due and every pass
            // retries at once, so the failure takes the normal cadence.
            self.usage_next_probe_ms.insert(
                identity.id.clone(),
                self.now_ms + self.config.usage.minutes * 60_000,
            );
            eprintln!(
                "aifd: cannot spawn the usage probe of {}: {error}",
                identity.id
            );
        }
    }

    /// Apply one finished probe.
    ///
    /// A success stores the record whole and resets the wait, so a reason
    /// the probe itself reports, such as a pay-as-you-go key, reaches the
    /// panel. A failure keeps the last good record, names the reason, and
    /// doubles the wait up to the cap. Both paths clear the in-flight mark
    /// and schedule the next probe.
    fn on_usage_result(&mut self, identity: &str, result: Result<UsageRecord, String>) {
        self.usage_in_flight.remove(identity);
        let current_wait = self
            .usage_wait_minutes
            .get(identity)
            .copied()
            .unwrap_or(self.config.usage.minutes);
        let next_wait = match result {
            Ok(mut record) => {
                record.harness = self.identity_harness(identity);
                if record.models.is_empty() {
                    record.models = self.identity_models(identity);
                }
                record.updated_ms = self.now_ms;
                self.usage_records.insert(identity.to_string(), record);
                self.config.usage.minutes
            }
            Err(reason) => {
                if let Some(record) = self.usage_records.get_mut(identity) {
                    record.error = Some(reason);
                } else {
                    self.usage_records.insert(
                        identity.to_string(),
                        UsageRecord {
                            harness: self.identity_harness(identity),
                            models: self.identity_models(identity),
                            error: Some(reason),
                            ..UsageRecord::default()
                        },
                    );
                }
                current_wait.saturating_mul(2).min(USAGE_WAIT_CAP_MINUTES)
            }
        };
        self.usage_wait_minutes
            .insert(identity.to_string(), next_wait);
        self.usage_next_probe_ms
            .insert(identity.to_string(), self.now_ms + next_wait * 60_000);
        self.changed = true;
    }

    /// The harness of one identity: the stored record wins, then the id
    /// form decides. An unknown OpenCode provider id reads as OpenCode.
    fn identity_harness(&self, identity: &str) -> Harness {
        if let Some(record) = self.usage_records.get(identity) {
            return record.harness;
        }
        match identity {
            "claude" => Harness::Claude,
            "codex" => Harness::Codex,
            _ => Harness::Opencode,
        }
    }

    /// The configured models that map to one identity.
    fn identity_models(&self, identity: &str) -> Vec<String> {
        usage::identities(&self.config)
            .into_iter()
            .find(|candidate| candidate.id == identity)
            .map(|candidate| candidate.models)
            .unwrap_or_default()
    }

    /// The usage rows of the state view, in panel order.
    ///
    /// The order is claude first, then the OpenCode providers sorted, then
    /// codex, as [`usage::identities`] returns them. Each row joins the
    /// last good record with the live factory spend, so the spend always
    /// shows even before the first probe returns.
    fn usage_views(&self) -> Vec<UsageView> {
        if !self.config.usage.enabled {
            return Vec::new();
        }
        let mut views = Vec::new();
        for identity in usage::identities(&self.config) {
            let spend = self
                .usage_spend
                .get(&identity.id)
                .map(|totals| totals.total_usd)
                .unwrap_or(0.0);
            let record = self
                .usage_records
                .get(&identity.id)
                .cloned()
                .unwrap_or_else(|| UsageRecord {
                    harness: identity.harness,
                    models: identity.models.clone(),
                    ..UsageRecord::default()
                });
            views.push(UsageView::from_record(&identity.id, &record, spend));
        }
        views
    }

    /// Add one finished turn cost to the identity and model of the task.
    ///
    /// The identity comes from the bound role of the task, so a repository
    /// override counts under its own identity.
    fn add_turn_cost(&mut self, task_id: &str, cost_usd: f64) {
        let Some(task) = self.table.by_id.get(task_id) else {
            return;
        };
        let Ok(role) = self.resolved_task_role(task) else {
            return;
        };
        let identity = usage::identity_of(role.settings.harness, &role.settings.model);
        let model = role.settings.model.clone();
        self.usage_spend
            .entry(identity.clone())
            .or_default()
            .add(&model, cost_usd);
        self.changed = true;
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
    /// it writes no state and raises no dirty flag. A poll whose alias the
    /// config no longer holds changes nothing: the poller of a removed
    /// repository sends one last message while its old fetch drains.
    fn apply_poll(&mut self, repo: &str, fresh: RepoSnapshot, started_ms: u64) {
        if !self.config.repos.contains_key(repo) {
            eprintln!("the poll of {repo} names a repository the config no longer holds");
            return;
        }
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
        self.settle_confirming(Some(repo));
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
    ///
    /// A release task stays out, as it does in
    /// [`Daemon::cancel_absent_restored`]: the train, not one pull request,
    /// is its unit. The release task carries the lowest pull request of its
    /// batch, and the agent merges the batch in ascending order, so the
    /// first merge would otherwise cancel the run in the middle of its
    /// work.
    fn cancel_item_tasks(&mut self, repo: &str, kind: ItemKind, number: u64) {
        let ticket_conversation = self
            .ticket_conversations
            .contains_key(&(repo.to_string(), number));
        let ids: Vec<String> = self
            .table
            .active()
            .iter()
            .filter(|task| task.repo == repo && task.kind == kind && task.number == number)
            .filter(|task| task.stage != Stage::Release)
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
        self.confirming.remove(id);
        self.review_tickets.remove(id);
        self.release_batches.remove(id);
        self.pending_chats.remove(id);
        self.ticket_turn_text.remove(id);
        self.paused.tasks.remove(id);
        self.interrupted.remove(id);
        self.restored_ids.remove(id);
        // A rule that outlives its task names an unknown task in the state
        // file, and the next load discards the complete state.
        self.allowed_permissions.remove(id);
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
                            && (review_transitioned(fresh, task.number)
                                || review_handed_off(fresh, task.number)))
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
            if work.stage == Stage::Review && self.head_already_reviewed(&work, &review_tickets) {
                continue;
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

    /// Fail every running task whose process printed nothing for
    /// [`RUN_SILENCE_MS`].
    ///
    /// The failure stops the process and requeues the task, so a stalled
    /// harness does not hold its stage slot for ever. The last attempt
    /// opens a stuck row like any other failure.
    fn fail_silent_runs(&mut self) {
        let silent: Vec<Task> = self
            .table
            .active()
            .into_iter()
            .filter(|task| {
                task.state == TaskState::Running
                    && self.sessions.contains_key(&task.id)
                    && !self.stopping_sessions.contains_key(&task.id)
            })
            .filter(|task| {
                let last = self
                    .last_event_ms
                    .get(&task.id)
                    .copied()
                    .unwrap_or(self.now_ms);
                self.now_ms >= last.saturating_add(RUN_SILENCE_MS)
            })
            .cloned()
            .collect();
        for task in silent {
            let reason = format!(
                "no output for {} minutes; the process is stopped",
                RUN_SILENCE_MS / 60_000
            );
            eprintln!("task {}: {reason}", task.id);
            self.fail_run(&task, &reason);
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
            allowed_permissions: self
                .allowed_permissions
                .get(&task.id)
                .cloned()
                .unwrap_or_default(),
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
    ///
    /// The requeue that leads into the last attempt also drops every saved
    /// session identity, so the last attempt starts fresh instead of
    /// repeating a resume the harness already refused twice. A task that
    /// holds a queued chat message keeps its session; see
    /// [`Daemon::drop_saved_session`].
    fn fail_task(&mut self, id: &str, reason: &str) {
        let Some(task) = self.table.by_id.get(id).cloned() else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        self.stop_session(id, "cannot stop the failed session");
        self.confirming.remove(id);
        let final_attempt = task.attempt >= tasks::MAX_ATTEMPTS;
        if let Err(e) =
            self.table
                .transition(id, TaskState::Failed(reason.to_string()), self.now_ms)
        {
            eprintln!("task {id}: {e:#}");
            return;
        }
        self.changed = true;
        // A one-shot harness cannot answer a live ask, so its permission
        // and question rows survive the failure and wait in the inbox. A
        // steerable claude task loses them, as before, and the stuck row
        // always drops: the fresh failure replaces it.
        let keep_asks = !self.task_capabilities(&task).permission_responses;
        self.decisions.drop_for_task_keep_asks(id, keep_asks);
        if final_attempt {
            let failed = self.table.by_id.get(id).cloned().unwrap_or(task);
            eprintln!("task {id} is stuck on attempt {}: {reason}", failed.attempt);
            let row = Decision::stuck(&failed, reason, self.now_ms);
            self.decisions.push(row);
        } else if let Err(e) = self.table.transition(id, TaskState::Queued, self.now_ms) {
            eprintln!("task {id}: {e:#}");
        } else if self
            .table
            .by_id
            .get(id)
            .is_some_and(|task| task.attempt == tasks::MAX_ATTEMPTS)
        {
            self.drop_saved_session(id);
        }
    }

    /// Drop every saved session identity of one task.
    ///
    /// The task failed twice, and the second run carried the saved session.
    /// The harness no longer knows that session: it was purged, the
    /// worktree was rebuilt, or the harness was reinstalled. The last
    /// attempt then starts a fresh session. The next `Started` event saves
    /// the new identity again, so the session stays resumable after a
    /// restart.
    ///
    /// A queued chat message names that same session, and
    /// [`Daemon::resume_pending_chats`] discards every queued message of a
    /// task it cannot resume. The drop therefore waits while a chat waits,
    /// like the `Done` path and the cancel path that both keep the marker
    /// for a pending chat. The retry resumes the saved session instead, and
    /// the operator sees a stuck row when that resume fails again.
    fn drop_saved_session(&mut self, id: &str) {
        if self.pending_chats.contains_key(id) {
            return;
        }
        let Some(task) = self.table.by_id.get_mut(id) else {
            return;
        };
        task.session_id = None;
        let ticket_chat = task.purpose == TaskPurpose::TicketChat;
        let ticket_key = (task.repo.clone(), task.number);
        let marker_task = task.clone();
        self.remove_task_session_marker(&marker_task);
        if ticket_chat {
            if let Some(conversation) = self.ticket_conversations.get_mut(&ticket_key) {
                conversation.session_id = None;
            }
        }
        self.changed = true;
        eprintln!(
            "task {id}: the saved session did not resume; attempt {} starts a fresh session",
            tasks::MAX_ATTEMPTS
        );
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
            RunEvent::TurnEnd {
                ok,
                summary,
                cost_usd,
                ..
            } => {
                if let Some(cost_usd) = cost_usd {
                    self.add_turn_cost(&task_id, cost_usd);
                }
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
    /// fails from the turn result, or settles through
    /// [`Daemon::settle_finished_run`]. A one-shot turn is only a step
    /// boundary.
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
            self.settle_finished_run(&task);
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
    /// A terminal task ignores the exit. A parked task stays resumable, and
    /// so does a task that waits for its GitHub transition.
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
        // A task that waits for its GitHub transition keeps its state. Its
        // process is gone, and the confirmation sweep decides the result.
        if self.confirming.contains_key(id) {
            return;
        }
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
            self.settle_finished_run(&task);
        } else {
            self.fail_run(&task, detail);
        }
    }

    /// Finish one successful run: complete it, or wait for GitHub.
    ///
    /// An agent that exits with success has not always done the work. The
    /// daemon marks a pipeline task `Done` only after GitHub shows the
    /// stage transition, because the gates are edge-triggered: a stage that
    /// ends without its transition would never open again, and the board
    /// row would say done for ever. A run without the transition therefore
    /// waits in [`Daemon::confirming`] and asks its repository for a poll
    /// at once. A non-pipeline task carries no stage transition, so it
    /// completes here.
    fn settle_finished_run(&mut self, task: &Task) {
        if self.stage_transitioned(task) {
            self.complete_task(task);
            return;
        }
        self.confirming.insert(
            task.id.clone(),
            self.now_ms.saturating_add(CONFIRM_GRACE_MS),
        );
        self.changed = true;
        self.reconcile(Some(&task.repo));
    }

    /// True when GitHub shows the stage transition of one finished task.
    ///
    /// Each stage has one visible result: the refine labels the ticket, the
    /// implement opens a pull request for it, the review takes the pull
    /// request out of the draft state, and the release merges every pull
    /// request of its batch. An item that carries `needs-human` counts as
    /// transitioned: the agent took the documented human path, and the
    /// inbox row carries the work from there. A task that is not a pipeline
    /// task has no transition to check.
    fn stage_transitioned(&self, task: &Task) -> bool {
        if task.purpose != TaskPurpose::Pipeline {
            return true;
        }
        if task.stage == Stage::Release {
            return self.open_batch_prs(task).is_empty();
        }
        let Some(fresh) = self.snapshot.repos.get(&task.repo) else {
            return false;
        };
        let number = task.number;
        let needs_human = match task.kind {
            ItemKind::Issue => fresh
                .issues
                .get(&number)
                .is_some_and(|issue| issue.labels.iter().any(|l| l == NEEDS_HUMAN_LABEL)),
            ItemKind::Pr => fresh
                .prs
                .get(&number)
                .is_some_and(|pull| pull.labels.iter().any(|l| l == NEEDS_HUMAN_LABEL)),
        };
        if needs_human {
            return true;
        }
        match task.stage {
            Stage::Refine => refine_transitioned(fresh, number),
            // The `refined` label is not part of the check. The agent is
            // asked to remove it, and `complete_task` removes a forgotten
            // one, so a pull request alone proves the implementation.
            Stage::Implement => self
                .links
                .get(&task.repo)
                .is_some_and(|links| !links.prs_of(number).is_empty()),
            Stage::Review => {
                review_transitioned(fresh, number)
                    || fresh.prs.get(&number).is_none_or(|pull| !pull.open)
            }
            Stage::Release => true,
        }
    }

    /// The pull requests of a release batch that GitHub still shows open,
    /// ascending. An empty answer means the batch is through.
    ///
    /// A task without a batch entry has nothing to check, so it reports
    /// none. A repository without a snapshot reports the whole batch: the
    /// daemon cannot confirm a merge it never polled.
    fn open_batch_prs(&self, task: &Task) -> Vec<u64> {
        let Some(batch) = self.release_batches.get(&task.id) else {
            return Vec::new();
        };
        let Some(fresh) = self.snapshot.repos.get(&task.repo) else {
            return batch.clone();
        };
        let mut open: Vec<u64> = batch
            .iter()
            .copied()
            .filter(|number| fresh.prs.get(number).is_some_and(|pull| pull.open))
            .collect();
        open.sort_unstable();
        open
    }

    /// Complete or fail the finished runs that wait for their transition.
    ///
    /// `repo` limits the sweep to one repository, for the poll that just
    /// arrived. `None` sweeps every repository, so a deadline expires even
    /// while the polls fail.
    fn settle_confirming(&mut self, repo: Option<&str>) {
        let waiting: Vec<(String, u64)> = self
            .confirming
            .iter()
            .map(|(id, deadline)| (id.clone(), *deadline))
            .collect();
        for (id, deadline) in waiting {
            let Some(task) = self.table.by_id.get(&id).cloned() else {
                self.confirming.remove(&id);
                continue;
            };
            if repo.is_some_and(|alias| task.repo != alias) {
                continue;
            }
            if task.state.is_terminal() {
                self.confirming.remove(&id);
                continue;
            }
            if self.stage_transitioned(&task) {
                self.confirming.remove(&id);
                self.complete_task(&task);
            } else if self.now_ms >= deadline {
                self.confirming.remove(&id);
                let reason = self.unconfirmed_reason(&task);
                self.fail_run(&task, &reason);
            }
        }
    }

    /// The failure reason of a run that never showed its transition.
    fn unconfirmed_reason(&self, task: &Task) -> String {
        let number = task.number;
        match task.stage {
            Stage::Refine => {
                format!("the refine run ended, but ticket #{number} still carries `to-refine`")
            }
            Stage::Implement => {
                format!("the implement run ended, but no PR closes ticket #{number}")
            }
            Stage::Review => format!("the review run ended, but PR #{number} is still a draft"),
            Stage::Release => {
                let open = self.open_batch_prs(task).first().copied().unwrap_or(number);
                format!("the release run ended, but PR #{open} is still open")
            }
        }
    }

    /// Remove a `refined` label that a finished implementation left behind.
    ///
    /// The prompt asks the agent to remove the label, and a forgotten one
    /// keeps the implement gate open, so the next poll would start the
    /// stage again. A failed call goes to standard error only: the work is
    /// done, and the label alone must not fail the task.
    fn clear_refined_label(&self, task: &Task) {
        if task.purpose != TaskPurpose::Pipeline {
            return;
        }
        let labelled = self.snapshot.repos.get(&task.repo).is_some_and(|fresh| {
            fresh
                .issues
                .get(&task.number)
                .is_some_and(|issue| issue.labels.iter().any(|l| l == gates::REFINED))
        });
        if !labelled {
            return;
        }
        let Some(owner_repo) = self
            .config
            .repos
            .get(&task.repo)
            .map(|repo| repo.owner_repo.clone())
        else {
            return;
        };
        let gh = GhClient::new(&*self.exec);
        if let Err(error) = gh.remove_label(&owner_repo, task.number, gates::REFINED) {
            eprintln!(
                "cannot remove {} from {} issue {}: {error:#}",
                gates::REFINED,
                task.repo,
                task.number
            );
        }
    }

    /// Complete one task and apply its stage-specific success action.
    fn complete_task(&mut self, task: &Task) {
        if task.stage == Stage::Review {
            match self.review_contract_met(task) {
                Ok(true) => {}
                Ok(false) => {
                    let reason = format!(
                        "the review left pull request {} a draft with no {NEEDS_HUMAN_LABEL} label",
                        task.number
                    );
                    eprintln!("task {}: {reason}", task.id);
                    self.fail_run(task, &reason);
                    return;
                }
                Err(error) => {
                    let reason = format!("cannot read the reviewed pull request: {error:#}");
                    eprintln!("task {}: {reason}", task.id);
                    self.fail_run(task, &reason);
                    return;
                }
            }
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
        self.confirming.remove(&task.id);
        // A finished pipeline agent has no next turn: its input closes at
        // `Done`. The process would still hold a live slot of the stage,
        // and a release task keeps one id across batches, so the next batch
        // could never start. The refine path stops its session the same way.
        if task.purpose == TaskPurpose::Pipeline && self.task_capabilities(task).live_input {
            self.stop_session(&task.id, "cannot stop the completed session");
        }
        // A live-input task without a saved message loses its restart data
        // at `Done`. A saved message keeps the marker until its next turn.
        // A resumable one-shot task keeps the marker for later follow-ups.
        if self.task_capabilities(task).live_input && !self.pending_chats.contains_key(&task.id) {
            self.remove_task_session_marker(task);
        }
        self.decisions.drop_for_task(&task.id);
        // The permission rules armed one retry; an agent that adapted
        // without them, and a finished task, leave no rules behind.
        self.allowed_permissions.remove(&task.id);
        match task.stage {
            Stage::Release => self.finish_train(&task.repo, true, false),
            Stage::Implement => self.clear_refined_label(task),
            Stage::Refine | Stage::Review => {}
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
        self.append_task_log(task, &format!("aif: dispatch failed: {reason}\n"));
    }

    /// Append one complete line to the log of the task.
    ///
    /// The caller supplies the trailing newline. The runner tees its own
    /// output into the same file in append mode, so both writers add to the
    /// end and neither one truncates the other. Every daemon-side append
    /// goes through this one helper, so no site drifts.
    ///
    /// A failure goes to standard error and returns. The log is a record,
    /// never a control path: a full disk must not fail a dispatch or a chat.
    fn append_task_log(&self, task: &Task, line: &str) {
        if let Some(parent) = task.log_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent) {
                    eprintln!("cannot create the log directory {}: {e}", parent.display());
                    return;
                }
            }
        }
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

    /// Whether one finished review run met the two-outcome contract.
    ///
    /// The review stage ends in one of two ways: the agent finds nothing and
    /// marks the pull request ready for review, or it repairs every finding,
    /// pushes, and marks it ready. The `needs-human` label names the one
    /// explicit rest state between them, where the agent leaves the draft on
    /// purpose. A run that leaves a plain draft did neither, so it did not do
    /// the work the stage exists for.
    ///
    /// The last poll cannot answer this. The agent flipped the draft seconds
    /// ago, so the snapshot still reports a draft. The daemon reads the live
    /// pull request instead.
    ///
    /// A pull request that is no longer open left the pipeline, and
    /// [`Daemon::reconcile_removed`] drops its tasks on the next poll. This
    /// reports success for it, so the daemon does not start a retry that
    /// GitHub already made pointless.
    fn review_contract_met(&self, task: &Task) -> Result<bool> {
        let Some(repo_cfg) = self.config.repos.get(&task.repo) else {
            bail!("no such repository \"{}\"", task.repo);
        };
        let pr = GhClient::new(&*self.exec).fetch_pull(&repo_cfg.owner_repo, task.number)?;
        Ok(!pr.open || !pr.draft || pr.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL))
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
            Action::SavePrompt {
                request,
                role,
                base_revision,
                text,
            } => self.save_prompt(request, role, base_revision, text),
            Action::ResetPrompt {
                request,
                role,
                base_revision,
            } => self.reset_prompt(request, role, base_revision),
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
        let delta = self.config.topology_delta(&candidate);
        if !delta.changed.is_empty() {
            self.push_settings_result(
                request,
                SettingsOperation::Save,
                SettingsResultStatus::RestartRequired,
                current_revision,
                Some(format!(
                    "the path, lane, remote, or release change of {} requires a daemon restart",
                    delta.changed.join(", ")
                )),
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
        let notes = self.activate_config(candidate, revision.clone());
        self.push_settings_result(
            request,
            SettingsOperation::Save,
            SettingsResultStatus::Saved,
            revision,
            (!notes.is_empty()).then(|| notes.join("; ")),
        );
    }

    /// Reload the factory file without changing it on disk.
    ///
    /// The reload also refreshes the prompt views, so an operator who edited
    /// a prompt file with another program sees the new text in the UI.
    fn reload_settings(&mut self, request: String) {
        self.refresh_prompts();
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
        let delta = self.config.topology_delta(&candidate);
        if !delta.changed.is_empty() {
            self.push_settings_result(
                request,
                SettingsOperation::Reload,
                SettingsResultStatus::RestartRequired,
                revision,
                Some(format!(
                    "the path, lane, remote, or release change of {} requires a daemon restart",
                    delta.changed.join(", ")
                )),
            );
            return;
        }
        let notes = self.activate_config(candidate, revision.clone());
        self.push_settings_result(
            request,
            SettingsOperation::Reload,
            SettingsResultStatus::Reloaded,
            revision,
            (!notes.is_empty()).then(|| notes.join("; ")),
        );
    }

    /// Read the complete config and compute its compare-and-save revision.
    fn read_factory_file(&self) -> Result<(String, String)> {
        let text = fs::read_to_string(&self.config_path)
            .with_context(|| format!("cannot read {}", self.config_path.display()))?;
        let revision = config::file_revision(&text);
        Ok((text, revision))
    }

    /// Bring one configured repository live in the running daemon.
    ///
    /// The call prepares the checkout marker, starts the train, seeds the
    /// lane reservations, and spawns the poller thread. The caller activates
    /// the configuration first, so the alias resolves in `self.config`. On
    /// failure the repository stays configured and the error names the
    /// cause; the written config file never rolls back.
    fn add_repository(&mut self, alias: &str) -> Result<()> {
        let Some(repo) = self.config.repos.get(alias).cloned() else {
            bail!("repo.{alias}: no configured repository");
        };
        self.worktrees
            .prepare_checkout(&*self.exec, &repo.path)
            .with_context(|| format!("repo.{alias}: cannot prepare the checkout marker"))?;
        if !self.trains.contains_key(alias) {
            self.trains.insert(alias.to_string(), Train::new(alias));
        }
        for (stage, count) in &repo.lanes {
            self.limits
                .lanes
                .insert((*stage, alias.to_string()), *count);
        }
        if let Some(wake) = crate::poll::spawn_poller(&repo, self.poll_tx.clone()) {
            self.wake.insert(alias.to_string(), wake);
        } else {
            eprintln!("repo.{alias}: cannot start the poller thread");
        }
        self.changed = true;
        Ok(())
    }

    /// Retire one removed repository and every live record it owns.
    ///
    /// The call cancels every active task of the alias first, so the
    /// process escalation runs while the old configuration still resolves
    /// the worktree paths, and then retires every remaining row. The return
    /// counts the stopped active tasks. The call never touches a worktree
    /// directory on disk: a removal keeps every checkout.
    fn remove_repository(&mut self, alias: &str) -> usize {
        let active: Vec<String> = self
            .table
            .active()
            .iter()
            .filter(|task| task.repo == alias)
            .map(|task| task.id.clone())
            .collect();
        let stopped = active.len();
        for id in &active {
            self.cancel_task(id, false);
        }
        let ids: Vec<String> = self
            .table
            .by_id
            .values()
            .filter(|task| task.repo == alias)
            .map(|task| task.id.clone())
            .collect();
        for id in &ids {
            self.retire_task(id);
        }
        // The wake sender dies with the entry, so the poller thread ends.
        self.wake.remove(alias);
        self.snapshot.repos.remove(alias);
        self.gates.forget_repo(alias);
        self.trains.remove(alias);
        self.links.remove(alias);
        self.policies.remove(alias);
        self.pending_stacked.remove(alias);
        self.restore_repos.remove(alias);
        let conversations: Vec<(String, u64)> = self
            .ticket_conversations
            .keys()
            .filter(|(repo, _)| repo == alias)
            .cloned()
            .collect();
        for key in conversations {
            self.ticket_conversations.remove(&key);
        }
        self.limits.lanes.retain(|(_, repo), _| repo != alias);
        self.paused.lanes.retain(|(_, repo), _| repo != alias);
        self.ticket_controller.forget_repo(alias);
        self.pending_ready.retain(|work| work.repo != alias);
        // A release gate names its repository, not a task, so the retire
        // above cannot drop it and no later drive would revisit the alias.
        let open: Vec<String> = self
            .decisions
            .open()
            .iter()
            .filter(|row| row.repo == alias)
            .map(|row| row.id.clone())
            .collect();
        for id in open {
            self.decisions.take(&id);
        }
        self.changed = true;
        stopped
    }

    /// Install a validated configuration in the running daemon.
    ///
    /// The call removes the repositories the candidate drops while the old
    /// configuration still resolves their worktree paths, then swaps the
    /// configuration and brings the added repositories live. The return
    /// carries one human-readable note per reconciled alias.
    fn activate_config(&mut self, config: Config, revision: String) -> Vec<String> {
        let mut notes = Vec::new();
        let delta = self.config.topology_delta(&config);
        for alias in &delta.removed {
            let stopped = self.remove_repository(alias);
            notes.push(format!("removed {alias}: stopped {stopped} active task(s)"));
        }
        for stage in Stage::ALL {
            let old_limit = self.config.stage(stage).limit;
            if self.limits.limit(stage) == old_limit {
                self.limits.stage.insert(stage, config.stage(stage).limit);
            }
        }
        self.config = config;
        self.settings_revision = revision;
        self.changed = true;
        for alias in &delta.added {
            match self.add_repository(alias) {
                Ok(()) => notes.push(format!("added {alias}")),
                Err(error) => notes.push(format!("added {alias}: {error:#}")),
            }
        }
        notes
    }

    /// Write the prompt file of one role and refresh the prompt views.
    ///
    /// The save is a compare-and-swap on the effective prompt: a request
    /// whose base revision is not the current one is refused as stale, and
    /// the result carries the current revision so the UI can retry with it.
    /// A template with an unknown placeholder never reaches the disk; the
    /// message names the placeholder. A running task keeps its prompt; the
    /// next task of the role reads the new file at its start.
    fn save_prompt(
        &mut self,
        request: String,
        role: ExecutionRole,
        base_revision: String,
        text: String,
    ) {
        let Some(file_name) = prompts::file_name(role) else {
            self.refuse_promptless_role(request, SettingsOperation::SavePrompt, role);
            return;
        };
        let Some(current_revision) = self.check_prompt_revision(
            &request,
            SettingsOperation::SavePrompt,
            file_name,
            role,
            &base_revision,
        ) else {
            return;
        };
        if let Err(error) = prompts::check(role, &text) {
            self.refresh_prompts();
            self.push_settings_result(
                request,
                SettingsOperation::SavePrompt,
                SettingsResultStatus::Invalid,
                current_revision,
                Some(format!("the prompt is invalid: {error:#}")),
            );
            return;
        }
        if let Err(error) = prompts::save(&self.prompts_dir, role, &text) {
            self.refresh_prompts();
            self.push_settings_result(
                request,
                SettingsOperation::SavePrompt,
                SettingsResultStatus::Failed,
                current_revision,
                Some(format!("cannot save the prompt: {error:#}")),
            );
            return;
        }
        self.refresh_prompts();
        // The revision comes from the refreshed view, so the result and the
        // view never name two different texts.
        let revision = self.prompt_revision(role);
        self.push_settings_result(
            request,
            SettingsOperation::SavePrompt,
            SettingsResultStatus::Saved,
            revision,
            Some(format!(
                "prompt saved to {file_name}; the next {role} task uses it"
            )),
        );
    }

    /// Remove the prompt file of one role, so the built-in template applies
    /// to the next task of the role.
    fn reset_prompt(&mut self, request: String, role: ExecutionRole, base_revision: String) {
        let (Some(file_name), Some(builtin)) = (prompts::file_name(role), prompts::builtin(role))
        else {
            self.refuse_promptless_role(request, SettingsOperation::ResetPrompt, role);
            return;
        };
        let Some(current_revision) = self.check_prompt_revision(
            &request,
            SettingsOperation::ResetPrompt,
            file_name,
            role,
            &base_revision,
        ) else {
            return;
        };
        if let Err(error) = prompts::reset(&self.prompts_dir, role) {
            self.push_settings_result(
                request,
                SettingsOperation::ResetPrompt,
                SettingsResultStatus::Failed,
                current_revision,
                Some(format!("cannot remove the prompt file: {error:#}")),
            );
            return;
        }
        self.refresh_prompts();
        debug_assert_eq!(self.prompt_revision(role), config::file_revision(builtin));
        self.push_settings_result(
            request,
            SettingsOperation::ResetPrompt,
            SettingsResultStatus::Saved,
            self.prompt_revision(role),
            Some(format!(
                "built-in prompt restored; the next {role} task uses it"
            )),
        );
    }

    /// Compare one prompt request against the effective prompt on disk.
    ///
    /// The current revision comes back when the request may proceed. A
    /// stale or unreadable prompt pushes the result itself and yields
    /// `None`. A stale request also refreshes the prompt views, so the UI
    /// shows the text that won.
    fn check_prompt_revision(
        &mut self,
        request: &str,
        operation: SettingsOperation,
        file_name: &str,
        role: ExecutionRole,
        base_revision: &str,
    ) -> Option<String> {
        let current = match prompts::load(&self.prompts_dir, role) {
            Ok(template) => template,
            // An unreadable file has no revision to compare. A reset is the
            // only way out of that state, so it proceeds and removes the
            // file. A save still refuses: nothing may overwrite a file the
            // daemon cannot read.
            Err(_) if operation == SettingsOperation::ResetPrompt => {
                return Some(self.prompt_revision(role))
            }
            Err(error) => {
                self.push_settings_result(
                    request.to_string(),
                    operation,
                    SettingsResultStatus::Failed,
                    self.prompt_revision(role),
                    Some(format!("cannot read the prompt: {error:#}")),
                );
                return None;
            }
        };
        let current_revision = config::file_revision(&current.text);
        if base_revision != current_revision {
            self.refresh_prompts();
            self.push_settings_result(
                request.to_string(),
                operation,
                SettingsResultStatus::Stale,
                current_revision,
                Some(format!(
                    "{file_name} changed on disk after this edit started; \
                     repeat the action to overwrite it"
                )),
            );
            return None;
        }
        Some(current_revision)
    }

    /// Refuse one prompt request against a role with no template.
    ///
    /// Only a client that outran the daemon can send one: the Settings view
    /// shows no prompt row for the theory roles.
    fn refuse_promptless_role(
        &self,
        request: String,
        operation: SettingsOperation,
        role: ExecutionRole,
    ) {
        self.push_settings_result(
            request,
            operation,
            SettingsResultStatus::Failed,
            self.prompt_revision(role),
            Some(format!("the {role} role has no prompt template")),
        );
    }

    /// The revision the prompt views hold for one role.
    fn prompt_revision(&self, role: ExecutionRole) -> String {
        self.prompts
            .iter()
            .find(|view| view.role == role)
            .map(|view| view.revision.clone())
            .unwrap_or_default()
    }

    /// Read every prompt file again and mark the state changed when one
    /// view differs.
    fn refresh_prompts(&mut self) {
        let fresh = prompt_views(&self.prompts_dir);
        if fresh != self.prompts {
            self.prompts = fresh;
            self.changed = true;
        }
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
    /// Every accepted message also gets one durable user line in the task
    /// log, so the transcript keeps what the human typed across session
    /// switches, refocus, and restarts. The result is true when the daemon
    /// delivered or queued the message.
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
                        self.log_chat_line(&task, text);
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
                        self.log_chat_line(&task, text);
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
        self.log_chat_line(&task, text);
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

    /// Append one user line for an accepted chat message to the task log.
    ///
    /// The line uses the claude user shape, so the transcript parser of
    /// every harness renders it as the human voice. The runner itself never
    /// echoes a typed message into the log, so this line is the only
    /// durable record of the text.
    ///
    /// `chat` calls this exactly once per accepted message: on the live
    /// delivery success, on the live delivery failure, and on the queue
    /// path. `deliver_one_shot_chat` calls it once for the text answer of
    /// a one-shot question row, which takes the same queue path.
    /// `resume_pending_chats` calls it never, because the queue path
    /// already wrote the line. A refused message writes nothing.
    ///
    /// The daemon sends two messages of its own through `chat`: the ticket
    /// refinement handoff and the applied-proposal note. The agent receives
    /// both as user text, so both get the same line and the transcript
    /// stays a true record of the conversation.
    fn log_chat_line(&self, task: &Task, text: &str) {
        // `serde_json` escapes the text, so a quotation mark, a backslash,
        // or a newline stays inside one JSON line.
        let content = serde_json::Value::String(text.to_string());
        self.append_task_log(
            task,
            &format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{content}}}}}\n"
            ),
        );
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

    /// Whether the reviewed-sha marker of the worktree of `work` names its
    /// head.
    ///
    /// The gate memory is empty after a restart, so every open draft pull
    /// request looks new, and a pull request that an earlier review left a
    /// draft would get a second review of the same head. The marker on disk
    /// outlives the restart. An operator answer clears it, so the fresh
    /// review after an answer still starts. A marker read failure counts as
    /// not reviewed: a spare review costs less than a lost one.
    fn head_already_reviewed(&self, work: &ReadyWork, tickets: &BTreeSet<u64>) -> bool {
        let Some(head) = work.head_sha.as_deref() else {
            return false;
        };
        let Some(repo_cfg) = self.config.repos.get(&work.repo) else {
            return false;
        };
        let path = self.review_worktree_path(repo_cfg, work.number, tickets);
        match self.worktrees.read_reviewed_sha(&path) {
            Ok(Some(sha)) if sha == head => {
                eprintln!(
                    "{} pr {}: head {} was reviewed already; the gate does not fire",
                    work.repo, work.number, head
                );
                true
            }
            Ok(_) => false,
            Err(error) => {
                eprintln!(
                    "{} pr {}: cannot read the reviewed-sha marker: {error:#}",
                    work.repo, work.number
                );
                false
            }
        }
    }

    /// The worktree a review of pull request `number` runs in.
    ///
    /// A pull request that closes exactly one ticket reviews in the worktree
    /// of that ticket; any other pull request gets its own. This mirrors
    /// [`Daemon::review_item`] for a review that has no task yet.
    fn review_worktree_path(
        &self,
        repo_cfg: &RepoConfig,
        number: u64,
        tickets: &BTreeSet<u64>,
    ) -> PathBuf {
        match tickets.iter().next().filter(|_| tickets.len() == 1) {
            Some(&ticket) => self.worktrees.issue_path(repo_cfg, ticket),
            None => self.worktrees.pr_path(repo_cfg, number),
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
                    task,
                    request_id,
                    tool,
                    input,
                },
                Response::Allow,
            ) => {
                if self.task_is_one_shot(task) {
                    // The row carries the rule the human granted; the
                    // retried run receives it in the job.
                    self.allow_one_shot_permission(task, tool, input);
                } else if let Err(error) = self.answer_session(
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
                if self.task_is_one_shot(task) {
                    // The row closes and the task keeps its state; the
                    // operator can still retry it from the pipeline.
                } else if let Err(error) = self.answer_session(
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
                if self.task_is_one_shot(task) {
                    // The row carries no option list, so the daemon cannot
                    // fill an answers payload; it keeps the row open.
                    eprintln!(
                        "the answer for {}: a one-shot question row carries no options",
                        decision.id
                    );
                    self.decisions.push(decision.clone());
                    return;
                }
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
                let result = if self.task_is_one_shot(task) {
                    self.deliver_one_shot_chat(task, text)
                } else {
                    self.send_to_session(task, text)
                };
                if let Err(error) = result {
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

    /// Whether the task's harness cannot answer a live permission ask.
    ///
    /// A one-shot harness such as opencode has no steering channel, so its
    /// ask rows travel the rule-and-retry path instead.
    fn task_is_one_shot(&self, task: &str) -> bool {
        self.table
            .by_id
            .get(task)
            .is_some_and(|task| !self.task_capabilities(task).permission_responses)
    }

    /// Whether one open row is the ask of a one-shot task.
    ///
    /// Only those rows persist: a claude ask belongs to a live session and
    /// dies with it, so the state file never mirrors it.
    fn row_is_one_shot_ask(&self, row: &Decision) -> bool {
        match &row.kind {
            DecisionKind::Permission { task, .. } | DecisionKind::Question { task, .. } => {
                self.task_is_one_shot(task)
            }
            DecisionKind::Stuck { .. }
            | DecisionKind::NeedsHuman { .. }
            | DecisionKind::ReleaseGate { .. } => false,
        }
    }

    /// Record one allowed permission rule of a one-shot task and requeue it.
    ///
    /// The recorded rules reach the next dispatch in the job, and the
    /// opencode runner maps them to the `OPENCODE_PERMISSION` environment
    /// value. A failed task requeues from attempt 1 through `retry_task`; a
    /// task the auto-retry already requeued keeps its queue slot and simply
    /// arms the rule for its next dispatch.
    fn allow_one_shot_permission(&mut self, task: &str, tool: &str, input: &serde_json::Value) {
        let patterns = input
            .get("patterns")
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|pattern| pattern.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let rules = self
            .allowed_permissions
            .entry(task.to_string())
            .or_default();
        let rule = AllowedPermission {
            permission: tool.to_string(),
            patterns,
        };
        if !rules.contains(&rule) {
            rules.push(rule);
        }
        let failed = self
            .table
            .by_id
            .get(task)
            .is_some_and(|task| matches!(task.state, TaskState::Failed(_)));
        if failed {
            self.retry_task(task);
        }
    }

    /// Queue one question answer as the follow-up chat of a one-shot task.
    ///
    /// The text waits in `pending_chats`, the terminal task reopens, and
    /// `resume_pending_chats` resumes the recorded session with the text.
    ///
    /// The answer obeys the chat policy, so the inbox opens no door the
    /// chat bar keeps shut. [`Daemon::input_mode`] refuses a run that left
    /// no session marker, a task whose worktree a sibling holds, and a
    /// queued task that never ran: `resume_pending_chats` uses the queued
    /// text as the whole prompt, so a queued task would start with the
    /// answer in place of its stage prompt. A refusal names the reason,
    /// the caller re-pushes the row, and the answer is not lost.
    fn deliver_one_shot_chat(&mut self, task: &str, text: &str) -> Result<()> {
        let task_value = self
            .table
            .by_id
            .get(task)
            .ok_or_else(|| anyhow!("no task holds this answer"))?
            .clone();
        if let InputMode::Closed { reason } = self.input_mode(&task_value) {
            bail!("{reason}");
        }
        self.pending_chats
            .entry(task.to_string())
            .or_default()
            .push(text.to_string());
        // The queue path owns the log line. Without it the answer reaches
        // the agent and leaves no record in the transcript.
        self.log_chat_line(&task_value, text);
        self.reopen_for_pending_chat(task);
        Ok(())
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
        // The gate tracker still remembers the item as ready, so nothing
        // would fire again and the stage would stop here. Forgetting the
        // item makes the next poll re-open every gate of it, and
        // `admit_ready` replaces a terminal task with a fresh one that
        // resumes the saved session through the worktree marker.
        self.gates.forget(&repo, kind, number);
        // The reviewed-sha marker says this head was reviewed, and the
        // gate would skip it. The answer asks for a fresh review of the
        // same head, so the marker goes.
        if kind == ItemKind::Pr {
            let tickets: BTreeSet<u64> = self
                .links
                .get(&repo)
                .map(|links| links.tickets_of(number).into_iter().collect())
                .unwrap_or_default();
            let path = self.review_worktree_path(&repo_cfg, number, &tickets);
            if let Err(error) = self.worktrees.clear_reviewed_sha(&path) {
                eprintln!("{repo} pr {number}: cannot clear the reviewed-sha marker: {error:#}");
            }
        }
        // A parked agent waits for exactly this answer, so it gets the
        // text as a chat message instead of a new run.
        if let Some(text) = comment {
            if let Some(id) = self.parked_task_of(&repo, kind, number) {
                self.chat(&id, text);
            }
        }
        self.reconcile(Some(&repo));
    }

    /// The parked pipeline task of one item that takes a typed answer.
    ///
    /// Only a live-input session waits inside a turn. A one-shot task has
    /// no process to answer, so the fresh gate run carries the item on.
    fn parked_task_of(&self, repo: &str, kind: ItemKind, number: u64) -> Option<String> {
        self.table
            .active()
            .into_iter()
            .find(|task| {
                task.repo == repo
                    && task.kind == kind
                    && task.number == number
                    && task.purpose == TaskPurpose::Pipeline
                    && task.state == TaskState::AwaitingUser
                    && self.task_capabilities(task).live_input
            })
            .map(|task| task.id.clone())
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
        self.confirming.remove(id);
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
        self.allowed_permissions.remove(id);
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
    /// The template comes from the prompt file of the task's role in the
    /// config directory, read at this moment, or from the built-in default.
    /// Every placeholder must be known; an unknown one is an error that
    /// names it, never a silent literal.
    fn render_prompt(
        &mut self,
        task: &Task,
        repo_cfg: &RepoConfig,
        worktree: &Path,
    ) -> Result<String> {
        let role = Self::execution_role(task);
        let template = self.prompt_template(role)?;
        // A crash between the write and the rename can leave a blank file.
        // An agent with no instructions is worse than a failed dispatch.
        if template.trim().is_empty() {
            bail!(
                "the {role} prompt file {} is empty",
                prompts::file_name(role).unwrap_or_default()
            );
        }
        let values = self.placeholder_values(task, repo_cfg, worktree)?;
        prompts::fill_template(&template, &values)
    }

    /// The placeholder values of one task, in the order of
    /// [`prompts::placeholders`] for its role.
    fn placeholder_values(
        &self,
        task: &Task,
        repo_cfg: &RepoConfig,
        worktree: &Path,
    ) -> Result<Vec<(&'static str, String)>> {
        if Self::is_ticket_creation(task) {
            return Ok(vec![
                ("repo", task.repo.clone()),
                ("owner_repo", repo_cfg.owner_repo.clone()),
                ("worktree", worktree.display().to_string()),
            ]);
        }
        if Self::is_ticket_chat(task) {
            let issue = self
                .snapshot
                .repos
                .get(&task.repo)
                .and_then(|snapshot| snapshot.issues.get(&task.number))
                .ok_or_else(|| anyhow!("the issue is absent from the current snapshot"))?;
            return Ok(vec![
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
            ]);
        }
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
        Ok(vec![
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
        ])
    }

    /// Read the prompt template of one role at this moment.
    ///
    /// The prompt file wins; an absent file yields the built-in. The read
    /// also refreshes the prompt view of the role, so a file another
    /// program edited reaches the UI at the next task start.
    fn prompt_template(&mut self, role: ExecutionRole) -> Result<String> {
        let template = prompts::load(&self.prompts_dir, role)?;
        let view = prompt_view(role, &template);
        if let Some(slot) = self.prompts.iter_mut().find(|view| view.role == role) {
            if *slot != view {
                *slot = view;
                self.changed = true;
            }
        }
        Ok(template.text)
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
            usage: self.usage_records.clone(),
            spend: self.usage_spend.clone(),
            asks: self
                .decisions
                .open()
                .iter()
                .filter(|row| self.row_is_one_shot_ask(row))
                .cloned()
                .collect(),
            allowed_permissions: self.allowed_permissions.clone(),
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

/// True when GitHub shows the review handed the pull request to a human.
///
/// The agent adds the `needs-human` label first and writes its question
/// comment after. The label closes the review gate, so a poll between those
/// two steps would otherwise cancel the run and leave the operator a label
/// with no question. This transition is as valid as the ready flip.
fn review_handed_off(fresh: &RepoSnapshot, number: u64) -> bool {
    fresh
        .prs
        .get(&number)
        .is_some_and(|pr| pr.open && pr.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL))
}

/// The effective prompt view of every role that has a template, in role
/// order. The theory roles carry no template, so they get no view.
///
/// An unreadable prompt file yields the built-in view and one line on
/// standard error; the dispatch reports the same read error to the task.
fn prompt_views(prompts_dir: &Path) -> Vec<PromptView> {
    prompts::ROLES
        .into_iter()
        .filter_map(|role| {
            let builtin = prompts::builtin(role)?;
            let template = prompts::load(prompts_dir, role).unwrap_or_else(|error| {
                eprintln!("cannot read the {role} prompt: {error:#}");
                prompts::Template {
                    text: builtin.to_string(),
                    from_file: false,
                }
            });
            Some(prompt_view(role, &template))
        })
        .collect()
}

/// The socket view of one loaded template.
fn prompt_view(role: ExecutionRole, template: &prompts::Template) -> PromptView {
    PromptView {
        role,
        source: if template.from_file {
            PromptSource::File
        } else {
            PromptSource::Builtin
        },
        text: template.text.clone(),
        revision: config::file_revision(&template.text),
    }
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

/// Pack one finished usage probe for the shared inbound queue.
fn inbound_usage((identity, result): (String, Result<UsageRecord, String>)) -> Inbound {
    Inbound::Usage { identity, result }
}

/// The current time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionRole, Harness, RoleSettings, StageConfig};
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, Pr, RepoSnapshot};
    use crate::prompts::{
        scan_placeholders, IMPLEMENT_PROMPT, REFINE_PROMPT, RELEASE_PROMPT, REVIEW_PROMPT,
    };
    use crate::tasks::MAX_ATTEMPTS;
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

    /// The `fetch_pull` step that one review completion reads.
    ///
    /// [`Daemon::review_contract_met`] runs this call before it completes a
    /// review task, so every test that finishes a review scripts one step.
    /// `draft` and the labels shape the verdict.
    fn gh_pull_step(number: u64, draft: bool, labels: &[&str]) -> Step {
        let url = format!("repos/acme/borsuk/pulls/{number}");
        let names = labels
            .iter()
            .map(|name| format!("{{\"name\":\"{name}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(
            "{{\"number\":{number},\"node_id\":\"prnode-{number}\",\"title\":\"pr {number}\",\
             \"body\":null,\"state\":\"open\",\"labels\":[{names}],\"draft\":{draft},\
             \"head\":{{\"sha\":\"sha{number}\",\"ref\":\"branch-{number}\"}}}}"
        );
        gh_step(
            &["api", "-i", "-X", "GET", &url],
            CmdOut::ok(format!("HTTP/1.1 200 OK\r\n\r\n{body}")),
        )
    }

    /// The `fetch_pull` step of a review that met the contract.
    fn gh_pull_ready(number: u64) -> Step {
        gh_pull_step(number, false, &[])
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
            usage: crate::config::UsageConfig::default(),
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

    /// One open, ready pull request whose branch closes `ticket`.
    fn linked_pr(number: u64, ticket: u64) -> Pr {
        let mut pull = pr(number, false, &[]);
        pull.head_ref = format!("aif/borsuk/issue-{ticket}");
        pull
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
            // The probes call real programs through the scripted exec, so a
            // rig keeps them off unless a usage test turns them on.
            config.usage.enabled = false;
            tweak(&mut config);
            let exec = scripted(steps);
            let jobs = Arc::new(Mutex::new(Vec::new()));
            let sessions = Arc::new(Mutex::new(Vec::new()));
            let roles = Arc::new(Mutex::new(Vec::new()));
            let (poll_tx, poll_rx) = mpsc::channel::<DaemonMsg>();
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
                poll_tx,
                poll_rx,
                wake,
                action_rx,
                runner_factory,
                paused,
            );
            let clock_t = t.clone();
            daemon.clock = Arc::new(move || *clock_t.lock().unwrap());
            // The probes read their credential files under this home, so a
            // test never reads the credentials of the operator.
            daemon.set_usage_home(dir.join("usage-home"));
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

        /// Apply one poll that shows the finished implementation of
        /// ticket 142: the ticket lost its labels, and pull request 5
        /// closes it. A running implement task confirms its stage
        /// transition from this poll.
        fn poll_implemented(&mut self) {
            self.poll(vec![issue(142, &[])], vec![linked_pr(5, 142)]);
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
    fn a_second_failed_resume_drops_the_saved_session_for_the_last_attempt() {
        let dir = temp_root();
        let wt = issue_wt(&dir, 142);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &wt, 142, &rig_gitdir(&dir))
            .into_iter()
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .chain(reuse_issue_steps(&rig_repo(&dir), &wt, &rig_gitdir(&dir)))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "session-1"));
        let marker = |rig: &Rig| {
            rig.daemon
                .worktrees
                .read_task_session(&wt, "borsuk/implement-i142")
                .unwrap()
        };

        // A failure on attempt 1 keeps the saved session for attempt 2.
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 2);
        assert_eq!(
            rig.task("borsuk/implement-i142").session_id.as_deref(),
            Some("session-1")
        );
        assert_eq!(marker(&rig), Some("session-1".to_string()));
        assert_eq!(rig.job_count(), 2);
        assert_eq!(rig.job(1).resume.as_deref(), Some("session-1"));

        // A second failure clears every saved identity: attempt 3 runs
        // fresh instead of repeating the same failed resume.
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);
        assert_eq!(rig.task("borsuk/implement-i142").session_id, None);
        assert_eq!(marker(&rig), None);
        assert_eq!(rig.job_count(), 3);
        assert_eq!(rig.job(2).resume, None);

        // The final failure opens the stuck row as today, and no session
        // data survives the terminal state.
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert!(rig.decision("stuck:borsuk/implement-i142:3").is_some());
        assert_eq!(rig.task("borsuk/implement-i142").session_id, None);
        assert_eq!(marker(&rig), None);
    }

    #[test]
    fn a_second_failed_ticket_chat_resume_clears_the_conversation_session() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir, vec![], |_| {});
        rig.poll(vec![issue(7, &[])], vec![]);
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
        rig.event(started("borsuk/ticket-i7", "session-ticket-7"));
        let key = ("borsuk".to_string(), 7);
        let conversation = |rig: &Rig| rig.daemon.ticket_conversations[&key].session_id.clone();
        let checkout = rig.repo.clone();
        let marker = |rig: &Rig| {
            rig.daemon
                .worktrees
                .read_task_session(&checkout, "borsuk/ticket-i7")
                .unwrap()
        };
        assert_eq!(conversation(&rig).as_deref(), Some("session-ticket-7"));

        // A failure on attempt 1 keeps every saved identity.
        rig.event(exited("borsuk/ticket-i7", false, "boom"));
        assert_eq!(rig.task("borsuk/ticket-i7").attempt, 2);
        assert_eq!(conversation(&rig).as_deref(), Some("session-ticket-7"));
        assert_eq!(marker(&rig).as_deref(), Some("session-ticket-7"));
        assert_eq!(rig.job(1).resume.as_deref(), Some("session-ticket-7"));

        // The requeue into the last attempt clears the task, the marker,
        // and the conversation of the ticket chat.
        rig.event(exited("borsuk/ticket-i7", false, "boom"));
        assert_eq!(rig.task("borsuk/ticket-i7").attempt, 3);
        assert_eq!(rig.task("borsuk/ticket-i7").session_id, None);
        assert_eq!(conversation(&rig), None);
        assert_eq!(marker(&rig), None);
        assert_eq!(rig.job(2).resume, None, "the last attempt runs fresh");
    }

    #[test]
    fn a_queued_chat_message_keeps_the_saved_session_of_the_last_attempt() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 3);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 2);

        // The operator types while attempt 2 runs. A one-shot harness takes
        // no live input, so the message waits for the next turn.
        assert!(rig.daemon.chat("borsuk/implement-i142", "check the parser"));

        rig.event(exited("borsuk/implement-i142", false, "boom"));

        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);
        assert_eq!(
            rig.task("borsuk/implement-i142").session_id.as_deref(),
            Some("ses-142"),
            "the queued message names the saved session, so the drop waits"
        );
        assert_eq!(rig.job_count(), 3);
        assert_eq!(
            rig.job(2).prompt,
            "check the parser",
            "the last attempt carries the typed message, not a fresh stage prompt"
        );
        assert_eq!(
            rig.job(2).resume.as_deref(),
            Some("ses-142"),
            "the follow-up turn resumes the session the message names"
        );
        assert!(
            !rig.daemon
                .pending_chats
                .contains_key("borsuk/implement-i142"),
            "the delivered message leaves no queue behind"
        );
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
                .chain(std::iter::once(gh_pull_ready(5)))
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

        // GitHub shows the review transition: the pull request left the
        // draft state.
        rig.poll(vec![], vec![pr(5, false, &[])]);
        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = issue_wt(&dir, 5).join(".aif").join("reviewed-sha");
        assert_eq!(fs::read_to_string(marker).unwrap().trim_end(), "sha5");

        rig.poll(vec![], vec![pr(5, false, &[]), pr(6, true, &[])]);
        rig.event(turn_finished("borsuk/review-p6", false, "lint"));
        assert_eq!(rig.task("borsuk/review-p6").attempt, 2);
        assert!(!issue_wt(&dir, 6).join(".aif").join("reviewed-sha").exists());
    }

    /// The review stage has two outcomes, and both leave the pull request
    /// ready for review. A run that ends on a plain draft did neither, so
    /// the clean exit is not a success.
    ///
    /// The daemon waits for GitHub first. Every poll still shows the plain
    /// draft, so the task waits in `confirming` until the grace runs out,
    /// then fails with "the review run ended, but PR #5 is still a draft"
    /// and retries. [`Daemon::complete_task`] never runs, so the daemon
    /// reads no live pull request on this path.
    #[test]
    fn a_review_that_leaves_a_plain_draft_fails_and_retries() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);

        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));

        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Running,
            "the finished run waits for the poll"
        );

        rig.set_now(T0 + CONFIRM_GRACE_MS);
        rig.drive();

        let task = rig.task("borsuk/review-p5");
        assert_eq!(
            task.state,
            TaskState::Queued,
            "a review that left the draft alone must run again"
        );
        assert_eq!(task.attempt, 2);
        assert!(
            !worktree.join(".aif").join("reviewed-sha").exists(),
            "a review that did no work marks no reviewed head"
        );
        assert!(
            !rig.exec.calls().iter().any(|call| {
                call.program == "gh" && call.argv().contains(&"repos/acme/borsuk/pulls/5")
            }),
            "the unconfirmed run reads no live pull request: {:?}",
            rig.exec.calls()
        );
    }

    /// The agent adds the `needs-human` label first and writes its question
    /// comment after. The label closes the review gate, so a poll between
    /// those two steps must not cancel the run. A cancelled run leaves the
    /// label with no question, and the operator reads an empty inbox row.
    #[test]
    fn a_needs_human_label_keeps_the_running_review_alive() {
        let dir = temp_root();
        let steps = fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Running);

        rig.poll(vec![], vec![pr(5, true, &[NEEDS_HUMAN_LABEL])]);

        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Running,
            "the agent must reach its question comment"
        );
    }

    /// The `needs-human` label is the explicit rest state of the stage. The
    /// agent left the draft on purpose, so the run met the contract.
    ///
    /// The label is the visible result of the stage, so a poll must show it
    /// before the task ends. [`Daemon::complete_task`] then reads the live
    /// pull request as a second guard, and the label satisfies it too.
    #[test]
    fn a_review_that_labels_needs_human_completes() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(std::iter::once(gh_pull_step(5, true, &[NEEDS_HUMAN_LABEL])))
            .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);

        // The agent labels the pull request, and the next poll shows it.
        rig.poll(vec![], vec![pr(5, true, &[NEEDS_HUMAN_LABEL])]);
        rig.event(turn_finished(
            "borsuk/review-p5",
            true,
            "asked the operator",
        ));

        assert_eq!(
            rig.task("borsuk/review-p5").state,
            TaskState::Done,
            "the labelled rest state ends the run"
        );
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
        rig.poll(vec![], vec![pr(5, false, &[])]);
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
            .chain(std::iter::once(gh_pull_ready(5)))
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

        rig.poll(vec![], vec![pr(5, false, &[])]);
        rig.event(turn_finished("borsuk/review-p5", true, "lgtm"));

        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = worktree.join(".aif").join("reviewed-sha");
        assert_eq!(fs::read_to_string(marker).unwrap().trim_end(), "sha5");
    }

    /// The gate memory is empty after a restart, so a draft pull request
    /// that a legacy review left a draft would get a second review of the
    /// same head. The reviewed-sha marker on disk outlives the restart.
    #[test]
    fn a_reviewed_head_gets_no_second_review_after_a_restart() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(std::iter::once(gh_pull_step(5, true, &[NEEDS_HUMAN_LABEL])))
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &issue_wt(&dir, 5),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        assert_eq!(rig.job_count(), 1);
        rig.poll(vec![], vec![pr(5, true, &[NEEDS_HUMAN_LABEL])]);
        rig.event(turn_finished("borsuk/review-p5", true, "asked"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = issue_wt(&dir, 5).join(".aif").join("reviewed-sha");
        assert!(marker.exists());
        // The completed task stopped its process; the exit frees the slot.
        rig.event(exited("borsuk/review-p5", true, "stopped"));

        // A restart forgets every gate. The same head, a draft without the
        // label, must not start a second review.
        rig.daemon.gates.forget("borsuk", ItemKind::Pr, 5);
        rig.poll(vec![], vec![pr(5, true, &[])]);
        assert_eq!(rig.job_count(), 1, "the reviewed head fires no gate");
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);

        // A push moves the head, and the new head gets its review.
        let mut pushed = pr(5, true, &[]);
        pushed.head_sha = "sha5b".to_string();
        rig.poll(vec![], vec![pushed]);
        assert_eq!(rig.job_count(), 2, "a new head reviews");
    }

    /// The answer to a needs-human pull request asks for a fresh review of
    /// the same head, so the reviewed-sha marker must go with the label.
    #[test]
    fn a_needs_human_answer_clears_the_reviewed_marker() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(vec![
                    gh_pull_step(5, true, &[NEEDS_HUMAN_LABEL]),
                    gh_step(
                        &[
                            "api",
                            "-X",
                            "POST",
                            "repos/acme/borsuk/issues/5/comments",
                            "-f",
                            "body=use the fast path",
                        ],
                        CmdOut::ok(""),
                    ),
                    gh_step(
                        &[
                            "api",
                            "-i",
                            "-X",
                            "DELETE",
                            "repos/acme/borsuk/issues/5/labels/needs-human",
                        ],
                        gh_ok(),
                    ),
                ])
                .chain(reuse_issue_steps(
                    &rig_repo(&dir),
                    &issue_wt(&dir, 5),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.poll(vec![], vec![pr(5, true, &[NEEDS_HUMAN_LABEL])]);
        rig.event(turn_finished("borsuk/review-p5", true, "asked"));
        assert_eq!(rig.task("borsuk/review-p5").state, TaskState::Done);
        let marker = issue_wt(&dir, 5).join(".aif").join("reviewed-sha");
        assert!(marker.exists());
        // The completed task stopped its process; the exit frees the slot.
        rig.event(exited("borsuk/review-p5", true, "stopped"));

        let row = rig
            .daemon
            .decisions
            .open()
            .iter()
            .find(|row| {
                matches!(
                    &row.kind,
                    DecisionKind::NeedsHuman {
                        kind: ItemKind::Pr,
                        number: 5,
                        ..
                    }
                )
            })
            .map(|row| row.id.clone())
            .expect("the label opens a needs-human row");
        rig.act(Action::Answer {
            decision_id: row,
            response: Response::Text {
                text: "use the fast path".to_string(),
            },
        });
        assert!(!marker.exists(), "the answer forgets the reviewed head");

        rig.poll(vec![], vec![pr(5, true, &[])]);
        assert_eq!(rig.job_count(), 2, "the same head gets its fresh review");
    }

    #[test]
    fn a_review_without_a_stored_sha_takes_the_polled_head() {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 5);
        let steps: Vec<Step> = fresh_issue_steps(&rig_repo(&dir), &worktree, 5, &rig_gitdir(&dir))
            .into_iter()
            .chain(std::iter::once(gh_pull_ready(5)))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(5, true, &[])]);
        rig.daemon
            .table
            .by_id
            .get_mut("borsuk/review-p5")
            .unwrap()
            .head_sha = None;
        rig.poll(vec![], vec![pr(5, false, &[])]);

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

    // ------------------------------------------------------------------
    // One-shot ask propagation
    // ------------------------------------------------------------------

    /// The ask row id of the implement task's `n`-th auto-rejected ask.
    fn ask_row_id(n: usize) -> String {
        format!("perm:borsuk/implement-i142:rej-{n}")
    }

    /// The auto-rejected ask event the opencode runner emits for one line.
    fn opencode_ask(n: usize, tool: &str, patterns: &[&str]) -> RunEvent {
        RunEvent::Ask {
            task: "borsuk/implement-i142".to_string(),
            request_id: format!("rej-{n}"),
            tool: tool.to_string(),
            input: json!({"patterns": patterns}),
            suggestions: serde_json::Value::Null,
            needs_human: tool == "question",
        }
    }

    /// Run an opencode implement task to its final failure, with the ask
    /// open from attempt 1. Each retry re-hits the ask, so the row
    /// refreshes. The requeued runs start inside the event handling, so
    /// every non-final failure consumes one dispatch.
    fn opencode_rig_failed_with_ask(dir: &Path) -> Rig {
        let mut rig = opencode_rig(dir, MAX_ATTEMPTS as usize);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(opencode_ask(
            1,
            "external_directory",
            &["/home/navaro/.cargo/registry/src/*"],
        ));
        let opened = rig
            .decision(&ask_row_id(1))
            .expect("the ask opens a row")
            .opened_ms;
        for attempt in 1..=MAX_ATTEMPTS {
            rig.event(exited(
                "borsuk/implement-i142",
                false,
                "opencode exited with code 1",
            ));
            assert!(
                rig.decision(&ask_row_id(1)).is_some(),
                "attempt {attempt} keeps the ask row"
            );
            if attempt < MAX_ATTEMPTS {
                let task = rig.task("borsuk/implement-i142");
                assert_eq!(task.attempt, attempt + 1, "attempt {attempt} requeues");
                assert_eq!(
                    rig.job_count(),
                    attempt as usize + 1,
                    "the auto-retry of attempt {attempt} starts at once"
                );
                rig.event(started("borsuk/implement-i142", "ses-142"));
                rig.event(opencode_ask(
                    1,
                    "external_directory",
                    &["/home/navaro/.cargo/registry/src/*"],
                ));
            }
        }
        assert!(matches!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed(_)
        ));
        let row = rig
            .decision(&ask_row_id(1))
            .expect("the final failure keeps the ask row");
        assert_eq!(row.opened_ms, opened, "the refresh keeps the open time");
        assert!(
            rig.decision(&format!("stuck:borsuk/implement-i142:{MAX_ATTEMPTS}"))
                .is_some(),
            "the final failure opens the stuck row"
        );
        rig
    }

    /// An opencode task that fails keeps its permission row open, and a
    /// claude task that fails still loses its rows.
    #[test]
    fn an_opencode_failure_keeps_the_ask_row_and_a_claude_failure_loses_it() {
        let dir = temp_root();
        let rig = opencode_rig_failed_with_ask(&dir);
        assert!(rig.daemon.allowed_permissions.is_empty());

        // A claude task with a live ask loses the row on failure, as today.
        let mut claude = Rig::make_with(vec![], |config| {
            config.stages.get_mut(&Stage::Refine).unwrap().yolo = false;
        });
        claude.poll(vec![issue(142, &["to-refine"])], vec![]);
        claude.event(RunEvent::Ask {
            task: "borsuk/refine-i142".to_string(),
            request_id: "req-1".to_string(),
            tool: "Bash".to_string(),
            input: json!({"command": "ls"}),
            suggestions: serde_json::Value::Null,
            needs_human: false,
        });
        assert!(claude.decision("perm:borsuk/refine-i142:req-1").is_some());
        claude.event(exited("borsuk/refine-i142", false, "boom"));
        assert!(
            claude.decision("perm:borsuk/refine-i142:req-1").is_none(),
            "the claude failure still drops its ask row"
        );
    }

    /// The kept ask row survives a `state.json` round trip.
    #[test]
    fn the_kept_ask_row_survives_a_restart() {
        let dir = temp_root();
        {
            let mut rig = opencode_rig_failed_with_ask(&dir);
            rig.drive();
        }
        let steps = reuse_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 142), &rig_gitdir(&dir));
        let second = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, Harness::Opencode);
        });
        let row = second
            .decision(&ask_row_id(1))
            .expect("the ask row survives the restart");
        assert!(matches!(row.kind, DecisionKind::Permission { .. }));
        assert!(
            second
                .decision(&format!("stuck:borsuk/implement-i142:{MAX_ATTEMPTS}"))
                .is_some(),
            "the stuck row survives as before"
        );
    }

    /// A completed task closes its ask row: an agent that adapted without
    /// the permission leaves no inbox noise.
    #[test]
    fn a_completed_opencode_task_closes_its_ask_row() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, MAX_ATTEMPTS as usize);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(opencode_ask(1, "external_directory", &["/tmp/*"]));
        rig.event(exited(
            "borsuk/implement-i142",
            false,
            "opencode exited with code 1",
        ));
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 2);
        assert_eq!(rig.job_count(), 2, "the auto-retry starts at once");
        assert!(rig.decision(&ask_row_id(1)).is_some());

        // The retry adapts without the permission and finishes.
        rig.event(started("borsuk/implement-i142", "ses-142b"));
        rig.poll_implemented();
        rig.event(exited("borsuk/implement-i142", true, "done"));

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert!(
            rig.decision(&ask_row_id(1)).is_none(),
            "the success closes the ask row"
        );
    }

    /// Cancelling the task closes its ask row.
    #[test]
    fn a_cancelled_opencode_task_closes_its_ask_row() {
        let dir = temp_root();
        let mut rig = opencode_rig_failed_with_ask(&dir);
        assert!(rig.decision(&ask_row_id(1)).is_some());

        rig.act(Action::Abort {
            task: "borsuk/implement-i142".to_string(),
        });

        assert!(
            rig.decision(&ask_row_id(1)).is_none(),
            "the cancel closes the ask row"
        );
        assert!(
            rig.daemon.allowed_permissions.is_empty(),
            "the cancel clears the permission rules"
        );
    }

    /// An `Allow` answer records the rule, requeues the failed task from
    /// attempt 1, and the next dispatch carries the rule in the job.
    #[test]
    fn an_allow_answer_arms_the_rule_and_requeues_from_attempt_1() {
        let dir = temp_root();
        let mut rig = opencode_rig_failed_with_ask(&dir);

        rig.act(Action::Answer {
            decision_id: ask_row_id(1),
            response: Response::Allow,
        });

        assert!(
            rig.decision(&ask_row_id(1)).is_none(),
            "the answer closes the row"
        );
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.attempt, 1, "the requeue starts at attempt 1");
        assert_eq!(rig.job_count(), 4, "the requeued task starts at once");
        assert_eq!(
            rig.daemon.allowed_permissions.get("borsuk/implement-i142"),
            Some(&vec![AllowedPermission {
                permission: "external_directory".to_string(),
                patterns: vec!["/home/navaro/.cargo/registry/src/*".to_string()],
            }]),
        );

        let job = rig.job(3);
        assert_eq!(job.task, "borsuk/implement-i142");
        assert_eq!(
            job.allowed_permissions,
            vec![AllowedPermission {
                permission: "external_directory".to_string(),
                patterns: vec!["/home/navaro/.cargo/registry/src/*".to_string()],
            }],
        );

        // The granted run completes and leaves no rules behind.
        rig.event(started("borsuk/implement-i142", "ses-142c"));
        rig.poll_implemented();
        rig.event(exited("borsuk/implement-i142", true, "done"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert!(rig.daemon.allowed_permissions.is_empty());
    }

    /// A `Deny` answer closes the row and leaves the task state unchanged.
    #[test]
    fn a_deny_answer_closes_the_row_and_keeps_the_state() {
        let dir = temp_root();
        let mut rig = opencode_rig_failed_with_ask(&dir);

        rig.act(Action::Answer {
            decision_id: ask_row_id(1),
            response: Response::Deny {
                message: "not now".to_string(),
            },
        });

        assert!(rig.decision(&ask_row_id(1)).is_none());
        assert!(matches!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed(_)
        ));
        assert!(rig.daemon.allowed_permissions.is_empty());
        assert_eq!(rig.job_count(), 3, "the deny requeues nothing");
    }

    /// A text answer to a one-shot question resumes the recorded session
    /// with the text through the pending-chats path, and the queue path
    /// writes the one user line that records the answer.
    #[test]
    fn a_text_answer_resumes_a_one_shot_task_with_a_session_marker() {
        let dir = temp_root();
        let mut rig = opencode_rig_failed_with_ask(&dir);
        rig.event(opencode_ask(2, "question", &[]));
        let row_id = ask_row_id(2);

        rig.act(Action::Answer {
            decision_id: row_id.clone(),
            response: Response::Text {
                text: "use the vendored sources".to_string(),
            },
        });

        assert_eq!(rig.job_count(), 4, "the answer resumes the task at once");
        let job = rig.job(3);
        assert_eq!(job.task, "borsuk/implement-i142");
        assert_eq!(job.prompt, "use the vendored sources");
        assert_eq!(job.resume.as_deref(), Some("ses-142"));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Running,
            "the answer reopens the terminal task"
        );
        assert!(
            !rig.daemon
                .pending_chats
                .contains_key("borsuk/implement-i142"),
            "the resume delivers the queued text"
        );
        assert!(rig.decision(&row_id).is_none());
        assert_eq!(
            logged_lines(&rig.task("borsuk/implement-i142").log_path),
            vec![
                r#"{"type":"user","message":{"role":"user","content":"use the vendored sources"}}"#
            ],
            "the answer leaves one durable record in the task log"
        );
    }

    /// Without a session marker the row re-pushes and a log line names the
    /// reason, so the text is not lost.
    #[test]
    fn a_text_answer_without_a_session_marker_keeps_the_row_open() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, MAX_ATTEMPTS as usize);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(opencode_ask(
            1,
            "external_directory",
            &["/home/navaro/.cargo/registry/src/*"],
        ));
        for _ in 0..MAX_ATTEMPTS {
            rig.event(exited(
                "borsuk/implement-i142",
                false,
                "opencode exited with code 1",
            ));
        }
        let question = opencode_ask(2, "question", &[]);
        rig.event(question);
        let row_id = ask_row_id(2);
        assert!(matches!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed(_)
        ));

        rig.act(Action::Answer {
            decision_id: row_id.clone(),
            response: Response::Text {
                text: "just pick one".to_string(),
            },
        });

        assert!(
            rig.decision(&row_id).is_some(),
            "the row re-pushes when no session marker exists"
        );
        assert!(
            !rig.daemon
                .pending_chats
                .contains_key("borsuk/implement-i142"),
            "the daemon queues no chat it cannot deliver"
        );
        assert!(matches!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed(_)
        ));
    }

    /// An `Answers` payload on a one-shot question row is refused: the row
    /// carries no option list, so the inbox cannot produce picks.
    #[test]
    fn an_answers_answer_on_a_one_shot_row_is_refused() {
        let dir = temp_root();
        let mut rig = opencode_rig_failed_with_ask(&dir);
        rig.event(opencode_ask(2, "question", &[]));
        let row_id = ask_row_id(2);

        rig.act(Action::Answer {
            decision_id: row_id.clone(),
            response: Response::Answers {
                updated_input: json!({"answers": {"Database": "Postgres"}}),
            },
        });

        assert!(
            rig.decision(&row_id).is_some(),
            "the refused answer re-pushes the row"
        );
        assert!(rig.daemon.pending_chats.is_empty());
    }

    /// A queued task takes no text answer. `resume_pending_chats` uses the
    /// queued text as the whole prompt, so a queued task would start with
    /// the answer in place of its stage prompt. The chat bar refuses the
    /// same task, and the inbox must not open a door the chat bar shuts.
    #[test]
    fn a_text_answer_on_a_queued_one_shot_task_keeps_the_row_open() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, MAX_ATTEMPTS as usize);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(opencode_ask(2, "question", &[]));
        let row_id = ask_row_id(2);
        // The pause holds the requeued task, so the failure leaves it
        // queued instead of running the next attempt at once.
        rig.act(Action::Pause {
            scope: PauseScope::Task {
                task: "borsuk/implement-i142".to_string(),
            },
            paused: true,
        });
        rig.event(exited(
            "borsuk/implement-i142",
            false,
            "opencode exited with code 1",
        ));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);

        rig.act(Action::Answer {
            decision_id: row_id.clone(),
            response: Response::Text {
                text: "use the vendored sources".to_string(),
            },
        });

        assert!(
            rig.decision(&row_id).is_some(),
            "the queued task re-pushes the row"
        );
        assert!(
            rig.daemon.pending_chats.is_empty(),
            "the daemon queues no text it must not deliver"
        );
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Queued);
        assert!(
            logged_lines(&rig.task("borsuk/implement-i142").log_path).is_empty(),
            "a refused answer writes no user line"
        );
    }

    /// A retired task takes its permission rules with it. A rule that
    /// outlives its task names an unknown task in `state.json`, and
    /// `a_runtime_with_an_unknown_allowed_permission_task_discards_the_complete_state`
    /// shows that the next load then discards the complete state.
    #[test]
    fn a_retire_drops_the_permission_rules_of_the_task() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, MAX_ATTEMPTS as usize);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(opencode_ask(
            1,
            "external_directory",
            &["/home/navaro/.cargo/registry/src/*"],
        ));

        // The task still runs, so the answer arms the rule and requeues
        // nothing. Every attempt then fails and the task ends failed.
        rig.act(Action::Answer {
            decision_id: ask_row_id(1),
            response: Response::Allow,
        });
        assert!(rig
            .daemon
            .allowed_permissions
            .contains_key("borsuk/implement-i142"));
        for _ in 0..MAX_ATTEMPTS {
            rig.event(exited(
                "borsuk/implement-i142",
                false,
                "opencode exited with code 1",
            ));
        }
        assert!(matches!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed(_)
        ));

        // The issue leaves GitHub, so the poll retires the failed task.
        rig.poll(vec![], vec![]);

        assert!(!rig.daemon.table.by_id.contains_key("borsuk/implement-i142"));
        assert!(
            rig.daemon.allowed_permissions.is_empty(),
            "the retire drops the permission rules"
        );
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
        // The running process has one deadline left: the silence limit.
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(RUN_SILENCE_MS))
        );
    }

    /// A process that prints nothing for the silence limit is stalled. The
    /// daemon stops it and retries the task, so the stage slot frees up.
    #[test]
    fn a_silent_run_fails_and_retries() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let session = rig.session(0);

        rig.set_now(T0 + RUN_SILENCE_MS - 1);
        rig.drive();
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert!(!session.stopped.load(Ordering::SeqCst));

        rig.set_now(T0 + RUN_SILENCE_MS);
        rig.drive();
        assert!(
            session.stopped.load(Ordering::SeqCst),
            "the stalled process stops"
        );
        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.attempt, 2, "the silence counts as one failed attempt");
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
        second.poll_implemented();
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

    /// Pin the restart resume of one harness.
    ///
    /// The restored task dispatches with its saved session id, and the
    /// dispatch binds the configured harness. The runner unit tests pin the
    /// exact resume shape (`--resume <id>` for claude, `--session <id>` for
    /// opencode, a `thread/resume` request for codex); this rig pins the
    /// dispatch contract that feeds them.
    ///
    /// `set_role_harness` clears every field of another harness, so the role
    /// binding that the first rig persists still validates when the second
    /// rig loads the state file.
    fn restart_resumes_for_harness(harness: Harness) {
        let dir = temp_root();
        let worktree = issue_wt(&dir, 142);
        let steps = fresh_issue_steps(&rig_repo(&dir), &worktree, 142, &rig_gitdir(&dir));
        let mut first = Rig::make_in(dir.clone(), steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, harness);
        });
        first.poll(vec![issue(142, &["refined"])], vec![]);
        first.event(started("borsuk/implement-i142", "session-142"));
        drop(first);

        let steps = reuse_issue_steps(&rig_repo(&dir), &worktree, &rig_gitdir(&dir));
        let mut second = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, harness);
        });
        second.poll(vec![issue(142, &["refined"])], vec![]);

        assert_eq!(second.job_count(), 1);
        assert_eq!(second.job(0).resume.as_deref(), Some("session-142"));
        assert_eq!(
            second.roles.lock().unwrap()[0].settings.harness,
            harness,
            "the dispatch binds the configured harness"
        );
    }

    #[test]
    fn a_restart_resumes_the_opencode_session_of_the_same_task() {
        restart_resumes_for_harness(Harness::Opencode);
    }

    #[test]
    fn a_restart_resumes_the_codex_session_of_the_same_task() {
        restart_resumes_for_harness(Harness::Codex);
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
        assert_eq!(
            second.task("borsuk/release").state,
            TaskState::Running,
            "the release waits for GitHub to show the merge"
        );

        // The merge reaches the next poll. The release completes, and the
        // finished task retires with its empty batch.
        second.poll(vec![], vec![]);
        assert!(!second.daemon.table.by_id.contains_key("borsuk/release"));
        assert_eq!(second.daemon.trains["borsuk"].in_flight, None);

        second.set_now(T0 + 61 * 60_000);
        second.poll(vec![], vec![]);
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
            assert_eq!(
                logged_lines(&first.task("borsuk/implement-i142").log_path),
                vec![
                    r#"{"type":"user","message":{"role":"user","content":"add a regression test"}}"#
                ],
                "the queue path logged the user line before the restart"
            );
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

        second.poll_implemented();
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
        assert!(
            !second.daemon.interrupted.contains("borsuk/implement-i142"),
            "the retire drops the restart mark, so a later task of the same \
             id never reads the restart notice"
        );
        assert!(
            !second.daemon.restored_ids.contains("borsuk/implement-i142"),
            "the retire drops the restore mark"
        );
        assert_eq!(second.job_count(), 0);
    }

    #[test]
    fn a_restored_done_review_of_a_merged_pr_is_retired_at_the_first_poll() {
        let dir = temp_root();
        {
            let steps: Vec<Step> =
                fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                    .into_iter()
                    .chain(std::iter::once(gh_pull_ready(5)))
                    .collect();
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

    /// The two scripted calls of one answered `needs-human` row on
    /// issue 142: the comment and the label removal.
    fn answer_steps(body: &str) -> Vec<Step> {
        let field = format!("body={body}");
        vec![
            gh_step(
                &[
                    "api",
                    "-X",
                    "POST",
                    "repos/acme/borsuk/issues/142/comments",
                    "-f",
                    field.as_str(),
                ],
                CmdOut::ok(""),
            ),
            gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/142/labels/needs-human",
                ],
                gh_ok(),
            ),
        ]
    }

    /// An answered row restarts the stage of its item. The one-shot refine
    /// ended, so the next poll queues a fresh refine task.
    #[test]
    fn an_answered_row_starts_the_stage_of_a_finished_one_shot_task_again() {
        let mut rig = Rig::make_with(answer_steps("use Postgres"), |config| {
            set_role_harness(config, ExecutionRole::Refine, Harness::Opencode);
        });
        rig.poll(vec![issue(142, &["to-refine", NEEDS_HUMAN_LABEL])], vec![]);
        rig.event(exited("borsuk/refine-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert_eq!(rig.job_count(), 1);

        rig.act(Action::Answer {
            decision_id: "human:borsuk:i142".to_string(),
            response: Response::Text {
                text: "use Postgres".to_string(),
            },
        });
        assert_eq!(rig.job_count(), 1, "the answer alone starts no run");

        rig.poll(vec![issue(142, &["to-refine"])], vec![]);

        let task = rig.task("borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.attempt, 1, "the fresh task starts at attempt 1");
        assert_eq!(rig.job_count(), 2, "the answer restarts the refine");
    }

    /// A parked agent waits inside its turn, so the answer reaches it as a
    /// chat message and its session continues.
    #[test]
    fn an_answered_row_delivers_its_text_to_the_parked_session() {
        let mut rig = Rig::make(answer_steps("use Postgres"));
        rig.poll(vec![issue(142, &["to-refine", NEEDS_HUMAN_LABEL])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));
        assert_eq!(
            rig.task("borsuk/refine-i142").state,
            TaskState::AwaitingUser
        );

        rig.act(Action::Answer {
            decision_id: "human:borsuk:i142".to_string(),
            response: Response::Text {
                text: "use Postgres".to_string(),
            },
        });

        assert_eq!(
            rig.session(0).sends.lock().unwrap().as_slice(),
            &["use Postgres".to_string()],
            "the parked agent reads the answer"
        );
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Running);
        assert_eq!(rig.job_count(), 1, "the parked session takes no new run");
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

    /// A finished pipeline task closes its input, so its process must not
    /// stay alive: the daemon stops it at `Done`, and a later chat message
    /// is refused instead of being queued for a resume.
    #[test]
    fn a_completed_pipeline_task_stops_its_process_and_refuses_a_chat() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        let session = rig.session(0);
        let task = rig.task("borsuk/refine-i142");
        rig.daemon.complete_task(&task);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(
            session.stopped.load(Ordering::SeqCst),
            "the completed task stops its process"
        );

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "continue after this process exits".to_string(),
        });

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(!rig.daemon.pending_chats.contains_key("borsuk/refine-i142"));
        rig.event(exited("borsuk/refine-i142", false, "stopped"));
        assert_eq!(rig.job_count(), 1, "no resume starts");
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
        rig.poll_implemented();
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
        let (poll_tx, poll_rx) = mpsc::channel();
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
            poll_tx,
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

    /// A reload whose only topology difference is one added repository
    /// answers `Reloaded` and brings the repository live: the config gains
    /// the alias, the lane reservations follow, the file keeps its content,
    /// and the pushed state view names the new repository.
    #[test]
    fn a_topology_reload_brings_an_added_repository_live() {
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
                common_dir_step(&added, &added.join(".git")),
            ],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let topology = format!(
            "{}\n[repo.second]\npath = \"{}\"\n\n[repo.second.lanes]\nrefine = 1\n",
            settings_config_text(&rig.repo, "m"),
            added.display()
        );
        fs::write(&config_path, &topology).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        let (view_tx, view_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| view_tx.send(view).unwrap()));

        rig.act(Action::ReloadSettings {
            request: "reload-topology".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the topology reload must push a settings result");
        };
        assert_eq!(result.status, crate::sock::SettingsResultStatus::Reloaded);
        assert_eq!(result.message.as_deref(), Some("added second"));
        assert_eq!(rig.daemon.config.repos.len(), 2);
        assert_eq!(rig.daemon.config.repos["second"].path, added);
        assert!(rig.daemon.trains.contains_key("second"));
        assert!(rig.daemon.wake.contains_key("second"));
        assert_eq!(
            rig.daemon
                .limits
                .lanes
                .get(&(Stage::Refine, "second".to_string())),
            Some(&1),
            "the lane reservations of the added repository go live"
        );
        assert_eq!(fs::read_to_string(config_path).unwrap(), topology);
        let view = view_rx
            .try_recv()
            .expect("the reload must push a state view");
        assert!(view.repos.iter().any(|repo| repo.alias == "second"));
    }

    /// A save with `AddRepository` answers `Saved`, writes the file with the
    /// old tables intact, and brings the repository live: the config, the
    /// trains, and the poller wake senders gain the alias, and the pushed
    /// state view names it. The candidate of the typed edit carries only a
    /// `path`, so the seeded lane map stays empty here; the reload test
    /// above covers the lanes.
    #[test]
    fn a_save_that_adds_a_repository_answers_saved_and_starts_its_poller() {
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
                common_dir_step(&added, &added.join(".git")),
            ],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::write(&config_path, &original).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        let (view_tx, view_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| view_tx.send(view).unwrap()));

        rig.act(Action::SaveSettings {
            request: "add-second".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: SettingsEdit::AddRepository {
                alias: "second".to_string(),
                path: added.display().to_string(),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the add must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(result.message.as_deref(), Some("added second"));
        let written = fs::read_to_string(config_path).unwrap();
        assert_eq!(result.revision, crate::config::file_revision(&written));
        assert!(written.starts_with(&original), "the old tables survive");
        assert!(written.contains(&format!("[repo.second]\npath = \"{}\"", added.display())));
        assert_eq!(rig.daemon.config.repos.len(), 2);
        assert_eq!(rig.daemon.config.repos["second"].path, added);
        assert_eq!(rig.daemon.trains["second"].repo, "second");
        assert!(rig.daemon.wake.contains_key("second"));
        assert!(
            rig.daemon.limits.lanes.is_empty(),
            "the candidate carries no lanes, so the map gains nothing"
        );
        let mut view = None;
        while let Ok(next) = view_rx.try_recv() {
            view = Some(next);
        }
        let view = view.expect("the add must push a state view");
        assert!(view.repos.iter().any(|repo| repo.alias == "second"));
        assert!(view
            .settings
            .repositories
            .iter()
            .any(|row| row.repository == "second"));
    }

    /// A save that removes a repository answers `Saved` with the stopped
    /// active-task count, and every live record of the alias disappears:
    /// the tasks retire with their sessions stopped, the runtime maps and
    /// the open decisions drop the alias, `state.json` names it no more,
    /// and the pushed state view shows the reduced factory. The checkout
    /// and its worktrees stay on disk.
    #[test]
    fn a_save_that_removes_a_repository_answers_saved_and_stops_its_tasks() {
        let dir = temp_root();
        fs::create_dir_all(rig_repo(&dir).join(".git")).unwrap();
        let worktree = dir
            .join("state")
            .join("worktrees")
            .join("borsuk")
            .join("issue-142");
        fs::create_dir_all(&worktree).unwrap();
        let mut rig = Rig::make_in(dir, vec![], |config| {
            config
                .repos
                .get_mut("borsuk")
                .unwrap()
                .lanes
                .insert(Stage::Refine, 1);
        });
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::write(&config_path, &original).unwrap();

        // Seed every alias-keyed record: a running refine task, a running
        // ticket conversation, the release policy, a paused lane, a
        // needs-human decision, and the confirmed-mutation cache.
        rig.poll(
            vec![
                issue(142, &["to-refine"]),
                issue(143, &["needs-human"]),
                issue(7, &[]),
            ],
            vec![],
        );
        rig.act(Action::Ticket(TicketAction::Chat {
            request: "chat-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
        }));
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
        rig.daemon
            .ticket_controller
            .record_confirmed_mutation("borsuk", 42);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        let (view_tx, view_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| view_tx.send(view).unwrap()));

        rig.act(Action::SaveSettings {
            request: "remove-borsuk".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: SettingsEdit::RemoveRepository {
                alias: "borsuk".to_string(),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the removal must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(
            result.message.as_deref(),
            Some("removed borsuk: stopped 2 active task(s)")
        );
        assert!(rig.daemon.config.repos.is_empty());
        assert!(!fs::read_to_string(config_path)
            .unwrap()
            .contains("repo.borsuk"));
        let refine = rig.session(0);
        let chat = rig.session(1);
        assert!(
            refine.stopped.load(Ordering::SeqCst),
            "the refine session receives the stop"
        );
        assert!(
            chat.stopped.load(Ordering::SeqCst),
            "the ticket session receives the stop"
        );
        assert!(rig
            .daemon
            .table
            .by_id
            .keys()
            .all(|id| !id.starts_with("borsuk/")));
        assert!(!rig.daemon.snapshot.repos.contains_key("borsuk"));
        assert!(!rig.daemon.links.contains_key("borsuk"));
        assert!(!rig.daemon.trains.contains_key("borsuk"));
        assert!(!rig.daemon.wake.contains_key("borsuk"));
        assert!(!rig.daemon.policies.contains_key("borsuk"));
        assert!(!rig.daemon.pending_stacked.contains("borsuk"));
        assert!(!rig
            .daemon
            .limits
            .lanes
            .contains_key(&(Stage::Refine, "borsuk".to_string())));
        assert!(!rig
            .daemon
            .paused
            .lanes
            .contains_key(&(Stage::Release, "borsuk".to_string())));
        assert!(!rig
            .daemon
            .ticket_conversations
            .contains_key(&("borsuk".to_string(), 7)));
        assert_eq!(
            rig.daemon.ticket_controller.last_mutation_ms("borsuk"),
            None
        );
        assert!(
            rig.daemon
                .decisions
                .open()
                .iter()
                .all(|row| row.repo != "borsuk"),
            "the needs-human row of the removed repository goes"
        );
        assert!(worktree.exists(), "a removal never deletes a worktree");
        assert!(rig.repo.exists(), "a removal never deletes the checkout");

        rig.daemon.force_save_state();
        let state = fs::read_to_string(&rig.daemon.state_path).unwrap();
        assert!(!state.contains("borsuk"), "state.json: {state}");

        rig.drive();
        let mut view = None;
        while let Ok(next) = view_rx.try_recv() {
            view = Some(next);
        }
        let view = view.expect("the removal must push a state view");
        assert!(view.repos.is_empty());
        assert!(view.trains.is_empty());
        assert!(view.lanes.is_empty());
        assert!(view.links.is_empty());
        assert!(view.settings.repositories.is_empty());
    }

    /// A removal stops an in-flight release: the release task retires, its
    /// live session receives the stop, and the train of the repository goes.
    #[test]
    fn a_removal_stops_an_in_flight_release_task() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir))
                .into_iter()
                .chain(reuse_train_steps(
                    &rig_repo(&dir),
                    &train_wt(&dir),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::write(&config_path, &original).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.poll(vec![], vec![pr(2, false, &[]), pr(3, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2, 3],
        });
        rig.event(started("borsuk/release", "session-release"));
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release")
        );
        let session = rig.session(0);

        rig.act(Action::SaveSettings {
            request: "remove-release".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: SettingsEdit::RemoveRepository {
                alias: "borsuk".to_string(),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the removal must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(
            result.message.as_deref(),
            Some("removed borsuk: stopped 1 active task(s)")
        );
        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/release"),
            "the release task retires with its repository"
        );
        assert!(session.stopped.load(Ordering::SeqCst));
        assert!(!rig.daemon.trains.contains_key("borsuk"));
        assert!(rig.daemon.config.repos.is_empty());
    }

    /// A save that changes the `path` of a staying repository answers
    /// `RestartRequired`, and the live configuration keeps the old path.
    #[test]
    fn a_path_change_of_a_staying_repository_answers_restart_required() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let moved = dir.join("moved-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(moved.join(".git")).unwrap();
        let mut rig = Rig::make_in(
            dir,
            vec![git_step(
                &moved,
                &["remote", "get-url", "origin"],
                CmdOut::ok("git@github.com:acme/borsuk.git\n"),
            )],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let changed = settings_config_text(&moved, "m");
        fs::write(&config_path, &changed).unwrap();
        let mut settings = Config::parse(&changed).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "after-save".to_string();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "move-path".to_string(),
            base_revision: crate::config::file_revision(&changed),
            edit: SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the path change must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::RestartRequired);
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("borsuk")),
            "the message names the changed alias"
        );
        assert_eq!(rig.daemon.config.repos["borsuk"].path, rig.repo);
        assert_eq!(fs::read_to_string(config_path).unwrap(), changed);
    }

    /// A save that resolves a new git remote for a staying repository
    /// answers `RestartRequired`: the poller of the live alias keeps the
    /// remote it started with, so the daemon cannot switch the remote in
    /// place. The live configuration and the file keep the old state.
    #[test]
    fn a_remote_change_of_a_staying_repository_answers_restart_required() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut rig = Rig::make_in(
            dir,
            vec![git_step(
                &repo,
                &["remote", "get-url", "origin"],
                CmdOut::ok("git@github.com:acme/other.git\n"),
            )],
            |_| {},
        );
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::write(&config_path, &original).unwrap();
        let mut settings = Config::parse(&original).unwrap().roles[&ExecutionRole::Refine].clone();
        settings.model = "after-save".to_string();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "move-remote".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: SettingsEdit::Global {
                role: ExecutionRole::Refine,
                settings,
                limit: Some(2),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the remote change must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::RestartRequired);
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("borsuk")),
            "the message names the changed alias"
        );
        assert_eq!(rig.daemon.config.repos["borsuk"].owner_repo, "acme/borsuk");
        assert_eq!(fs::read_to_string(config_path).unwrap(), original);
    }

    /// A save that adds a repository whose path holds no `.git` answers
    /// `Invalid` with the git error, and neither the file nor the live
    /// configuration changes.
    #[test]
    fn a_save_adding_a_repository_without_git_answers_invalid() {
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
        let config_path = rig.repo.parent().unwrap().join("factory.toml");
        let original = settings_config_text(&rig.repo, "m");
        fs::write(&config_path, &original).unwrap();
        let plain = rig.repo.parent().unwrap().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SaveSettings {
            request: "add-plain".to_string(),
            base_revision: crate::config::file_revision(&original),
            edit: SettingsEdit::AddRepository {
                alias: "second".to_string(),
                path: plain.display().to_string(),
            },
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the invalid add must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Invalid);
        let message = result.message.expect("the result must name the git error");
        assert!(message.contains("second"), "message: {message}");
        assert!(
            message.contains("holds no .git entry"),
            "message: {message}"
        );
        assert_eq!(rig.daemon.config.repos.len(), 1);
        assert_eq!(fs::read_to_string(config_path).unwrap(), original);
    }

    /// A poll message whose alias the config does not hold changes nothing:
    /// the snapshot, the links, and the task table stay empty.
    #[test]
    fn a_poll_for_a_repository_the_config_dropped_changes_nothing() {
        let mut rig = Rig::make(vec![]);
        rig.daemon.handle(Inbound::Poll(DaemonMsg::Polled {
            started_ms: T0,
            repo: "gone".to_string(),
            snapshot: RepoSnapshot::default(),
        }));
        rig.daemon.handle(Inbound::Poll(DaemonMsg::PollFailed {
            repo: "gone".to_string(),
            error: "the fetch failed".to_string(),
        }));

        assert!(rig.daemon.snapshot.repos.is_empty());
        assert!(rig.daemon.links.is_empty());
        assert!(rig.daemon.table.by_id.is_empty());
        assert!(rig.daemon.decisions.open().is_empty());
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
        drop(sends);
        assert!(!rig.daemon.table.by_id.contains_key("borsuk/refine-i7"));
        // The agent received each handoff as user text, so each handoff
        // gets the same durable line as a typed message. The transcript
        // stays a true record of the conversation.
        let logged = logged_lines(&rig.task("borsuk/ticket-i7").log_path);
        assert_eq!(logged.len(), 2, "each handoff wrote one user line");
        assert!(
            logged.iter().all(|line| line.contains("to-refine label")),
            "the handoff text reaches the transcript: {logged:?}"
        );
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
        rig.poll_implemented();
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
        // The merge of pull request 2 confirms the release, so the train
        // takes a second batch.
        rig.poll(vec![], vec![pr(5, false, &["release-stacked"])]);
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

    /// A finished claude release keeps one task id across batches. Its
    /// process must not outlive the task, or the live slot of the release
    /// stage stays taken and the next batch never starts.
    #[test]
    fn a_finished_release_frees_its_session_for_the_next_batch() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir))
                .into_iter()
                .chain(reuse_train_steps(
                    &rig_repo(&dir),
                    &train_wt(&dir),
                    &rig_gitdir(&dir),
                ))
                .collect();
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &[]), pr(5, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2],
        });
        assert_eq!(rig.job_count(), 1);

        rig.event(turn_finished("borsuk/release", true, "released"));
        rig.poll(vec![], vec![pr(5, false, &[])]);
        let first = rig.session(0);
        assert!(
            first.stopped.load(Ordering::SeqCst),
            "the completed release stops its process"
        );

        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![5],
        });
        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Queued,
            "the next batch waits for the exit of the stopped process"
        );
        rig.event(exited("borsuk/release", true, "stopped"));
        assert_eq!(rig.job_count(), 2, "the next batch starts");
        assert_eq!(rig.task("borsuk/release").state, TaskState::Running);
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
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(std::iter::once(gh_pull_ready(5)))
                .collect();
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

    /// A release merges the pull requests of its batch, so the item that
    /// names the release task leaves GitHub in the middle of the run. The
    /// poll must not cancel the task: the train, not one pull request, is
    /// its unit. The same poll also shows the batch through, so the turn
    /// end confirms the stage at once and the train closes.
    #[test]
    fn a_dropped_pr_keeps_the_in_flight_release_and_closes_the_train() {
        let dir = temp_root();
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(std::iter::once(gh_pull_ready(5)))
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

        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Running,
            "the merge of its own pull request never cancels the release"
        );
        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/review-p5"),
            "the dropped review retires"
        );

        rig.event(turn_finished("borsuk/release", true, "released"));

        assert_eq!(rig.task("borsuk/release").state, TaskState::Done);
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight, None,
            "the finished release closes the train"
        );
    }

    #[test]
    fn a_merged_first_pull_request_keeps_the_running_release() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &[]), pr(3, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2, 3],
        });
        rig.event(started("borsuk/release", "session-release"));

        // The agent merges the batch in ascending order, so the lowest
        // pull request leaves GitHub first. It names the release task.
        rig.poll(vec![], vec![pr(3, false, &[])]);

        assert_eq!(rig.task("borsuk/release").state, TaskState::Running);
        assert_eq!(
            rig.daemon.trains["borsuk"].in_flight.as_deref(),
            Some("borsuk/release"),
            "the train stays in flight for the rest of the batch"
        );
        assert_eq!(rig.daemon.trains["borsuk"].batch(), &[3]);

        rig.event(turn_finished("borsuk/release", true, "released"));
        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Running,
            "pull request 3 is still open"
        );

        rig.poll(vec![], vec![]);

        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/release"),
            "the confirmed release completes and retires with its batch"
        );
        assert_eq!(rig.daemon.trains["borsuk"].in_flight, None);
        assert!(rig.daemon.trains["borsuk"].batch().is_empty());
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
        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Running,
            "the release waits for the merge of its batch"
        );
        assert_eq!(rig.daemon.trains["borsuk"].batch(), &[2]);

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
        let steps: Vec<Step> =
            fresh_issue_steps(&rig_repo(&dir), &issue_wt(&dir, 5), 5, &rig_gitdir(&dir))
                .into_iter()
                .chain(std::iter::once(gh_pull_ready(5)))
                .collect();
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
                .chain(std::iter::once(gh_pull_ready(5)))
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

        rig.poll(
            vec![issue(142, &[]), issue(143, &["refined"])],
            vec![linked_pr(5, 142)],
        );
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

        rig.poll_implemented();
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
    }

    // ------------------------------------------------------------------
    // The GitHub confirmation of a finished run
    // ------------------------------------------------------------------

    /// A finished implementation waits for GitHub. The poll that shows the
    /// pull request completes it, and the same poll still carries the
    /// `refined` label, so the daemon removes the forgotten label.
    #[test]
    fn a_polled_pull_request_completes_a_finished_implementation() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = issue_wt(&dir, 142);
        let gitdir = rig_gitdir(&dir);
        let steps: Vec<Step> = fresh_issue_steps(&repo, &worktree, 142, &gitdir)
            .into_iter()
            .chain(vec![gh_step(
                &[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/142/labels/refined",
                ],
                gh_ok(),
            )])
            .chain(reuse_issue_steps(&repo, &worktree, &gitdir))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Implement, Harness::Opencode);
        });
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Running,
            "the exit alone never completes the task"
        );
        let mut pull = pr(5, true, &[]);
        pull.head_ref = "aif/borsuk/issue-142".to_string();

        rig.poll(vec![issue(142, &["refined"])], vec![pull]);

        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Done);
        assert!(
            rig.exec.calls().iter().any(|call| {
                call.program == "gh"
                    && call.argv()
                        == [
                            "api",
                            "-i",
                            "-X",
                            "DELETE",
                            "repos/acme/borsuk/issues/142/labels/refined",
                        ]
            }),
            "the forgotten label leaves GitHub: {:?}",
            rig.exec.calls()
        );
    }

    /// A review that ends without its transition fails after the grace and
    /// retries. The last attempt names the pull request in its stuck row.
    #[test]
    fn an_unconfirmed_review_fails_after_the_grace_and_retries() {
        let dir = temp_root();
        let repo = rig_repo(&dir);
        let worktree = issue_wt(&dir, 5);
        let gitdir = rig_gitdir(&dir);
        let steps: Vec<Step> = fresh_issue_steps(&repo, &worktree, 5, &gitdir)
            .into_iter()
            .chain(reuse_issue_steps(&repo, &worktree, &gitdir))
            .chain(reuse_issue_steps(&repo, &worktree, &gitdir))
            .collect();
        let mut rig = Rig::make_in(dir, steps, |config| {
            set_role_harness(config, ExecutionRole::Review, Harness::Opencode);
        });
        rig.poll(vec![], vec![pr(5, true, &[])]);

        for attempt in 1..=MAX_ATTEMPTS {
            rig.event(exited("borsuk/review-p5", true, "code 0"));
            assert_eq!(
                rig.task("borsuk/review-p5").state,
                TaskState::Running,
                "the finished run waits for the poll"
            );
            rig.set_now(T0 + u64::from(attempt) * CONFIRM_GRACE_MS);
            rig.drive();
        }

        assert_eq!(rig.task("borsuk/review-p5").attempt, MAX_ATTEMPTS);
        assert_eq!(rig.job_count() as u32, MAX_ATTEMPTS, "each failure retries");
        let row = rig
            .decision(&format!("stuck:borsuk/review-p5:{MAX_ATTEMPTS}"))
            .expect("the last attempt opens a stuck row");
        assert!(
            matches!(row.kind, DecisionKind::Stuck { ref reason, .. }
                if reason == "the review run ended, but PR #5 is still a draft"),
            "the row names the missing transition: {:?}",
            row.kind
        );
    }

    /// A run that hands its item to a human is finished. The label is the
    /// documented human path, so the task completes without its gate
    /// transition, and the inbox row carries the work from there.
    #[test]
    fn a_needs_human_ticket_completes_its_refine_at_once() {
        let mut rig = Rig::make_with(vec![], |config| {
            set_role_harness(config, ExecutionRole::Refine, Harness::Opencode);
        });
        rig.poll(vec![issue(142, &["to-refine", NEEDS_HUMAN_LABEL])], vec![]);

        rig.event(exited("borsuk/refine-i142", true, "code 0"));

        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
        assert!(rig.daemon.confirming.is_empty());
        assert!(
            rig.decision("human:borsuk:i142").is_some(),
            "the inbox row carries the ticket from here"
        );
    }

    /// A release completes only after every pull request of its batch left
    /// GitHub. The completion drains the train, and the finished task
    /// retires with its merged batch.
    #[test]
    fn a_release_waits_for_the_whole_batch_to_merge() {
        let dir = temp_root();
        let steps = fresh_train_steps(&rig_repo(&dir), &train_wt(&dir), &rig_gitdir(&dir));
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![], vec![pr(2, false, &[]), pr(3, false, &[])]);
        rig.act(Action::Go {
            repo: "borsuk".to_string(),
            prs: vec![2, 3],
        });

        rig.event(turn_finished("borsuk/release", true, "released"));

        assert_eq!(
            rig.task("borsuk/release").state,
            TaskState::Running,
            "one pull request of the batch is still open"
        );
        rig.poll(vec![], vec![pr(3, false, &[])]);
        assert_eq!(rig.task("borsuk/release").state, TaskState::Running);

        rig.poll(vec![], vec![]);

        assert!(
            !rig.daemon.table.by_id.contains_key("borsuk/release"),
            "the completed release retires with its merged batch"
        );
        assert_eq!(rig.daemon.trains["borsuk"].in_flight, None);
        assert!(rig.daemon.trains["borsuk"].batch().is_empty());
    }

    /// The process of a task that waits for its transition may exit. The
    /// exit drops the session and nothing else: the poll decides.
    #[test]
    fn an_exit_never_fails_a_run_that_waits_for_its_transition() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(turn_finished("borsuk/implement-i142", true, "done"));
        assert_eq!(rig.task("borsuk/implement-i142").state, TaskState::Running);

        rig.event(exited("borsuk/implement-i142", true, "code 0"));

        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.state, TaskState::Running, "the exit fails nothing");
        assert_eq!(task.attempt, 1);

        rig.poll_implemented();

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
        rig.poll_implemented();
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
        rig.poll_implemented();
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
        rig.poll_implemented();

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
        rig.poll_implemented();

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
        assert_eq!(
            rig.job(1).resume.as_deref(),
            Some("ses-142"),
            "the first retry keeps the session"
        );
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(
            rig.task("borsuk/implement-i142").session_id,
            None,
            "the requeue into the last attempt drops the dead session"
        );
        // The fresh last attempt mints a new session, and its failure opens
        // the stuck row while that new session stays saved.
        rig.event(started("borsuk/implement-i142", "ses-3"));
        rig.event(exited("borsuk/implement-i142", false, "boom"));
        assert_eq!(
            rig.task("borsuk/implement-i142").state,
            TaskState::Failed("boom".to_string())
        );
        assert_eq!(rig.task("borsuk/implement-i142").attempt, 3);
        assert_eq!(
            rig.task("borsuk/implement-i142").session_id.as_deref(),
            Some("ses-3")
        );
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
        assert_eq!(job.resume.as_deref(), Some("ses-3"));
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
        rig.poll_implemented();
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
        rig.poll_implemented();
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
        rig.poll_implemented();
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
    // The chat user line in the task log
    // ------------------------------------------------------------------

    /// The lines of one task log. A log that no writer created yet holds
    /// no lines, so a refusal and an absent file read the same.
    fn logged_lines(log: &Path) -> Vec<String> {
        match fs::read_to_string(log) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("the task log {log:?} must be readable: {error}"),
        }
    }

    #[test]
    fn a_live_chat_writes_one_user_line_into_the_task_log() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "continue with Postgres".to_string(),
        });

        assert_eq!(
            rig.session(0).sends.lock().unwrap().as_slice(),
            &["continue with Postgres".to_string()]
        );
        let log = rig.task("borsuk/refine-i142").log_path;
        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.ends_with('\n'), "the line ends with a newline");
        assert_eq!(
            logged_lines(&log),
            vec![r#"{"type":"user","message":{"role":"user","content":"continue with Postgres"}}"#],
            "the live success appends exactly one user line"
        );
    }

    #[test]
    fn a_failed_live_send_still_writes_one_user_line() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        rig.session(0).fail_send.store(true, Ordering::SeqCst);

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: "queued for the resumed turn".to_string(),
        });

        assert!(rig.session(0).sends.lock().unwrap().is_empty());
        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/refine-i142")
                .map(Vec::as_slice),
            Some(&["queued for the resumed turn".to_string()][..])
        );
        let log = rig.task("borsuk/refine-i142").log_path;
        assert_eq!(
            logged_lines(&log),
            vec![
                r#"{"type":"user","message":{"role":"user","content":"queued for the resumed turn"}}"#
            ],
            "the live failure appends exactly one user line"
        );
    }

    #[test]
    fn a_queued_chat_writes_one_user_line_into_the_task_log() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));

        rig.daemon
            .chat("borsuk/implement-i142", "add a regression test");

        assert_eq!(
            rig.daemon
                .pending_chats
                .get("borsuk/implement-i142")
                .map(Vec::as_slice),
            Some(&["add a regression test".to_string()][..])
        );
        let log = rig.task("borsuk/implement-i142").log_path;
        assert_eq!(
            logged_lines(&log),
            vec![r#"{"type":"user","message":{"role":"user","content":"add a regression test"}}"#],
            "the queue path appends exactly one user line"
        );
    }

    #[test]
    fn the_logged_chat_line_reads_back_as_the_typed_text() {
        let mut rig = Rig::make(vec![]);
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        rig.event(started("borsuk/refine-i142", "sid-142"));
        // A quotation mark, a backslash, a newline, and a wide character
        // must all stay inside one JSON line.
        let typed = "use \"psql\"\nnot C:\\tools\\psql — ok?";

        rig.act(Action::Chat {
            task: "borsuk/refine-i142".to_string(),
            text: typed.to_string(),
        });

        let lines = logged_lines(&rig.task("borsuk/refine-i142").log_path);
        assert_eq!(lines.len(), 1, "the typed newline never splits the record");
        assert_eq!(
            crate::tui::transcript::parse(&lines[0]),
            vec![crate::tui::transcript::Entry::User {
                text: typed.to_string()
            }],
            "the session view reads back the exact typed text"
        );
    }

    #[test]
    fn a_chat_without_a_task_or_a_session_writes_no_user_line() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);

        assert!(
            !rig.daemon.chat("borsuk/implement-i999", "no such task"),
            "an unknown task takes no message"
        );

        // The run started, but no event reported a session id yet, and the
        // worktree holds no session marker.
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.session_id, None);
        assert!(
            !rig.daemon
                .chat("borsuk/implement-i142", "there is no session"),
            "a task without a session takes no message"
        );
        assert!(
            logged_lines(&task.log_path).is_empty(),
            "a refused message leaves the log empty"
        );
    }

    #[test]
    fn a_worktree_hold_refuses_the_chat_and_writes_no_user_line() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 1);
        let mut pull = pr(7, true, &[]);
        pull.head_ref = "feature/landing".to_string();
        pull.body = "Closes #142".to_string();
        rig.poll(vec![issue(142, &["refined"])], vec![pull]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        assert_eq!(rig.job_count(), 2, "the review gate opened");
        // The review of the linked pull request owns the issue worktree.
        rig.event(started("borsuk/review-p7", "sid-7"));

        let task = rig.task("borsuk/implement-i142");
        assert!(rig.daemon.sibling_refusal(&task).is_some());
        assert!(
            !rig.daemon
                .chat("borsuk/implement-i142", "adjust the implementation"),
            "the worktree hold takes no message"
        );

        assert!(
            logged_lines(&task.log_path).is_empty(),
            "the worktree hold leaves the log empty"
        );
    }

    #[test]
    fn a_closed_input_appends_no_second_user_line() {
        let dir = temp_root();
        let mut rig = opencode_rig(&dir, 0);
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(started("borsuk/implement-i142", "ses-142"));
        rig.poll_implemented();
        rig.event(exited("borsuk/implement-i142", true, "code 0"));
        // A ticket chat of the same issue works in the shared checkout, so
        // it holds no worktree. It is a prior stage, so it holds the turn.
        let chat_log = rig
            .daemon
            .log_path("borsuk", Stage::Refine, ItemKind::Issue, 142);
        rig.daemon
            .table
            .upsert_ticket_chat("borsuk", 142, chat_log, rig.daemon.now_ms)
            .unwrap();
        rig.act(Action::Chat {
            task: "borsuk/implement-i142".to_string(),
            text: "extend the change".to_string(),
        });
        let log = rig.task("borsuk/implement-i142").log_path;
        assert_eq!(logged_lines(&log).len(), 1, "the first chat wrote its line");

        // The reopen queued the task. A queued task takes no message.
        let queued = rig.task("borsuk/implement-i142");
        assert_eq!(queued.state, TaskState::Queued);
        assert!(matches!(
            rig.daemon.input_mode(&queued),
            InputMode::Closed { .. }
        ));
        assert!(
            !rig.daemon
                .chat("borsuk/implement-i142", "and one more thing"),
            "the closed input takes no message"
        );

        assert_eq!(
            logged_lines(&log),
            vec![r#"{"type":"user","message":{"role":"user","content":"extend the change"}}"#],
            "the closed input appends nothing to the earlier line"
        );
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
            rig.poll_implemented();
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
        // The one-shot refine reports its result, and the poll that shows
        // the `refined` label completes it.
        rig.event(exited("borsuk/refine-i142", true, "code 0"));
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.task("borsuk/refine-i142").state, TaskState::Done);
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
        rig.poll_implemented();
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
    fn the_pushed_view_carries_the_live_role_binding_of_each_task() {
        let mut rig = Rig::make(vec![]);
        let (push_tx, push_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| push_tx.send(view).unwrap()));

        // The poll queues the refine task, and the drive starts it. The
        // start binds the role, and the view carries that binding.
        rig.poll(vec![issue(142, &["to-refine"])], vec![]);
        let view = last_view(&push_rx);
        let task = pushed_task(&view, "borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Running);
        let binding = task
            .binding
            .as_ref()
            .expect("a running task ships its binding");
        assert_eq!(binding.harness, Harness::Claude);
        assert_eq!(binding.model, "m");
        assert_eq!(binding.effort, None);

        // The issue flips to refined, and the turn ends in success. The
        // task completes. The binding survives the end of the task, so a
        // finished task still shows what it ran with.
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        rig.event(turn_ended("borsuk/refine-i142"));
        let view = last_view(&push_rx);
        let task = pushed_task(&view, "borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Done);
        assert!(task.binding.is_some());

        // A refine request replaces the terminal task. The replacement
        // holds no binding until its own first run.
        rig.act(Action::Refine {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 142,
        });
        let view = last_view(&push_rx);
        let task = pushed_task(&view, "borsuk/refine-i142");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.binding, None);
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
        rig.poll_implemented();
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
        rig.poll_implemented();

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
    // Prompt templates
    // ------------------------------------------------------------------

    /// The placeholder values of every role match the placeholder set the
    /// save-time check accepts, name for name and in order, so a prompt
    /// that passes the check never fails at dispatch.
    #[test]
    fn the_placeholder_values_of_every_role_match_the_checked_set() {
        let dir = temp_root();
        let mut rig = Rig::make_in(dir.clone(), vec![], |_| {});
        let issue = issue(142, &["refined"]);
        let mut repo_snapshot = RepoSnapshot::default();
        repo_snapshot.issues.insert(142, issue);
        rig.daemon
            .snapshot
            .repos
            .insert("borsuk".to_string(), repo_snapshot);
        let repo_cfg = rig.daemon.config.repos["borsuk"].clone();
        for role in prompts::ROLES {
            let (stage, kind, number) = match role {
                ExecutionRole::Refine => (Stage::Refine, ItemKind::Issue, 142),
                ExecutionRole::Implement => (Stage::Implement, ItemKind::Issue, 142),
                ExecutionRole::Review => (Stage::Review, ItemKind::Pr, 7),
                ExecutionRole::Release => (Stage::Release, ItemKind::Pr, 0),
                ExecutionRole::TicketCreate => (Stage::Refine, ItemKind::Issue, TICKET_NUMBER),
                ExecutionRole::TicketChat => (Stage::Refine, ItemKind::Issue, 142),
                ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => {
                    panic!("a theory role has no prompt template")
                }
            };
            let mut task = Task::new("borsuk", stage, kind, number, PathBuf::new(), T0);
            task.purpose = match role {
                ExecutionRole::TicketCreate => TaskPurpose::TicketCreate,
                ExecutionRole::TicketChat => TaskPurpose::TicketChat,
                _ => TaskPurpose::Pipeline,
            };
            assert_eq!(Daemon::execution_role(&task), role);
            let values = rig
                .daemon
                .placeholder_values(&task, &repo_cfg, &dir)
                .unwrap_or_else(|error| panic!("{role}: {error:#}"));
            let names = values.iter().map(|(name, _)| *name).collect::<Vec<_>>();
            assert_eq!(
                Some(names.as_slice()),
                prompts::placeholders(role),
                "{role}"
            );
        }
    }

    /// `execution_role` never yields a theory role today, so no dispatch
    /// asks for a prompt that does not exist.
    #[test]
    fn no_dispatched_task_takes_a_theory_role() {
        for stage in [
            Stage::Refine,
            Stage::Implement,
            Stage::Review,
            Stage::Release,
        ] {
            for purpose in [
                TaskPurpose::Pipeline,
                TaskPurpose::TicketCreate,
                TaskPurpose::TicketChat,
            ] {
                let mut task = Task::new("borsuk", stage, ItemKind::Issue, 1, PathBuf::new(), T0);
                task.purpose = purpose;
                let role = Daemon::execution_role(&task);
                assert!(
                    prompts::file_name(role).is_some(),
                    "{stage:?}/{purpose:?} dispatches {role}, which has no prompt"
                );
            }
        }
    }

    #[test]
    fn a_prompt_save_writes_the_file_pushes_the_result_and_reaches_the_next_task() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir.clone(), steps, |_| {});
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        let (state_tx, state_rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| state_tx.send(view).unwrap()));
        let builtin_revision = config::file_revision(IMPLEMENT_PROMPT);
        let view = rig
            .daemon
            .prompts
            .iter()
            .find(|view| view.role == ExecutionRole::Implement)
            .unwrap()
            .clone();
        assert_eq!(view.source, PromptSource::Builtin);
        assert_eq!(view.revision, builtin_revision);

        rig.act(Action::SavePrompt {
            request: "prompt-1".to_string(),
            role: ExecutionRole::Implement,
            base_revision: builtin_revision,
            text: "implement #{number} of {repo}\n".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the prompt save must push a settings result");
        };
        assert_eq!(result.request, "prompt-1");
        assert_eq!(result.operation, SettingsOperation::SavePrompt);
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(
            result.revision,
            config::file_revision("implement #{number} of {repo}\n")
        );
        assert!(result.message.unwrap().contains("implement.md"));
        assert_eq!(
            fs::read_to_string(rig.prompts.join("implement.md")).unwrap(),
            "implement #{number} of {repo}\n"
        );
        let view = rig
            .daemon
            .prompts
            .iter()
            .find(|view| view.role == ExecutionRole::Implement)
            .unwrap();
        assert_eq!(view.source, PromptSource::File);
        assert_eq!(view.text, "implement #{number} of {repo}\n");
        let pushed = std::iter::from_fn(|| state_rx.try_recv().ok())
            .last()
            .expect("a new prompt publishes the state");
        assert_eq!(
            pushed.settings.prompts[1].text,
            "implement #{number} of {repo}\n"
        );

        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job(0).prompt, "implement #142 of borsuk\n");
    }

    #[test]
    fn a_stale_prompt_save_is_refused_and_names_the_current_revision() {
        let mut rig = Rig::make(vec![]);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("refine.md"), "edited outside {number}\n").unwrap();

        rig.act(Action::SavePrompt {
            request: "prompt-stale".to_string(),
            role: ExecutionRole::Refine,
            base_revision: config::file_revision(REFINE_PROMPT),
            text: "operator text {number}\n".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the stale save must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Stale);
        assert_eq!(
            result.revision,
            config::file_revision("edited outside {number}\n")
        );
        assert!(result.message.unwrap().contains("refine.md"));
        assert_eq!(
            fs::read_to_string(rig.prompts.join("refine.md")).unwrap(),
            "edited outside {number}\n",
            "a stale save leaves the file alone"
        );
        let view = rig
            .daemon
            .prompts
            .iter()
            .find(|view| view.role == ExecutionRole::Refine)
            .unwrap();
        assert_eq!(view.source, PromptSource::File);
        assert_eq!(
            view.text, "edited outside {number}\n",
            "a stale save refreshes the view with the text that won"
        );

        rig.act(Action::SavePrompt {
            request: "prompt-retry".to_string(),
            role: ExecutionRole::Refine,
            base_revision: result.revision,
            text: "operator text {number}\n".to_string(),
        });
        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the retry must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(
            fs::read_to_string(rig.prompts.join("refine.md")).unwrap(),
            "operator text {number}\n"
        );
    }

    #[test]
    fn an_unknown_placeholder_blocks_the_prompt_save_before_the_disk() {
        let mut rig = Rig::make(vec![]);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SavePrompt {
            request: "prompt-bad".to_string(),
            role: ExecutionRole::Release,
            base_revision: config::file_revision(RELEASE_PROMPT),
            text: "release {frobnicate}\n".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the invalid save must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Invalid);
        assert_eq!(result.revision, config::file_revision(RELEASE_PROMPT));
        let message = result.message.unwrap();
        assert!(message.contains("{frobnicate}"), "{message}");
        assert!(message.contains("{pr_list}"), "{message}");
        assert!(!rig.prompts.join("release.md").exists());
    }

    /// A theory role has no prompt template. A client that outran the
    /// daemon and sent a prompt action against one gets a named refusal,
    /// and no file appears.
    #[test]
    fn a_prompt_action_against_a_theory_role_is_refused_by_name() {
        let mut rig = Rig::make(vec![]);
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        for (index, action) in [
            Action::SavePrompt {
                request: "theory-save".to_string(),
                role: ExecutionRole::TheoryAudit,
                base_revision: String::new(),
                text: "audit the model\n".to_string(),
            },
            Action::ResetPrompt {
                request: "theory-reset".to_string(),
                role: ExecutionRole::TheoryChat,
                base_revision: String::new(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            rig.act(action);
            let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
                panic!("the refusal must push a settings result");
            };
            assert_eq!(result.status, SettingsResultStatus::Failed);
            let role = if index == 0 {
                "theory.audit"
            } else {
                "theory.chat"
            };
            assert_eq!(
                result.message.unwrap(),
                format!("the {role} role has no prompt template")
            );
        }
        assert!(!rig.prompts.exists(), "no prompt file was written");
        assert!(rig
            .daemon
            .prompts
            .iter()
            .all(|view| prompts::file_name(view.role).is_some()));
    }

    /// A prompt file the daemon cannot read blocks a save, because nothing
    /// may overwrite a text the daemon never saw. A reset still works: it
    /// is the only way out of that state.
    #[test]
    fn a_reset_recovers_an_unreadable_prompt_file() {
        use std::os::unix::fs::PermissionsExt;
        let mut rig = Rig::make(vec![]);
        fs::create_dir_all(&rig.prompts).unwrap();
        let path = rig.prompts.join("release.md");
        fs::write(&path, "release {pr_list}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_to_string(&path).is_ok() {
            // The root user reads every file, so the state under test
            // cannot exist in this run.
            return;
        }
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::SavePrompt {
            request: "unreadable-save".to_string(),
            role: ExecutionRole::Release,
            base_revision: config::file_revision(RELEASE_PROMPT),
            text: "release {pr_list}\n".to_string(),
        });
        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the save must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Failed);
        assert!(result.message.unwrap().contains("cannot read the prompt"));

        rig.act(Action::ResetPrompt {
            request: "unreadable-reset".to_string(),
            role: ExecutionRole::Release,
            base_revision: config::file_revision(RELEASE_PROMPT),
        });
        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the reset must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert!(!rig.prompts.join("release.md").exists());
    }

    /// A blank prompt file never starts an agent. A crash between the
    /// write and the rename can leave one behind.
    #[test]
    fn an_empty_prompt_file_fails_the_dispatch_by_name() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("implement.md"), " \n\t").unwrap();

        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job_count(), 0, "an empty prompt blocks the dispatch");
        let task = rig.task("borsuk/implement-i142");
        assert_eq!(task.attempt, 2, "the failed dispatch requeues the task");
        assert_eq!(task.state, TaskState::Queued);
        let log = fs::read_to_string(&task.log_path).unwrap();
        assert!(
            log.contains("implement.md is empty"),
            "the dispatch must name the empty file: {log}"
        );
    }

    #[test]
    fn a_prompt_reset_removes_the_file_and_the_next_task_reads_the_builtin() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("implement.md"), "custom {number}\n").unwrap();
        rig.daemon.refresh_prompts();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::ResetPrompt {
            request: "prompt-reset".to_string(),
            role: ExecutionRole::Implement,
            base_revision: config::file_revision("custom {number}\n"),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the reset must push a settings result");
        };
        assert_eq!(result.operation, SettingsOperation::ResetPrompt);
        assert_eq!(result.status, SettingsResultStatus::Saved);
        assert_eq!(result.revision, config::file_revision(IMPLEMENT_PROMPT));
        assert!(!rig.prompts.join("implement.md").exists());
        let view = rig
            .daemon
            .prompts
            .iter()
            .find(|view| view.role == ExecutionRole::Implement)
            .unwrap();
        assert_eq!(view.source, PromptSource::Builtin);

        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert!(rig
            .job(0)
            .prompt
            .starts_with("You implement ticket #142 of borsuk"));
    }

    #[test]
    fn the_state_view_carries_every_prompt_and_a_dispatch_refreshes_an_outside_edit() {
        let dir = temp_root();
        let steps = fresh_issue_steps(
            &rig_repo(&dir),
            &issue_wt(&dir, 142),
            142,
            &rig_gitdir(&dir),
        );
        let mut rig = Rig::make_in(dir, steps, |_| {});
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_pusher(Box::new(move |view| tx.send(view).unwrap()));
        rig.drive();
        let view = rx.try_recv().expect("the first drive publishes a view");
        assert_eq!(
            view.settings
                .prompts
                .iter()
                .map(|prompt| prompt.role)
                .collect::<Vec<_>>(),
            prompts::ROLES.to_vec(),
            "the view carries one prompt per role that has a template"
        );
        assert!(view
            .settings
            .prompts
            .iter()
            .all(|prompt| prompt.source == PromptSource::Builtin));
        assert_eq!(
            view.settings.prompts[1].text, IMPLEMENT_PROMPT,
            "the view shows the built-in text of the implement role"
        );

        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("implement.md"), "outside {number}\n").unwrap();
        rig.poll(vec![issue(142, &["refined"])], vec![]);
        assert_eq!(rig.job(0).prompt, "outside 142\n");
        let view = std::iter::from_fn(|| rx.try_recv().ok())
            .last()
            .expect("the dispatch publishes a view");
        let prompt = &view.settings.prompts[1];
        assert_eq!(prompt.source, PromptSource::File);
        assert_eq!(prompt.text, "outside {number}\n");
    }

    #[test]
    fn a_settings_reload_refreshes_the_prompt_views() {
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
        fs::create_dir_all(rig.repo.join(".git")).unwrap();
        fs::write(&config_path, settings_config_text(&rig.repo, "m")).unwrap();
        fs::create_dir_all(&rig.prompts).unwrap();
        fs::write(rig.prompts.join("review.md"), "outside review {number}\n").unwrap();
        let (tx, rx) = mpsc::channel();
        rig.daemon
            .set_ticket_pusher(Box::new(move |push| tx.send(push).unwrap()));

        rig.act(Action::ReloadSettings {
            request: "reload-prompts".to_string(),
        });

        let Push::SettingsResult(result) = rx.try_recv().unwrap() else {
            panic!("the reload must push a settings result");
        };
        assert_eq!(result.status, SettingsResultStatus::Reloaded);
        let view = rig
            .daemon
            .prompts
            .iter()
            .find(|view| view.role == ExecutionRole::Review)
            .unwrap();
        assert_eq!(view.source, PromptSource::File);
        assert_eq!(view.text, "outside review {number}\n");
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

    // ------------------------------------------------------------------
    // Usage probes
    // ------------------------------------------------------------------

    /// A rig with the usage probes on.
    fn usage_rig() -> Rig {
        Rig::make_with(Vec::new(), |config| {
            config.usage.enabled = true;
        })
    }

    #[test]
    fn a_due_identity_spawns_one_probe_and_an_in_flight_identity_spawns_none() {
        let mut rig = usage_rig();
        let usage_rx = rig.daemon.usage_rx.take().unwrap();

        rig.daemon.drive();

        let (identity, result) = usage_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first drive must spawn the claude probe");
        assert_eq!(identity, "claude");
        assert!(
            result.is_err(),
            "the scripted rig gives the probe no credentials: {result:?}"
        );

        // The probe thread ran, but the answer is not applied yet, so the
        // identity stays in flight and a second drive spawns nothing.
        rig.daemon.drive();
        assert!(
            usage_rx.try_recv().is_err(),
            "a drive while the probe is in flight must spawn nothing"
        );

        rig.daemon.handle(Inbound::Usage { identity, result });

        let record = rig.daemon.usage_records.get("claude").unwrap();
        assert!(record.error.is_some());
        assert!(!rig.daemon.usage_in_flight.contains("claude"));

        // The failure started the backoff, so the next drive waits.
        rig.daemon.drive();
        assert!(
            usage_rx.try_recv().is_err(),
            "a drive inside the backoff window must spawn nothing"
        );
    }

    #[test]
    fn a_usage_result_applies_the_record_clears_in_flight_and_resets_the_wait() {
        let mut rig = usage_rig();
        let usage_rx = rig.daemon.usage_rx.take().unwrap();
        rig.daemon.drive();
        let _ = usage_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Ok(UsageRecord {
                harness: Harness::Claude,
                mode: usage::UsageMode::Plan,
                windows: vec![usage::UsageWindow {
                    label: "5 hour".to_string(),
                    used_percent: 30.0,
                    resets_at_ms: Some(T0 + 3_600_000),
                }],
                ..UsageRecord::default()
            }),
        });

        let record = rig.daemon.usage_records.get("claude").unwrap();
        assert_eq!(record.error, None);
        assert_eq!(record.mode, usage::UsageMode::Plan);
        assert_eq!(record.windows.len(), 1);
        assert_eq!(record.updated_ms, T0);
        assert!(!rig.daemon.usage_in_flight.contains("claude"));
        assert_eq!(
            rig.daemon.usage_wait_minutes["claude"],
            rig.daemon.config.usage.minutes
        );

        // The success resets the wait, so the next drive spawns nothing.
        rig.daemon.drive();
        assert!(usage_rx.try_recv().is_err());
        let deadline = rig.daemon.next_deadline().unwrap();
        assert!(
            deadline <= Duration::from_millis(10 * 60_000),
            "the usage moment must bound the sleep: {deadline:?}"
        );
    }

    #[test]
    fn a_successful_probe_keeps_the_reason_it_reports_itself() {
        let mut rig = usage_rig();
        let _usage_rx = rig.daemon.usage_rx.take().unwrap();

        // The z.ai and zen probes answer Ok with a reason instead of a
        // failure, because the factory spend of the identity stays valid.
        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Ok(UsageRecord {
                harness: Harness::Claude,
                mode: usage::UsageMode::Api,
                error: Some("pay as you go key: factory spend only".to_string()),
                ..UsageRecord::default()
            }),
        });

        let record = rig.daemon.usage_records.get("claude").unwrap();
        assert_eq!(
            record.error.as_deref(),
            Some("pay as you go key: factory spend only")
        );
        assert_eq!(record.mode, usage::UsageMode::Api);
        // The probe succeeded, so the wait stays at the configured cadence.
        assert_eq!(
            rig.daemon.usage_wait_minutes["claude"],
            rig.daemon.config.usage.minutes
        );
        let views = rig.daemon.usage_views();
        assert_eq!(
            views[0].error.as_deref(),
            Some("pay as you go key: factory spend only")
        );
    }

    #[test]
    fn a_later_good_probe_drops_the_error_of_the_failed_one() {
        let mut rig = usage_rig();
        let _usage_rx = rig.daemon.usage_rx.take().unwrap();
        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Err("rate limited".to_string()),
        });
        assert!(rig.daemon.usage_records["claude"].error.is_some());

        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Ok(UsageRecord {
                harness: Harness::Claude,
                mode: usage::UsageMode::Plan,
                ..UsageRecord::default()
            }),
        });

        assert_eq!(rig.daemon.usage_records["claude"].error, None);
    }

    #[test]
    fn a_failed_probe_doubles_the_wait_and_keeps_the_last_good_record() {
        let mut rig = usage_rig();
        let usage_rx = rig.daemon.usage_rx.take().unwrap();
        rig.daemon.drive();
        let _ = usage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Ok(UsageRecord {
                harness: Harness::Claude,
                mode: usage::UsageMode::Plan,
                windows: vec![usage::UsageWindow {
                    label: "weekly".to_string(),
                    used_percent: 10.0,
                    resets_at_ms: None,
                }],
                updated_ms: T0,
                ..UsageRecord::default()
            }),
        });

        *rig.t.lock().unwrap() = T0 + 60_000;
        rig.daemon.handle(Inbound::Usage {
            identity: "claude".to_string(),
            result: Err("rate limited".to_string()),
        });

        let record = rig.daemon.usage_records.get("claude").unwrap();
        assert_eq!(
            record.windows.len(),
            1,
            "the last good windows must stay visible"
        );
        assert_eq!(record.updated_ms, T0, "the last good time must stay");
        assert_eq!(record.error.as_deref(), Some("rate limited"));
        assert_eq!(rig.daemon.usage_wait_minutes["claude"], 20);

        // Before the doubled moment no probe spawns; after it, one does.
        // The failure landed at T0 + 1m, so the moment is T0 + 21m. A bare
        // drive does not refresh the clock, so the test moves both.
        for (now, due) in [(T0 + 15 * 60_000, false), (T0 + 21 * 60_000, true)] {
            *rig.t.lock().unwrap() = now;
            rig.daemon.now_ms = now;
            rig.daemon.drive();
            let arrived = usage_rx
                .recv_timeout(Duration::from_millis(if due { 5_000 } else { 50 }))
                .is_ok();
            assert_eq!(
                arrived, due,
                "the doubled wait must gate the probe at {now}"
            );
        }
    }

    #[test]
    fn turn_end_costs_accumulate_per_identity_and_survive_a_restart() {
        let mut rig = Rig::make_with(Vec::new(), |config| {
            config.usage.enabled = true;
            set_role_harness(config, ExecutionRole::Review, Harness::Codex);
        });
        rig.daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                142,
                PathBuf::from("logs/refine.jsonl"),
                T0,
            )
            .unwrap();
        rig.daemon
            .table
            .upsert_queued(
                "borsuk",
                Stage::Review,
                ItemKind::Issue,
                9,
                PathBuf::from("logs/review.jsonl"),
                T0,
            )
            .unwrap();
        let ids = rig.daemon.table.order.clone();

        rig.daemon.add_turn_cost(&ids[0], 0.5);
        rig.daemon.add_turn_cost(&ids[1], 0.25);
        rig.daemon.add_turn_cost(&ids[0], 0.25);

        let claude = &rig.daemon.usage_spend["claude"];
        assert_eq!(claude.total_usd, 0.75);
        let codex = &rig.daemon.usage_spend["codex"];
        assert_eq!(codex.total_usd, 0.25);
        assert_eq!(claude.models["m"], 0.75);

        // The spend shows in the view before any probe returns.
        let views = rig.daemon.usage_views();
        let claude_row = views.iter().find(|row| row.identity == "claude").unwrap();
        assert_eq!(claude_row.factory_spend_usd, 0.75);
        let codex_row = views.iter().find(|row| row.identity == "codex").unwrap();
        assert_eq!(codex_row.factory_spend_usd, 0.25);

        // A restart loads the totals back from state.json.
        rig.daemon.force_save_state();
        let reloaded = DaemonState::load(&rig.daemon.state_path);
        assert_eq!(reloaded.runtime.spend["claude"].total_usd, 0.75);
        assert_eq!(reloaded.runtime.spend["codex"].total_usd, 0.25);
    }

    #[test]
    fn a_retired_identity_loses_its_moment_and_never_pins_the_loop_awake() {
        let mut rig = usage_rig();
        let _usage_rx = rig.daemon.usage_rx.take().unwrap();
        // A role edit can retire an identity. The daemon keeps its past-due
        // moment, which would make every deadline zero and spin the loop.
        rig.daemon
            .usage_next_probe_ms
            .insert("codex".to_string(), T0 - 60_000);
        rig.daemon
            .usage_wait_minutes
            .insert("codex".to_string(), 20);
        rig.daemon
            .usage_next_probe_ms
            .insert("claude".to_string(), T0 + 10 * 60_000);
        rig.daemon.now_ms = T0;

        rig.daemon.drive();

        assert!(!rig.daemon.usage_next_probe_ms.contains_key("codex"));
        assert!(!rig.daemon.usage_wait_minutes.contains_key("codex"));
        assert_eq!(
            rig.daemon.next_deadline(),
            Some(Duration::from_millis(10 * 60_000)),
            "only the live identity may set the wake moment"
        );
    }

    #[test]
    fn a_disabled_usage_table_spawns_no_probe_and_keeps_the_view_empty() {
        let mut rig = Rig::make_with(Vec::new(), |config| {
            config.usage.enabled = false;
        });
        let usage_rx = rig.daemon.usage_rx.take().unwrap();

        rig.daemon.drive();

        assert!(usage_rx.try_recv().is_err());
        assert!(rig.daemon.usage_views().is_empty());
        assert!(rig.daemon.usage_in_flight.is_empty());
    }
}
