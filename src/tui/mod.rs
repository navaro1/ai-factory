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
//! The shell draws the pipeline view itself. Later chunks replace the session
//! and inbox placeholders.

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
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
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

use crate::sock::{Client, Push, StateView};
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
}

impl App {
    /// Apply one state push.
    ///
    /// The push proves the connection, clamps the selection to the new row
    /// count, and drops an expired toast.
    fn apply_state(&mut self, view: StateView) {
        let count = pipeline::rows(&view).len();
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

    /// Apply one key press. Returns false when the app wants to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Esc if self.help => self.help = false,
            _ if self.help => {}
            KeyCode::Char('1') => self.view = View::Pipeline,
            KeyCode::Char('2') => self.view = View::Session,
            KeyCode::Char('3') => self.view = View::Inbox,
            KeyCode::Char('j') if self.view == View::Pipeline => pipeline::move_selection(self, 1),
            KeyCode::Char('k') if self.view == View::Pipeline => pipeline::move_selection(self, -1),
            _ => {}
        }
        true
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
    fn draw(&mut self, app: &App) -> Result<()>;
}

/// The drawing surface over the real terminal.
struct RealTerminal {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl Surface for RealTerminal {
    fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal
            .draw(|frame| render(frame, app))
            .context("cannot draw a frame")?;
        Ok(())
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
    run_loop(&mut surface, &mut app, rx.into_iter())
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
/// Every handled message leads to one draw. A quiet channel causes no draw.
fn run_loop(
    surface: &mut impl Surface,
    app: &mut App,
    msgs: impl Iterator<Item = Msg>,
) -> Result<()> {
    for msg in msgs {
        match msg {
            Msg::Key(key) => {
                if !app.handle_key(key) {
                    return Ok(());
                }
            }
            Msg::State(view) => app.apply_state(view),
            Msg::Connected => app.connected = true,
            Msg::Disconnected(reason) => app.mark_disconnected(reason),
            Msg::Input(reason) => return Err(anyhow!("terminal input stopped: {reason}")),
            Msg::Resize => {}
        }
        surface.draw(app)?;
    }
    Ok(())
}

/// Draw the whole shell into the frame.
///
/// Later chunks replace the session and inbox placeholders.
fn render(f: &mut Frame, app: &App) {
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
    draw_header(f, app, header);
    match app.view {
        View::Pipeline => pipeline::draw(f, app, body),
        View::Session => draw_placeholder(f, "session", body),
        View::Inbox => draw_placeholder(f, "inbox", body),
    }
    draw_toast(f, app, body);
    draw_footer(f, footer);
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

/// Draw the key hints of this chunk.
fn draw_footer(f: &mut Frame, area: Rect) {
    let hints = Span::styled(" 1/2/3 views   j/k move   ? help   q quit ", THEME.dim());
    f.render_widget(Paragraph::new(Line::from(hints)), area);
}

/// Draw a placeholder for a view that a later chunk wires in.
fn draw_placeholder(f: &mut Frame, name: &str, area: Rect) {
    let text = Span::styled(format!(" the {name} view is not wired yet "), THEME.dim());
    f.render_widget(Paragraph::new(Line::from(text)), area);
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

/// Draw the help overlay over the whole frame.
fn draw_help(f: &mut Frame, area: Rect) {
    let panel = centered(40, 9, area);
    f.render_widget(Clear, panel);
    let rows: [(&str, &str); 7] = [
        ("1", "pipeline view"),
        ("2", "session view"),
        ("3", "inbox view"),
        ("j", "move down"),
        ("k", "move up"),
        ("?", "toggle this help"),
        ("q", "quit"),
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
    use crate::model::Stage;
    use crate::tui::pipeline::render_to_string;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A surface that counts draws.
    struct CountingSurface {
        draws: usize,
    }

    impl Surface for CountingSurface {
        fn draw(&mut self, _app: &App) -> Result<()> {
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
        fn draw(&mut self, _app: &App) -> Result<()> {
            self.draws.fetch_add(1, Ordering::SeqCst);
            self.drew.send(()).context("cannot report a test draw")?;
            Ok(())
        }
    }

    /// A key press of one character.
    fn key(character: char) -> Msg {
        Msg::Key(KeyEvent::new(
            KeyCode::Char(character),
            crossterm::event::KeyModifiers::empty(),
        ))
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
            run_loop(&mut surface, &mut app, msg_rx.into_iter())
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
        run_loop(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view()), key('q')].into_iter(),
        )
        .unwrap();
        assert_eq!(surface.draws, 1);
    }

    #[test]
    fn keys_switch_views_and_toggle_help() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App::default();
        run_loop(&mut surface, &mut app, vec![key('3'), key('?')].into_iter()).unwrap();
        assert_eq!(app.view, View::Inbox);
        assert!(app.help);
        // While the overlay is open, the view keys do nothing.
        run_loop(&mut surface, &mut app, vec![key('1')].into_iter()).unwrap();
        assert_eq!(app.view, View::Inbox);
        // Escape closes the overlay, then the view keys work again.
        run_loop(
            &mut surface,
            &mut app,
            vec![Msg::Key(KeyEvent::new(
                KeyCode::Esc,
                crossterm::event::KeyModifiers::empty(),
            ))]
            .into_iter(),
        )
        .unwrap();
        assert!(!app.help);
        run_loop(&mut surface, &mut app, vec![key('1')].into_iter()).unwrap();
        assert_eq!(app.view, View::Pipeline);
        run_loop(&mut surface, &mut app, vec![key('?')].into_iter()).unwrap();
        assert!(app.help);
    }

    #[test]
    fn a_state_push_connects_and_clamps_the_selection() {
        let mut surface = CountingSurface { draws: 0 };
        let mut app = App {
            selection: Selection::Row(10_000),
            ..App::default()
        };
        run_loop(
            &mut surface,
            &mut app,
            vec![Msg::State(crate::tui::pipeline::sample_view())].into_iter(),
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
        let app = App::default();
        let text = render_to_string(&app);
        assert!(text.contains("connecting to the daemon"));
        assert!(text.contains("down"));
        assert!(text.contains("waiting for the first state push"));
    }

    #[test]
    fn a_disconnected_app_keeps_the_last_state_under_the_banner() {
        let app = App {
            state: Some(crate::tui::pipeline::sample_view()),
            connected: false,
            disconnect: Some("no such file".to_string()),
            ..App::default()
        };
        let text = render_to_string(&app);
        assert!(text.contains("daemon disconnected - reconnecting"));
        // The last state stays visible below the banner.
        assert!(text.contains("refine"));
        assert!(text.contains("i142 queued"));
    }

    #[test]
    fn the_help_overlay_lists_the_keys_of_this_chunk() {
        let app = App {
            help: true,
            ..App::default()
        };
        let text = render_to_string(&app);
        assert!(text.contains("keys"));
        assert!(text.contains("pipeline view"));
        assert!(text.contains("move down"));
        assert!(text.contains("quit"));

        let closed = App::default();
        let text = render_to_string(&closed);
        assert!(!text.contains("pipeline view"));
    }

    #[test]
    fn a_fresh_toast_shows_and_an_expired_one_does_not() {
        let fresh = App {
            toast: Some(("sent".to_string(), Instant::now() + Duration::from_secs(4))),
            ..App::default()
        };
        assert!(render_to_string(&fresh).contains("sent"));
        let expired = App {
            toast: Some(("sent".to_string(), Instant::now())),
            ..App::default()
        };
        assert!(!render_to_string(&expired).contains("sent"));
    }

    #[test]
    fn the_header_shows_the_pause_flag_and_socket_state() {
        let mut state = crate::tui::pipeline::sample_view();
        state.paused.global = true;
        state.paused.stages = vec![Stage::Refine];
        let app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
        assert!(text.contains("live"));
        assert!(text.contains("paused"));
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
