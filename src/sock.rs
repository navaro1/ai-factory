//! The wire types, the socket server, and the socket client.
//!
//! The wire format is one JSON object per line over a Unix socket. The
//! daemon sends [`Push`] messages and receives [`Action`] messages.
//!
//! The daemon event loop owns all mutable state and never blocks on a
//! client. It rebuilds a [`StateView`] and hands it to [`Server::publish`].
//! A pusher thread coalesces publishes to at most one push every
//! [`PUSH_COALESCE_MS`] milliseconds. Every connected UI owns a bounded
//! channel; the server drops a subscriber whose channel fills, so a slow or
//! dead UI can never stall the factory. The only locks in this module guard
//! the one-slot publish buffer and the subscriber list. Both are plumbing,
//! not domain state.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufWriter, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ReleasePolicy};
use crate::decisions::{Decision, Decisions};
use crate::model::{ItemKind, Stage};
use crate::sched::{Limits, Paused};
use crate::tasks::{TaskState, TaskTable};
use crate::trains::Train;

/// The minimum time between two state pushes, in milliseconds.
///
/// The daemon may mark the state dirty many times per second. The pusher
/// thread sends at most one push per subscriber set in this window.
pub const PUSH_COALESCE_MS: u64 = 50;

/// How many pushes a subscriber may buffer before the server drops it.
///
/// A client that stops reading fills this buffer in about one second at the
/// coalesced push rate. The drop protects the daemon, never the client.
const SUBSCRIBER_CAPACITY: usize = 16;

/// Everything the UI draws, in one snapshot.
///
/// The daemon rebuilds the view when its state changes and pushes it over
/// the socket. The view carries no transcripts: the UI tails a task's log
/// file itself, which is why [`TaskView::log_path`] is part of the view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateView {
    /// The configured repositories, in alias order.
    pub repos: Vec<RepoView>,
    /// The four pipeline stages, in pipeline order.
    pub stages: Vec<StageView>,
    /// The strict lane reservations.
    pub lanes: Vec<LaneView>,
    /// The tasks, in daemon insertion order.
    pub tasks: Vec<TaskView>,
    /// The open decisions, in push order.
    pub decisions: Vec<Decision>,
    /// The release train of each configured repository.
    pub trains: Vec<TrainView>,
    /// What the operator paused.
    pub paused: PausedView,
}

/// What the session view's input bar does for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum InputMode {
    /// A live Claude session receives the message at once.
    Live,
    /// The next message relaunches the parked Claude session.
    Resume,
    /// The next message waits for the running turn and starts the next one.
    NextTurn,
    /// The next message queues a follow-up turn that continues the session.
    Follow,
    /// The task takes no message, and the reason tells the human why.
    Closed {
        /// The sentence that tells the human why the input is closed.
        reason: String,
    },
}

/// The daemon state one state view is built from.
///
/// Every field borrows one piece of the state the daemon event loop owns.
/// Build the view with [`StateInput::build`].
///
/// `policies` holds the active release policy per repository alias. An
/// entry overrides the config file value; a missing entry falls back to
/// the config. A repository without an entry in `trains` gets an empty
/// train view. A stage limit counts as overridden when the runtime limit
/// differs from the config file value. Each task must have an entry in
/// `input_modes`. The daemon adds queued message counts after this build.
#[derive(Debug, Clone, Copy)]
pub struct StateInput<'a> {
    /// The loaded configuration file.
    pub config: &'a Config,
    /// The stage limits and lane reservations, with runtime overrides.
    pub limits: &'a Limits,
    /// What the operator paused.
    pub paused: &'a Paused,
    /// The task table.
    pub table: &'a TaskTable,
    /// The open decision queue.
    pub decisions: &'a Decisions,
    /// The release trains, keyed by repository alias.
    pub trains: &'a BTreeMap<String, Train>,
    /// The active release policies, keyed by repository alias.
    pub policies: &'a BTreeMap<String, ReleasePolicy>,
    /// The input mode of each task, keyed by task id. The daemon decides
    /// every mode; this module only serializes it.
    pub input_modes: &'a BTreeMap<String, InputMode>,
    /// The current time in milliseconds since the Unix epoch.
    pub now_ms: u64,
}

impl StateInput<'_> {
    /// Build the state view.
    pub fn build(&self) -> Result<StateView> {
        let Self {
            config,
            limits,
            paused,
            table,
            decisions,
            trains,
            policies,
            input_modes,
            now_ms,
        } = *self;
        let repos = config
            .repos
            .values()
            .map(|repo| RepoView {
                alias: repo.alias.clone(),
                owner_repo: repo.owner_repo.clone(),
            })
            .collect();
        let running = table.counts_by_stage();
        let stages = Stage::ALL
            .iter()
            .map(|&stage| {
                let limit = limits.limit(stage);
                StageView {
                    stage,
                    limit,
                    overridden: limit != config.stage(stage).limit,
                    running: running[&stage],
                    queued: table
                        .by_id
                        .values()
                        .filter(|task| task.stage == stage && task.state == TaskState::Queued)
                        .count(),
                }
            })
            .collect();
        let lanes = limits
            .lanes
            .iter()
            .map(|((stage, repo), slots)| LaneView {
                stage: *stage,
                repo: repo.clone(),
                slots: *slots,
            })
            .collect();
        let tasks = table
            .order
            .iter()
            .map(|id| {
                let task = table
                    .by_id
                    .get(id)
                    .ok_or_else(|| anyhow!("task table order names missing task \"{id}\""))?;
                Ok(TaskView {
                    id: task.id.clone(),
                    repo: task.repo.clone(),
                    stage: task.stage,
                    kind: task.kind,
                    number: task.number,
                    state: task.state.clone(),
                    attempt: task.attempt,
                    log_path: task.log_path.clone(),
                    input: input_modes
                        .get(id)
                        .cloned()
                        .ok_or_else(|| anyhow!("task \"{id}\" has no input mode"))?,
                    queued_messages: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let trains = config
            .repos
            .keys()
            .map(|repo| {
                let policy = policies.get(repo).unwrap_or(&config.repos[repo].release);
                match trains.get(repo) {
                    Some(train) => TrainView {
                        repo: repo.clone(),
                        queue: train.queue.clone(),
                        stacked: train.stacked.clone(),
                        batch: train.batch().to_vec(),
                        policy: policy.clone(),
                        next_fire_ms: train.next_deadline_ms(policy, now_ms),
                        in_flight: train.in_flight.clone(),
                    },
                    None => TrainView {
                        repo: repo.clone(),
                        queue: Vec::new(),
                        stacked: Vec::new(),
                        batch: Vec::new(),
                        policy: policy.clone(),
                        next_fire_ms: None,
                        in_flight: None,
                    },
                }
            })
            .collect();
        let paused = PausedView {
            global: paused.global,
            overrides: paused
                .stages
                .iter()
                .map(|(stage, value)| PauseOverrideView {
                    scope: PauseScope::Stage { stage: *stage },
                    paused: *value,
                })
                .chain(
                    paused
                        .lanes
                        .iter()
                        .map(|((stage, repo), value)| PauseOverrideView {
                            scope: PauseScope::Lane {
                                stage: *stage,
                                repo: repo.clone(),
                            },
                            paused: *value,
                        }),
                )
                .chain(paused.tasks.iter().map(|(task, value)| PauseOverrideView {
                    scope: PauseScope::Task { task: task.clone() },
                    paused: *value,
                }))
                .collect(),
        };
        Ok(StateView {
            repos,
            stages,
            lanes,
            tasks,
            decisions: decisions.open().to_vec(),
            trains,
            paused,
        })
    }
}

/// One repository in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoView {
    /// The repository alias from the config file.
    pub alias: String,
    /// The `owner/name` GitHub slug. Empty before the first resolve.
    pub owner_repo: String,
}

/// One pipeline stage in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageView {
    /// The stage.
    pub stage: Stage,
    /// The effective task limit of the stage.
    pub limit: usize,
    /// True when the runtime limit differs from the config file value.
    pub overridden: bool,
    /// How many tasks of the stage run right now.
    pub running: usize,
    /// How many tasks of the stage wait to start.
    pub queued: usize,
}

/// One lane reservation in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneView {
    /// The stage the reservation applies to.
    pub stage: Stage,
    /// The repository the slots stay reserved for.
    pub repo: String,
    /// How many slots of the stage stay reserved for the repository.
    pub slots: usize,
}

/// One task in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// The task id, for example `borsuk/implement-i142`.
    pub id: String,
    /// The repository alias.
    pub repo: String,
    /// The pipeline stage.
    pub stage: Stage,
    /// Whether the item is an issue or a pull request.
    pub kind: ItemKind,
    /// The issue or pull request number.
    pub number: u64,
    /// The task state.
    pub state: TaskState,
    /// The current attempt number, starting at 1.
    pub attempt: u32,
    /// The JSON lines log of the task. The UI tails this file itself.
    pub log_path: PathBuf,
    /// What the session view's input bar does for this task.
    pub input: InputMode,
    /// How many chat messages wait to run for this task.
    pub queued_messages: usize,
}

/// One release train in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainView {
    /// The repository alias.
    pub repo: String,
    /// The ready pull request numbers that wait in the queue.
    pub queue: Vec<u64>,
    /// The pull request numbers the human stacked for the next batch.
    pub stacked: Vec<u64>,
    /// The active batch or the exact batch that a failed train must retry.
    pub batch: Vec<u64>,
    /// The active release policy.
    pub policy: ReleasePolicy,
    /// The next automatic fire time, in milliseconds since the Unix epoch.
    /// None when the policy or the queue gives no deadline.
    pub next_fire_ms: Option<u64>,
    /// The task id of the batch in flight, when one is.
    pub in_flight: Option<String>,
}

/// The paused flags in the state view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PausedView {
    /// True when the whole factory takes no new work.
    pub global: bool,
    /// More specific states that override an inherited pause state.
    pub overrides: Vec<PauseOverrideView>,
}

/// One explicit state below the whole-factory pause level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseOverrideView {
    /// The exact scope that owns this state.
    pub scope: PauseScope,
    /// True when the scope is paused. False when it is resumed.
    pub paused: bool,
}

/// One message from the daemon to a UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Push {
    /// The whole current state.
    State(StateView),
}

/// One command from a UI or from `aif stop` to the daemon.
///
/// Every variant names its target explicitly. The daemon resolves each
/// target against its own state and refuses what does not exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Queue a refine task for one item.
    Refine {
        /// The repository alias.
        repo: String,
        /// Whether the item is an issue or a pull request.
        kind: ItemKind,
        /// The issue or pull request number.
        number: u64,
    },
    /// Send a chat message into a live interactive session.
    Chat {
        /// The task id of the session.
        task: String,
        /// The message text.
        text: String,
    },
    /// Answer one open decision.
    Answer {
        /// The id of the open decision row.
        decision_id: String,
        /// The response. It must fit the decision kind.
        response: crate::decisions::Response,
    },
    /// Abort one task and kill its process.
    Abort {
        /// The task id.
        task: String,
    },
    /// Retry one failed task from attempt 1.
    Retry {
        /// The task id.
        task: String,
    },
    /// Stack or unstack one pull request for the next release batch.
    Stack {
        /// The repository alias.
        repo: String,
        /// The pull request number.
        pr: u64,
        /// True to stack, false to unstack.
        on: bool,
    },
    /// Fire the release train of one repository with the given batch.
    Go {
        /// The repository alias.
        repo: String,
        /// The pull request numbers to release.
        prs: Vec<u64>,
    },
    /// Set the release policy of one repository.
    Policy {
        /// The repository alias.
        repo: String,
        /// The new policy.
        policy: ReleasePolicy,
    },
    /// Set the task limit of one stage.
    Limit {
        /// The stage.
        stage: Stage,
        /// The new limit. At least 1.
        limit: usize,
    },
    /// Set the lane reservation of one repository on one stage.
    Lane {
        /// The stage.
        stage: Stage,
        /// The repository alias.
        repo: String,
        /// The new reservation. 0 removes it.
        slots: usize,
    },
    /// Pause or resume one scope.
    Pause {
        /// What to pause or resume.
        scope: PauseScope,
        /// True to pause, false to resume.
        paused: bool,
    },
    /// Start an interactive ticket-creation session for one repository.
    TicketCreate {
        /// The repository alias.
        repo: String,
    },
    /// Force an early poll of one repository, or of all when None.
    Reconcile {
        /// The repository alias, or None for every repository.
        repo: Option<String>,
    },
    /// Shut the daemon down.
    Stop,
}

/// The scope of one [`Action::Pause`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum PauseScope {
    /// The whole factory.
    Global,
    /// One stage.
    Stage {
        /// The stage to pause or resume.
        stage: Stage,
    },
    /// One repository lane inside one stage.
    Lane {
        /// The stage of the lane.
        stage: Stage,
        /// The repository alias.
        repo: String,
    },
    /// One task.
    Task {
        /// The stable task identifier.
        task: String,
    },
}

/// One subscriber of the push stream.
struct Subscriber {
    /// The identity used to remove the entry again.
    id: u64,
    /// The bounded channel the writer thread drains into the socket.
    tx: SyncSender<Arc<Push>>,
    /// A socket handle that can stop both client threads.
    stream: UnixStream,
    /// The last snapshot queued for this subscriber.
    last: Option<Arc<Push>>,
}

/// The newest state and whether subscribers need it.
#[derive(Default)]
struct PublishedState {
    current: Option<Arc<Push>>,
    dirty: bool,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        if let Err(error) = self.stream.shutdown(Shutdown::Both) {
            if error.kind() != std::io::ErrorKind::NotConnected {
                eprintln!("aifd: cannot close a control client: {error}");
            }
        }
    }
}

/// The control socket server.
///
/// The daemon owns one server. Dropping it stops the accept thread, removes
/// the socket file, and ends the pusher thread, so the daemon should drop
/// the server before it exits. Clients that connected earlier then see their
/// streams close.
pub struct Server {
    path: PathBuf,
    socket_dev: u64,
    socket_ino: u64,
    published: Arc<Mutex<PublishedState>>,
    registry: Arc<Mutex<Vec<Subscriber>>>,
    stopping: Arc<AtomicBool>,
    wake: SyncSender<()>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("path", &self.path).finish()
    }
}

impl Server {
    /// Bind the socket at `path` and start the accept and pusher threads.
    ///
    /// The call replaces a stale socket file left by a dead daemon and fails
    /// when another daemon still listens on the path. The socket file gets
    /// mode 0600. The second return value delivers every client action; the
    /// daemon event loop drains it.
    pub fn bind(path: &Path) -> Result<(Server, Receiver<Action>)> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() {
                    bail!(
                        "cannot bind {}: the existing path is not a socket",
                        path.display()
                    );
                }
                match UnixStream::connect(path) {
                    // The probe connection lands in the live daemon's backlog.
                    // It carries no actions, so the live daemon ignores it.
                    Ok(_) => bail!("another daemon is already listening on {}", path.display()),
                    Err(_) => {
                        fs::remove_file(path).with_context(|| {
                            format!("cannot remove the stale socket file {}", path.display())
                        })?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect the socket path {}", path.display()));
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let listener =
            UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot set mode 0600 on {}", path.display()))?;
        let metadata = fs::metadata(path)
            .with_context(|| format!("cannot inspect the bound socket {}", path.display()))?;

        let published = Arc::new(Mutex::new(PublishedState::default()));
        let registry: Arc<Mutex<Vec<Subscriber>>> = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = sync_channel::<()>(1);
        let (actions, actions_rx) = channel::<Action>();

        {
            let published = Arc::clone(&published);
            let registry = Arc::clone(&registry);
            thread::spawn(move || run_pusher(published, registry, wake_rx));
        }
        {
            let stopping = Arc::clone(&stopping);
            let registry = Arc::clone(&registry);
            let published = Arc::clone(&published);
            let actions = actions.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    match stream {
                        Ok(stream) => attach_client(
                            stream,
                            Arc::clone(&registry),
                            Arc::clone(&published),
                            actions.clone(),
                        ),
                        Err(error) => {
                            eprintln!("aifd: control socket accept failed: {error}");
                            return;
                        }
                    }
                }
            });
        }

        Ok((
            Server {
                path: path.to_path_buf(),
                socket_dev: metadata.dev(),
                socket_ino: metadata.ino(),
                published,
                registry,
                stopping,
                wake,
            },
            actions_rx,
        ))
    }

    /// The socket path the server bound.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hand the current state to the pusher thread.
    ///
    /// The call does not wait for a client and keeps only the newest view.
    /// The pusher thread sends at most one push per [`PUSH_COALESCE_MS`]
    /// milliseconds to every subscriber.
    pub fn publish(&self, state: StateView) {
        let mut published = lock(&self.published);
        published.current = Some(Arc::new(Push::State(state)));
        published.dirty = true;
        drop(published);
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                eprintln!("aifd: the control socket pusher stopped")
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // A connection to our own socket wakes the blocked accept, which
        // then sees the stop flag and exits.
        if let Err(error) = UnixStream::connect(&self.path) {
            eprintln!(
                "aifd: cannot wake the control socket at {}: {error}",
                self.path.display()
            );
        }
        // Dropping the subscriber senders ends the writer threads, and each
        // writer drop closes its client stream.
        lock(&self.registry).clear();
        match fs::symlink_metadata(&self.path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.dev() == self.socket_dev
                    && metadata.ino() == self.socket_ino =>
            {
                if let Err(error) = fs::remove_file(&self.path) {
                    eprintln!(
                        "aifd: cannot remove the control socket {}: {error}",
                        self.path.display()
                    );
                }
            }
            // A replacement at this path belongs to another owner.
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "aifd: cannot inspect the control socket {}: {error}",
                self.path.display()
            ),
        }
    }
}

/// Lock a mutex and take the guard even when the mutex is poisoned.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("aifd: a control socket lock was poisoned");
            mutex.clear_poison();
            PoisonError::into_inner(error)
        }
    }
}

/// Send `push` to every subscriber and drop the ones that cannot keep up.
fn broadcast(registry: &mut Vec<Subscriber>, push: &Arc<Push>) {
    registry.retain_mut(|subscriber| {
        if subscriber
            .last
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, push))
        {
            return true;
        }
        match subscriber.tx.try_send(Arc::clone(push)) {
            Ok(()) => {
                subscriber.last = Some(Arc::clone(push));
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    });
}

/// Coalesce publishes and push at most one state per window.
fn run_pusher(
    published: Arc<Mutex<PublishedState>>,
    registry: Arc<Mutex<Vec<Subscriber>>>,
    wake_rx: Receiver<()>,
) {
    let window = Duration::from_millis(PUSH_COALESCE_MS);
    let mut last_push: Option<Instant> = None;
    while wake_rx.recv().is_ok() {
        if let Some(last) = last_push {
            let quiet = window.saturating_sub(last.elapsed());
            if !quiet.is_zero() {
                // Wait for the rest of the window. A publish during this
                // wait refreshes the current snapshot. The next push uses it.
                thread::sleep(quiet);
            }
        }
        let mut registry = lock(&registry);
        let push = {
            let mut published = lock(&published);
            if published.dirty {
                published.dirty = false;
                published.current.clone()
            } else {
                None
            }
        };
        if let Some(push) = push {
            broadcast(&mut registry, &push);
            last_push = Some(Instant::now());
        }
    }
}

/// Remove the subscriber with `id` from the registry.
fn unregister(registry: &Mutex<Vec<Subscriber>>, id: u64) {
    lock(registry).retain(|subscriber| subscriber.id != id);
}

/// Register one accepted client and start its reader and writer threads.
///
/// The writer thread drains the subscriber channel into the socket. The
/// reader thread turns incoming lines into [`Action`]s. Both threads
/// unregister the subscriber when the client goes away.
fn attach_client(
    stream: UnixStream,
    registry: Arc<Mutex<Vec<Subscriber>>>,
    published: Arc<Mutex<PublishedState>>,
    actions: Sender<Action>,
) {
    let write_half = match stream.try_clone() {
        Ok(write_half) => write_half,
        Err(error) => {
            eprintln!("aifd: cannot clone a control client for writes: {error}");
            return;
        }
    };
    let shutdown_half = match stream.try_clone() {
        Ok(shutdown_half) => shutdown_half,
        Err(error) => {
            eprintln!("aifd: cannot clone a control client for shutdown: {error}");
            return;
        }
    };
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = sync_channel::<Arc<Push>>(SUBSCRIBER_CAPACITY);
    // The registry lock keeps a broadcast from passing the initial push.
    // The state lock is second here and in the pusher, so the order is safe.
    {
        let mut registry = lock(&registry);
        let initial = lock(&published).current.clone();
        if let Some(push) = &initial {
            if let Err(error) = tx.try_send(Arc::clone(push)) {
                eprintln!("aifd: cannot send the initial state: {error}");
                return;
            }
        }
        registry.push(Subscriber {
            id,
            tx: tx.clone(),
            stream: shutdown_half,
            last: initial,
        });
    }
    {
        let registry = Arc::clone(&registry);
        thread::spawn(move || {
            let mut writer = BufWriter::new(write_half);
            for push in rx {
                let line = match serde_json::to_string(push.as_ref()) {
                    Ok(line) => line,
                    Err(error) => {
                        eprintln!("aifd: cannot encode a state push: {error}");
                        break;
                    }
                };
                if let Err(error) = writeln!(writer, "{line}") {
                    eprintln!("aifd: cannot write a state push: {error}");
                    break;
                }
                if let Err(error) = writer.flush() {
                    eprintln!("aifd: cannot flush a state push: {error}");
                    break;
                }
            }
            unregister(&registry, id);
        });
    }
    {
        let registry = Arc::clone(&registry);
        let actions = actions.clone();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("aifd: cannot read a control action: {error}");
                        break;
                    }
                }
                match serde_json::from_str::<Action>(line.trim()) {
                    Ok(action) => {
                        if let Err(error) = actions.send(action) {
                            eprintln!("aifd: cannot deliver a control action: {error}");
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("aifd: dropped a malformed control line: {error}");
                    }
                }
            }
            unregister(&registry, id);
        });
    }
}

/// The client half of the control socket.
///
/// The TUI and `aif stop` share this half: [`Client::connect`] reaches the
/// daemon, [`Client::send`] sends one [`Action`], and [`Client::pushes`]
/// iterates the pushes the daemon sends.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to the daemon socket at `path`.
    pub fn connect(path: &Path) -> Result<Client> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("cannot connect to {}", path.display()))?;
        Ok(Client { stream })
    }

    /// Send one action to the daemon.
    ///
    /// The call writes one JSON line and flushes it.
    pub fn send(&mut self, action: &Action) -> Result<()> {
        let line = serde_json::to_string(action).context("cannot encode the action")?;
        writeln!(self.stream, "{line}")
            .and_then(|_| self.stream.flush())
            .context("cannot send the action to the daemon")
    }

    /// Start an iterator over the pushes of the daemon.
    ///
    /// The first push is the current state, and every later push is a
    /// coalesced update. The iterator ends on a clean close and yields one
    /// error before it ends on a read timeout or a broken stream.
    pub fn pushes(&self) -> Result<Pushes> {
        let stream = self
            .stream
            .try_clone()
            .context("cannot read from the daemon socket")?;
        Ok(Pushes {
            reader: std::io::BufReader::new(stream),
            failed: false,
        })
    }

    /// Set how long one push read may block before it fails.
    pub fn set_read_timeout(&self, timeout: Duration) -> Result<()> {
        self.stream
            .set_read_timeout(Some(timeout))
            .context("cannot set the read timeout")
    }
}

/// The push iterator of one connected client.
#[derive(Debug)]
pub struct Pushes {
    reader: std::io::BufReader<UnixStream>,
    failed: bool,
}

impl Iterator for Pushes {
    type Item = Result<Push>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            // A clean close ends the stream.
            Ok(0) => None,
            Ok(_) => match serde_json::from_str::<Push>(line.trim()) {
                Ok(push) => Some(Ok(push)),
                Err(error) => {
                    self.failed = true;
                    Some(Err(anyhow::anyhow!("bad push line: {error}")))
                }
            },
            Err(error) => {
                self.failed = true;
                Some(Err(anyhow::Error::from(error).context("cannot read a push")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir();
            for attempt in 0..1000 {
                let path = base.join(format!(
                    "aif-sock-test-{}-{tag}-{attempt}",
                    std::process::id()
                ));
                if fs::create_dir(&path).is_ok() {
                    return TempDir(path);
                }
            }
            panic!("cannot create a temporary directory");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("cannot remove test directory {}: {error}", self.0.display());
                }
            }
        }
    }

    /// A state view with one stage and one repository, marked by `label`.
    fn sample_view(label: usize) -> StateView {
        StateView {
            repos: vec![RepoView {
                alias: format!("repo-{label}"),
                owner_repo: String::new(),
            }],
            stages: vec![StageView {
                stage: Stage::Refine,
                limit: 3,
                overridden: false,
                running: 0,
                queued: 0,
            }],
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions: Vec::new(),
            trains: Vec::new(),
            paused: PausedView {
                global: false,
                overrides: Vec::new(),
            },
        }
    }

    /// One action of every variant.
    fn every_action() -> Vec<Action> {
        vec![
            Action::Refine {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 142,
            },
            Action::Chat {
                task: "borsuk/refine-i142".to_string(),
                text: "use sqlite".to_string(),
            },
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-1".to_string(),
                response: crate::decisions::Response::Deny {
                    message: "not this file".to_string(),
                },
            },
            Action::Abort {
                task: "borsuk/implement-i142".to_string(),
            },
            Action::Retry {
                task: "borsuk/review-p7".to_string(),
            },
            Action::Stack {
                repo: "borsuk".to_string(),
                pr: 7,
                on: true,
            },
            Action::Go {
                repo: "borsuk".to_string(),
                prs: vec![7, 9],
            },
            Action::Policy {
                repo: "borsuk".to_string(),
                policy: ReleasePolicy::Interval { minutes: 30 },
            },
            Action::Limit {
                stage: Stage::Implement,
                limit: 5,
            },
            Action::Lane {
                stage: Stage::Implement,
                repo: "borsuk".to_string(),
                slots: 1,
            },
            Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Refine,
                },
                paused: true,
            },
            Action::TicketCreate {
                repo: "qubitsok".to_string(),
            },
            Action::Reconcile { repo: None },
            Action::Stop,
        ]
    }

    #[test]
    fn every_action_round_trips_through_one_json_line() {
        for action in every_action() {
            let text = serde_json::to_string(&action).unwrap();
            assert!(!text.contains('\n'), "a wire line must not hold a newline");
            assert_eq!(
                serde_json::from_str::<Action>(&text).unwrap(),
                action,
                "line: {text}"
            );
        }
    }

    #[test]
    fn the_wire_shapes_use_the_documented_tags() {
        let text = serde_json::to_string(&Action::Stop).unwrap();
        assert_eq!(text, "{\"action\":\"stop\"}");

        let text = serde_json::to_string(&Action::Limit {
            stage: Stage::Refine,
            limit: 2,
        })
        .unwrap();
        assert_eq!(
            text,
            "{\"action\":\"limit\",\"stage\":\"refine\",\"limit\":2}"
        );

        let scope = PauseScope::Lane {
            stage: Stage::Release,
            repo: "borsuk".to_string(),
        };
        let text = serde_json::to_string(&scope).unwrap();
        assert_eq!(
            text,
            "{\"scope\":\"lane\",\"stage\":\"release\",\"repo\":\"borsuk\"}"
        );
    }

    #[test]
    fn pause_scope_json_accepts_one_lane_and_one_task() {
        let lane = serde_json::from_str::<PauseScope>(
            "{\"scope\":\"lane\",\"stage\":\"implement\",\"repo\":\"borsuk\"}",
        );
        let task = serde_json::from_str::<PauseScope>(
            "{\"scope\":\"task\",\"task\":\"borsuk/implement-i44\"}",
        );

        assert!(lane.is_ok(), "a repository lane must be a pause scope");
        assert!(task.is_ok(), "one task must be a pause scope");
    }

    #[test]
    fn paused_view_json_keeps_one_specific_override() {
        let input = serde_json::json!({
            "global": true,
            "overrides": [{
                "scope": {"scope": "task", "task": "borsuk/implement-i44"},
                "paused": false
            }]
        });

        let view: PausedView = serde_json::from_value(input.clone()).unwrap();
        let output = serde_json::to_value(view).unwrap();

        assert_eq!(output["overrides"], input["overrides"]);
    }

    #[test]
    fn the_pause_protocol_rejects_removed_repository_fields() {
        let repository_scope =
            serde_json::from_str::<PauseScope>("{\"scope\":\"repo\",\"repo\":\"borsuk\"}");
        let old_view = serde_json::from_value::<PausedView>(serde_json::json!({
            "global": false,
            "stages": ["refine"],
            "repos": ["borsuk"],
            "overrides": []
        }));

        assert!(repository_scope.is_err(), "a repository is not one lane");
        assert!(
            old_view.is_err(),
            "removed pause fields must fail to decode"
        );
    }

    #[test]
    fn a_state_view_round_trips_through_json() {
        let view = StateView {
            decisions: vec![crate::decisions::Decision::release_gate(
                "borsuk",
                vec![7, 9],
                1_000,
            )],
            ..sample_view(1)
        };
        let push = Push::State(view.clone());
        let text = serde_json::to_string(&push).unwrap();
        assert!(text.contains("\"type\":\"state\""), "line: {text}");
        assert_eq!(serde_json::from_str::<Push>(&text).unwrap(), push);
    }

    #[test]
    fn a_train_batch_survives_json_and_is_required() {
        let train = TrainView {
            repo: "borsuk".to_string(),
            queue: vec![9],
            stacked: vec![7],
            batch: vec![7],
            policy: ReleasePolicy::Manual,
            next_fire_ms: None,
            in_flight: Some("borsuk/release-p7".to_string()),
        };
        let text = serde_json::to_string(&train).unwrap();
        assert_eq!(serde_json::from_str::<TrainView>(&text).unwrap(), train);

        let mut old_value = serde_json::to_value(&train).unwrap();
        old_value.as_object_mut().unwrap().remove("batch");
        assert!(serde_json::from_value::<TrainView>(old_value).is_err());
    }

    /// One input mode of every variant.
    fn every_input_mode() -> Vec<InputMode> {
        vec![
            InputMode::Live,
            InputMode::Resume,
            InputMode::NextTurn,
            InputMode::Follow,
            InputMode::Closed {
                reason: "The task holds no session to continue.".to_string(),
            },
        ]
    }

    #[test]
    fn every_input_mode_round_trips_through_json_with_the_documented_tags() {
        for mode in every_input_mode() {
            let text = serde_json::to_string(&mode).unwrap();
            assert!(text.contains("\"mode\":"), "line: {text}");
            assert_eq!(serde_json::from_str::<InputMode>(&text).unwrap(), mode);
        }
        assert_eq!(
            serde_json::to_string(&InputMode::NextTurn).unwrap(),
            "{\"mode\":\"next_turn\"}"
        );
        assert_eq!(
            serde_json::to_string(&InputMode::Closed {
                reason: "no session".to_string()
            })
            .unwrap(),
            "{\"mode\":\"closed\",\"reason\":\"no session\"}"
        );
    }

    #[test]
    fn a_task_view_keeps_the_input_and_the_queued_count_through_json() {
        let mut view = sample_view(1);
        view.tasks.push(TaskView {
            id: "borsuk/implement-i142".to_string(),
            repo: "borsuk".to_string(),
            stage: Stage::Implement,
            kind: ItemKind::Issue,
            number: 142,
            state: TaskState::Running,
            attempt: 1,
            log_path: PathBuf::from("/state/logs/borsuk__implement-i142.jsonl"),
            input: InputMode::NextTurn,
            queued_messages: 2,
        });
        let text = serde_json::to_string(&Push::State(view.clone())).unwrap();
        assert!(
            text.contains("\"input\":{\"mode\":\"next_turn\"}"),
            "line: {text}"
        );
        assert!(text.contains("\"queued_messages\":2"), "line: {text}");
        let push = serde_json::from_str::<Push>(&text).unwrap();
        assert_eq!(push, Push::State(view));
    }

    #[test]
    fn the_view_build_rejects_a_missing_input_mode() {
        let config = Config::parse(&config_text()).unwrap();
        let limits = Limits::from_config(&config);
        let paused = Paused::default();
        let mut table = TaskTable::new();
        table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                142,
                PathBuf::from("/state/logs/borsuk__refine-i142.jsonl"),
                1_000,
            )
            .unwrap();
        let decisions = Decisions::new();
        let trains = BTreeMap::new();
        let policies = BTreeMap::new();
        let input_modes = BTreeMap::new();

        let error = StateInput {
            config: &config,
            limits: &limits,
            paused: &paused,
            table: &table,
            decisions: &decisions,
            trains: &trains,
            policies: &policies,
            input_modes: &input_modes,
            now_ms: 0,
        }
        .build()
        .unwrap_err();

        assert!(
            error.to_string().contains("refine-i142"),
            "message: {error}"
        );
        assert!(error.to_string().contains("input mode"), "message: {error}");
    }

    #[test]
    fn broadcast_skips_the_initial_push_that_a_subscriber_already_received() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (tx, rx) = sync_channel(SUBSCRIBER_CAPACITY);
        let initial = Arc::new(Push::State(sample_view(1)));
        let mut registry = vec![Subscriber {
            id: 1,
            tx,
            stream,
            last: Some(Arc::clone(&initial)),
        }];

        broadcast(&mut registry, &initial);

        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn bind_creates_the_socket_with_mode_0600() {
        let dir = TempDir::new("mode");
        let path = dir.path().join("daemon.sock");

        let (_server, _rx) = Server::bind(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn bind_creates_the_socket_directory() {
        let dir = TempDir::new("mkdir");
        let path = dir.path().join("aif").join("nested").join("daemon.sock");

        let (_server, _rx) = Server::bind(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let dir = TempDir::new("stale");
        let path = dir.path().join("daemon.sock");
        // Dropping a listener closes it without removing its socket path.
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        // A parallel child can briefly inherit the listener before it
        // starts its program. Wait for this bounded test race to end.
        let deadline = Instant::now() + Duration::from_secs(1);
        while UnixStream::connect(&path).is_ok() {
            assert!(Instant::now() < deadline, "the leftover socket stayed live");
            thread::sleep(Duration::from_millis(1));
        }

        let (server, _rx) = Server::bind(&path).unwrap();
        server.publish(sample_view(1));

        let client = Client::connect(&path).unwrap();
        client.set_read_timeout(Duration::from_secs(5)).unwrap();
        let push = client.pushes().unwrap().next().unwrap().unwrap();
        assert_eq!(push, Push::State(sample_view(1)));
    }

    #[test]
    fn bind_refuses_to_replace_a_plain_file_at_the_socket_path() {
        let dir = TempDir::new("plain");
        let path = dir.path().join("daemon.sock");
        fs::write(&path, "not a socket").unwrap();

        let error = Server::bind(&path).unwrap_err();

        assert!(
            error.to_string().contains("not a socket"),
            "message: {error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a socket");
    }

    #[test]
    fn bind_refuses_a_path_with_a_live_daemon() {
        let dir = TempDir::new("live");
        let path = dir.path().join("daemon.sock");
        let (_server, _rx) = Server::bind(&path).unwrap();

        let error = Server::bind(&path).unwrap_err();

        assert!(
            error.to_string().contains("already listening"),
            "message: {error}"
        );
    }

    #[test]
    fn a_client_cannot_connect_to_a_missing_daemon() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("absent.sock");

        assert!(Client::connect(&path).is_err());
    }

    #[test]
    fn a_round_trip_connects_answers_and_pushes_again() {
        let dir = TempDir::new("round");
        let path = dir.path().join("daemon.sock");
        let (server, rx) = Server::bind(&path).unwrap();
        let mut first = sample_view(1);
        first.repos[0].alias = "borsuk".to_string();
        server.publish(first.clone());

        let mut client = Client::connect(&path).unwrap();
        client.set_read_timeout(Duration::from_secs(5)).unwrap();
        let mut second_client = Client::connect(&path).unwrap();
        second_client
            .set_read_timeout(Duration::from_secs(5))
            .unwrap();
        let mut pushes = client.pushes().unwrap();
        let mut second_pushes = second_client.pushes().unwrap();

        // Both clients receive the current state at once.
        assert_eq!(pushes.next().unwrap().unwrap(), Push::State(first.clone()));
        assert_eq!(
            second_pushes.next().unwrap().unwrap(),
            Push::State(first.clone())
        );

        // An action reaches the daemon receiver.
        let action = Action::Refine {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 142,
        };
        client.send(&action).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), action);

        // A stop action from the second client arrives too.
        second_client.send(&Action::Stop).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Action::Stop
        );

        // Apply the action as the daemon will. The next push carries its
        // visible queued-task effect to both clients.
        let mut second = first;
        second.stages[0].queued = 1;
        second.tasks.push(TaskView {
            id: "borsuk/refine-i142".to_string(),
            repo: "borsuk".to_string(),
            stage: Stage::Refine,
            kind: ItemKind::Issue,
            number: 142,
            state: TaskState::Queued,
            attempt: 1,
            log_path: PathBuf::from("/state/logs/borsuk__refine-i142.jsonl"),
            input: InputMode::NextTurn,
            queued_messages: 1,
        });
        server.publish(second.clone());
        assert_eq!(pushes.next().unwrap().unwrap(), Push::State(second.clone()));
        assert_eq!(
            second_pushes.next().unwrap().unwrap(),
            Push::State(second.clone())
        );
    }

    #[test]
    fn a_client_that_goes_away_does_not_harm_the_daemon() {
        let dir = TempDir::new("gone");
        let path = dir.path().join("daemon.sock");
        let (server, _rx) = Server::bind(&path).unwrap();
        server.publish(sample_view(1));
        let client = Client::connect(&path).unwrap();
        client.set_read_timeout(Duration::from_secs(5)).unwrap();
        let mut pushes = client.pushes().unwrap();
        assert_eq!(pushes.next().unwrap().unwrap(), Push::State(sample_view(1)));

        // The client goes away without a clean close from our side first.
        drop(client);
        drop(pushes);

        // The daemon keeps publishing; the subscriber list self-cleans.
        // The daemon attaches a connect before or after the publish below.
        // The first push is therefore the state current at attach time: view
        // 1 or view 2 itself. Read past an initial view 1 push until view 2
        // arrives. The push bound and the read timeout keep the loop finite,
        // and a daemon harmed by the dropped client never delivers view 2,
        // so the test still fails in that case.
        let survivor = Client::connect(&path).unwrap();
        survivor.set_read_timeout(Duration::from_secs(5)).unwrap();
        let mut survivor_pushes = survivor.pushes().unwrap();
        let second = sample_view(2);
        server.publish(second.clone());
        let mut saw_second = false;
        for _ in 0..4 {
            match survivor_pushes.next() {
                Some(Ok(Push::State(state))) if state == second => {
                    saw_second = true;
                    break;
                }
                // The initial push of the state at attach time. Skip it.
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("the survivor read failed: {error}"),
                None => panic!("the survivor stream ended before view 2"),
            }
        }
        assert!(saw_second, "the survivor never received view 2");
    }

    #[test]
    fn rapid_publishes_coalesce_into_few_pushes_and_the_last_one_wins() {
        let dir = TempDir::new("coalesce");
        let path = dir.path().join("daemon.sock");
        let (server, _rx) = Server::bind(&path).unwrap();

        let client = Client::connect(&path).unwrap();
        // A short timeout bounds the final quiet read of the drain loop.
        client.set_read_timeout(Duration::from_millis(300)).unwrap();
        let mut pushes = client.pushes().unwrap();

        const CHANGES: usize = 20;
        for label in 0..CHANGES {
            server.publish(sample_view(label));
            thread::sleep(Duration::from_millis(5));
        }

        // Collect pushes until the stream falls quiet for one window.
        let mut seen = Vec::new();
        for _ in 0..CHANGES {
            match pushes.next() {
                Some(Ok(push)) => seen.push(push),
                Some(Err(_)) | None => break,
            }
        }
        assert!(
            seen.len() <= 4,
            "20 rapid changes must produce at most four pushes, got {}",
            seen.len()
        );
        assert_eq!(
            seen.last(),
            Some(&Push::State(sample_view(CHANGES - 1))),
            "the last push must carry the newest state"
        );
    }

    /// A state view whose single push is far larger than a socket buffer.
    fn huge_view(label: usize) -> StateView {
        let mut view = sample_view(label);
        view.repos[0].owner_repo = "x".repeat(200_000);
        view
    }

    #[test]
    fn a_subscriber_that_never_reads_is_dropped_and_the_daemon_keeps_running() {
        let dir = TempDir::new("slow");
        let path = dir.path().join("daemon.sock");
        let (server, _rx) = Server::bind(&path).unwrap();

        // This client reads nothing after connecting.
        let stalled = Client::connect(&path).unwrap();
        // This client drains everything it is sent, on its own thread, so
        // its subscriber channel never fills during the publish phase.
        let reader = Client::connect(&path).unwrap();
        reader.set_read_timeout(Duration::from_secs(10)).unwrap();
        let reader_pushes = reader.pushes().unwrap();
        let (healthy_tx, healthy_rx) = channel::<Push>();
        thread::spawn(move || {
            for push in reader_pushes {
                match push {
                    Ok(push) => healthy_tx.send(push).unwrap(),
                    Err(_) => break,
                }
            }
        });

        // One push is about 200 KB, so a stalled reader wedges its writer
        // thread in the kernel buffer and then fills the 16-slot channel.
        const PUSHES: usize = 24;
        for label in 0..PUSHES {
            server.publish(huge_view(label));
            thread::sleep(Duration::from_millis(60));
        }
        // One more push: the stalled subscriber is gone by now and the
        // healthy client still receives the newest state.
        let final_view = huge_view(PUSHES);
        server.publish(final_view.clone());
        thread::sleep(Duration::from_millis(100));

        // The healthy client receives every coalesced push and the final
        // push, in order.
        let mut healthy = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match healthy_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(push) => healthy.push(push),
                Err(_) => break,
            }
        }
        assert!(
            !healthy.is_empty(),
            "the healthy client must keep receiving pushes"
        );
        assert_eq!(
            healthy.last(),
            Some(&Push::State(final_view)),
            "the daemon must keep serving a reading client"
        );

        // The stalled client saw fewer pushes than the healthy one and
        // never saw the final state.
        stalled
            .set_read_timeout(Duration::from_millis(500))
            .unwrap();
        let stalled_pushes = stalled.pushes().unwrap();
        let mut stalled_seen = 0;
        for push in stalled_pushes {
            match push {
                Ok(_) => stalled_seen += 1,
                Err(error) => {
                    let timed_out = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        )
                    });
                    assert!(!timed_out, "the dropped client stayed open: {error:#}");
                    break;
                }
            }
        }
        assert!(
            stalled_seen < healthy.len(),
            "the stalled client must be cut off: {stalled_seen} vs {}",
            healthy.len()
        );
    }

    #[test]
    fn a_server_drop_removes_the_socket_file_and_closes_clients() {
        let dir = TempDir::new("drop");
        let path = dir.path().join("daemon.sock");
        let (server, _rx) = Server::bind(&path).unwrap();
        server.publish(sample_view(1));
        let client = Client::connect(&path).unwrap();
        client.set_read_timeout(Duration::from_millis(500)).unwrap();
        let mut pushes = client.pushes().unwrap();
        assert_eq!(pushes.next().unwrap().unwrap(), Push::State(sample_view(1)));

        drop(server);

        assert!(!path.exists(), "the socket file must be removed on drop");
        assert!(
            pushes.next().is_none(),
            "the server must close the client stream cleanly"
        );
    }

    #[test]
    fn a_server_drop_preserves_a_replacement_at_the_socket_path() {
        let dir = TempDir::new("drop-replacement");
        let path = dir.path().join("daemon.sock");
        let (server, _rx) = Server::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();

        drop(server);

        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    }

    /// Parse text for a config with four stages and two repositories.
    fn config_text() -> String {
        let mut text = String::new();
        for stage in Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nrunner = \"runner\"\nlimit = 3\n"
            ));
        }
        text.push_str("[repo.borsuk]\npath = \"/tmp/b\"\nlanes = { implement = 1 }\n");
        text.push_str(
            "[repo.qubitsok]\npath = \"/tmp/q\"\nrelease = { policy = \"interval\", minutes = 30 }\n",
        );
        text
    }

    #[test]
    fn the_view_build_rejects_a_missing_ordered_task() {
        let config = Config::parse(&config_text()).unwrap();
        let limits = Limits::from_config(&config);
        let paused = Paused::default();
        let mut table = TaskTable::new();
        table.order.push("missing-task".to_string());
        let decisions = Decisions::new();
        let trains = BTreeMap::new();
        let policies = BTreeMap::new();

        let error = StateInput {
            config: &config,
            limits: &limits,
            paused: &paused,
            table: &table,
            decisions: &decisions,
            trains: &trains,
            policies: &policies,
            input_modes: &BTreeMap::new(),
            now_ms: 0,
        }
        .build()
        .unwrap_err();

        assert!(
            error.to_string().contains("missing-task"),
            "message: {error}"
        );
    }

    #[test]
    fn the_view_build_describes_every_merged_module() {
        let config = Config::parse(&config_text()).unwrap();
        let mut limits = Limits::from_config(&config);
        // A runtime limit override on implement: 3 in the file, 5 at runtime.
        limits.stage.insert(Stage::Implement, 5);

        let mut table = TaskTable::new();
        let log = PathBuf::from("/state/logs/borsuk__implement-i142.jsonl");
        let worker = table
            .upsert_queued("borsuk", Stage::Implement, ItemKind::Issue, 142, log, 1_000)
            .unwrap()
            .id
            .clone();
        table
            .transition(&worker, TaskState::Running, 2_000)
            .unwrap();
        table
            .upsert_queued(
                "qubitsok",
                Stage::Refine,
                ItemKind::Issue,
                7,
                PathBuf::from("/state/logs/qubitsok__refine-i7.jsonl"),
                3_000,
            )
            .unwrap();

        let mut decisions = Decisions::new();
        decisions
            .push(crate::decisions::Decision::stuck(
                &table.by_id[&worker],
                "three failures",
                4_000,
            ))
            .unwrap();

        let mut trains = BTreeMap::new();
        let mut borsuk = Train::new("borsuk");
        borsuk.enqueue(7);
        borsuk.enqueue(9);
        borsuk.stacked = vec![7];
        borsuk.last_fire_ms = Some(60_000);
        borsuk.fire(&[7], 90_000).unwrap();
        trains.insert("borsuk".to_string(), borsuk);

        let mut policies = BTreeMap::new();
        policies.insert(
            "borsuk".to_string(),
            ReleasePolicy::Interval { minutes: 30 },
        );

        let paused = Paused {
            global: false,
            stages: [(Stage::Review, true)].into_iter().collect(),
            lanes: [((Stage::Refine, "qubitsok".to_string()), true)]
                .into_iter()
                .collect(),
            tasks: BTreeMap::new(),
        };

        // The daemon decides the input mode of each task. The build only
        // looks the entries up.
        let mut input_modes = BTreeMap::new();
        input_modes.insert("borsuk/implement-i142".to_string(), InputMode::Live);
        input_modes.insert(
            "qubitsok/refine-i7".to_string(),
            InputMode::Closed {
                reason: "no session".to_string(),
            },
        );
        let view = StateInput {
            config: &config,
            limits: &limits,
            paused: &paused,
            table: &table,
            decisions: &decisions,
            trains: &trains,
            policies: &policies,
            input_modes: &input_modes,
            now_ms: 120_000,
        }
        .build()
        .unwrap();

        // Repositories come from the config in alias order.
        assert_eq!(
            view.repos,
            vec![
                RepoView {
                    alias: "borsuk".to_string(),
                    owner_repo: String::new(),
                },
                RepoView {
                    alias: "qubitsok".to_string(),
                    owner_repo: String::new(),
                },
            ]
        );

        // The stage row shows the runtime limit, the override flag, and the
        // running and queued counts.
        let implement = &view.stages[1];
        assert_eq!(implement.stage, Stage::Implement);
        assert_eq!(implement.limit, 5);
        assert!(
            implement.overridden,
            "the runtime limit differs from the file"
        );
        assert_eq!(implement.running, 1);
        assert_eq!(implement.queued, 0);
        let refine = &view.stages[0];
        assert!(!refine.overridden);
        assert_eq!(refine.running, 0);
        assert_eq!(refine.queued, 1);

        // The lane reservation of borsuk on implement appears.
        assert_eq!(
            view.lanes,
            vec![LaneView {
                stage: Stage::Implement,
                repo: "borsuk".to_string(),
                slots: 1,
            }]
        );

        // The tasks keep insertion order and carry the log path.
        assert_eq!(view.tasks.len(), 2);
        assert_eq!(view.tasks[0].id, "borsuk/implement-i142");
        assert_eq!(view.tasks[0].state, TaskState::Running);
        assert_eq!(view.tasks[0].attempt, 1);
        assert_eq!(
            view.tasks[0].log_path,
            PathBuf::from("/state/logs/borsuk__implement-i142.jsonl")
        );
        assert_eq!(view.tasks[1].id, "qubitsok/refine-i7");
        assert_eq!(view.tasks[1].state, TaskState::Queued);

        // Each task view carries its input mode. The daemon fills the
        // queued count from its pending chat queue after this build.
        assert_eq!(view.tasks[0].input, InputMode::Live);
        assert_eq!(view.tasks[0].queued_messages, 0);
        assert_eq!(
            view.tasks[1].input,
            InputMode::Closed {
                reason: "no session".to_string()
            }
        );
        assert_eq!(view.tasks[1].queued_messages, 0);

        // The open decisions ride along unchanged.
        assert_eq!(view.decisions.len(), 1);
        assert_eq!(view.decisions[0].id, "stuck:borsuk/implement-i142:1");

        // The train view carries the waiting queue and the full active batch.
        assert_eq!(view.trains.len(), 2, "one train view per repository");
        let borsuk_view = &view.trains[0];
        assert_eq!(borsuk_view.queue, vec![9]);
        assert_eq!(borsuk_view.stacked, vec![7]);
        assert_eq!(borsuk_view.batch, vec![7]);
        assert_eq!(borsuk_view.policy, ReleasePolicy::Interval { minutes: 30 });
        assert_eq!(borsuk_view.next_fire_ms, None);
        assert_eq!(borsuk_view.in_flight.as_deref(), Some("borsuk/release-p7"));
        // A repository without a train entry gets an empty view with the
        // config policy.
        assert_eq!(view.trains[1].repo, "qubitsok");
        assert!(view.trains[1].queue.is_empty());
        assert_eq!(
            view.trains[1].policy,
            ReleasePolicy::Interval { minutes: 30 }
        );
        assert_eq!(view.trains[1].next_fire_ms, None);

        // The paused flags round off the view.
        assert_eq!(
            view.paused,
            PausedView {
                global: false,
                overrides: vec![
                    PauseOverrideView {
                        scope: PauseScope::Stage {
                            stage: Stage::Review,
                        },
                        paused: true,
                    },
                    PauseOverrideView {
                        scope: PauseScope::Lane {
                            stage: Stage::Refine,
                            repo: "qubitsok".to_string(),
                        },
                        paused: true,
                    },
                ],
            }
        );
    }
}
