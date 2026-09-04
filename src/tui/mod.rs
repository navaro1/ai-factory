//! Holds the app shell and the terminal UI event loop.
//!
//! Three threads cooperate:
//!
//! - The key reader thread turns crossterm events into `Msg` values.
//! - The socket reader thread connects to the daemon, turns pushes into
//!   `Msg` values, and reconnects with `backoff_delay`.
//! - The main thread owns the `App` and blocks on the one channel. It draws
//!   one frame per message. The session view also draws when its file poll
//!   finds new log data.
//!
//! The shell draws the pipeline view itself and drives the session and
//! inbox views through their contracts: `show`, `on_redraw`, `draw`, and
//! `poll`, and `handle_key` for the session, and `observe`, `draw`, and
//! `handle_key` for the inbox.
//!
//! A view that holds the keyboard for text input keeps a typed `q` away
//! from the quit handler and takes a typed `!` as a character. The session
//! view holds the keyboard while its chat bar is focused and open; `esc`
//! or `tab` releases the focus, `h` and `l` then switch the live sessions,
//! and `i` or `enter` takes the focus back. A session bar that cannot take
//! text, and every other view, leaves the `1` through `5` view keys and
//! `?` to the shell. Every state without an open text input opens the
//! inbox on `!`, and the ctrl-q and ctrl-c quit chords work from every
//! view.

pub mod inbox;
pub mod markdown;
pub mod pipeline;
pub mod session;
pub mod settings;
pub mod theme;
pub mod tickets;
pub mod transcript;

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::catalog;
use crate::decisions::DecisionKind;
use crate::exec::RealExec;
use crate::model::{ItemKind, Stage};
use crate::sock::{Action, Client, Push, StateView, TaskView, TicketAction, WireProtocolMismatch};
use crate::tasks::TaskState;
use inbox::{ActionSink, Inbox};
use session::SessionView;
use settings::Settings;
use theme::THEME;
use tickets::Tickets;

/// The view the shell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum View {
    /// The pipeline view. This is the home view.
    #[default]
    Pipeline,
    /// The transcript and steering view of one task.
    Session,
    /// The decisions inbox.
    Inbox,
    /// All open GitHub issues.
    Tickets,
    /// The execution role settings editor.
    Settings,
}

/// What the operator has marked in the visible view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Selection {
    /// Nothing is marked.
    #[default]
    None,
    /// The visible row at this zero-based index.
    Row(usize),
}

/// One message the main loop acts on.
///
/// Every value is either an input event or a socket event. The main loop
/// draws one frame per message and never wakes up on its own.
#[derive(Debug)]
enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// The terminal changed its size.
    Resize,
    /// One state push from the daemon.
    State(StateView),
    /// Full data for one focused issue.
    TicketDetails(crate::sock::TicketDetails),
    /// The mention statuses of one focused issue.
    TicketMentions(crate::sock::TicketMentions),
    /// One repository label catalog.
    TicketLabels(crate::sock::TicketLabels),
    /// One ticket mutation result.
    TicketResult(crate::sock::TicketResult),
    /// One fetched question for one `NeedsHuman` detail screen.
    Ask(crate::sock::AskView),
    /// One settings save or reload result.
    SettingsResult(crate::sock::SettingsResult),
    /// The parsed `opencode models` probe result of this shell start.
    HarnessModels(Result<Vec<String>, String>),
    /// The socket reader reached the daemon.
    Connected,
    /// The daemon went away, with the reason for the banner.
    Disconnected(String),
    /// A permanent background failure that must leave the terminal UI.
    Fatal(String),
    /// The key reader died, so the UI cannot see input anymore.
    Input(String),
}

/// The task the shell chose for the session view.
///
/// The daemon creates a refine task some time after an `r` or `n` key. The
/// choice waits in the app until a state push carries the matching task.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    /// A refine task of one item.
    Refine {
        /// The repository alias.
        repo: String,
        /// The item kind.
        kind: ItemKind,
        /// The item number.
        number: u64,
    },
    /// The ticket-creation task of one repository.
    Create {
        /// The repository alias.
        repo: String,
    },
}

impl Wanted {
    /// True when `task` is the task this choice waits for.
    ///
    /// A ticket creation turns into the refine task of item zero, which is
    /// how the daemon names its ticket-creation session.
    fn matches(&self, task: &TaskView) -> bool {
        let refine = task.stage == Stage::Refine;
        match self {
            Wanted::Refine { repo, kind, number } => {
                refine && task.repo == *repo && task.kind == *kind && task.number == *number
            }
            Wanted::Create { repo } => {
                refine && task.repo == *repo && task.kind == ItemKind::Issue && task.number == 0
            }
        }
    }
}

/// One action that waits for the operator to confirm it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    /// Abort one task.
    Abort {
        /// The task id.
        task: String,
    },
    /// Fire one release train with the stacked pull requests.
    Go {
        /// The repository alias.
        repo: String,
        /// The pull request numbers in the batch.
        prs: Vec<u64>,
    },
    /// Add the `to-refine` label to one ticket.
    Refine {
        /// The repository alias.
        repo: String,
        /// The issue number.
        number: u64,
    },
}

impl Confirm {
    /// Send the confirmed action and toast what was sent.
    fn send(self, app: &mut App, sink: &mut impl ActionSink) {
        match self {
            Confirm::Abort { task } => {
                emit(
                    app,
                    sink,
                    Action::Abort { task: task.clone() },
                    format!("sent abort {task}"),
                );
            }
            Confirm::Go { repo, prs } => {
                let current = app.state.as_ref().is_some_and(|state| {
                    state.trains.iter().any(|train| {
                        train.repo == repo
                            && train.in_flight.is_none()
                            && train.batch.is_empty()
                            && train.stacked == prs
                    })
                });
                if !current {
                    app.show_toast("release batch changed; press g again");
                    return;
                }
                emit(
                    app,
                    sink,
                    Action::Go {
                        repo: repo.clone(),
                        prs: prs.clone(),
                    },
                    format!("sent release {repo} {}", pr_text(&prs)),
                );
            }
            Confirm::Refine { repo, number } => {
                emit(
                    app,
                    sink,
                    Action::Ticket(TicketAction::ToggleLabel {
                        request: tickets::request_code(),
                        repo: repo.clone(),
                        number,
                        label: crate::gates::TO_REFINE.to_string(),
                        on: true,
                    }),
                    format!("sent to-refine {repo} #{number}"),
                );
            }
        }
    }
}

/// How long a toast stays on screen.
const TOAST_TIME: Duration = Duration::from_secs(3);

/// The model the main loop owns.
#[derive(Debug, Default)]
struct App {
    /// The last state the daemon pushed. None before the first push.
    state: Option<StateView>,
    /// True while the socket reader holds a connection.
    connected: bool,
    /// The view the shell shows.
    view: View,
    /// The row the operator marked.
    selection: Selection,
    /// The toast text and the instant it expires at.
    toast: Option<(String, Instant)>,
    /// True while the help overlay covers the view.
    help: bool,
    /// The reason the daemon went away, for the banner.
    disconnect: Option<String>,
    /// The session view with its transcript and input bar.
    session: SessionView,
    /// The decisions inbox.
    inbox: Inbox,
    /// The open issue list and its nested views.
    tickets: Tickets,
    /// The execution role settings editor.
    settings: Settings,
    /// The task id the session view follows.
    session_task: Option<String>,
    /// The task the shell still waits for, from `r` or `n`.
    wanted: Option<Wanted>,
    /// The action that waits for the operator to confirm it.
    confirm: Option<Confirm>,
    /// The body rectangle of the last drawn frame.
    body: Rect,
}

impl App {
    /// Apply one state push.
    ///
    /// The push proves the connection, clamps the selection to the new row
    /// count, and drops an expired toast. It feeds the inbox and follows
    /// the session task, so both views stay in step with the daemon.
    /// Send one mention-status request for a pull request detail whose
    /// description just became visible, and one ask request for a
    /// `NeedsHuman` detail whose question just became visible.
    fn send_pending_pr_mention(&mut self, sink: &mut impl ActionSink) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Some(action) = self.inbox.take_pending_pr_mention(state) {
            sink.send_action(action);
        }
        if let Some(action) = self.inbox.take_pending_ask(state) {
            sink.send_action(action);
        }
    }

    fn apply_state(&mut self, view: StateView) {
        let count = pipeline::rows(&view).len();
        let keyed_selection = self
            .state
            .as_ref()
            .map(|state| pipeline::selection_key(state, self.selection));
        let next_selection = match keyed_selection {
            Some(Some(key)) => pipeline::selection_for_key(&view, &key),
            Some(None) => Selection::None,
            None => match self.selection {
                Selection::Row(index) if count > 0 => Selection::Row(index.min(count - 1)),
                _ => Selection::None,
            },
        };
        self.inbox.observe(&view);
        self.tickets.observe_state(&view);
        self.session.set_tabs(live_session_ids(&view));
        let wanted_task = self.wanted.as_ref().and_then(|wanted| {
            view.tasks
                .iter()
                .rfind(|task| wanted.matches(task))
                .cloned()
        });
        let shown_task = match &wanted_task {
            Some(task) => Some(task.clone()),
            None => self
                .session_task
                .as_deref()
                .and_then(|id| view.tasks.iter().find(|task| task.id == id).cloned()),
        };
        self.state = Some(view);
        self.connected = true;
        self.disconnect = None;
        self.selection = next_selection;
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.toast = None;
        }
        if let Some(task) = wanted_task {
            self.session_task = Some(task.id.clone());
            self.wanted = None;
            self.session.show(&task);
        } else if let Some(task) = shown_task {
            self.session.show(&task);
        }
    }

    /// Mark the daemon as gone and remember the reason.
    fn mark_disconnected(&mut self, reason: String) {
        self.connected = false;
        self.disconnect = Some(reason);
        self.tickets.delivery_failed(None);
        self.settings.delivery_failed(None);
    }

    /// The toast text while it is still fresh.
    fn visible_toast(&self) -> Option<&str> {
        let (text, until) = self.toast.as_ref()?;
        (Instant::now() < *until).then_some(text.as_str())
    }

    /// Toast `text` for one toast period.
    fn show_toast(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now() + TOAST_TIME));
    }

    /// True while a view holds the keyboard for text input.
    ///
    /// The session view owns it while its chat bar holds the focus; the
    /// shell releases the focus with `esc` or `tab` and takes it back with
    /// `i` or `enter`. The inbox owns it while a reason input is open.
    fn text_focus(&self) -> bool {
        match self.view {
            View::Session => self.session.chat_focus(),
            View::Inbox => self.inbox.typing(),
            View::Tickets => self.tickets.typing(),
            View::Settings => self.settings.typing(),
            View::Pipeline => false,
        }
    }

    /// True when a typed character lands in an open text input now.
    ///
    /// The session bar takes text only while it is focused and open. A
    /// closed bar and a bar with no task swallow the letters, so the
    /// shell keeps the `!` inbox key there. Every other view takes text
    /// while its own input is open.
    fn types_text(&self) -> bool {
        if matches!(self.view, View::Session) && !self.session.input_enabled() {
            return false;
        }
        self.text_focus()
    }

    /// Enter the session view and give the chat bar the focus.
    fn enter_session(&mut self) {
        self.view = View::Session;
        self.session.set_chat_focus(true);
    }

    /// Show the previous or next live session, in state-push order.
    ///
    /// The step wraps at the ends. With no live session the call does
    /// nothing. With a shown session that is not live, `1` shows the
    /// first live session and `-1` the last.
    fn cycle_session(&mut self, delta: isize) {
        let live = self
            .state
            .as_ref()
            .map(live_session_ids)
            .unwrap_or_default();
        if live.is_empty() {
            return;
        }
        let next = match self
            .session_task
            .as_deref()
            .and_then(|id| live.iter().position(|entry| entry == id))
        {
            Some(position) => {
                let len = live.len() as isize;
                (position as isize + delta).rem_euclid(len) as usize
            }
            None => {
                if delta < 0 {
                    live.len() - 1
                } else {
                    0
                }
            }
        };
        self.session_task = Some(live[next].clone());
        self.wanted = None;
        self.show_session_task();
    }

    /// The visible transcript height of the session view.
    ///
    /// The session view draws a one-row task header and a three-row input
    /// bar around the transcript. The last drawn frame supplies the height.
    fn session_page(&self) -> u16 {
        self.body.height.saturating_sub(4).max(1)
    }

    /// Show the session task from the current state, if it is still there.
    fn show_session_task(&mut self) {
        let Some(id) = self.session_task.clone() else {
            return;
        };
        let Some(task) = self
            .state
            .as_ref()
            .and_then(|state| state.tasks.iter().find(|task| task.id == id).cloned())
        else {
            return;
        };
        self.session.show(&task);
    }

    /// Enter the inbox view and select its oldest row, as `!` promises.
    fn open_inbox_oldest(&mut self) {
        self.view = View::Inbox;
        if let Some(state) = self.state.as_ref() {
            self.inbox.select_oldest(state);
        }
    }

    /// Apply one key press. Returns false when the app wants to quit.
    ///
    /// The quit chords work in every state. An open text input takes the
    /// typed `q` and `!` as characters; every other state quits on `q`
    /// and opens the inbox on `!`.
    fn handle_key(&mut self, key: KeyEvent, sink: &mut impl ActionSink) -> bool {
        if quit_chord(key) {
            return false;
        }
        // An overlay covers the text input below it, so the global key
        // still works while the overlay is open.
        if inbox_key(key) && (self.help || self.confirm.is_some() || !self.types_text()) {
            self.confirm = None;
            self.help = false;
            self.open_inbox_oldest();
            return true;
        }
        if self.confirm.is_some() {
            self.confirm_key(key, sink);
            return true;
        }
        if self.help {
            if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                self.help = false;
            }
            return true;
        }
        if key.code == KeyCode::Char('q') && !self.text_focus() {
            return false;
        }
        match self.view {
            View::Session => {
                let focused = self.session.chat_focus();
                // A bar that cannot take text holds no keyboard. The
                // shell keeps its view keys alive: no task, a closed
                // input, or a released focus leaves 1 through 5 and ?
                // free for view switching.
                if !focused || !self.session.input_enabled() {
                    match key.code {
                        KeyCode::Char('1') => {
                            self.view = View::Pipeline;
                            return true;
                        }
                        KeyCode::Char('2') => {
                            self.enter_session();
                            return true;
                        }
                        KeyCode::Char('3') => {
                            self.view = View::Inbox;
                            return true;
                        }
                        KeyCode::Char('4') => {
                            self.view = View::Tickets;
                            return true;
                        }
                        KeyCode::Char('5') => {
                            self.view = View::Settings;
                            return true;
                        }
                        KeyCode::Char('?') => {
                            self.help = true;
                            return true;
                        }
                        _ => {}
                    }
                }
                match key.code {
                    KeyCode::Esc if focused => self.session.set_chat_focus(false),
                    KeyCode::Esc => self.view = View::Pipeline,
                    KeyCode::Tab => self.session.set_chat_focus(!focused),
                    KeyCode::Char('h') | KeyCode::Left if !focused => self.cycle_session(-1),
                    KeyCode::Char('l') | KeyCode::Right if !focused => self.cycle_session(1),
                    KeyCode::Char('i') | KeyCode::Enter if !focused => {
                        self.session.set_chat_focus(true);
                    }
                    _ => {
                        if let Some(action) = self.session.handle_key(key, self.session_page()) {
                            let text = match &action {
                                Action::Chat { task, .. } => format!("sent chat {task}"),
                                Action::Abort { task } => format!("sent abort {task}"),
                                _ => "sent".to_string(),
                            };
                            emit(self, sink, action, text);
                        }
                    }
                }
            }
            View::Inbox if self.inbox.typing() => {
                self.inbox_dispatch(key, sink);
            }
            View::Inbox if self.inbox.detail_open() => {
                self.inbox_dispatch(key, sink);
            }
            View::Inbox => {
                if digit_key(key) && self.inbox_row_owns(key) {
                    self.inbox_dispatch(key, sink);
                    return true;
                }
                match key.code {
                    KeyCode::Char('1') => self.view = View::Pipeline,
                    KeyCode::Char('2') => self.enter_session(),
                    KeyCode::Char('3') => {}
                    KeyCode::Char('4') => self.view = View::Tickets,
                    KeyCode::Char('5') => self.view = View::Settings,
                    KeyCode::Char('?') => self.help = true,
                    KeyCode::Esc => self.view = View::Pipeline,
                    _ => {
                        self.inbox_dispatch(key, sink);
                    }
                }
            }
            View::Pipeline => match key.code {
                KeyCode::Char('1') => {}
                KeyCode::Char('2') => self.enter_session(),
                KeyCode::Char('3') => self.view = View::Inbox,
                KeyCode::Char('4') => self.view = View::Tickets,
                KeyCode::Char('5') => self.view = View::Settings,
                KeyCode::Char('?') => self.help = true,
                KeyCode::Char('j') | KeyCode::Down => pipeline::move_selection(self, 1),
                KeyCode::Char('k') | KeyCode::Up => pipeline::move_selection(self, -1),
                KeyCode::Char('h') | KeyCode::Left => pipeline::move_horizontal(self, -1),
                KeyCode::Char('l') | KeyCode::Right => pipeline::move_horizontal(self, 1),
                _ => pipeline::handle_key(self, key, sink),
            },
            View::Tickets => {
                if !self.tickets.typing() {
                    match key.code {
                        KeyCode::Char('1') => {
                            self.view = View::Pipeline;
                            return true;
                        }
                        KeyCode::Char('2') => {
                            self.enter_session();
                            return true;
                        }
                        KeyCode::Char('3') => {
                            self.view = View::Inbox;
                            return true;
                        }
                        KeyCode::Char('4') => return true,
                        KeyCode::Char('5') => {
                            self.view = View::Settings;
                            return true;
                        }
                        KeyCode::Char('?') => {
                            self.help = true;
                            return true;
                        }
                        _ => {}
                    }
                }
                if key.code == KeyCode::Char('m') && self.tickets.focus_plain() {
                    if let Some((repo, number)) = self.tickets.focus_key() {
                        if self.tickets.focus_has_label(crate::gates::TO_REFINE) {
                            self.show_toast("the ticket already has to-refine");
                        } else {
                            self.confirm = Some(Confirm::Refine { repo, number });
                        }
                    }
                    return true;
                }
                let nested = self.tickets.focus_open() || self.tickets.typing();
                if let Some(action) = self
                    .state
                    .as_ref()
                    .and_then(|state| self.tickets.handle_key(state, key))
                {
                    let copy = action.clone();
                    if !emit(self, sink, action, "sent ticket request".to_string()) {
                        self.tickets.delivery_failed(Some(&copy));
                    }
                }
                if key.code == KeyCode::Esc && !nested {
                    self.view = View::Pipeline;
                }
            }
            View::Settings => {
                if !self.settings.typing() {
                    match key.code {
                        KeyCode::Char('1') => {
                            self.view = View::Pipeline;
                            return true;
                        }
                        KeyCode::Char('2') => {
                            self.enter_session();
                            return true;
                        }
                        KeyCode::Char('3') => {
                            self.view = View::Inbox;
                            return true;
                        }
                        KeyCode::Char('4') => {
                            self.view = View::Tickets;
                            return true;
                        }
                        KeyCode::Char('5') => return true,
                        KeyCode::Char('?') => {
                            self.help = true;
                            return true;
                        }
                        _ => {}
                    }
                }
                if let Some(action) = self
                    .state
                    .as_ref()
                    .and_then(|state| self.settings.handle_key(state, key))
                {
                    let copy = action.clone();
                    if !emit(self, sink, action, "sent settings request".to_string()) {
                        self.settings.delivery_failed(Some(&copy));
                    }
                }
            }
        }
        true
    }

    /// Apply one key to the confirmation prompt.
    ///
    /// `y` sends the confirmed action. Escape cancels it. Every other key
    /// waits, so the prompt cannot be dismissed by accident.
    fn confirm_key(&mut self, key: KeyEvent, sink: &mut impl ActionSink) {
        match key.code {
            KeyCode::Char('y') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
                if let Some(confirm) = self.confirm.take() {
                    confirm.send(self, sink);
                }
            }
            KeyCode::Esc => self.confirm = None,
            _ => {}
        }
    }

    /// True when the kind of the selected inbox row consumes this key, so
    /// it must not reach the global handler.
    ///
    /// A `Question` row numbers its options with the digits, and a
    /// `ReleaseGate` row toggles pull requests with them.
    fn inbox_row_owns(&self, key: KeyEvent) -> bool {
        if !digit_key(key) {
            return false;
        }
        let Some(state) = self.state.as_ref() else {
            return false;
        };
        let Some(id) = self.inbox.selected_id() else {
            return false;
        };
        let Some(decision) = state.decisions.iter().find(|decision| decision.id == id) else {
            return false;
        };
        matches!(
            decision.kind,
            DecisionKind::Question { .. } | DecisionKind::ReleaseGate { .. }
        )
    }

    /// Apply one key to the inbox and report whether an action crossed.
    ///
    /// A sent action toasts. An open-session outcome switches to the
    /// session view of that task. An open-ticket outcome switches to the
    /// Tickets view focus of one issue.
    fn inbox_dispatch(&mut self, key: KeyEvent, sink: &mut impl ActionSink) -> bool {
        let Some(state) = self.state.as_ref() else {
            return false;
        };
        let mut counted = CountingSink {
            inner: sink,
            sent: 0,
        };
        let outcome = self.inbox.handle_key(state, key, &mut counted);
        let sent = counted.sent > 0;
        if sent {
            self.show_toast("sent");
        }
        match outcome {
            inbox::InboxOutcome::None => {}
            inbox::InboxOutcome::OpenSession(task) => {
                self.session_task = Some(task);
                self.wanted = None;
                self.enter_session();
                self.show_session_task();
            }
            inbox::InboxOutcome::OpenTicket { repo, number } => {
                self.view = View::Tickets;
                if let Some(action) = self.tickets.open(&repo, number) {
                    emit(self, sink, action, "sent ticket request".to_string());
                }
            }
        }
        sent
    }
}

/// True when the key combination quits the UI from anywhere.
fn quit_chord(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
}

/// True when this key opens the inbox from any view.
fn inbox_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('!')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// True when the key is one of the digit keys 1 through 9.
fn digit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('1'..='9'))
}

/// The ids of the live sessions, in state-push order.
///
/// A live session runs or waits for an answer; a queued task has no
/// session yet and a terminal task has stopped.
fn live_session_ids(state: &StateView) -> Vec<String> {
    state
        .tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Running | TaskState::AwaitingUser))
        .map(|task| task.id.clone())
        .collect()
}

/// Send one action to the sink and toast what was sent.
fn emit(app: &mut App, sink: &mut impl ActionSink, action: Action, toast: String) -> bool {
    let sent = sink.send_action(action);
    if sent {
        app.show_toast(&toast);
    }
    sent
}

/// The pull request numbers as `#3 #7`.
fn pr_text(prs: &[u64]) -> String {
    prs.iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An action sink that counts the actions that cross it.
struct CountingSink<'a, S: ActionSink> {
    /// The sink the actions go to.
    inner: &'a mut S,
    /// How many actions crossed.
    sent: usize,
}

impl<S: ActionSink> ActionSink for CountingSink<'_, S> {
    fn send_action(&mut self, action: Action) -> bool {
        let sent = self.inner.send_action(action);
        self.sent += usize::from(sent);
        sent
    }
}

/// The wait before the next reconnect try.
///
/// The wait starts at one second and doubles per failed attempt, capped at
/// ten seconds.
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(4);
    Duration::from_secs(1u64 << shift).min(Duration::from_secs(10))
}

/// The drawing surface the main loop paints on.
///
/// The trait exists so tests can count draws without a real terminal.
trait Surface {
    /// Draw one frame of the app.
    fn draw(&mut self, app: &mut App) -> Result<()>;
}

/// The drawing surface over the real terminal.
struct RealTerminal {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl Surface for RealTerminal {
    fn draw(&mut self, app: &mut App) -> Result<()> {
        let mut render_result = Ok(());
        let terminal_result = self
            .terminal
            .draw(|frame| render_result = render(frame, app))
            .map(|_| ())
            .context("cannot draw a frame");
        finish_draw(terminal_result, render_result)
    }
}

/// Combine the terminal and render results without hiding either error.
fn finish_draw(terminal: Result<()>, render: Result<()>) -> Result<()> {
    match (terminal, render) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(terminal), Err(render)) => Err(anyhow!("{terminal:#}; {render:#}")),
    }
}

/// The action sink over the daemon socket.
///
/// The link opens the socket on the first send and reopens it once when
/// the daemon went away. A send that still fails is reported on stderr.
struct DaemonLink {
    /// The path of the daemon socket.
    socket: PathBuf,
    /// The open client, if any.
    client: Option<Client>,
}

impl ActionSink for DaemonLink {
    fn send_action(&mut self, action: Action) -> bool {
        if self.client.is_none() {
            self.client = self.connect();
        }
        let Some(client) = self.client.as_mut() else {
            return false;
        };
        if client.send(&action).is_ok() {
            return true;
        }
        // The daemon went away. One fresh connection gets one new try.
        self.client = self.connect();
        if let Some(client) = self.client.as_mut() {
            if let Err(error) = client.send(&action) {
                eprintln!("tui: cannot send the action to the daemon: {error:#}");
                return false;
            }
            return true;
        }
        false
    }
}

impl DaemonLink {
    /// Open the daemon socket, and report a failure on stderr.
    fn connect(&mut self) -> Option<Client> {
        match Client::connect(&self.socket) {
            Ok(client) => Some(client),
            Err(error) => {
                eprintln!("tui: cannot connect to the daemon: {error:#}");
                None
            }
        }
    }
}

/// Owns raw mode and the alternate screen. Restores the terminal on drop.
struct RawMode;

impl RawMode {
    /// Switch the terminal into raw mode and the alternate screen.
    fn enable() -> Result<RawMode> {
        enable_terminal_with(
            enable_raw_mode,
            || execute!(stdout(), EnterAlternateScreen),
            restore_terminal,
        )?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Err(error) = restore_terminal() {
            eprintln!("cannot restore the terminal: {error:#}");
        }
    }
}

/// Enable terminal modes and restore the terminal if screen entry fails.
fn enable_terminal_with(
    enable: impl FnOnce() -> std::io::Result<()>,
    enter: impl FnOnce() -> std::io::Result<()>,
    restore: impl FnOnce() -> Result<()>,
) -> Result<()> {
    enable().context("cannot enable raw mode")?;
    if let Err(enter_error) = enter() {
        let enter_error = anyhow!("cannot enter the alternate screen: {enter_error}");
        return match restore() {
            Ok(()) => Err(enter_error),
            Err(restore_error) => Err(anyhow!(
                "{enter_error:#}; cannot restore the terminal: {restore_error:#}"
            )),
        };
    }
    Ok(())
}

/// Restore the real terminal after the terminal UI stops.
fn restore_terminal() -> Result<()> {
    restore_terminal_with(
        || execute!(stdout(), LeaveAlternateScreen),
        disable_raw_mode,
    )
}

/// Attempt both terminal restore steps and report all failures.
fn restore_terminal_with(
    leave: impl FnOnce() -> std::io::Result<()>,
    disable: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    let leave_error = leave().err();
    let disable_error = disable().err();
    match (leave_error, disable_error) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(anyhow!("cannot leave the alternate screen: {error}")),
        (None, Some(error)) => Err(anyhow!("cannot disable raw mode: {error}")),
        (Some(leave_error), Some(disable_error)) => Err(anyhow!(
            "cannot leave the alternate screen: {leave_error}; cannot disable raw mode: {disable_error}"
        )),
    }
}

/// Run the terminal UI against the daemon socket at `path`.
///
/// The call blocks until the operator quits. It restores the terminal
/// before it returns, also on error.
pub fn run(socket: &Path) -> Result<()> {
    let _raw = RawMode::enable()?;
    let backend = CrosstermBackend::new(stdout());
    let mut surface = RealTerminal {
        terminal: Terminal::new(backend).context("cannot create the terminal")?,
    };
    let (tx, rx) = channel();
    spawn_model_probe(tx.clone());
    spawn_key_thread(tx.clone());
    spawn_socket_thread(tx, socket.to_path_buf());
    let mut app = App::default();
    let mut link = DaemonLink {
        socket: socket.to_path_buf(),
        client: None,
    };
    run_loop(&mut surface, &mut app, &rx, &mut link)
}

/// Run `opencode models` once in the background.
///
/// The thread sends one message with the parsed model list or the failure
/// reason. The shell never blocks on this thread; a late result still
/// refreshes an open model value list.
fn spawn_model_probe(tx: Sender<Msg>) {
    thread::spawn(move || {
        let result =
            catalog::fetch_opencode_models(&RealExec).map_err(|error| format!("{error:#}"));
        let _ = tx.send(Msg::HarnessModels(result));
    });
}

/// Read crossterm events until the channel dies.
fn spawn_key_thread(tx: Sender<Msg>) {
    thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                if tx.send(Msg::Key(key)).is_err() {
                    return;
                }
            }
            Ok(Event::Resize(_, _)) => {
                if tx.send(Msg::Resize).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                if let Err(send_error) = tx.send(Msg::Input(error.to_string())) {
                    eprintln!("cannot report the terminal input failure: {send_error}");
                }
                return;
            }
        }
    });
}

/// Connect to the daemon, forward its pushes, and reconnect with backoff.
///
/// The thread reports each connect result. This rule keeps every connection
/// error visible to the main loop.
fn spawn_socket_thread(tx: Sender<Msg>, socket: PathBuf) {
    thread::spawn(move || {
        let mut attempt: u32 = 0;
        loop {
            let mut failure: Option<String> = None;
            match Client::connect(&socket) {
                Ok(client) => {
                    attempt = 0;
                    if tx.send(Msg::Connected).is_err() {
                        return;
                    }
                    match client.pushes() {
                        Ok(pushes) => {
                            for push in pushes {
                                match push {
                                    Ok(Push::State(view)) => {
                                        if tx.send(Msg::State(view)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::TicketDetails(details)) => {
                                        if tx.send(Msg::TicketDetails(details)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::TicketMentions(mentions)) => {
                                        if tx.send(Msg::TicketMentions(mentions)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::TicketLabels(labels)) => {
                                        if tx.send(Msg::TicketLabels(labels)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::TicketResult(result)) => {
                                        if tx.send(Msg::TicketResult(result)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::Ask(ask)) => {
                                        if tx.send(Msg::Ask(ask)).is_err() {
                                            return;
                                        }
                                    }
                                    Ok(Push::SettingsResult(result)) => {
                                        if tx.send(Msg::SettingsResult(result)).is_err() {
                                            return;
                                        }
                                    }
                                    Err(error) => {
                                        if error.downcast_ref::<WireProtocolMismatch>().is_some() {
                                            if tx.send(Msg::Fatal(format!("{error:#}"))).is_err() {
                                                return;
                                            }
                                            return;
                                        }
                                        failure = Some(format!("{error:#}"));
                                        break;
                                    }
                                }
                            }
                        }
                        Err(error) => failure = Some(format!("{error:#}")),
                    }
                }
                Err(error) => failure = Some(format!("{error:#}")),
            }
            let reason = failure.unwrap_or_else(|| "the daemon closed the connection".to_string());
            if tx.send(Msg::Disconnected(reason)).is_err() {
                return;
            }
            thread::sleep(backoff_delay(attempt));
            attempt = attempt.saturating_add(1);
        }
    });
}

/// Consume messages and session file changes until the app quits.
///
/// Every handled message leads to one draw. The pipeline and inbox block
/// without a deadline. The session adds one file poll deadline and draws
/// only when that poll finds new visible log data.
fn run_loop(
    surface: &mut impl Surface,
    app: &mut App,
    rx: &Receiver<Msg>,
    sink: &mut impl ActionSink,
) -> Result<()> {
    loop {
        let now = Instant::now();
        let polls_log = app.view == View::Session
            || (app.view == View::Tickets && app.tickets.needs_poll())
            || (app.view == View::Tickets && app.tickets.status_refresh_due(now));
        let msg = if polls_log {
            match rx.recv_timeout(session::POLL_INTERVAL) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let changed = match app.view {
                        View::Session => app.session.poll(now),
                        View::Tickets => app.tickets.poll(now),
                        View::Pipeline | View::Inbox | View::Settings => false,
                    };
                    if app.view == View::Tickets {
                        if let Some(action) = app.tickets.take_status_refresh(now) {
                            sink.send_action(action);
                        }
                    }
                    if changed {
                        draw_app(surface, app, now)?;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match rx.recv() {
                Ok(msg) => msg,
                Err(_) => return Ok(()),
            }
        };
        if !handle_message(app, msg, sink)? {
            return Ok(());
        }
        draw_app(surface, app, Instant::now())?;
    }
}

/// Apply one shell message. False requests a clean exit.
fn handle_message(app: &mut App, msg: Msg, sink: &mut impl ActionSink) -> Result<bool> {
    match msg {
        Msg::Key(key) => {
            let handled = app.handle_key(key, sink);
            app.send_pending_pr_mention(sink);
            Ok(handled)
        }
        Msg::State(view) => {
            app.apply_state(view);
            app.send_pending_pr_mention(sink);
            Ok(true)
        }
        Msg::TicketDetails(details) => {
            app.tickets.observe_details(details);
            Ok(true)
        }
        Msg::TicketMentions(mentions) => {
            app.tickets.observe_mentions(mentions.clone());
            app.inbox.observe_pr_mentions(&mentions);
            Ok(true)
        }
        Msg::TicketLabels(labels) => {
            app.tickets.observe_labels(labels);
            Ok(true)
        }
        Msg::TicketResult(result) => {
            app.tickets.observe_result(result);
            Ok(true)
        }
        Msg::Ask(ask) => {
            app.inbox.observe_ask(&ask);
            Ok(true)
        }
        Msg::SettingsResult(result) => {
            app.settings.observe_result(result);
            Ok(true)
        }
        Msg::HarnessModels(result) => {
            app.settings.observe_models(result);
            Ok(true)
        }
        Msg::Connected => {
            app.connected = true;
            Ok(true)
        }
        Msg::Disconnected(reason) => {
            app.mark_disconnected(reason);
            Ok(true)
        }
        Msg::Fatal(reason) => Err(anyhow!(reason)),
        Msg::Input(reason) => Err(anyhow!("terminal input stopped: {reason}")),
        Msg::Resize => Ok(true),
    }
}

/// Read the session log and draw one frame.
fn draw_app(surface: &mut impl Surface, app: &mut App, now: Instant) -> Result<()> {
    if app.view == View::Session {
        app.session.on_redraw(now);
    } else if app.view == View::Tickets {
        app.tickets.on_redraw(now);
    }
    surface.draw(app)
}

/// Run a finite message list through the same handlers in a test.
#[cfg(test)]
fn run_messages(
    surface: &mut impl Surface,
    app: &mut App,
    msgs: impl Iterator<Item = Msg>,
    sink: &mut impl ActionSink,
) -> Result<()> {
    for msg in msgs {
        if !handle_message(app, msg, sink)? {
            return Ok(());
        }
        draw_app(surface, app, Instant::now())?;
    }
    Ok(())
}

/// Draw the whole shell into the frame.
///
/// The call records the body rectangle on the app, so the next key press
/// knows the visible transcript height of the session view.
fn render(f: &mut Frame, app: &mut App) -> Result<()> {
    render_with_clock(f, app, inbox::now_ms)
}

/// Draw the shell with an injected inbox clock.
fn render_with_clock(
    f: &mut Frame,
    app: &mut App,
    clock: impl FnOnce() -> Result<u64>,
) -> Result<()> {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().fg(THEME.text).bg(THEME.background)),
        area,
    );
    let (header, body, footer) = if app.connected {
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        (chunks[0], chunks[1], chunks[2])
    } else {
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        draw_banner(f, app, chunks[1]);
        (chunks[0], chunks[2], chunks[3])
    };
    app.body = body;
    draw_header(f, app, header);
    match app.view {
        View::Pipeline => {
            let now = clock().context("cannot read the system clock")?;
            pipeline::draw(f, app, body, now);
        }
        View::Session => {
            let decisions = app
                .state
                .as_ref()
                .map(|state| state.decisions.as_slice())
                .unwrap_or(&[]);
            app.session.draw(f, body, decisions);
        }
        View::Inbox => {
            if let Some(state) = app.state.as_ref() {
                let now = clock().context("cannot read the system clock")?;
                inbox::draw(f, body, state, &app.inbox, now);
            }
        }
        View::Tickets => {
            if let Some(state) = app.state.as_ref() {
                app.tickets.draw(f, body, state);
            }
        }
        View::Settings => {
            if let Some(state) = app.state.as_ref() {
                app.settings.draw(f, body, state);
            }
        }
    }
    draw_toast(f, app, body);
    draw_footer(f, app, footer);
    draw_confirm(f, app, area);
    if app.help {
        draw_help(f, area);
    }
    Ok(())
}

/// Draw the title row: app name, view tabs, pause flag, and socket state.
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let sides = Layout::horizontal([Constraint::Min(20), Constraint::Length(14)]).split(area);
    let title = Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD);
    let mut tabs = vec![Span::styled(" aif ", title)];
    tabs.push(tab_span("1", "pipeline", app.view == View::Pipeline));
    tabs.push(tab_span("2", "session", app.view == View::Session));
    tabs.push(tab_span("3", "inbox", app.view == View::Inbox));
    tabs.push(tab_span("4", "tickets", app.view == View::Tickets));
    tabs.push(tab_span("5", "settings", app.view == View::Settings));
    f.render_widget(Paragraph::new(Line::from(tabs)), sides[0]);

    let mut status = Vec::new();
    if app.state.as_ref().is_some_and(|state| state.paused.global) {
        status.push(Span::styled(
            "paused ",
            Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
        ));
    }
    let (mark, word, color) = if app.connected {
        ("●", "live", THEME.ok)
    } else {
        ("●", "down", THEME.error)
    };
    status.push(Span::styled(format!("{mark} "), Style::default().fg(color)));
    status.push(Span::styled(word, Style::default().fg(color)));
    f.render_widget(
        Paragraph::new(Line::from(status)).alignment(Alignment::Right),
        sides[1],
    );
}

/// The styled tab label of one view.
fn tab_span(number: &str, label: &str, active: bool) -> Span<'static> {
    let text = format!(" {number} {label} ");
    if active {
        Span::styled(
            text,
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(text, THEME.dim())
    }
}

/// Draw the banner that reports a lost daemon connection.
fn draw_banner(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Paragraph::new(banner_text(app)).style(THEME.banner()), area);
}

/// The banner line for the current connection state.
fn banner_text(app: &App) -> String {
    let reason = app.disconnect.as_deref().unwrap_or_default();
    let suffix = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };
    if app.state.is_some() {
        format!(" daemon disconnected - reconnecting{suffix} ")
    } else {
        format!(" connecting to the daemon{suffix} ")
    }
}

/// Draw the key hints and the inbox badge.
///
/// The badge renders in every view, so an open decision stays visible from
/// anywhere.
fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let sides = Layout::horizontal([Constraint::Min(20), Constraint::Length(14)]).split(area);
    let hints = Span::styled(
        " 1/2/3/4/5 views   h/j/k/l move   ! inbox   ? help   ctrl-q quit ",
        THEME.dim(),
    );
    f.render_widget(Paragraph::new(Line::from(hints)), sides[0]);
    let Some(state) = app.state.as_ref() else {
        return;
    };
    let style = if inbox::open_count(state) > 0 {
        Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD)
    } else {
        THEME.dim()
    };
    let badge = inbox::badge(state);
    f.render_widget(
        Paragraph::new(badge)
            .style(style)
            .alignment(Alignment::Right),
        sides[1],
    );
}

/// Draw the fresh toast at the lower right of the body.
fn draw_toast(f: &mut Frame, app: &App, body: Rect) {
    let Some(text) = app.visible_toast() else {
        return;
    };
    let width = (text.chars().count() as u16 + 2).min(body.width);
    if width == 0 || body.height == 0 {
        return;
    }
    let area = Rect {
        x: body.x + body.width.saturating_sub(width),
        y: body.y + body.height.saturating_sub(1),
        width,
        height: 1,
    };
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(text).style(THEME.toast()), area);
}

/// Draw the confirmation prompt over the view.
///
/// The prompt names the action and lists the pull requests of a release.
fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(confirm) = app.confirm.as_ref() else {
        return;
    };
    let question = match confirm {
        Confirm::Abort { task } => format!("abort {task}?"),
        Confirm::Go { repo, prs } => format!("release {repo}: {}", pr_text(prs)),
        Confirm::Refine { repo, number } => format!("add to-refine to {repo} #{number}?"),
    };
    let hint = "y confirm - esc cancel";
    let width = question.chars().count().max(hint.len()) as u16 + 6;
    let panel = centered(width, 5, area);
    f.render_widget(Clear, panel);
    let lines = vec![
        Line::from(Span::raw(question)),
        Line::from(String::new()),
        Line::from(Span::styled(hint, THEME.dim())),
    ];
    let block = Paragraph::new(lines).block(Block::bordered().title(" confirm "));
    f.render_widget(block, panel);
}

/// Draw the help overlay over the whole frame.
fn draw_help(f: &mut Frame, area: Rect) {
    let panel = centered(78, 18, area);
    f.render_widget(Clear, panel);
    let rows: [(&str, &str); 32] = [
        ("1 2 3 4 5", "switch view"),
        ("esc", "home / cancel settings edit"),
        ("!", "inbox, oldest decision"),
        ("j k Up Down", "move inside a lane"),
        ("h l Left Right", "move between lanes"),
        ("?", "toggle this help"),
        ("ctrl-q", "quit"),
        ("+ -", "stage limit / repo lane"),
        ("p P", "pause selected / all"),
        ("r n", "refine / new ticket"),
        ("x R", "abort / retry"),
        ("space", "toggle the selected PR"),
        ("g s", "release / policy"),
        ("enter", "open the selected task session"),
        ("enter", "send the chat message"),
        ("enter", "open selected decision details"),
        ("s", "submit selected question answer"),
        ("o", "open task session from details"),
        ("ctrl-x", "abort the shown task"),
        ("PageUp PageDown", "scroll content"),
        ("End", "follow the tail"),
        ("esc tab i", "leave or take the chat focus"),
        ("h l", "switch session, chat unfocused"),
        ("y n i t c s w 1-9", "inbox answers"),
        ("g", "fire the release gate"),
        ("/ n e L c a m", "search and ticket keys"),
        ("h l", "repo / ticket / settings scope"),
        ("j k", "settings role"),
        ("Tab", "select settings field"),
        ("Enter", "edit settings value"),
        ("s r", "save / reload settings"),
        ("d", "remove repository override"),
    ];
    let mut sorted = rows.to_vec();
    sorted.sort_by_key(|(key, text)| std::cmp::Reverse(key.len() + text.len()));
    let mut lines = Vec::new();
    for row in 0..16 {
        let mut spans = Vec::new();
        for (column, index) in [row, sorted.len() - 1 - row].into_iter().enumerate() {
            let (key, text) = sorted[index];
            if column > 0 {
                spans.push(Span::raw("    "));
            }
            spans.push(Span::styled(
                format!(" {key}  "),
                Style::default()
                    .fg(THEME.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(text));
        }
        lines.push(Line::from(spans));
    }
    let block = Paragraph::new(lines).block(Block::bordered().title(" keys "));
    f.render_widget(block, panel);
}

/// A rectangle of the given size at the center of `outer`.
fn centered(width: u16, height: u16, outer: Rect) -> Rect {
    Rect {
        x: outer.x + outer.width.saturating_sub(width) / 2,
        y: outer.y + outer.height.saturating_sub(height) / 2,
        width: width.min(outer.width),
        height: height.min(outer.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::Decision;
    use crate::model::ItemKind;
    use crate::tasks::{Task, TaskState};
    use crate::tui::pipeline::render_to_string;
    use std::cell::Cell;
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A surface that counts draws.
    struct CountingSurface {
        draws: usize,
    }

    impl Surface for CountingSurface {
        fn draw(&mut self, _app: &mut App) -> Result<()> {
            self.draws += 1;
            Ok(())
        }
    }

    /// A surface that publishes each draw to its test.
    struct SharedCountingSurface {
        draws: Arc<AtomicUsize>,
        drew: Sender<()>,
    }

    impl Surface for SharedCountingSurface {
        fn draw(&mut self, _app: &mut App) -> Result<()> {
            self.draws.fetch_add(1, Ordering::SeqCst);
            self.drew.send(()).context("cannot report a test draw")?;
            Ok(())
        }
    }

    /// A surface that publishes the visible text of each frame.
    struct FrameSurface {
        drew: Sender<String>,
    }

    impl Surface for FrameSurface {
        fn draw(&mut self, app: &mut App) -> Result<()> {
            self.drew
                .send(render_to_string(app))
                .context("cannot report a test frame")
        }
    }

    /// An action sink that records what it sent.
    #[derive(Default)]
    struct FakeSink(Vec<Action>);

    impl ActionSink for FakeSink {
        fn send_action(&mut self, action: Action) -> bool {
            self.0.push(action);
            true
        }
    }

    struct FailSink;

    impl ActionSink for FailSink {
        fn send_action(&mut self, _action: Action) -> bool {
            false
        }
    }

    /// A key press of one character.
    fn key(character: char) -> Msg {
        Msg::Key(KeyEvent::new(
            KeyCode::Char(character),
            crossterm::event::KeyModifiers::empty(),
        ))
    }

    /// A key press of one raw key code.
    fn key_code(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, crossterm::event::KeyModifiers::empty()))
    }

    /// One permission decision for `borsuk/implement-i140`.
    fn permission_decision(request_id: &str, opened_ms: u64) -> Decision {
        let task = Task::new(
            "borsuk",
            Stage::Implement,
            ItemKind::Issue,
            140,
            PathBuf::from("borsuk-implement-i140.jsonl"),
            1_000,
        );
        Decision::permission(&task, request_id, "Write", serde_json::json!({}), opened_ms)
    }

    /// One ticket summary for the shell ticket tests.
    fn ticket_summary(repo: &str, number: u64) -> crate::sock::TicketSummary {
        crate::sock::TicketSummary {
            repo: repo.to_string(),
            number,
            title: "Improve the ticket list".to_string(),
            labels: vec!["ui".to_string()],
            updated_at: "2026-08-30T12:00:00Z".to_string(),
            group: crate::sock::TicketGroup::Untouched,
        }
    }

    /// One full issue response for the shell ticket tests.
    fn ticket_details(repo: &str, number: u64) -> crate::sock::TicketDetails {
        crate::sock::TicketDetails {
            request: "shell-details".to_string(),
            repo: repo.to_string(),
            issue: crate::model::Issue {
                number,
                node_id: format!("node-{number}"),
                title: "Improve the ticket list".to_string(),
                body: "Show every issue without leaving the terminal.".to_string(),
                labels: vec!["ui".to_string()],
                author: "piotr".to_string(),
                assignees: Vec::new(),
                updated_at: "2026-08-30T12:00:00Z".to_string(),
                github_url: format!("https://github.com/acme/{repo}/issues/{number}"),
                open: true,
            },
            proposal: None,
            chat_error: None,
        }
    }

    /// Open the plain ticket focus on one pushed ticket and clear the sink.
    fn open_ticket_focus(surface: &mut CountingSurface, app: &mut App, sink: &mut FakeSink) {
        let mut state = crate::tui::pipeline::sample_view();
        state.tickets.push(ticket_summary("borsuk", 7));
        run_messages(
            surface,
            app,
            vec![Msg::State(state), key('4'), key_code(KeyCode::Enter)].into_iter(),
            sink,
        )
        .unwrap();
        sink.0.clear();
        app.tickets.observe_details(ticket_details("borsuk", 7));
    }

    #[test]
    fn backoff_grows_from_one_second_to_ten() {
        let expected: [Duration; 6] = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(10),
            Duration::from_secs(10),
        ];
        for (attempt, want) in expected.iter().enumerate() {
            assert_eq!(backoff_delay(attempt as u32), *want);
        }
    }

    #[test]
    fn every_reconnect_failure_reaches_the_main_loop() {
        let socket =
            std::env::temp_dir().join(format!("aif-missing-daemon-{}.sock", uuid::Uuid::new_v4()));
        let (tx, rx) = channel();
        spawn_socket_thread(tx, socket);

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Msg::Disconnected(_))
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Msg::Disconnected(_))
        ));
    }

    #[test]
    fn an_incompatible_daemon_exits_an_eighty_column_tui_with_full_recovery_text() {
        let socket =
            std::env::temp_dir().join(format!("aif-old-daemon-{}.sock", uuid::Uuid::new_v4()));
        let (server, _actions) = crate::sock::Server::bind(&socket).unwrap();
        let mut old = crate::tui::pipeline::sample_view();
        old.protocol_revision = 0;
        server.publish(old);
        let (tx, rx) = channel();
        spawn_socket_thread(tx, socket);

        let connected = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let incompatible = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut app = App::default();
        let mut sink = FakeSink::default();
        assert!(handle_message(&mut app, connected, &mut sink).unwrap());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app).unwrap())
            .unwrap();

        let error = handle_message(&mut app, incompatible, &mut sink).unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains("daemon wire protocol revision 0"),
            "{message}"
        );
        assert!(message.contains("aif stop"), "{message}");
        assert!(message.contains("start `aif` again"), "{message}");
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "a permanent mismatch must stop its socket thread"
        );
    }

    #[test]
    fn the_loop_draws_once_per_message_and_not_during_a_quiet_interval() {
        let quiet = Duration::from_millis(80);
        let draws = Arc::new(AtomicUsize::new(0));
        let (draw_tx, draw_rx) = channel();
        let (msg_tx, msg_rx) = channel();
        let thread_draws = Arc::clone(&draws);
        let handle = thread::spawn(move || {
            let mut surface = SharedCountingSurface {
                draws: thread_draws,
                drew: draw_tx,
            };
            let mut app = App::default();
            let mut sink = FakeSink::default();
            run_loop(&mut surface, &mut app, &msg_rx, &mut sink)
        });

        msg_tx.send(Msg::Resize).unwrap();
        draw_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let before = draws.load(Ordering::SeqCst);
        assert_eq!(before, 1);
        assert_eq!(
            draw_rx.recv_timeout(quiet),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        assert_eq!(draws.load(Ordering::SeqCst), before);

        msg_tx.send(key('q')).unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn the_session_poll_draws_new_log_text_without_an_input_message() {
        let dir = std::env::temp_dir().join(format!("aif-session-poll-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("task.jsonl");
        fs::write(&log, "first\n").unwrap();

        let mut state = crate::tui::pipeline::sample_view();
        state.tasks[0].log_path = log.clone();
        let mut app = App::default();
        app.apply_state(state);
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;

        let (draw_tx, draw_rx) = channel();
        let (msg_tx, msg_rx) = channel();
        let handle = thread::spawn(move || {
            let mut surface = FrameSurface { drew: draw_tx };
            let mut sink = FakeSink::default();
            run_loop(&mut surface, &mut app, &msg_rx, &mut sink)
        });

        msg_tx.send(Msg::Resize).unwrap();
        let first = draw_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(first.contains("first"), "initial frame: {first}");
        assert_eq!(
            draw_rx.recv_timeout(session::POLL_INTERVAL * 2),
            Err(RecvTimeoutError::Timeout),
            "an unchanged log must not cause a periodic draw"
        );

        // Append the new line the way a real agent does. A rewrite would
        // truncate the file first; a poll that observes the truncated
        // file reports a log restart and clears the transcript.
        let mut log_file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        log_file.write_all(b"second\n").unwrap();
        let second = draw_rx.recv_timeout(session::POLL_INTERVAL * 3).ok();

        let ctrl_q = Msg::Key(KeyEvent::new(
            KeyCode::Char('q'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        msg_tx.send(ctrl_q).unwrap();
        handle.join().unwrap().unwrap();
        fs::remove_dir_all(dir).unwrap();

        let second = second.expect("the file poll did not draw a new frame");
        assert!(second.contains("second"), "polled frame: {second}");
    }

    #[test]
    fn the_loop_stops_on_q_without_a_last_draw() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view()), key('q')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(surface.draws, 1);
    }

    #[test]
    fn keys_switch_views_and_toggle_help() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![key('3'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox);
        assert!(app.help);
        // While the overlay is open, the view keys do nothing.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('1')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox);
        // Escape closes the overlay, then the view keys work again.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.help);
        run_messages(
            &mut surface,
            &mut app,
            vec![key('1')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Pipeline);
        run_messages(
            &mut surface,
            &mut app,
            vec![key('4')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Tickets);
        run_messages(
            &mut surface,
            &mut app,
            vec![key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.help);
    }

    #[test]
    fn h_and_l_move_between_pipeline_stage_lanes() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('j'),
                key('j'),
                key('j'),
                key('l'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        let state = app.state.as_ref().unwrap();
        let Selection::Row(index) = app.selection else {
            panic!("the pipeline must hold a selection");
        };
        assert_eq!(
            pipeline::rows(state)[index],
            pipeline::Row::Ticket { index: 2 }
        );
    }

    #[test]
    fn a_state_push_connects_and_clamps_the_selection() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(10_000),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.connected);
        let count = pipeline::rows(app.state.as_ref().unwrap()).len();
        assert_eq!(app.selection, Selection::Row(count - 1));
    }

    #[test]
    fn a_state_push_keeps_the_selected_release_pr_by_identity() {
        let mut first = crate::tui::pipeline::sample_view();
        first.trains[0].queue = vec![9];
        let selected = pipeline::Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 9,
        };
        let index = pipeline::rows(&first)
            .iter()
            .position(|row| row == &selected)
            .unwrap();
        let mut app = App {
            state: Some(first),
            connected: true,
            selection: Selection::Row(index),
            ..App::default()
        };
        let next = crate::tui::pipeline::sample_view();

        app.apply_state(next);

        let Selection::Row(index) = app.selection else {
            panic!("the pull request must stay selected");
        };
        assert_eq!(pipeline::rows(app.state.as_ref().unwrap())[index], selected);
    }

    #[test]
    fn a_state_push_keeps_the_selected_task_by_id() {
        let first = crate::tui::pipeline::sample_view();
        let selected_id = first.tasks[1].id.clone();
        let mut app = App {
            state: Some(first),
            connected: true,
            selection: Selection::Row(3),
            ..App::default()
        };
        let mut next = crate::tui::pipeline::sample_view();
        let mut earlier = next.tasks[0].clone();
        earlier.id = "borsuk/refine-i141".to_string();
        earlier.number = 141;
        next.tasks.insert(0, earlier);

        app.apply_state(next);

        let Selection::Row(index) = app.selection else {
            panic!("the task must stay selected");
        };
        let pipeline::Row::Ticket { index } = pipeline::rows(app.state.as_ref().unwrap())[index]
        else {
            panic!("the selection must remain a task");
        };
        assert_eq!(app.state.as_ref().unwrap().tasks[index].id, selected_id);
    }

    #[test]
    fn a_state_push_clears_a_selection_that_no_longer_exists() {
        let first = crate::tui::pipeline::sample_view();
        let selected_id = first.tasks[1].id.clone();
        let mut app = App {
            state: Some(first),
            connected: true,
            selection: Selection::Row(3),
            ..App::default()
        };
        let mut next = crate::tui::pipeline::sample_view();
        next.tasks.retain(|task| task.id != selected_id);

        app.apply_state(next);

        assert_eq!(app.selection, Selection::None);
    }

    #[test]
    fn a_model_discovery_message_fills_the_open_value_list() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.settings = crate::sock::SettingsView {
            revision: "rev-one".to_string(),
            global: crate::config::ExecutionRole::ALL
                .into_iter()
                .map(|role| crate::sock::GlobalRoleSettingsView {
                    role,
                    settings: crate::config::RoleSettings {
                        harness: crate::config::Harness::Opencode,
                        program: "opencode".to_string(),
                        model: "model-one".to_string(),
                        effort: None,
                        extra_args: Vec::new(),
                        agent: None,
                        profile: None,
                        permission_mode: None,
                        permission_handler: None,
                        tools: Vec::new(),
                        disallowed_tools: Vec::new(),
                        strict_mcp: None,
                        auto_approve: Some(false),
                        approval_policy: None,
                        sandbox: None,
                    },
                    limit: None,
                })
                .collect(),
            repositories: Vec::new(),
        };

        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(state),
                key('5'),
                key('j'),
                key_code(KeyCode::Tab),
                key_code(KeyCode::Tab),
                key_code(KeyCode::Enter),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        let screen = render_to_string(&mut app);
        assert!(screen.contains("discovering models..."), "{screen}");

        handle_message(
            &mut app,
            Msg::HarnessModels(Ok(vec!["zai-coding-plan/glm-5.3-flash".to_string()])),
            &mut sink,
        )
        .unwrap();
        let screen = render_to_string(&mut app);
        assert!(screen.contains("zai-coding-plan/glm-5.3-flash"), "{screen}");
    }

    #[test]
    fn banner_text_covers_every_state() {
        let mut app = App::default();
        assert_eq!(banner_text(&app), " connecting to the daemon ");
        app.disconnect = Some("no such file".to_string());
        assert_eq!(
            banner_text(&app),
            " connecting to the daemon (no such file) "
        );
        app.state = Some(crate::tui::pipeline::sample_view());
        assert_eq!(
            banner_text(&app),
            " daemon disconnected - reconnecting (no such file) "
        );
        app.disconnect = None;
        assert_eq!(banner_text(&app), " daemon disconnected - reconnecting ");
    }

    #[test]
    fn an_unconnected_app_shows_the_connecting_banner() {
        let mut app = App::default();
        let text = render_to_string(&mut app);
        assert!(text.contains("connecting to the daemon"));
        assert!(text.contains("down"));
        assert!(text.contains("waiting for the first state push"));
    }

    #[test]
    fn a_disconnected_app_keeps_the_last_state_under_the_banner() {
        let mut app = App {
            state: Some(crate::tui::pipeline::sample_view()),
            connected: false,
            disconnect: Some("no such file".to_string()),
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("daemon disconnected - reconnecting"));
        // The last state stays visible below the banner.
        assert!(text.contains("refine"));
        assert!(text.contains("queued · i142"));
    }

    #[test]
    fn the_help_overlay_lists_the_keys_of_this_chunk() {
        let mut app = App {
            help: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("keys"));
        let open_session_row = text
            .lines()
            .find(|line| line.contains("open the selected task session"))
            .expect("the help misses the open-session row");
        assert!(
            open_session_row.contains("enter"),
            "the open-session row has the wrong key: {open_session_row}"
        );
        for entry in [
            "switch view",
            "oldest decision",
            "j k Up Down",
            "h l Left Right",
            "move between lanes",
            "leave or take the chat focus",
            "switch session, chat unfocused",
            "toggle the selected PR",
            "ctrl-q",
            "send the chat message",
            "open selected decision details",
            "submit selected question answer",
            "open task session from details",
            "PageUp PageDown",
            "PageDown",
            "End",
            "/ n e L c a m",
        ] {
            assert!(text.contains(entry), "the help misses {entry}");
        }

        let mut closed = App::default();
        let text = render_to_string(&mut closed);
        assert!(!text.contains("PageUp"));
    }

    #[test]
    fn a_fresh_toast_shows_and_an_expired_one_does_not() {
        let mut fresh = App {
            toast: Some(("sent".to_string(), Instant::now() + Duration::from_secs(4))),
            ..App::default()
        };
        assert!(render_to_string(&mut fresh).contains("sent"));
        let mut expired = App {
            toast: Some(("sent".to_string(), Instant::now())),
            ..App::default()
        };
        assert!(!render_to_string(&mut expired).contains("sent"));
    }

    #[test]
    fn the_header_shows_the_pause_flag_and_socket_state() {
        let mut state = crate::tui::pipeline::sample_view();
        state.paused.global = true;
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("live"));
        assert!(text.contains("paused"));
    }

    #[test]
    fn q_types_into_the_session_input_and_never_quits() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;
        assert!(app.session.is_showing("borsuk/refine-i142"));

        // The loop keeps running: the next key still reaches the app, and
        // both letters land in the input buffer.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('q'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        let screen = render_to_string(&mut app);
        assert!(screen.contains("q?▏"), "session screen: {screen}");
        assert!(!app.help, "the question mark went into the buffer too");
    }

    #[test]
    fn the_quit_chord_stops_the_loop_from_anywhere() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let ctrl_q = Msg::Key(KeyEvent::new(
            KeyCode::Char('q'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        run_messages(
            &mut surface,
            &mut app,
            vec![ctrl_q, key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.help, "the loop stopped at the quit chord");
    }

    #[test]
    fn q_types_into_the_inbox_reason_and_never_quits() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![permission_decision("req-1", 1_000)];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('3'), key('n'), key('q'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.help, "the letters went into the reason input");
        let text = render_to_string(&mut app);
        assert!(text.contains("reason: q?"), "screen: {text}");
    }

    #[test]
    fn the_new_ticket_form_types_a_digit_instead_of_switching_views() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let ctrl_s = Msg::Key(KeyEvent::new(
            KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('4'),
                key('n'),
                key('1'),
                ctrl_s,
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Tickets, "1 must not switch the view");
        assert!(app.tickets.typing(), "the form kept the keyboard");
        let [crate::sock::Action::Ticket(crate::sock::TicketAction::Create { repo, title, .. })] =
            sink.0.as_slice()
        else {
            panic!("ctrl-s must send the typed title: {:?}", sink.0);
        };
        assert_eq!(repo, "borsuk", "the form targets the first repository");
        assert_eq!(title, "1", "the digit went into the title");
    }

    #[test]
    fn a_failed_ticket_send_keeps_the_draft_and_allows_a_retry() {
        let mut app = App {
            view: View::Tickets,
            state: Some(crate::tui::pipeline::sample_view()),
            ..App::default()
        };
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut FailSink,
        );
        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut FailSink,
        );
        app.handle_key(ctrl_s, &mut FailSink);

        let mut sink = FakeSink::default();
        app.handle_key(ctrl_s, &mut sink);

        let [Action::Ticket(crate::sock::TicketAction::Create { title, .. })] = sink.0.as_slice()
        else {
            panic!("the retry must send the retained ticket draft");
        };
        assert_eq!(title, "x");
    }

    #[test]
    fn a_disconnect_unlocks_a_pending_ticket_send() {
        let mut app = App {
            view: View::Tickets,
            state: Some(crate::tui::pipeline::sample_view()),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut sink,
        );
        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut sink,
        );
        app.handle_key(ctrl_s, &mut sink);

        app.mark_disconnected("lost".to_string());
        app.handle_key(ctrl_s, &mut sink);

        assert_eq!(sink.0.len(), 2, "the disconnect must allow one retry");
    }

    #[test]
    fn bang_enters_the_inbox_and_selects_the_oldest_row() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![
            permission_decision("req-1", 9_000),
            permission_decision("req-2", 2_000),
        ];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox);
        assert_eq!(
            app.inbox.selected_id(),
            Some("perm:borsuk/implement-i140:req-2"),
            "the oldest decision is selected"
        );
    }

    #[test]
    fn bang_enters_the_inbox_from_the_session_view() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![
            permission_decision("req-1", 9_000),
            permission_decision("req-2", 2_000),
        ];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state)].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;

        // esc releases the chat focus, so the shell takes the ! key.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Inbox);
        assert_eq!(
            app.inbox.selected_id(),
            Some("perm:borsuk/implement-i140:req-2")
        );
    }

    #[test]
    fn a_focused_open_bar_types_bang_and_a_released_bar_leaves_it_to_the_shell() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![
            permission_decision("req-1", 9_000),
            permission_decision("req-2", 2_000),
        ];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state)].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;

        run_messages(
            &mut surface,
            &mut app,
            vec![key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            app.view,
            View::Session,
            "the focused open bar keeps the session view"
        );
        assert_eq!(app.session.input_text(), "!", "the bar took the ! as text");

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            app.view,
            View::Inbox,
            "the released focus leaves ! to the shell"
        );
        assert_eq!(
            app.inbox.selected_id(),
            Some("perm:borsuk/implement-i140:req-2"),
            "the oldest decision is selected"
        );
    }

    #[test]
    fn bang_opens_the_inbox_when_the_session_bar_cannot_type() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();

        // A closed input disables the focused bar, so ! stays global.
        let mut state = crate::tui::pipeline::sample_view();
        state.tasks[0].input = crate::sock::InputMode::Closed {
            reason: "the session is parked".to_string(),
        };
        state.decisions = vec![
            permission_decision("req-1", 9_000),
            permission_decision("req-2", 2_000),
        ];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('2')].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;

        run_messages(
            &mut surface,
            &mut app,
            vec![key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox, "! left the closed session");
        assert_eq!(
            app.inbox.selected_id(),
            Some("perm:borsuk/implement-i140:req-2"),
            "the oldest decision is selected"
        );

        // A session with no shown task disables the focused bar too.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('2')].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = None;
        app.session.clear();
        run_messages(
            &mut surface,
            &mut app,
            vec![key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox, "! left the taskless session");
    }

    #[test]
    fn bang_types_into_the_inbox_reason_and_the_tickets_search() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![permission_decision("req-1", 1_000)];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('3'), key('n'), key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox, "the reason input keeps the view");
        assert!(app.inbox.typing(), "the reason input stays open");
        let text = render_to_string(&mut app);
        assert!(text.contains("reason: !"), "screen: {text}");

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('4'), key('/'), key('!')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Tickets, "the search keeps the view");
        let text = render_to_string(&mut app);
        assert!(text.contains(" /!▌"), "screen: {text}");
    }

    #[test]
    fn bang_closes_an_inbox_detail_and_selects_the_oldest_item() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![
            permission_decision("newer", 9_000),
            permission_decision("oldest", 2_000),
        ];
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(state),
                key('3'),
                key('j'),
                key_code(KeyCode::Enter),
                key('!'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Inbox);
        assert!(!app.inbox.detail_open());
        assert_eq!(
            app.inbox.selected_id(),
            Some("perm:borsuk/implement-i140:oldest")
        );
    }

    #[test]
    fn w_on_a_needs_human_issue_opens_the_tickets_focus_and_sends_details() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![Decision::needs_human(
            "borsuk",
            ItemKind::Issue,
            142,
            "Choose the storage",
            1_000,
        )];
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(state),
                key('3'),
                key_code(KeyCode::Enter),
                key('w'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Tickets);
        assert!(app.tickets.focus_open());
        let asks = sink
            .0
            .iter()
            .filter(|action| matches!(action, Action::Ask { .. }))
            .count();
        assert_eq!(asks, 1, "the ask request must go out once: {:?}", sink.0);
        let details: Vec<&Action> = sink
            .0
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::Ticket(crate::sock::TicketAction::Details { .. })
                )
            })
            .collect();
        assert_eq!(details.len(), 1, "sink: {:?}", sink.0);
        assert!(matches!(
            details[0],
            Action::Ticket(crate::sock::TicketAction::Details { repo, number, .. })
                if repo == "borsuk" && *number == 142
        ));
    }

    #[test]
    fn enter_opens_inbox_details_esc_returns_and_o_opens_the_task_session() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![permission_decision("req-1", 1_000)];
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('3'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Inbox);
        assert!(app.inbox.detail_open());

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc)].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Inbox);
        assert!(!app.inbox.detail_open());

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Enter), key('o')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/implement-i140"));
        assert!(app.session.is_showing("borsuk/implement-i140"));
    }

    #[test]
    fn enter_on_a_pipeline_ticket_opens_its_session() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        // Three `j` presses walk from no selection to the first ticket:
        // stage header, repository header, ticket.
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('j'),
                key('j'),
                key('j'),
                key_code(KeyCode::Enter),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/refine-i142"));
        assert!(app.session.is_showing("borsuk/refine-i142"));
        assert!(sink.0.is_empty(), "enter must not send an action");

        // Escape releases the chat focus first and stays in the view.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert!(!app.session.chat_focus());

        // A second escape leaves the view.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Pipeline);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/refine-i142"));
    }

    #[test]
    fn the_inbox_badge_shows_in_every_view() {
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![permission_decision("req-1", 1_000)];
        for view in [View::Pipeline, View::Session, View::Inbox] {
            let mut app = App {
                state: Some(state.clone()),
                connected: true,
                view,
                ..App::default()
            };
            let text = render_to_string(&mut app);
            assert!(text.contains("! 1 open"), "view {view:?} misses the badge");
        }
    }

    #[test]
    fn an_inbox_clock_error_propagates_from_render() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            state: Some(crate::tui::pipeline::sample_view()),
            connected: true,
            view: View::Inbox,
            ..App::default()
        };
        let mut render_result = Ok(());

        terminal
            .draw(|frame| {
                render_result =
                    render_with_clock(frame, &mut app, || Err(anyhow!("clock before epoch")));
            })
            .unwrap();

        let error = render_result.unwrap_err();
        assert!(format!("{error:#}").contains("cannot read the system clock: clock before epoch"));
    }

    #[test]
    fn a_pipeline_clock_error_propagates_from_render() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            state: Some(crate::tui::pipeline::sample_view()),
            connected: true,
            view: View::Pipeline,
            ..App::default()
        };
        let mut render_result = Ok(());

        terminal
            .draw(|frame| {
                render_result =
                    render_with_clock(frame, &mut app, || Err(anyhow!("clock before epoch")));
            })
            .unwrap();

        let error = render_result.unwrap_err();
        assert!(format!("{error:#}").contains("cannot read the system clock: clock before epoch"));
    }

    #[test]
    fn a_frame_reports_both_terminal_and_render_errors() {
        let error = finish_draw(
            Err(anyhow!("terminal write failed")),
            Err(anyhow!("clock read failed")),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("terminal write failed"), "error: {text}");
        assert!(text.contains("clock read failed"), "error: {text}");
    }

    #[test]
    fn y_confirms_the_aborting_of_the_selected_task() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(2),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('x'),
                key('n'),
                key('y'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.0,
            vec![Action::Abort {
                task: "borsuk/refine-i142".to_string()
            }],
            "only y sent the abort"
        );
        assert!(app.confirm.is_none());
        assert!(app.visible_toast().is_some());
    }

    #[test]
    fn esc_cancels_the_abort_so_y_sends_nothing() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(2),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('x'),
                key_code(KeyCode::Esc),
                key('y'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(sink.0.is_empty());
        assert!(app.confirm.is_none());
    }

    #[test]
    fn a_modified_y_does_not_confirm_a_destructive_action() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(2),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        let ctrl_y = Msg::Key(KeyEvent::new(
            KeyCode::Char('y'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('x'),
                ctrl_y,
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(sink.0.is_empty());
        assert!(app.confirm.is_some());
    }

    #[test]
    fn y_confirms_the_release_of_the_stacked_batch() {
        let mut surface = CountingSurface { draws: 0 };
        let mut state = crate::tui::pipeline::sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked = vec![7];
        let train_row = pipeline::rows(&state)
            .iter()
            .position(|row| {
                row == &pipeline::Row::Train {
                    repo: "borsuk".to_string(),
                }
            })
            .unwrap();
        let mut app = App {
            selection: Selection::Row(train_row),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('g'), key('y')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.0,
            vec![Action::Go {
                repo: "borsuk".to_string(),
                prs: vec![7]
            }]
        );
        assert!(app.confirm.is_none());
        assert!(app.visible_toast().is_some());
    }

    #[test]
    fn y_blocks_a_release_that_changed_after_its_confirmation_opened() {
        let mut surface = CountingSurface { draws: 0 };
        let mut first = crate::tui::pipeline::sample_view();
        first.trains[0].in_flight = None;
        first.trains[0].batch.clear();
        first.trains[0].stacked = vec![7];
        let train_row = pipeline::rows(&first)
            .iter()
            .position(|row| {
                row == &pipeline::Row::Train {
                    repo: "borsuk".to_string(),
                }
            })
            .unwrap();
        let mut changed = first.clone();
        changed.trains[0].stacked = vec![9];
        let mut app = App {
            selection: Selection::Row(train_row),
            ..App::default()
        };
        let mut sink = FakeSink::default();

        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(first), key('g'), Msg::State(changed), key('y')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(sink.0.is_empty());
        assert!(app.confirm.is_none());
        assert_eq!(
            app.visible_toast(),
            Some("release batch changed; press g again")
        );
    }

    #[test]
    fn m_asks_before_it_adds_to_refine() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(
            matches!(
                app.confirm,
                Some(Confirm::Refine { ref repo, number: 7 }) if repo == "borsuk"
            ),
            "m must open the confirm prompt"
        );
        assert!(sink.0.is_empty(), "the prompt must send nothing yet");
        let text = render_to_string(&mut app);
        assert!(
            text.contains("add to-refine to borsuk #7?"),
            "screen: {text}"
        );

        run_messages(
            &mut surface,
            &mut app,
            vec![key('y')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.0.len(), 1);
        let Some(Action::Ticket(TicketAction::ToggleLabel {
            repo,
            number,
            label,
            on,
            ..
        })) = sink.0.first()
        else {
            panic!("y must send the to-refine toggle");
        };
        assert_eq!(repo, "borsuk");
        assert_eq!(*number, 7);
        assert_eq!(label, "to-refine");
        assert!(*on);
        assert!(app.confirm.is_none());
        assert_eq!(app.visible_toast(), Some("sent to-refine borsuk #7"));
    }

    #[test]
    fn esc_cancels_the_to_refine_prompt() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('m'), key_code(KeyCode::Esc), key('y')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(sink.0.is_empty(), "a cancelled prompt must send nothing");
        assert!(app.confirm.is_none());
    }

    #[test]
    fn m_reports_a_ticket_that_already_has_to_refine() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);
        let mut labeled = ticket_details("borsuk", 7);
        labeled
            .issue
            .labels
            .push(crate::gates::TO_REFINE.to_string());
        app.tickets.observe_details(labeled);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(
            app.confirm.is_none(),
            "a labeled ticket must skip the prompt"
        );
        assert_eq!(
            app.visible_toast(),
            Some("the ticket already has to-refine")
        );
        assert!(sink.0.is_empty());
    }

    #[test]
    fn m_stays_out_of_the_nested_ticket_views() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('e'), key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(app.confirm.is_none(), "the editor keeps the m key");
        assert!(!app.tickets.focus_plain(), "the editor holds the keyboard");
        let screen = render_to_string(&mut app);
        let title_row = screen
            .lines()
            .position(|line| line.contains(" title "))
            .expect("the editor title block is visible");
        let draft = screen
            .lines()
            .nth(title_row + 1)
            .expect("the title draft row is visible");
        assert!(
            draft.trim_end_matches([' ', '│']).ends_with('m'),
            "m typed into the editor title: {draft}"
        );
    }

    #[test]
    fn m_stays_out_of_the_label_picker_and_the_chat() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);

        // The label picker owns the keyboard.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('L'), key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.confirm.is_none(), "the label picker keeps the m key");
        assert!(
            !app.tickets.focus_plain(),
            "the label picker holds the keyboard"
        );

        // The new-label form owns the keyboard too.
        app.tickets.observe_labels(crate::sock::TicketLabels {
            request: "labels-1".to_string(),
            repo: "borsuk".to_string(),
            labels: vec![crate::sock::RepoLabel {
                name: "ui".to_string(),
                color: "ededed".to_string(),
            }],
            error: None,
        });
        run_messages(
            &mut surface,
            &mut app,
            vec![key('n'), key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.confirm.is_none(), "the new-label form keeps the m key");
        assert!(
            !app.tickets.focus_plain(),
            "the new-label form holds the keyboard"
        );

        // The chat input owns the keyboard.
        let mut chatting = App::default();
        let mut chat_sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut chatting, &mut chat_sink);
        run_messages(
            &mut surface,
            &mut chatting,
            vec![key('c'), key('m')].into_iter(),
            &mut chat_sink,
        )
        .unwrap();
        assert!(chatting.confirm.is_none(), "the chat keeps the m key");
        assert!(
            !chatting.tickets.focus_plain(),
            "the chat holds the keyboard"
        );
    }

    #[test]
    fn m_stays_out_of_the_ticket_conflict_view() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        open_ticket_focus(&mut surface, &mut app, &mut sink);
        let details = ticket_details("borsuk", 7);
        app.tickets.observe_result(crate::sock::TicketResult {
            request: "content-1".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            kind: crate::sock::TicketResultKind::Conflict,
            message: "the issue changed on GitHub".to_string(),
            issue: None,
            conflict: Some(crate::sock::TicketConflict {
                remote: details.issue.clone(),
                pending: crate::sock::TicketContent {
                    title: "A local title".to_string(),
                    body: "A local body".to_string(),
                },
                source: crate::sock::TicketContentSource::Direct,
            }),
        });

        run_messages(
            &mut surface,
            &mut app,
            vec![key('m')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(app.confirm.is_none(), "the conflict view keeps the m key");
        assert!(
            !app.tickets.focus_plain(),
            "the conflict view holds the keyboard"
        );
        assert!(sink.0.is_empty());
    }

    #[test]
    fn the_help_overlay_names_the_new_ticket_keys() {
        let mut app = App {
            help: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        for entry in [
            "/ n e L c a m",
            "search and ticket keys",
            "repo / ticket / settings scope",
        ] {
            assert!(text.contains(entry), "the help misses {entry}");
        }
    }

    #[test]
    fn enter_sends_the_session_chat_action() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;
        run_messages(
            &mut surface,
            &mut app,
            vec![key('h'), key('i'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.0,
            vec![Action::Chat {
                task: "borsuk/refine-i142".to_string(),
                text: "hi".to_string()
            }]
        );
    }

    #[test]
    fn h_and_l_cycle_the_live_sessions_when_the_chat_is_unfocused() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('2'),
                key('l'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(app.view, View::Session);
        assert_eq!(
            app.session_task, None,
            "a focused chat bar types the letter instead of switching"
        );

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('l')].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(
            app.session_task.as_deref(),
            Some("borsuk/refine-i143"),
            "l shows the first live session"
        );
        assert!(app.session.is_showing("borsuk/refine-i143"));

        // The sample state holds five live sessions. From the first one,
        // two steps right and two steps left return to it, and one more
        // step left wraps to the last live session.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('l'), key('l'), key('h'), key('h'), key('h')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            app.session_task.as_deref(),
            Some("borsuk/release"),
            "the step wraps at the ends"
        );
    }

    #[test]
    fn view_keys_leave_a_session_whose_bar_cannot_type() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();

        // No shown task: the focused bar is disabled, so the view keys
        // stay with the shell.
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('2'),
                key('1'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Pipeline, "1 returns to the pipeline");
        assert!(app.session.input_text().is_empty(), "the bar stays empty");

        // A closed input disables the focused bar the same way.
        let mut state = crate::tui::pipeline::sample_view();
        state.tasks[0].input = crate::sock::InputMode::Closed {
            reason: "the session is parked".to_string(),
        };
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('2')].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;
        run_messages(
            &mut surface,
            &mut app,
            vec![key('3')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox, "3 left the closed session");
        assert!(sink.0.is_empty(), "no chat action left the view");
        assert!(app.session.input_text().is_empty(), "the bar stayed empty");

        run_messages(
            &mut surface,
            &mut app,
            vec![key('2'), key('5')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Settings, "2 re-entered, 5 left");

        run_messages(
            &mut surface,
            &mut app,
            vec![key('2'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert!(app.help, "? opened the help overlay from the session");

        // Escape closes the overlay, then 1 leaves the disabled session.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('1')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.help);
        assert_eq!(app.view, View::Pipeline, "1 left the closed session");

        // An unfocused bar keeps the view keys too.
        run_messages(
            &mut surface,
            &mut app,
            vec![key('2'), key_code(KeyCode::Esc), key('4')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            app.view,
            View::Tickets,
            "2 re-entered, esc released, 4 left"
        );
    }

    #[test]
    fn a_focused_live_bar_types_digits_instead_of_switching() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;
        run_messages(
            &mut surface,
            &mut app,
            vec![key('3')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session, "the focused bar keeps the view");
        assert_eq!(
            app.session.input_text(),
            "3",
            "the digit went into the message"
        );

        // Releasing the focus hands the view keys back to the shell.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc), key('3')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox, "3 switched views after esc");
    }

    #[test]
    fn i_takes_the_chat_focus_back_and_typing_sends_the_chat() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('2'),
                key_code(KeyCode::Esc),
                key('l'),
                key('i'),
                key('h'),
                key('i'),
                key_code(KeyCode::Enter),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(app.session.chat_focus());
        assert_eq!(
            sink.0,
            vec![Action::Chat {
                task: "borsuk/refine-i143".to_string(),
                text: "hi".to_string()
            }]
        );
    }

    #[test]
    fn tab_toggles_the_chat_focus_and_q_quits_only_unfocused() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('2'),
                key_code(KeyCode::Tab),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.session.chat_focus(), "tab releases the chat focus");

        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Tab), key('q'), key('x')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.session.chat_focus(), "tab takes the chat focus back");
        assert!(!app.help, "a focused chat types q instead of quitting");
        assert!(
            app.session.input_text().is_empty(),
            "a focused but disabled bar swallows the letters"
        );

        // A second tab releases the focus again, and q quits the loop.
        run_messages(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Tab)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.session.chat_focus());
        run_messages(
            &mut surface,
            &mut app,
            vec![key('q')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            surface.draws, 7,
            "q quit the loop, so nothing drew after it"
        );
    }

    #[test]
    fn enter_in_a_closed_session_sends_no_action_and_shows_no_toast() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.tasks[0].input = crate::sock::InputMode::Closed {
            reason: "the session is parked".to_string(),
        };
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state)].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;

        run_messages(
            &mut surface,
            &mut app,
            vec![key('h'), key('i'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert!(sink.0.is_empty(), "a closed input must send nothing");
        assert!(app.visible_toast().is_none(), "no send means no toast");
        let screen = render_to_string(&mut app);
        assert!(
            !screen.contains("hi▏"),
            "a closed input swallows the letters: {screen}"
        );
        assert!(
            screen.contains("the session is parked"),
            "the bar shows the reason: {screen}"
        );
    }

    #[test]
    fn r_follows_the_new_refine_task_on_the_next_push() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(8),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view()), key('r')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert_eq!(
            sink.0,
            vec![Action::Refine {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 140
            }]
        );

        // The daemon creates the refine task, and the next push resolves
        // the wanted choice.
        let mut state = crate::tui::pipeline::sample_view();
        state.tasks.push(TaskView {
            id: "borsuk/refine-i140".to_string(),
            repo: "borsuk".to_string(),
            stage: Stage::Refine,
            kind: ItemKind::Issue,
            number: 140,
            state: TaskState::Running,
            attempt: 1,
            log_path: PathBuf::from("borsuk-refine-i140.jsonl"),
            input: crate::sock::InputMode::Live,
            queued_messages: 0,
        });
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.session_task.as_deref(), Some("borsuk/refine-i140"));
        assert!(app.wanted.is_none());
        assert!(app.session.is_showing("borsuk/refine-i140"));
    }

    #[test]
    fn r_cannot_send_chat_to_the_previous_session_while_it_waits() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.selection = Selection::Row(8);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('r'), key('h'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(
            sink.0,
            vec![Action::Refine {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 140
            }]
        );
        assert_eq!(app.session.task_id(), None);
    }

    #[test]
    fn n_follows_the_ticket_create_task_on_the_next_push() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(4),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view()), key('n')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert_eq!(
            sink.0,
            vec![Action::TicketCreate {
                repo: "ryba".to_string()
            }]
        );

        // The ticket creation is the refine task of item zero.
        let mut state = crate::tui::pipeline::sample_view();
        state.tasks.push(TaskView {
            id: "ryba/refine-i0".to_string(),
            repo: "ryba".to_string(),
            stage: Stage::Refine,
            kind: ItemKind::Issue,
            number: 0,
            state: TaskState::Running,
            attempt: 1,
            log_path: PathBuf::from("ryba-refine-i0.jsonl"),
            input: crate::sock::InputMode::Live,
            queued_messages: 0,
        });
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(state)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.wanted.is_none());
        assert!(app.session.is_showing("ryba/refine-i0"));
    }

    #[test]
    fn n_cannot_send_chat_to_the_previous_session_while_it_waits() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_messages(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.selection = Selection::Row(4);

        run_messages(
            &mut surface,
            &mut app,
            vec![key('n'), key('h'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();

        assert_eq!(
            sink.0,
            vec![Action::TicketCreate {
                repo: "ryba".to_string()
            }]
        );
        assert_eq!(app.session.task_id(), None);
    }

    #[test]
    fn an_alternate_screen_failure_restores_the_terminal() {
        let restored = Cell::new(false);
        let error = enable_terminal_with(
            || Ok(()),
            || Err(std::io::Error::other("enter failed")),
            || {
                restored.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(restored.get());
        assert!(error.to_string().contains("enter failed"));
    }

    #[test]
    fn terminal_restore_attempts_both_steps_and_reports_both_errors() {
        let disabled = Cell::new(false);
        let error = restore_terminal_with(
            || Err(std::io::Error::other("leave failed")),
            || {
                disabled.set(true);
                Err(std::io::Error::other("disable failed"))
            },
        )
        .unwrap_err();
        let message = error.to_string();

        assert!(disabled.get());
        assert!(message.contains("leave failed"));
        assert!(message.contains("disable failed"));
    }

    #[test]
    fn key_five_opens_the_settings_view_and_updates_the_shell_text() {
        let mut app = App::default();
        let mut sink = FakeSink::default();

        assert!(app.handle_key(
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
            &mut sink
        ));

        assert_eq!(app.view, View::Settings);
        let text = render_to_string(&mut app);
        assert!(text.contains("5 settings"));
        assert!(text.contains("1/2/3/4/5 views"));
    }

    #[test]
    fn a_failed_settings_send_allows_a_later_successful_action() {
        let mut app = App {
            view: View::Settings,
            state: Some(crate::tui::pipeline::sample_view()),
            ..App::default()
        };
        app.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut FailSink,
        );
        let mut sink = FakeSink::default();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut sink,
        );
        assert_eq!(sink.0.len(), 1);
    }

    #[test]
    fn a_disconnect_allows_a_later_settings_action() {
        let mut app = App {
            view: View::Settings,
            state: Some(crate::tui::pipeline::sample_view()),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut sink,
        );
        app.mark_disconnected("lost".to_string());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut sink,
        );
        assert_eq!(sink.0.len(), 2);
    }

    #[test]
    fn the_help_overlay_lists_settings_actions() {
        let mut app = App {
            help: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        for entry in [
            "settings scope",
            "settings role",
            "select settings field",
            "edit settings value",
            "save / reload settings",
            "remove repository override",
            "home / cancel settings edit",
        ] {
            assert!(text.contains(entry), "the help misses {entry}");
        }
    }
}
