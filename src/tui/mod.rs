//! Holds the app shell and the terminal UI event loop.
//!
//! Three threads cooperate:
//!
//! - The key reader thread turns crossterm events into `Msg` values.
//! - The socket reader thread connects to the daemon, turns pushes into
//!   `Msg` values, and reconnects with `backoff_delay`.
//! - The main thread owns the `App`, blocks on the one channel, and draws
//!   one frame per message. Nothing draws on a timer.
//!
//! The shell draws the pipeline view itself and drives the session and
//! inbox views through their contracts: `show`, `on_redraw`, `draw`, and
//! `handle_key` for the session, and `observe`, `draw`, and `handle_key`
//! for the inbox. The loop never wakes on a timer, so it does not call the
//! session `poll`; the next message triggers the redraw instead.
//!
//! A view that holds the keyboard for text input eats every key first.
//! Thus a typed `q` or `!` lands in the text and never reaches the global
//! handler. The quit chords ctrl-q and ctrl-c work from anywhere.

pub mod inbox;
pub mod pipeline;
pub mod session;
pub mod theme;
pub mod transcript;

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
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

use crate::decisions::DecisionKind;
use crate::model::{ItemKind, Stage};
use crate::sock::{Action, Client, Push, StateView, TaskView};
use inbox::{ActionSink, Inbox};
use session::SessionView;
use theme::THEME;

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
    /// The socket reader reached the daemon.
    Connected,
    /// The daemon went away, with the reason for the banner.
    Disconnected(String),
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
}

impl Confirm {
    /// Send the confirmed action and toast what was sent.
    fn send(self, app: &mut App, sink: &mut impl ActionSink) {
        match self {
            Confirm::Abort { task } => emit(
                app,
                sink,
                Action::Abort { task: task.clone() },
                format!("sent abort {task}"),
            ),
            Confirm::Go { repo, prs } => emit(
                app,
                sink,
                Action::Go {
                    repo: repo.clone(),
                    prs: prs.clone(),
                },
                format!("sent release {repo} {}", pr_text(&prs)),
            ),
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
    fn apply_state(&mut self, view: StateView) {
        let count = pipeline::rows(&view).len();
        self.inbox.observe(&view);
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
        self.selection = match self.selection {
            Selection::Row(index) if count > 0 => Selection::Row(index.min(count - 1)),
            _ => Selection::None,
        };
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
    /// The session view always owns its input bar. The inbox owns it while
    /// a reason input is open.
    fn text_focus(&self) -> bool {
        match self.view {
            View::Session => true,
            View::Inbox => self.inbox.typing(),
            View::Pipeline => false,
        }
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
    /// The quit chords work in every state. A view that holds the keyboard
    /// for text input eats every other key first, so a typed `q` or `!`
    /// never reaches the global handler.
    fn handle_key(&mut self, key: KeyEvent, sink: &mut impl ActionSink) -> bool {
        if quit_chord(key) {
            return false;
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
                if let Some(action) = self.session.handle_key(key, self.session_page()) {
                    let text = match &action {
                        Action::Chat { task, .. } => format!("sent chat {task}"),
                        Action::Abort { task } => format!("sent abort {task}"),
                        _ => "sent".to_string(),
                    };
                    emit(self, sink, action, text);
                }
                if key.code == KeyCode::Esc {
                    self.view = View::Pipeline;
                }
            }
            View::Inbox if self.inbox.typing() => {
                self.inbox_dispatch(key, sink);
            }
            View::Inbox => {
                if digit_key(key) && self.inbox_row_owns(key) {
                    self.inbox_dispatch(key, sink);
                    return true;
                }
                match key.code {
                    KeyCode::Char('1') => self.view = View::Pipeline,
                    KeyCode::Char('2') => self.view = View::Session,
                    KeyCode::Char('3') => {}
                    KeyCode::Char('!') => self.open_inbox_oldest(),
                    KeyCode::Char('?') => self.help = true,
                    KeyCode::Esc => self.view = View::Pipeline,
                    _ => {
                        self.inbox_dispatch(key, sink);
                    }
                }
            }
            View::Pipeline => match key.code {
                KeyCode::Char('1') => {}
                KeyCode::Char('2') => self.view = View::Session,
                KeyCode::Char('3') => self.view = View::Inbox,
                KeyCode::Char('!') => self.open_inbox_oldest(),
                KeyCode::Char('?') => self.help = true,
                KeyCode::Char('j') => pipeline::move_selection(self, 1),
                KeyCode::Char('k') => pipeline::move_selection(self, -1),
                _ => pipeline::handle_key(self, key, sink),
            },
        }
        true
    }

    /// Apply one key to the confirmation prompt.
    ///
    /// `y` sends the confirmed action. Escape cancels it. Every other key
    /// waits, so the prompt cannot be dismissed by accident.
    fn confirm_key(&mut self, key: KeyEvent, sink: &mut impl ActionSink) {
        match key.code {
            KeyCode::Char('y') => {
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
    /// session view of that task.
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
        if let inbox::InboxOutcome::OpenSession(task) = outcome {
            self.session_task = Some(task);
            self.wanted = None;
            self.view = View::Session;
            self.show_session_task();
        }
        sent
    }
}

/// True when the key combination quits the UI from anywhere.
fn quit_chord(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
}

/// True when the key is one of the digit keys 1 through 9.
fn digit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('1'..='9'))
}

/// Send one action to the sink and toast what was sent.
fn emit(app: &mut App, sink: &mut impl ActionSink, action: Action, toast: String) {
    sink.send_action(action);
    app.show_toast(&toast);
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
    fn send_action(&mut self, action: Action) {
        self.sent += 1;
        self.inner.send_action(action);
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
        self.terminal
            .draw(|frame| render(frame, app))
            .context("cannot draw a frame")?;
        Ok(())
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
    fn send_action(&mut self, action: Action) {
        if self.client.is_none() {
            self.client = self.connect();
        }
        let Some(client) = self.client.as_mut() else {
            return;
        };
        if client.send(&action).is_ok() {
            return;
        }
        // The daemon went away. One fresh connection gets one new try.
        self.client = self.connect();
        if let Some(client) = self.client.as_mut() {
            if let Err(error) = client.send(&action) {
                eprintln!("tui: cannot send the action to the daemon: {error:#}");
            }
        }
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
    spawn_key_thread(tx.clone());
    spawn_socket_thread(tx, socket.to_path_buf());
    let mut app = App::default();
    let mut link = DaemonLink {
        socket: socket.to_path_buf(),
        client: None,
    };
    run_loop(&mut surface, &mut app, rx.into_iter(), &mut link)
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
                                    Err(error) => {
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

/// Consume messages until the app quits or the channel dies.
///
/// Every handled message leads to one draw. A quiet channel causes no
/// draw. In the session view the call advances the log tail before the
/// draw, so the transcript keeps up with the log file.
fn run_loop(
    surface: &mut impl Surface,
    app: &mut App,
    msgs: impl Iterator<Item = Msg>,
    sink: &mut impl ActionSink,
) -> Result<()> {
    for msg in msgs {
        match msg {
            Msg::Key(key) => {
                if !app.handle_key(key, sink) {
                    return Ok(());
                }
            }
            Msg::State(view) => app.apply_state(view),
            Msg::Connected => app.connected = true,
            Msg::Disconnected(reason) => app.mark_disconnected(reason),
            Msg::Input(reason) => return Err(anyhow!("terminal input stopped: {reason}")),
            Msg::Resize => {}
        }
        if app.view == View::Session {
            app.session.on_redraw(Instant::now());
        }
        surface.draw(app)?;
    }
    Ok(())
}

/// Draw the whole shell into the frame.
///
/// The call records the body rectangle on the app, so the next key press
/// knows the visible transcript height of the session view.
fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
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
        View::Pipeline => pipeline::draw(f, app, body),
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
                let now = inbox::now_ms().unwrap_or(0);
                inbox::draw(f, body, state, &app.inbox, now);
            }
        }
    }
    draw_toast(f, app, body);
    draw_footer(f, app, footer);
    draw_confirm(f, app, area);
    if app.help {
        draw_help(f, area);
    }
}

/// Draw the title row: app name, view tabs, pause flag, and socket state.
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let sides = Layout::horizontal([Constraint::Min(20), Constraint::Length(26)]).split(area);
    let title = Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD);
    let mut tabs = vec![Span::styled(" aif ", title)];
    tabs.push(tab_span("1", "pipeline", app.view == View::Pipeline));
    tabs.push(tab_span("2", "session", app.view == View::Session));
    tabs.push(tab_span("3", "inbox", app.view == View::Inbox));
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
        " 1/2/3 views   j/k move   ! inbox   ? help   ctrl-q quit ",
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
    let panel = centered(44, 19, area);
    f.render_widget(Clear, panel);
    let rows: [(&str, &str); 17] = [
        ("1 2 3", "switch view"),
        ("esc", "home view"),
        ("!", "inbox, oldest decision"),
        ("j k", "move the selection"),
        ("?", "toggle this help"),
        ("ctrl-q", "quit"),
        ("+ -", "stage limit / repo lane"),
        ("p P", "pause scope / all"),
        ("r n", "refine / new ticket"),
        ("x R", "abort / retry"),
        ("space g s", "stack / release / policy"),
        ("enter", "send the chat message"),
        ("ctrl-x", "abort the shown task"),
        ("PageUp PageDown", "scroll the transcript"),
        ("End", "follow the tail"),
        ("y n i t c", "inbox answers"),
        ("g", "fire the release gate"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, text)| {
            Line::from(vec![
                Span::styled(
                    format!(" {key} "),
                    Style::default()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {text}")),
            ])
        })
        .collect();
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

    /// An action sink that records what it sent.
    #[derive(Default)]
    struct FakeSink(Vec<Action>);

    impl ActionSink for FakeSink {
        fn send_action(&mut self, action: Action) {
            self.0.push(action);
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
            run_loop(&mut surface, &mut app, msg_rx.into_iter(), &mut sink)
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
    fn the_loop_stops_on_q_without_a_last_draw() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_loop(
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
        run_loop(
            &mut surface,
            &mut app,
            vec![key('3'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox);
        assert!(app.help);
        // While the overlay is open, the view keys do nothing.
        run_loop(
            &mut surface,
            &mut app,
            vec![key('1')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Inbox);
        // Escape closes the overlay, then the view keys work again.
        run_loop(
            &mut surface,
            &mut app,
            vec![key_code(KeyCode::Esc)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(!app.help);
        run_loop(
            &mut surface,
            &mut app,
            vec![key('1')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Pipeline);
        run_loop(
            &mut surface,
            &mut app,
            vec![key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert!(app.help);
    }

    #[test]
    fn a_state_push_connects_and_clamps_the_selection() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(10_000),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_loop(
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
        assert!(text.contains("i142 queued"));
    }

    #[test]
    fn the_help_overlay_lists_the_keys_of_this_chunk() {
        let mut app = App {
            help: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("keys"));
        for entry in [
            "switch view",
            "oldest decision",
            "ctrl-q",
            "PageUp PageDown",
            "PageDown",
            "End",
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
        state.paused.stages = vec![Stage::Refine];
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
        run_loop(
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
        run_loop(
            &mut surface,
            &mut app,
            vec![key('q'), key('?')].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.session.input_text(), "q?");
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
        run_loop(
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
        run_loop(
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
    fn bang_enters_the_inbox_and_selects_the_oldest_row() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![
            permission_decision("req-1", 9_000),
            permission_decision("req-2", 2_000),
        ];
        run_loop(
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
    fn enter_on_an_inbox_row_opens_the_task_session() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        let mut state = crate::tui::pipeline::sample_view();
        state.decisions = vec![permission_decision("req-1", 1_000)];
        run_loop(
            &mut surface,
            &mut app,
            vec![Msg::State(state), key('3'), key_code(KeyCode::Enter)].into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/implement-i140"));
        assert!(app.session.is_showing("borsuk/implement-i140"));
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
    fn y_confirms_the_aborting_of_the_selected_task() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(2),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_loop(
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
        run_loop(
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
    fn y_confirms_the_release_of_the_stacked_batch() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(18),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_loop(
            &mut surface,
            &mut app,
            vec![
                Msg::State(crate::tui::pipeline::sample_view()),
                key('g'),
                key('y'),
            ]
            .into_iter(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.0,
            vec![Action::Go {
                repo: "borsuk".to_string(),
                prs: vec![3]
            }]
        );
        assert!(app.confirm.is_none());
        assert!(app.visible_toast().is_some());
    }

    #[test]
    fn enter_sends_the_session_chat_action() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        let mut sink = FakeSink::default();
        run_loop(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
            &mut sink,
        )
        .unwrap();
        app.session_task = Some("borsuk/refine-i142".to_string());
        app.show_session_task();
        app.view = View::Session;
        run_loop(
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
    fn r_follows_the_new_refine_task_on_the_next_push() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(8),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_loop(
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
        });
        run_loop(
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
    fn n_follows_the_ticket_create_task_on_the_next_push() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(4),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        run_loop(
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
        });
        run_loop(
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
}
