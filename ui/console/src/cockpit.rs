use std::path::Path;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use std::io::BufRead;
use std::sync::mpsc;
use std::time::Duration;

use crate::control::{self, Envelope};
use crate::factory::FactoryPaths;
use crate::ids;

#[derive(Debug, Clone)]
pub struct CockpitState {
    pub status: Option<serde_json::Value>,
    pub records: Vec<String>,
    pub message: String,
    pub selected: usize,
}

pub fn run(root: &Path) -> Result<()> {
    let paths = FactoryPaths::open(root)?;
    paths.ensure()?;
    let (status_tx, status_rx) = mpsc::channel::<serde_json::Value>();
    let (records_tx, records_rx) = mpsc::channel::<String>();
    spawn_follow(&paths, status_tx, records_tx);

    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut state = CockpitState {
        status: None,
        records: Vec::new(),
        message: String::new(),
        selected: 0,
    };
    let socket = paths.socket();
    let result = event_loop(&mut terminal, &mut state, &status_rx, &records_rx, &socket);
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    result
}

fn spawn_follow(
    paths: &FactoryPaths,
    status_tx: mpsc::Sender<serde_json::Value>,
    records_tx: mpsc::Sender<String>,
) {
    let socket = paths.socket();
    let follow_socket = socket.clone();
    let follow_status = status_tx.clone();
    std::thread::spawn(move || {
        let socket = follow_socket;
        let status_tx = follow_status;
        let mut backoff = Duration::from_secs(1);
        loop {
            let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) else {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            };
            backoff = Duration::from_secs(1);
            let envelope = Envelope {
                v: control::PROTOCOL_VERSION,
                id: ids::new_id(),
                method: "events.follow".into(),
                params: serde_json::json!({}),
            };
            use std::io::Write;
            let mut line = serde_json::to_string(&envelope).unwrap_or_default();
            line.push('\n');
            if stream.write_all(line.as_bytes()).is_err() {
                continue;
            }
            let reader = std::io::BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.contains("\"in_reply_to\"") {
                    if line.contains("\"revision\"") {
                        let _ = status_tx.send(serde_json::json!({"connected": true}));
                    }
                    continue;
                }
                if line.contains("\"task_transition\"")
                    || line.contains("\"task_created\"")
                    || line.contains("\"source_batch\"")
                {
                    let _ = records_tx.send(line);
                }
            }
            let _ = status_tx.send(serde_json::json!({"connected": false}));
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    {
        let socket = socket.clone();
        std::thread::spawn(move || loop {
            if let Ok(reply) = control::request(&socket, "status", serde_json::json!({})) {
                if reply.ok {
                    let _ = status_tx.send(reply.result.unwrap_or_default());
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        });
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut CockpitState,
    status_rx: &mpsc::Receiver<serde_json::Value>,
    records_rx: &mpsc::Receiver<String>,
    socket: &std::path::Path,
) -> Result<()> {
    let mut last_status = std::time::Instant::now() - Duration::from_secs(5);
    loop {
        while let Ok(update) = status_rx.try_recv() {
            if update.get("connected").is_some() {
                state.message = if update["connected"].as_bool().unwrap_or(false) {
                    "daemon connected".into()
                } else {
                    "daemon disconnected; retrying".into()
                };
            } else {
                state.status = Some(update);
            }
        }
        while let Ok(record) = records_rx.try_recv() {
            state.records.push(record);
            if state.records.len() > 200 {
                let drop = state.records.len() - 200;
                state.records.drain(..drop);
            }
        }
        if last_status.elapsed() >= Duration::from_secs(2) {
            if let Ok(reply) = control::request(socket, "status", serde_json::json!({})) {
                if reply.ok {
                    state.status = reply.result;
                }
            }
            last_status = std::time::Instant::now();
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match (key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
                        (KeyCode::Char('c'), true)
                        | (KeyCode::Char('q'), false)
                        | (KeyCode::Esc, false) => return Ok(()),
                        (KeyCode::Char('s'), false) => {
                            let count = task_count(state);
                            if count > 0 {
                                state.selected = (state.selected + 1) % count;
                            }
                        }
                        (KeyCode::Char(c @ '1'..='9'), false) => {
                            let idx = (c as u8 - b'1') as usize;
                            if idx < task_count(state) {
                                state.selected = idx;
                            }
                        }
                        (KeyCode::Enter, false) => {
                            if let Some(task) = selected_task(state) {
                                match control::request(
                                    socket,
                                    "task.submit",
                                    serde_json::json!({"task": task}),
                                ) {
                                    Ok(reply) => {
                                        state.message = if reply.ok {
                                            format!("submitted {task}")
                                        } else {
                                            format!(
                                                "submit refused: {}",
                                                reply.error.map(|e| e.message).unwrap_or_default()
                                            )
                                        };
                                    }
                                    Err(err) => state.message = format!("{err:#}"),
                                }
                            }
                        }
                        (KeyCode::Char('c'), false) => {
                            if let Some(task) = selected_task(state) {
                                let _ = control::request(
                                    socket,
                                    "task.cancel",
                                    serde_json::json!({"task": task}),
                                );
                                state.message = format!("cancel requested for {task}");
                            }
                        }
                        (KeyCode::Char('r'), false) => {
                            if let Some(task) = selected_task(state) {
                                let _ = control::request(
                                    socket,
                                    "task.retry",
                                    serde_json::json!({"task": task}),
                                );
                                state.message = format!("retry requested for {task}");
                            }
                        }
                        (KeyCode::Char('C'), false) => {
                            if let Some(task) = selected_task(state) {
                                let _ = control::request(
                                    socket,
                                    "task.complete",
                                    serde_json::json!({"task": task}),
                                );
                                state.message = format!("completed {task}");
                            }
                        }
                        (KeyCode::Char('f'), false) => {
                            if let Some(task) = selected_task(state) {
                                let _ = control::request(
                                    socket,
                                    "task.fail",
                                    serde_json::json!({"task": task}),
                                );
                                state.message = format!("failed {task}");
                            }
                        }
                        (KeyCode::Char('p'), false) => {
                            let _ = control::request(socket, "pause", serde_json::json!({}));
                            state.message = "paused".into();
                        }
                        (KeyCode::Char('P'), false) => {
                            let _ = control::request(socket, "resume", serde_json::json!({}));
                            state.message = "resumed".into();
                        }
                        _ => {}
                    }
                }
            }
        }
        terminal.draw(|f| crate::view::draw_cockpit(f, state))?;
    }
}

fn task_count(state: &CockpitState) -> usize {
    state
        .status
        .as_ref()
        .and_then(|s| s.get("tasks"))
        .and_then(|t| t.as_array())
        .map(|t| t.len())
        .unwrap_or(0)
}

fn selected_task(state: &CockpitState) -> Option<String> {
    state
        .status
        .as_ref()?
        .get("tasks")?
        .as_array()?
        .get(state.selected)?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

pub fn print_status(status: &serde_json::Value) {
    let factory = status["factory_id"].as_str().unwrap_or("?");
    let paused = status["paused"].as_bool().unwrap_or(false);
    let stale = status["stale_source"].as_bool().unwrap_or(false);
    let revision = status["revision"].as_u64().unwrap_or(0);
    println!("factory {factory}  revision {revision}  paused {paused}  stale {stale}");
    if let Some(nodes) = status["nodes"].as_array() {
        for node in nodes {
            println!(
                "  {:<12} {:<8} {:<8} limit {} active {}{}",
                node["name"].as_str().unwrap_or("?"),
                node["agent"].as_str().unwrap_or("?"),
                node["exec"].as_str().unwrap_or("?"),
                node["limit"].as_u64().unwrap_or(1),
                node["active"].as_u64().unwrap_or(0),
                if node["paused"].as_bool().unwrap_or(false) {
                    "  [paused]"
                } else {
                    ""
                },
            );
        }
    }
    if let Some(tasks) = status["tasks"].as_array() {
        for task in tasks {
            println!(
                "  {:<40} {:<12} {}",
                task["id"].as_str().unwrap_or("?"),
                task["state"].as_str().unwrap_or("?"),
                task["title"].as_str().unwrap_or(""),
            );
        }
    }
}
