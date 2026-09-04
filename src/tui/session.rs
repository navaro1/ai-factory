//! Draws the session view and handles steering and scrolling.
//!
//! The session view is where a human watches one running agent and steers
//! it. The view tails the task's log file itself, using the
//! [`TaskView::log_path`] from the state push; the daemon pushes no
//! transcripts over the socket. The view parses each new log line with
//! [`crate::tui::transcript`] and keeps the last [`RING_CAP`] items in a
//! ring buffer.
//!
//! The daemon appends one user line to the log for every chat message it
//! accepts, so the transcript keeps what the human typed across task
//! switches, refocus, and restarts. The runner itself echoes no typed
//! message into its output stream; the daemon's line is the only record.
//! The view carries no local echo: the log tail delivers the user line at
//! the next poll.
//!
//! The input bar states what a typed message will do. The daemon says the
//! mode with [`TaskView::input`]; the bar renders a hint for that mode. A
//! closed input takes no message: the bar shows the daemon's reason, and
//! [`SessionView::handle_key`] sends nothing.
//!
//! The shell in `mod.rs` hosts this view. The contract is:
//!
//! 1. Create one [`SessionView::new`] at startup.
//! 2. Call [`SessionView::show`] with the chosen [`TaskView`] whenever the
//!    operator opens the view or the pushed state renames the task.
//! 3. Call [`SessionView::on_redraw`] right before each draw. If the main
//!    loop also wakes without a message, call [`SessionView::poll`] with
//!    the same instant; it re-reads the log at most once per
//!    [`POLL_INTERVAL`].
//! 4. Pass unhandled keys to [`SessionView::handle_key`]. It returns the
//!    [`Action`] to send over the control socket, or none.
//! 5. Draw with [`SessionView::draw`] inside the pane the shell reserves.
//!
//! The view holds a chat focus flag. A focused bar types; an unfocused bar
//! renders dim and swallows typing, so the shell can use plain letters for
//! its own keys. The shell releases the focus with `esc` or `tab` and
//! takes it back with `i` or `enter`. The header shows one tab per live
//! session when the daemon pushes more than one; the shell switches tabs
//! with `h` and `l`.
//!
//! A bar that cannot take text holds no keyboard. When the bar is
//! unfocused, or disabled with no task or a closed input, the shell keeps
//! its own view keys alive: `1` through `5` switch views and `?` opens
//! the help overlay.
//!
//! The view answers with [`Action::Chat`] and [`Action::Abort`] only. The
//! shell owns view switching and every other global key.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use serde_json::Value;

use super::theme::THEME;
use crate::decisions::{Decision, DecisionKind};
use crate::sock::{Action, InputMode, RoleBindingView, TaskView};
use crate::tui::transcript::{self, Entry};

/// How many parsed items the transcript ring buffer keeps.
pub const RING_CAP: usize = 2000;

/// The shortest gap between two file-change polls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The tallest the inline ask block gets before it is cut.
const ASK_MAX_ROWS: usize = 8;

/// The row count of the input bar, borders included.
const INPUT_ROWS: u16 = 3;

/// A bounded first-in first-out queue that drops its oldest items.
///
/// The transcript ring keeps the newest [`RING_CAP`] parsed items. The
/// buffer counts what it dropped, so the view can say so instead of
/// silently hiding history.
#[derive(Debug)]
pub struct Ring<T> {
    items: VecDeque<T>,
    cap: usize,
    dropped: u64,
}

impl<T> Ring<T> {
    /// Create an empty ring that keeps at most `cap` items.
    ///
    /// A cap of 0 keeps nothing.
    pub fn new(cap: usize) -> Self {
        Ring {
            items: VecDeque::with_capacity(cap.min(4096)),
            cap,
            dropped: 0,
        }
    }

    /// Push one item and drop the oldest one when the ring is full.
    pub fn push(&mut self, item: T) {
        if self.cap == 0 {
            self.dropped += 1;
            return;
        }
        if self.items.len() == self.cap {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }

    /// How many items the ring holds now.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when the ring holds no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many items the ring dropped since creation or the last clear.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Iterate the items, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }

    /// Drop every item. The dropped count resets too.
    pub fn clear(&mut self) {
        self.items.clear();
        self.dropped = 0;
    }
}

/// One open log file that yields only the lines appended after each read.
#[derive(Debug)]
struct LogTailer {
    path: PathBuf,
    file: Option<File>,
    offset: u64,
    partial: Vec<u8>,
    reported: bool,
}

impl LogTailer {
    fn new(path: PathBuf) -> Self {
        LogTailer {
            path,
            file: None,
            offset: 0,
            partial: Vec::new(),
            reported: false,
        }
    }

    /// Read every complete line appended since the last call into `out`.
    ///
    /// A missing log file is normal before the task starts; the tailer
    /// retries on every call. Any other read error prints once and stops
    /// this read; the next call reopens the file.
    ///
    /// The return value is true when the log was truncated or replaced.
    /// The caller then restarts its transcript, because the old history no
    /// longer exists in the file.
    fn read_lines(&mut self, out: &mut Vec<String>) -> bool {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(file) => {
                    self.file = Some(file);
                    self.reported = false;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => return false,
                Err(error) => {
                    self.report_open_error(error);
                    return false;
                }
            }
        }
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        let len = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.report_open_error(error);
                self.file = None;
                return false;
            }
        };
        let restarted = len < self.offset;
        if restarted {
            // The log was truncated or replaced. Start over.
            self.offset = 0;
            self.partial.clear();
        }
        if self.offset < len {
            if let Err(error) = file.seek(SeekFrom::Start(self.offset)) {
                self.report_open_error(error);
                self.file = None;
                return restarted;
            }
            let mut fresh = Vec::new();
            match file.read_to_end(&mut fresh) {
                Ok(_) => {}
                Err(error) => {
                    self.report_open_error(error);
                    self.file = None;
                    return restarted;
                }
            }
            self.offset += fresh.len() as u64;
            self.partial.extend_from_slice(&fresh);
        }
        while let Some(position) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line_bytes: Vec<u8> = self.partial.drain(..=position).collect();
            let mut line = &line_bytes[..line_bytes.len() - 1];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            out.push(String::from_utf8_lossy(line).into_owned());
        }
        restarted
    }

    /// Print one read error once until a read succeeds again.
    fn report_open_error(&mut self, error: std::io::Error) {
        if self.reported {
            return;
        }
        eprintln!(
            "aif: cannot read the task log {}: {error}",
            self.path.display()
        );
        self.reported = true;
    }
}

/// One question the pending ask block can show.
struct AskQuestion {
    header: String,
    question: String,
    options: Vec<String>,
}

/// Pull the question list out of a `Question` decision payload.
///
/// The payload is the `questions` array of the `AskUserQuestion` tool
/// input. The read is defensive: anything that does not match the recorded
/// shape yields an empty list instead of an error.
fn ask_questions(value: &Value) -> Vec<AskQuestion> {
    let items = value
        .as_array()
        .or_else(|| value.get("questions").and_then(Value::as_array));
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| AskQuestion {
            header: item
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("ask")
                .to_string(),
            question: item
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            options: item
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            option
                                .get("label")
                                .and_then(Value::as_str)
                                .map(String::from)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// The bottom hint of the input bar for one input mode.
///
/// A closed input shows the reason sentence from the daemon instead of a
/// key hint. The bar renders that sentence as is.
fn input_hint(mode: &InputMode) -> String {
    match mode {
        InputMode::Live => "enter send · ctrl-x abort · end tail".to_string(),
        InputMode::Resume => "enter send · resumes the parked chat".to_string(),
        InputMode::NextTurn => {
            "enter queue · lands after this turn · ctrl-x sends it now".to_string()
        }
        InputMode::Follow => "enter send · starts a follow-up turn".to_string(),
        InputMode::Closed { reason } => reason.clone(),
    }
}

/// The ` · harness · model` header segment of one bound role, plus
/// ` · variant` when the harness takes one.
fn binding_segment(binding: &RoleBindingView) -> String {
    let mut text = format!(" · {} · {}", binding.harness.program(), binding.model);
    if let Some(effort) = &binding.effort {
        text.push_str(&format!(" · {effort}"));
    }
    text
}

/// The session view: the transcript, the input bar, and the pending asks.
#[derive(Debug)]
pub struct SessionView {
    task: Option<TaskView>,
    /// The ids of the live sessions, in the order of the last state push.
    tabs: Vec<String>,
    /// True while the input bar takes the typing keys.
    chat_focus: bool,
    tailer: Option<LogTailer>,
    ring: Ring<Entry>,
    input: String,
    scroll_up: usize,
    last_poll: Option<Instant>,
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionView {
    /// Create an empty session view with no task.
    pub fn new() -> Self {
        SessionView {
            task: None,
            tabs: Vec::new(),
            chat_focus: true,
            tailer: None,
            ring: Ring::new(RING_CAP),
            input: String::new(),
            scroll_up: 0,
            last_poll: None,
        }
    }

    /// The ids of the live sessions, in the order of the last state push.
    pub fn tabs(&self) -> &[String] {
        &self.tabs
    }

    /// Replace the live-session tab list.
    pub fn set_tabs(&mut self, tabs: Vec<String>) {
        self.tabs = tabs;
    }

    /// True while the input bar takes the typing keys.
    pub fn chat_focus(&self) -> bool {
        self.chat_focus
    }

    /// Take or release the chat focus. The state survives a task switch.
    pub fn set_chat_focus(&mut self, focus: bool) {
        self.chat_focus = focus;
    }

    /// The text the input bar holds now.
    pub fn input_text(&self) -> &str {
        &self.input
    }

    /// The id of the task the view shows, when one is chosen.
    pub fn task_id(&self) -> Option<&str> {
        self.task.as_ref().map(|task| task.id.as_str())
    }

    /// True when the view shows the task with `task_id`.
    pub fn is_showing(&self, task_id: &str) -> bool {
        self.task_id() == Some(task_id)
    }

    /// Clear the shown task and all local session data.
    ///
    /// The tab list and the chat focus survive: they are shell-level
    /// state, not data of one task.
    pub fn clear(&mut self) {
        self.task = None;
        self.tailer = None;
        self.ring.clear();
        self.input.clear();
        self.scroll_up = 0;
        self.last_poll = None;
    }

    /// Show `task` in the view.
    ///
    /// The first call, and every call with a new task id or log path,
    /// resets the transcript and reopens the log from the beginning. A call
    /// for the same task updates the shown state, the attempt included,
    /// and keeps the transcript, the scroll, and the typed input.
    pub fn show(&mut self, task: &TaskView) {
        let same = self
            .task
            .as_ref()
            .is_some_and(|current| current.id == task.id && current.log_path == task.log_path);
        if !same {
            self.clear();
            self.tailer = Some(LogTailer::new(task.log_path.clone()));
        }
        self.task = Some(task.clone());
    }

    /// Read the new log bytes now. The shell calls this before each draw.
    pub fn on_redraw(&mut self, now: Instant) {
        self.ingest();
        self.last_poll = Some(now);
    }

    /// Read the new log bytes when the last read is [`POLL_INTERVAL`] old.
    ///
    /// The shell calls this on wakeups that carry no message, so a quiet
    /// agent still streams into the view at most five times a second. The
    /// return value is true when the visible transcript changed.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self
            .last_poll
            .is_none_or(|last| now.duration_since(last) >= POLL_INTERVAL)
        {
            let changed = self.ingest();
            self.last_poll = Some(now);
            return changed;
        }
        false
    }

    /// Read the tailer and push every parsed item into the ring.
    ///
    /// A restarted log clears the ring: the transcript restarts with the
    /// file, because the replaced history no longer exists.
    fn ingest(&mut self) -> bool {
        let mut lines = Vec::new();
        let restarted = self
            .tailer
            .as_mut()
            .is_some_and(|tailer| tailer.read_lines(&mut lines));
        let mut changed = restarted && !self.ring.is_empty();
        if restarted {
            self.ring.clear();
        }
        for line in lines {
            for entry in transcript::parse(&line) {
                self.ring.push(entry);
                changed = true;
            }
        }
        changed
    }

    /// True when the view follows the tail of the transcript.
    pub fn following(&self) -> bool {
        self.scroll_up == 0
    }

    /// True when the input bar can take a chat message.
    ///
    /// The bar needs a shown task whose input mode is open. The shell
    /// reads this to decide whether the view owns the typing keys: a bar
    /// that cannot take text leaves the view keys `1` through `5` and `?`
    /// to the shell.
    pub fn input_enabled(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| !matches!(task.input, InputMode::Closed { .. }))
    }

    /// True when the input bar accepts no chat message.
    ///
    /// A missing task and a closed task disable the bar. The bar swallows
    /// typing and Enter, so the shell sends no message it cannot deliver.
    fn input_is_disabled(&self) -> bool {
        !self.input_enabled()
    }

    /// The bottom-row hints of the session state.
    ///
    /// A bar that holds the focus shows only the release keys, because
    /// the shell gives `h` and `l` to the bar and sends `esc` to the
    /// focus. A bar that cannot take text keeps the `? help` key. A
    /// released focus names the session and pipeline keys, and a view
    /// with no task falls back to the shell view keys.
    pub fn footer_hints(&self) -> String {
        if self.task.is_none() {
            return "1 2 3 4 5 views · ? help".to_string();
        }
        if self.chat_focus {
            if self.input_enabled() {
                return "esc tab release focus".to_string();
            }
            return "esc tab release focus · ? help".to_string();
        }
        if self.input_enabled() {
            return "i enter focus · h l session · esc pipeline · ? help".to_string();
        }
        "h l session · esc pipeline · ? help".to_string()
    }

    /// Handle one key press. Returns the action to send to the daemon.
    ///
    /// `page` is the visible transcript height in rows; the shell passes
    /// the pane height, and the view uses it as the PageUp and PageDown
    /// step. A focused bar takes the typing keys: typing feeds the input
    /// bar, Enter sends one [`Action::Chat`] with the typed text, `ctrl-x`
    /// sends [`Action::Abort`], PageUp and PageDown scroll, and End returns
    /// to following the tail.
    ///
    /// Enter shows nothing at once. The daemon logs the accepted message,
    /// and the log tail delivers that line to the transcript at the next
    /// poll.
    ///
    /// An unfocused bar swallows typing and Enter and returns none,
    /// whatever the bar holds. `ctrl-x` and the scroll keys stay alive, so
    /// the shell can keep them forwarded while its own keys own the plain
    /// letters.
    ///
    /// A closed input swallows typing and Enter and returns none, whatever
    /// the focus says.
    pub fn handle_key(&mut self, key: KeyEvent, page: u16) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let page = usize::from(page.max(1));
        let disabled = self.input_is_disabled() || !self.chat_focus;
        match (key.code, key.modifiers) {
            (KeyCode::Char('x'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                let task = self.task.as_ref()?.id.clone();
                Some(Action::Abort { task })
            }
            (KeyCode::Enter, _) if !disabled => {
                let task = self.task.as_ref()?.id.clone();
                let text = std::mem::take(&mut self.input);
                if text.trim().is_empty() {
                    return None;
                }
                // The daemon appends one user line to the log for every
                // accepted message. The log tail delivers the line to the
                // transcript at the next poll, so no local echo lives here.
                Some(Action::Chat { task, text })
            }
            (KeyCode::Char(letter), modifiers)
                if !disabled
                    && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.push(letter);
                None
            }
            (KeyCode::Backspace, _) if !disabled => {
                self.input.pop();
                None
            }
            (KeyCode::PageUp, _) => {
                self.scroll_up = self.scroll_up.saturating_add(page);
                None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_up = self.scroll_up.saturating_sub(page);
                None
            }
            (KeyCode::End, _) => {
                self.scroll_up = 0;
                None
            }
            _ => None,
        }
    }

    /// Build the transcript display lines for one pane width.
    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let dropped = self.ring.dropped();
        if dropped > 0 {
            lines.push(Line::styled(
                format!("··· {dropped} earlier lines hidden ···"),
                transcript::dim_style(),
            ));
        }
        for entry in self.ring.iter() {
            lines.extend(transcript::render(entry, width));
        }
        if lines.is_empty() {
            lines.push(Line::styled(
                "no output yet; the agent has not written to the log",
                transcript::dim_style(),
            ));
        }
        lines
    }

    /// Build the inline ask lines for the decisions of the shown task.
    fn ask_lines(&self, decisions: &[Decision]) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for decision in decisions {
            match &decision.kind {
                DecisionKind::Permission {
                    task, tool, input, ..
                } if self.is_showing(task) => {
                    lines.push(Line::from(vec![
                        Span::styled("[ask] allow ", transcript::ask_style()),
                        Span::styled(format!("{tool}? "), transcript::ask_style()),
                        Span::styled(
                            transcript::claude_tool_summary(tool, input),
                            transcript::dim_style(),
                        ),
                    ]));
                    lines.push(Line::styled(
                        "      answer in the inbox: press !".to_string(),
                        transcript::dim_style(),
                    ));
                }
                DecisionKind::Question {
                    task, questions, ..
                } if self.is_showing(task) => {
                    for ask in ask_questions(questions) {
                        lines.push(Line::styled(
                            format!("[ask] {}: {}", ask.header, ask.question),
                            transcript::ask_style(),
                        ));
                        if !ask.options.is_empty() {
                            let numbered = ask
                                .options
                                .iter()
                                .enumerate()
                                .map(|(index, label)| format!("  {}) {label}", index + 1))
                                .collect::<Vec<_>>()
                                .join("   ");
                            lines.push(Line::styled(numbered, transcript::dim_style()));
                        }
                        lines.push(Line::styled(
                            "      answer in the inbox: press !".to_string(),
                            transcript::dim_style(),
                        ));
                    }
                }
                _ => {}
            }
            if lines.len() >= ASK_MAX_ROWS {
                break;
            }
        }
        lines.truncate(ASK_MAX_ROWS);
        lines
    }

    /// The one-row header above the transcript.
    ///
    /// With two or more live sessions the header is the tab strip: one
    /// label per live session, the shown one bright, with its attempt,
    /// queued count, and bound role at the end. Every other case is the
    /// plain task line.
    ///
    /// A shown task with no live session takes the plain line too. The
    /// strip would highlight no label. It would also hang the attempt of
    /// the shown task on an unrelated one. The header would then name
    /// neither the task nor its state.
    ///
    /// A view with no shown task keeps the strip. The strip hides nothing
    /// there, and it lists where the tab keys lead.
    ///
    /// Both branches end with the bound role of the shown task: the
    /// harness, the model, and the variant. The values come from
    /// `self.task`, so every state push and every tab switch updates
    /// them at once. A task without a binding, such as a queued task,
    /// shows no role text.
    fn header_line(&self) -> Line<'static> {
        let dim = transcript::dim_style();
        let binding = self
            .task
            .as_ref()
            .and_then(|task| task.binding.as_ref())
            .map(binding_segment);
        let shown_is_hidden = self
            .task
            .as_ref()
            .is_some_and(|task| !self.tabs.contains(&task.id));
        if self.tabs.len() >= 2 && !shown_is_hidden {
            let active = |id: &str| self.task.as_ref().is_some_and(|task| task.id == *id);
            let mut spans = Vec::new();
            for (index, id) in self.tabs.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" │ ", dim));
                }
                let style = if active(id) {
                    Style::default()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                spans.push(Span::styled(id.clone(), style));
            }
            if let Some(task) = &self.task {
                let mut info = format!(" · attempt {}", task.attempt);
                if task.queued_messages > 0 {
                    info.push_str(&format!(" · {} queued", task.queued_messages));
                }
                spans.push(Span::styled(info, dim));
                if let Some(segment) = binding {
                    spans.push(Span::styled(segment, dim));
                }
            }
            return Line::from(spans);
        }
        match &self.task {
            Some(task) => {
                let mut text = format!("{} · {} · attempt {}", task.id, task.state, task.attempt);
                if task.queued_messages > 0 {
                    text.push_str(&format!(" · {} queued", task.queued_messages));
                }
                if let Some(segment) = binding {
                    text.push_str(&segment);
                }
                Line::styled(text, dim)
            }
            None => Line::styled("no task selected".to_string(), dim),
        }
    }

    /// Draw the view into `area`.
    ///
    /// The layout, from the top: a one-row task header, the transcript,
    /// the inline ask block when a decision of this task waits, and the
    /// input bar at the bottom. The header shows the live-session tabs
    /// when the daemon pushes more than one. The input bar hint comes from
    /// the task's input mode, and a closed or unfocused bar renders dim.
    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, decisions: &[Decision]) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let input_rows = INPUT_ROWS.min(area.height);
        let rest = area.height - input_rows;
        let ask = self.ask_lines(decisions);
        let ask_rows = (ask.len() as u16).min(rest);
        let rest = rest - ask_rows;
        let header_rows = 1u16.min(rest);
        let transcript_rows = rest - header_rows;

        let header_rect = Rect::new(area.x, area.y, area.width, header_rows);
        let transcript_rect = Rect::new(area.x, area.y + header_rows, area.width, transcript_rows);
        let ask_rect = Rect::new(
            area.x,
            area.y + header_rows + transcript_rows,
            area.width,
            ask_rows,
        );
        let input_rect = Rect::new(
            area.x,
            area.y + header_rows + transcript_rows + ask_rows,
            area.width,
            input_rows,
        );

        if header_rows > 0 {
            frame.render_widget(Paragraph::new(self.header_line()), header_rect);
        }

        if transcript_rows > 0 {
            let lines = self.transcript_lines(transcript_rect.width);
            let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
            let height = transcript_rect.height.max(1);
            let tail_skip = total.saturating_sub(height);
            let scroll = u16::try_from(self.scroll_up).unwrap_or(u16::MAX);
            let skip = tail_skip.saturating_sub(scroll);
            frame.render_widget(Paragraph::new(lines).scroll((skip, 0)), transcript_rect);
        }

        if ask_rows > 0 {
            frame.render_widget(Paragraph::new(ask), ask_rect);
        }

        // The hint states what Enter will do for this task. A closed bar
        // shows the daemon's reason. An unfocused bar states the keys
        // that take the focus back and switch the tabs.
        let hint = if !self.chat_focus {
            let switch = match self.tabs.len() {
                0 | 1 => String::new(),
                _ => "h l session · ".to_string(),
            };
            format!("{switch}1-5 views · i or enter chats")
        } else {
            match &self.task {
                Some(task) => input_hint(&task.input),
                None => "select a task to chat".to_string(),
            }
        };
        let disabled = self.input_is_disabled() || !self.chat_focus;
        let mut block = Block::default()
            .borders(Borders::ALL)
            .title(" chat ")
            .title_bottom(Line::styled(hint, transcript::dim_style()).centered());
        let mut content = String::with_capacity(self.input.len() + 2);
        content.push_str(&self.input);
        content.push('▏');
        let mut line = Line::from(content);
        if disabled {
            block = block
                .border_style(THEME.dim())
                .title_style(transcript::dim_style());
            line = line.style(transcript::dim_style());
        }
        frame.render_widget(Paragraph::new(line).block(block), input_rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, Stage};
    use crate::tasks::TaskState;
    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// A temporary directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir();
            for attempt in 0..1000 {
                let path = base.join(format!(
                    "aif-session-test-{}-{tag}-{attempt}",
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
                if error.kind() != ErrorKind::NotFound {
                    eprintln!("cannot remove test directory {}: {error}", self.0.display());
                }
            }
        }
    }

    /// One running task whose log lives at `path`.
    fn sample_task(path: &Path) -> TaskView {
        TaskView {
            id: "borsuk/implement-i142".to_string(),
            repo: "borsuk".to_string(),
            stage: Stage::Implement,
            kind: ItemKind::Issue,
            number: 142,
            state: TaskState::Running,
            attempt: 1,
            log_path: path.to_path_buf(),
            input: crate::sock::InputMode::Live,
            queued_messages: 0,
            binding: None,
        }
    }

    /// The sample task with another input mode and queued count.
    fn task_with_mode(path: &Path, mode: InputMode, queued: usize) -> TaskView {
        TaskView {
            input: mode,
            queued_messages: queued,
            ..sample_task(path)
        }
    }

    /// One plain key press.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Build a real task through the task table API.
    fn table_task(repo: &str, stage: Stage, number: u64, log: &Path) -> crate::tasks::Task {
        let mut table = crate::tasks::TaskTable::new();
        table
            .upsert_queued(
                repo,
                stage,
                ItemKind::Issue,
                number,
                log.to_path_buf(),
                1_000,
            )
            .unwrap()
            .clone()
    }

    /// One letter key press.
    fn letter(letter: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE)
    }

    #[test]
    fn the_footer_hints_follow_the_chat_focus_and_task_state() {
        let dir = TempDir::new("footer-hints");
        let task = sample_task(dir.path());

        // No shown task: the shell keeps its view keys.
        let view = SessionView::new();
        assert_eq!(view.footer_hints(), "1 2 3 4 5 views · ? help");

        // A focused bar that can take text shows only the release keys.
        let mut view = SessionView::new();
        view.show(&task);
        assert!(view.chat_focus());
        assert_eq!(view.footer_hints(), "esc tab release focus");

        // A released focus offers the focus, session, and pipeline keys.
        view.set_chat_focus(false);
        assert_eq!(
            view.footer_hints(),
            "i enter focus · h l session · esc pipeline · ? help"
        );

        // A closed bar that holds the focus still owns esc and tab. The
        // shell keeps the help key, because the bar takes no text.
        let closed = task_with_mode(
            dir.path(),
            InputMode::Closed {
                reason: "the session is parked".to_string(),
            },
            0,
        );
        let mut view = SessionView::new();
        view.show(&closed);
        assert!(view.chat_focus());
        assert_eq!(
            view.footer_hints(),
            "esc tab release focus · ? help",
            "a focused closed bar releases the focus first"
        );

        // A released focus names no focus key, because the bar is closed.
        view.set_chat_focus(false);
        assert_eq!(view.footer_hints(), "h l session · esc pipeline · ? help");

        for hint in [
            "1 2 3 4 5 views · ? help",
            "esc tab release focus",
            "esc tab release focus · ? help",
            "i enter focus · h l session · esc pipeline · ? help",
            "h l session · esc pipeline · ? help",
        ] {
            assert!(hint.chars().count() <= crate::tui::HINT_CAP, "hint {hint}");
        }
    }

    #[test]
    fn the_ring_buffer_never_exceeds_its_bound() {
        let mut ring: Ring<usize> = Ring::new(RING_CAP);
        let feed = RING_CAP + 500;
        for item in 0..feed {
            ring.push(item);
        }

        assert_eq!(ring.len(), RING_CAP, "the ring holds at most the bound");
        assert_eq!(ring.dropped(), feed as u64 - RING_CAP as u64);
        assert_eq!(
            *ring.iter().next().unwrap(),
            feed - RING_CAP,
            "the oldest kept item is the first survivor"
        );
        assert_eq!(*ring.iter().last().unwrap(), feed - 1);

        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.dropped(), 0);
    }

    #[test]
    fn the_session_view_tails_the_log_file() {
        let dir = TempDir::new("tail");
        let log = dir.path().join("task.jsonl");
        fs::write(
            &log,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);

        // New bytes appended to the same file appear on the next read.
        fs::write(
            &log,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"second"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 2);

        // A truncated log restarts the read from the beginning.
        fs::write(&log, "fresh\n").unwrap();
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);
    }

    #[test]
    fn a_partial_line_waits_for_its_newline() {
        let dir = TempDir::new("partial");
        let log = dir.path().join("task.jsonl");
        let mut file = fs::File::create(&log).unwrap();
        write!(file, r#"{{"type":"assistant","mess"#).unwrap();
        file.flush().unwrap();

        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 0, "an unfinished line is not parsed yet");

        writeln!(
            file,
            "age\":{{\"content\":[{{\"type\":\"text\",\"text\":\"whole\"}}]}}}}"
        )
        .unwrap();
        drop(file);
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);
    }

    #[test]
    fn a_missing_log_file_is_quiet_until_it_appears() {
        let dir = TempDir::new("missing");
        let log = dir.path().join("not-yet.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        view.on_redraw(Instant::now());
        assert!(view.ring.is_empty());

        fs::write(&log, "hello\n").unwrap();
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1, "the raw fallback keeps unknown text");
    }

    #[test]
    fn a_dispatch_failure_line_replaces_the_no_output_placeholder() {
        let dir = TempDir::new("dispatch-line");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 80, 12), &[]))
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("no output yet"),
            "the empty log shows the placeholder: {screen}"
        );

        fs::write(
            &log,
            "aif: dispatch failed: cannot prepare the worktree: git worktree prune failed: boom\n",
        )
        .unwrap();
        view.on_redraw(Instant::now());

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 80, 12), &[]))
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("aif: dispatch failed: cannot prepare the worktree"),
            "the session view shows the reason: {screen}"
        );
        assert!(
            !screen.contains("no output yet"),
            "the placeholder is gone: {screen}"
        );
    }

    #[test]
    fn the_poll_reads_at_most_once_per_interval() {
        let dir = TempDir::new("poll");
        let log = dir.path().join("task.jsonl");
        fs::write(&log, "one\n").unwrap();
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        let start = Instant::now();
        view.poll(start);
        assert_eq!(view.ring.len(), 1);

        fs::write(&log, "one\ntwo\n").unwrap();
        view.poll(start + POLL_INTERVAL / 2);
        assert_eq!(view.ring.len(), 1, "the poll window holds the read back");

        view.poll(start + POLL_INTERVAL);
        assert_eq!(view.ring.len(), 2, "after the window the poll reads again");

        // A redraw always reads at once, whatever the window says.
        fs::write(&log, "one\ntwo\nthree\n").unwrap();
        view.on_redraw(start + POLL_INTERVAL / 2);
        assert_eq!(view.ring.len(), 3);
    }

    #[test]
    fn showing_a_different_task_resets_the_transcript() {
        let dir = TempDir::new("retarget");
        let first_log = dir.path().join("first.jsonl");
        let second_log = dir.path().join("second.jsonl");
        fs::write(&first_log, "first task line\n").unwrap();
        fs::write(&second_log, "second task line\n").unwrap();

        let mut view = SessionView::new();
        view.show(&sample_task(&first_log));
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);

        let mut other = sample_task(&second_log);
        other.id = "borsuk/refine-i7".to_string();
        view.show(&other);
        assert_eq!(view.ring.len(), 0, "the old transcript is gone");
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);
        assert!(view.is_showing("borsuk/refine-i7"));

        // A state push for the same task keeps the transcript and the scroll.
        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following());
        let mut pushed = other;
        pushed.attempt = 2;
        view.show(&pushed);
        assert_eq!(view.ring.len(), 1, "same task, same log: nothing resets");
        assert!(!view.following(), "the scroll survives a state push");
    }

    #[test]
    fn typing_and_enter_sends_one_chat_action() {
        let dir = TempDir::new("chat");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        for key in [letter(' '), letter('h'), letter('i'), letter(' ')] {
            assert_eq!(view.handle_key(key, 10), None);
        }
        let action = view.handle_key(key(KeyCode::Enter), 10);

        assert_eq!(
            action,
            Some(Action::Chat {
                task: "borsuk/implement-i142".to_string(),
                text: " hi ".to_string(),
            })
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Enter), 10),
            None,
            "the input is empty after one send"
        );
    }

    #[test]
    fn an_empty_or_blank_input_sends_nothing() {
        let dir = TempDir::new("blank");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        assert_eq!(view.handle_key(key(KeyCode::Enter), 10), None);
        assert_eq!(view.handle_key(letter(' '), 10), None);
        assert_eq!(view.handle_key(key(KeyCode::Backspace), 10), None);
        assert_eq!(view.handle_key(letter('a'), 10), None);
        assert_eq!(view.handle_key(key(KeyCode::Backspace), 10), None);
        assert_eq!(view.handle_key(key(KeyCode::Enter), 10), None);
        assert!(
            view.ring.is_empty(),
            "a blank send echoes nothing into the transcript"
        );
    }

    #[test]
    fn enter_sends_the_chat_and_echoes_nothing_locally() {
        let dir = TempDir::new("echo");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        for press in [
            letter('s'),
            letter('t'),
            letter('e'),
            letter('e'),
            letter('r'),
        ] {
            assert_eq!(view.handle_key(press, 10), None);
        }
        assert_eq!(
            view.handle_key(key(KeyCode::Enter), 10),
            Some(Action::Chat {
                task: "borsuk/implement-i142".to_string(),
                text: "steer".to_string(),
            })
        );

        assert!(
            view.ring.is_empty(),
            "the view echoes nothing; the daemon's log line is the only record"
        );
    }

    #[test]
    fn the_draw_shows_the_daemon_user_line_in_the_transcript() {
        let dir = TempDir::new("echo-draw");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        for press in [letter('h'), letter('i')] {
            view.handle_key(press, 10);
        }
        view.handle_key(key(KeyCode::Enter), 10);

        // The daemon appends the user line for the accepted message; the
        // log tail delivers it at the next poll.
        fs::write(
            &log,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n"
            ),
        )
        .unwrap();
        view.on_redraw(Instant::now());

        assert_eq!(
            view.ring.len(),
            1,
            "the tail delivers the user line exactly once"
        );
        assert_eq!(
            view.ring.iter().next(),
            Some(&Entry::User {
                text: "hi".to_string()
            }),
            "the daemon line parses to the exact typed text"
        );

        let screen = drawn_screen(&view);
        assert!(
            screen.contains("› hi"),
            "the sent message must show in the transcript: {screen}"
        );
    }

    #[test]
    fn a_session_re_entry_keeps_the_user_line_and_renders_it_once() {
        let dir = TempDir::new("re-entry");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        for press in [letter('h'), letter('i')] {
            view.handle_key(press, 10);
        }
        view.handle_key(key(KeyCode::Enter), 10);
        fs::write(
            &log,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n"
            ),
        )
        .unwrap();
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), 1);

        // A switch to another session and back resets the ring, and the
        // fresh tail of the same log restores the user line.
        let mut other = sample_task(&dir.path().join("other.jsonl"));
        other.id = "borsuk/refine-i7".to_string();
        view.show(&other);
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());

        assert_eq!(view.ring.len(), 1, "no duplicate after the re-entry");
        assert_eq!(
            view.ring.iter().next(),
            Some(&Entry::User {
                text: "hi".to_string()
            })
        );
        let screen = drawn_screen(&view);
        assert!(screen.contains("› hi"), "re-entry shows the line: {screen}");

        // The ticket chat refocuses with a clear and a fresh show of the
        // same task. That path restores the user line too.
        view.clear();
        assert!(view.ring.is_empty(), "the refocus empties the ring");
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());

        assert_eq!(view.ring.len(), 1, "no duplicate after the refocus");
        assert_eq!(
            view.ring.iter().next(),
            Some(&Entry::User {
                text: "hi".to_string()
            })
        );
    }

    #[test]
    fn ctrl_x_aborts_the_shown_task() {
        let dir = TempDir::new("abort");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            view.handle_key(ctrl_x, 10),
            Some(Action::Abort {
                task: "borsuk/implement-i142".to_string(),
            })
        );

        // A plain x is typed into the input bar, not an abort.
        assert_eq!(view.handle_key(letter('x'), 10), None);
    }

    #[test]
    fn keys_without_a_shown_task_send_nothing() {
        let mut view = SessionView::new();
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

        assert_eq!(view.handle_key(letter('h'), 10), None);
        assert_eq!(view.handle_key(key(KeyCode::Enter), 10), None);
        assert_eq!(view.handle_key(ctrl_x, 10), None);
    }

    #[test]
    fn tail_following_resumes_with_end_after_scrolling_up() {
        let dir = TempDir::new("scroll");
        let log = dir.path().join("task.jsonl");
        fs::write(&log, "one\ntwo\nthree\n").unwrap();
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());

        assert!(view.following());

        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following(), "PageUp stops following the tail");
        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following());

        view.handle_key(key(KeyCode::PageDown), 10);
        assert!(!view.following(), "one page down is not the tail yet");

        view.handle_key(key(KeyCode::End), 10);
        assert!(view.following(), "End returns to following the tail");

        // Paging down past the tail stays at the tail.
        view.handle_key(key(KeyCode::PageUp), 10);
        view.handle_key(key(KeyCode::PageDown), 500);
        assert!(view.following());
    }

    #[test]
    fn the_draw_shows_the_header_the_transcript_and_the_input_bar() {
        let dir = TempDir::new("draw");
        let log = dir.path().join("task.jsonl");
        fs::write(
            &log,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"refine done"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.on_redraw(Instant::now());
        view.handle_key(letter('h'), 10);

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 50, 12);
        terminal.draw(|frame| view.draw(frame, area, &[])).unwrap();

        let screen = terminal.backend().to_string();
        assert!(screen.contains("borsuk/implement-i142"), "header: {screen}");
        assert!(screen.contains("running"), "state: {screen}");
        assert!(screen.contains("refine done"), "transcript: {screen}");
        assert!(screen.contains("chat"), "input bar title: {screen}");
        assert!(screen.contains("enter send"), "hint: {screen}");
    }

    /// Render the view at `width` by `height` and return the screen text.
    fn draw_screen(view: &SessionView, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, width, height), &[]))
            .unwrap();
        terminal.backend().to_string()
    }

    /// The sample task bound to one opencode role.
    fn bound_task(path: &Path, effort: Option<&str>) -> TaskView {
        let mut task = sample_task(path);
        task.binding = Some(crate::sock::RoleBindingView {
            harness: crate::config::Harness::Opencode,
            model: "zai-coding-plan/glm-5.3-flash".to_string(),
            effort: effort.map(|value| value.to_string()),
        });
        task
    }

    #[test]
    fn the_header_shows_the_bound_harness_model_and_variant() {
        let dir = TempDir::new("bound");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, Some("xhigh")));

        let screen = draw_screen(&view, 120, 10);

        assert!(
            screen.contains("opencode · zai-coding-plan/glm-5.3-flash · xhigh"),
            "header: {screen}"
        );
    }

    #[test]
    fn the_header_ends_at_the_model_when_the_role_takes_no_variant() {
        let dir = TempDir::new("no-variant");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, None));

        let screen = draw_screen(&view, 120, 10);

        assert!(
            screen.contains("opencode · zai-coding-plan/glm-5.3-flash"),
            "header: {screen}"
        );
        assert!(!screen.contains("xhigh"), "header: {screen}");
    }

    #[test]
    fn the_header_shows_no_role_text_for_a_task_without_a_binding() {
        let dir = TempDir::new("unbound");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        let screen = draw_screen(&view, 120, 10);

        assert!(!screen.contains("opencode"), "header: {screen}");
        assert!(!screen.contains("glm-5.3-flash"), "header: {screen}");
    }

    #[test]
    fn the_tab_strip_tails_with_the_shown_tasks_bound_role() {
        let dir = TempDir::new("tabs");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, Some("xhigh")));
        view.set_tabs(vec![
            "borsuk/refine-i140".to_string(),
            "borsuk/implement-i142".to_string(),
        ]);

        let screen = draw_screen(&view, 160, 10);

        assert!(
            screen.contains(" │ "),
            "the header must be the strip: {screen}"
        );
        assert!(
            screen.contains("opencode · zai-coding-plan/glm-5.3-flash · xhigh"),
            "header: {screen}"
        );
    }

    #[test]
    fn the_header_takes_the_role_of_the_task_the_next_show_brings() {
        let dir = TempDir::new("swap");
        let mut view = SessionView::new();
        view.show(&bound_task(&dir.path().join("first.jsonl"), Some("xhigh")));
        let screen = draw_screen(&view, 120, 10);
        assert!(
            screen.contains("opencode · zai-coding-plan/glm-5.3-flash · xhigh"),
            "header: {screen}"
        );

        // Every state push and every tab switch calls `show` again. The
        // header takes the new task's role and drops the old one.
        let mut other = sample_task(&dir.path().join("second.jsonl"));
        other.id = "borsuk/refine-i140".to_string();
        other.binding = Some(crate::sock::RoleBindingView {
            harness: crate::config::Harness::Claude,
            model: "opus-5".to_string(),
            effort: None,
        });
        view.show(&other);

        let screen = draw_screen(&view, 120, 10);
        assert!(screen.contains("claude · opus-5"), "header: {screen}");
        assert!(!screen.contains("zai-coding-plan"), "header: {screen}");
        assert!(!screen.contains("xhigh"), "header: {screen}");
    }

    #[test]
    fn the_draw_marks_an_older_hidden_history() {
        let dir = TempDir::new("hidden");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        // Push more parsed items than the ring holds through the tailer.
        let mut text = String::new();
        for _ in 0..(RING_CAP + 10) {
            text.push_str("plain line\n");
        }
        fs::write(&log, text).unwrap();
        view.on_redraw(Instant::now());
        assert_eq!(view.ring.len(), RING_CAP);

        // The marker sits at the top of the transcript. Scroll up to it.
        view.handle_key(key(KeyCode::PageUp), 10_000);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 60, 10), &[]))
            .unwrap();

        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("earlier lines hidden"),
            "the view must say what it dropped: {screen}"
        );
    }

    #[test]
    fn the_draw_shows_a_pending_ask_for_this_task_only() {
        let dir = TempDir::new("ask");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        let shown = table_task("borsuk", Stage::Implement, 142, &log);
        let other = table_task("borsuk", Stage::Implement, 9, &log);

        let permission = Decision::permission(
            &shown,
            "req-1",
            "Write",
            serde_json::json!({"file_path": "docs/x.md"}),
            1_000,
        );
        let other_task = Decision::permission(
            &other,
            "req-2",
            "Bash",
            serde_json::json!({"command": "rm -rf /"}),
            1_000,
        );
        let question = Decision::question(
            &shown,
            "req-3",
            serde_json::json!([
                {
                    "question": "Deploy where?",
                    "header": "Target",
                    "options": [
                        {"label": "staging", "description": "the test cluster"},
                        {"label": "production", "description": "the real one"}
                    ],
                    "multiSelect": false
                }
            ]),
            2_000,
        );

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let decisions = vec![other_task, permission, question];
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 60, 20), &decisions))
            .unwrap();

        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("[ask] allow Write?"),
            "permission: {screen}"
        );
        assert!(
            screen.contains("[ask] Target: Deploy where?"),
            "question: {screen}"
        );
        assert!(screen.contains("staging"), "options: {screen}");
        assert!(
            !screen.contains("rm -rf /"),
            "an ask of another task must not show: {screen}"
        );
    }

    #[test]
    fn the_draw_fits_into_a_one_row_area() {
        let view = SessionView::new();

        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 30, 1), &[]))
            .unwrap();

        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("select a task to chat"),
            "one row shows the input bar hint: {screen}"
        );
    }

    #[test]
    fn ask_questions_reads_the_recorded_payload_shape() {
        let value = serde_json::json!([
            {"question": "q1", "header": "h1", "options": [{"label": "a"}, {"label": "b"}]},
            {"question": "q2", "header": "h2"}
        ]);

        let asks = ask_questions(&value);

        assert_eq!(asks.len(), 2);
        assert_eq!(asks[0].header, "h1");
        assert_eq!(asks[0].options, vec!["a".to_string(), "b".to_string()]);
        assert!(asks[1].options.is_empty());

        // The recorded tool input wraps the same list under `questions`.
        let wrapped = ask_questions(&serde_json::json!({"questions": value}));
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].options, vec!["a".to_string(), "b".to_string()]);

        // A wrong shape yields nothing.
        assert!(ask_questions(&serde_json::json!(null)).is_empty());
        assert!(ask_questions(&serde_json::json!("no")).is_empty());
    }

    /// Render the view at 80 by 12 and return the visible text.
    fn drawn_screen(view: &SessionView) -> String {
        draw_screen(view, 80, 12)
    }

    /// Render the view and return the style of the input bar top border.
    ///
    /// The input bar fills the bottom three rows, so its top border row is
    /// three rows above the bottom edge.
    fn drawn_border_style(view: &SessionView) -> ratatui::style::Style {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 80, 12);
        terminal.draw(|frame| view.draw(frame, area, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        buffer[(0, 9)].style()
    }

    #[test]
    fn the_frame_shows_a_hint_for_each_of_the_five_input_modes() {
        let dir = TempDir::new("modes");
        let log = dir.path().join("task.jsonl");
        let cases = [
            (InputMode::Live, "enter send · ctrl-x abort · end tail"),
            (InputMode::Resume, "enter send · resumes the parked chat"),
            (
                InputMode::NextTurn,
                "enter queue · lands after this turn · ctrl-x sends it now",
            ),
            (InputMode::Follow, "enter send · starts a follow-up turn"),
            (
                InputMode::Closed {
                    reason: "the session is parked".to_string(),
                },
                "the session is parked",
            ),
        ];
        for (mode, hint) in cases {
            let mut view = SessionView::new();
            view.show(&task_with_mode(&log, mode.clone(), 0));
            let screen = drawn_screen(&view);
            assert!(screen.contains(hint), "mode {mode:?}: {screen}");
        }
    }

    #[test]
    fn the_closed_bar_renders_dim_and_the_live_bar_does_not() {
        let dir = TempDir::new("dim");
        let log = dir.path().join("task.jsonl");

        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, InputMode::Live, 0));
        assert_ne!(
            drawn_border_style(&view).fg,
            Some(THEME.dim),
            "a live bar keeps its normal border"
        );

        let closed = InputMode::Closed {
            reason: "the session is parked".to_string(),
        };
        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, closed, 0));
        assert_eq!(
            drawn_border_style(&view).fg,
            Some(THEME.dim),
            "a closed bar renders a dim border"
        );
    }

    #[test]
    fn a_closed_input_swallows_typing_and_enter_but_keeps_abort_and_scroll() {
        let dir = TempDir::new("closed");
        let log = dir.path().join("task.jsonl");
        let closed = InputMode::Closed {
            reason: "the session is parked".to_string(),
        };
        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, closed, 0));

        for press in [letter('h'), letter('i'), key(KeyCode::Backspace)] {
            assert_eq!(view.handle_key(press, 10), None);
        }
        assert!(view.input.is_empty(), "a closed input swallows the letters");
        assert_eq!(
            view.handle_key(key(KeyCode::Enter), 10),
            None,
            "a closed input sends nothing on Enter"
        );

        // Scrolling still works in a closed session.
        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following());
        view.handle_key(key(KeyCode::End), 10);
        assert!(view.following());

        // ctrl-x still aborts the shown task.
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            view.handle_key(ctrl_x, 10),
            Some(Action::Abort {
                task: "borsuk/implement-i142".to_string(),
            })
        );
    }

    #[test]
    fn enter_in_next_turn_mode_sends_the_chat() {
        let dir = TempDir::new("next-turn");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, InputMode::NextTurn, 1));

        for press in [letter('h'), letter('i')] {
            assert_eq!(view.handle_key(press, 10), None);
        }
        assert_eq!(
            view.handle_key(key(KeyCode::Enter), 10),
            Some(Action::Chat {
                task: "borsuk/implement-i142".to_string(),
                text: "hi".to_string(),
            })
        );
        assert!(
            view.ring.is_empty(),
            "the queued message leaves the echo to the daemon's log line"
        );
    }

    #[test]
    fn no_selected_task_disables_the_input_bar() {
        let mut view = SessionView::new();

        assert_eq!(view.handle_key(letter('h'), 10), None);
        assert!(view.input.is_empty(), "a disabled bar swallows text");
        assert_eq!(view.handle_key(key(KeyCode::Enter), 10), None);

        let screen = drawn_screen(&view);
        assert!(screen.contains("select a task to chat"), "bar: {screen}");
        assert!(!screen.contains("enter send"), "bar: {screen}");
        assert_eq!(
            drawn_border_style(&view).fg,
            Some(THEME.dim),
            "a disabled bar renders dim"
        );
    }

    #[test]
    fn the_header_shows_the_queued_count_when_it_is_above_zero() {
        let dir = TempDir::new("queued");
        let log = dir.path().join("task.jsonl");

        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, InputMode::NextTurn, 2));
        let screen = drawn_screen(&view);
        assert!(
            screen.contains("borsuk/implement-i142 · running · attempt 1 · 2 queued"),
            "header: {screen}"
        );

        let mut view = SessionView::new();
        view.show(&task_with_mode(&log, InputMode::Live, 0));
        let screen = drawn_screen(&view);
        assert!(!screen.contains("queued"), "header: {screen}");
    }

    #[test]
    fn the_header_shows_a_tab_strip_when_two_sessions_are_live() {
        let dir = TempDir::new("tabs");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.set_tabs(vec![
            "borsuk/refine-i143".to_string(),
            "borsuk/implement-i142".to_string(),
        ]);

        let screen = drawn_screen(&view);

        assert!(
            screen.contains("borsuk/refine-i143"),
            "the first tab is listed: {screen}"
        );
        assert!(
            screen.contains("borsuk/implement-i142"),
            "the shown task keeps its full tab: {screen}"
        );
        assert!(
            screen.contains("attempt 1"),
            "the shown task keeps its attempt: {screen}"
        );
        assert!(
            !screen.contains("no task selected"),
            "the plain header is gone: {screen}"
        );

        // A single live session keeps the plain header.
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.set_tabs(vec!["borsuk/implement-i142".to_string()]);
        let screen = drawn_screen(&view);
        assert!(
            screen.contains("borsuk/implement-i142 · running · attempt 1"),
            "one tab stays plain: {screen}"
        );
    }

    /// A view with no shown task keeps the strip.
    ///
    /// Every `set_tabs` call follows a `show`, so no other test reaches
    /// this case. Without it, a guard that hid the strip whenever the
    /// shown task is absent would keep the suite green.
    #[test]
    fn the_header_keeps_the_tab_strip_when_no_task_is_shown() {
        let mut view = SessionView::new();
        view.set_tabs(vec![
            "borsuk/refine-i143".to_string(),
            "borsuk/implement-i142".to_string(),
        ]);

        let screen = drawn_screen(&view);

        assert!(
            screen.contains("borsuk/refine-i143"),
            "the strip lists the live sessions: {screen}"
        );
        assert!(
            screen.contains("borsuk/implement-i142"),
            "the strip lists the live sessions: {screen}"
        );
        assert!(
            !screen.contains("no task selected"),
            "the strip hides no task, so it stays: {screen}"
        );
    }

    #[test]
    fn the_header_names_a_shown_task_that_owns_no_live_session() {
        let dir = TempDir::new("no-live-tab");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        let mut shown = sample_task(&log);
        shown.id = "borsuk/release".to_string();
        shown.state = TaskState::Queued;
        view.show(&shown);
        view.set_tabs(vec![
            "borsuk/refine-i143".to_string(),
            "borsuk/implement-i142".to_string(),
        ]);

        let screen = drawn_screen(&view);

        assert!(
            screen.contains("borsuk/release · queued · attempt 1"),
            "the header names the shown task and its state: {screen}"
        );
        assert!(
            !screen.contains("borsuk/refine-i143"),
            "a strip that highlights no tab is gone: {screen}"
        );
    }

    #[test]
    fn an_unfocused_bar_swallows_typing_but_keeps_scroll_and_abort() {
        let dir = TempDir::new("unfocused");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.set_chat_focus(false);

        for press in [letter('h'), letter('l'), key(KeyCode::Backspace)] {
            assert_eq!(view.handle_key(press, 10), None);
        }
        assert!(view.input.is_empty(), "an unfocused bar swallows letters");
        assert_eq!(
            view.handle_key(key(KeyCode::Enter), 10),
            None,
            "an unfocused bar sends nothing on Enter"
        );

        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following(), "scrolling stays alive");
        view.handle_key(key(KeyCode::End), 10);
        assert!(view.following());

        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            view.handle_key(ctrl_x, 10),
            Some(Action::Abort {
                task: "borsuk/implement-i142".to_string(),
            }),
            "abort stays alive without the focus"
        );
    }

    #[test]
    fn the_draw_marks_an_unfocused_bar_with_a_switch_hint() {
        let dir = TempDir::new("hint");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.set_tabs(vec![
            "borsuk/refine-i143".to_string(),
            "borsuk/implement-i142".to_string(),
        ]);
        view.set_chat_focus(false);

        let screen = drawn_screen(&view);
        assert!(
            screen.contains("h l session · 1-5 views · i or enter chats"),
            "hint: {screen}"
        );
        assert_eq!(
            drawn_border_style(&view).fg,
            Some(THEME.dim),
            "an unfocused bar renders dim"
        );

        // With one live session there is nothing to switch between.
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        view.set_chat_focus(false);
        let screen = drawn_screen(&view);
        assert!(
            screen.contains("1-5 views · i or enter chats"),
            "hint: {screen}"
        );
        assert!(!screen.contains("switch session"), "hint: {screen}");

        // A focused bar shows the mode hint and keeps its border.
        view.set_chat_focus(true);
        let screen = drawn_screen(&view);
        assert!(screen.contains("enter send"), "hint: {screen}");
        assert_ne!(drawn_border_style(&view).fg, Some(THEME.dim));
    }

    #[test]
    fn the_chat_focus_survives_a_task_switch() {
        let dir = TempDir::new("focus-keep");
        let first_log = dir.path().join("first.jsonl");
        let second_log = dir.path().join("second.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&first_log));
        view.set_chat_focus(false);

        let mut other = sample_task(&second_log);
        other.id = "borsuk/refine-i7".to_string();
        view.show(&other);

        assert!(
            !view.chat_focus(),
            "cycling to another session keeps the focus released"
        );
        assert_eq!(view.tabs().len(), 0, "the tab list is shell-level data");
    }
}
