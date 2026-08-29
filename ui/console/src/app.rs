use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;

use crate::actions;
use crate::status;
use crate::view::Overlay;

pub struct App {
    pub report: Option<status::StatusReport>,
    pub selected: usize,
    pub overlay: Option<Overlay>,
    pub message: String,
    pub session: String,
}

enum Flow {
    Continue,
    Quit,
}

pub fn run() -> Result<()> {
    let root = status::repo_root()?;
    let session = status::session_name(&root);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        if let Ok(report) = status::report() {
            if tx.send(report).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });

    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        report: None,
        selected: 0,
        overlay: None,
        message: String::new(),
        session,
    };

    let result = event_loop(&mut terminal, &mut app, &rx);

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &mpsc::Receiver<status::StatusReport>,
) -> Result<()> {
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        while let Ok(report) = rx.try_recv() {
            if app.selected >= report.panes.len() {
                app.selected = 0;
            }
            app.report = Some(report);
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && matches!(handle_key(app, &key), Flow::Quit) {
                    return Ok(());
                }
            }
        }

        if last_draw.elapsed() >= Duration::from_millis(250) {
            terminal.draw(|f| crate::view::draw(f, app))?;
            last_draw = Instant::now();
        }
    }
}

fn handle_key(app: &mut App, key: &KeyEvent) -> Flow {
    if app.overlay.is_some() {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('l') => app.overlay = None,
            KeyCode::PageDown | KeyCode::Char('j') | KeyCode::Down => {
                if let Some(overlay) = app.overlay.as_mut() {
                    overlay.scroll = overlay.scroll.saturating_add(1);
                }
            }
            KeyCode::PageUp | KeyCode::Char('k') | KeyCode::Up => {
                if let Some(overlay) = app.overlay.as_mut() {
                    overlay.scroll = overlay.scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
        return Flow::Continue;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Flow::Quit,
        KeyCode::Char('s') => {
            let count = app.report.as_ref().map(|r| r.panes.len()).unwrap_or(0);
            if count > 0 {
                app.selected = (app.selected + 1) % count;
            }
        }
        KeyCode::Enter => press_enter_in_selected(app, false),
        KeyCode::Char('r') => press_enter_in_selected(app, true),
        KeyCode::Char('l') => open_scrollback(app),
        KeyCode::Char(c @ '1'..='9') => {
            let count = app.report.as_ref().map(|r| r.panes.len()).unwrap_or(0);
            let idx = (c as u8 - b'1') as usize;
            if idx < count {
                app.selected = idx;
            }
        }
        _ => {}
    }
    Flow::Continue
}

fn press_enter_in_selected(app: &mut App, force: bool) {
    let Some(target) = selected_pane(app) else {
        return;
    };
    if !force && target.state != "draft waiting" {
        app.message = format!("{} is {}, not draft waiting", target.role, target.state);
        return;
    }
    match actions::press_enter(&app.session, &target.pane) {
        Ok(()) => app.message = format!("sent enter to {}", target.role),
        Err(err) => app.message = format!("enter failed: {err}"),
    }
}

fn open_scrollback(app: &mut App) {
    let Some(target) = selected_pane(app) else {
        return;
    };
    match actions::dump_scrollback(&app.session, &target.pane) {
        Ok(text) => {
            app.overlay = Some(Overlay {
                title: format!("{} {} (j/k scroll, q close)", target.pane, target.role),
                text,
                scroll: 0,
            })
        }
        Err(err) => app.message = format!("scrollback failed: {err}"),
    }
}

struct SelectedPane {
    pane: String,
    role: String,
    state: String,
}

fn selected_pane(app: &App) -> Option<SelectedPane> {
    let report = app.report.as_ref()?;
    let pane = report.panes.get(app.selected)?;
    Some(SelectedPane {
        pane: pane.pane.clone(),
        role: pane.class.role.clone(),
        state: pane.class.state.clone(),
    })
}
