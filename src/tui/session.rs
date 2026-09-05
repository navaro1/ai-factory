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
//! The view splits its pane in two when the terminal is wide enough: a
//! left subagents panel of [`PANEL_COLS`] columns, and the transcript to
//! the right of it. The panel lists every subagent the session spawned
//! and the session meters; [`AgentRoster`] parses the same log lines the
//! transcript tail reads. `ctrl-a` moves the keyboard focus to the panel;
//! `enter` then replaces the transcript pane with the child transcript of
//! the selected row.
//!
//! The view holds a chat focus flag. A focused bar types; an unfocused bar
//! renders dim and swallows typing, so the shell can use plain letters for
//! its own keys. The shell releases the focus with `esc` or `tab` and
//! takes it back with `i` or `enter`. The header shows one tab per live
//! session when the daemon pushes more than one; the shell switches tabs
//! with `h` and `l`.
//!
//! The chat focus and the panel focus are exclusive: taking one releases
//! the other.
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

use super::agents::{AgentKind, AgentRoster, AgentStatus};
use super::theme::THEME;
use crate::config::Harness;
use crate::decisions::{Decision, DecisionKind};
use crate::sock::{Action, InputMode, RoleBindingView, TaskView};
use crate::tui::transcript::{self, Entry};
use crate::usage::{self, UsageView};

/// How many parsed items the transcript ring buffer keeps.
pub const RING_CAP: usize = 2000;

/// The shortest gap between two file-change polls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The width of the left subagents panel, in terminal columns.
const PANEL_COLS: u16 = 32;

/// The session width below which the panel hides, in terminal columns.
///
/// At or above this width the transcript keeps at least [`PANEL_COLS`]
/// columns of its own.
const PANEL_MIN_COLS: u16 = 64;

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
    /// True while the subagents panel takes the steering keys.
    panel_focus: bool,
    tailer: Option<LogTailer>,
    ring: Ring<Entry>,
    roster: AgentRoster,
    /// The panel row the selection marks, as a position in the panel
    /// display order: the agent rows first, then the bash rows.
    selected: usize,
    /// The task id of the open drill-in row, when one is open.
    open: Option<String>,
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
            panel_focus: false,
            tailer: None,
            ring: Ring::new(RING_CAP),
            roster: AgentRoster::new(),
            selected: 0,
            open: None,
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
    ///
    /// Taking the chat focus releases the panel focus, because the two
    /// focuses are exclusive.
    pub fn set_chat_focus(&mut self, focus: bool) {
        self.chat_focus = focus;
        if focus {
            self.panel_focus = false;
        }
    }

    /// True while the subagents panel takes the steering keys.
    pub fn panel_focus(&self) -> bool {
        self.panel_focus
    }

    /// Take or release the panel focus.
    ///
    /// Taking the panel focus releases the chat focus, because the two
    /// focuses are exclusive.
    pub fn set_panel_focus(&mut self, focus: bool) {
        self.panel_focus = focus;
        if focus {
            self.chat_focus = false;
        }
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
    /// state, not data of one task. The roster, the panel selection, and
    /// the drill-in belong to the task, so they reset.
    pub fn clear(&mut self) {
        self.task = None;
        self.tailer = None;
        self.ring.clear();
        self.roster.clear();
        self.panel_focus = false;
        self.selected = 0;
        self.open = None;
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
    /// Each raw line parses once into a JSON value. The transcript keeps
    /// its items, and the subagent roster reads the same value; a line
    /// that is not JSON never reaches the roster.
    ///
    /// A restarted log clears the ring and the roster: the transcript
    /// restarts with the file, because the replaced history no longer
    /// exists.
    fn ingest(&mut self) -> bool {
        let mut lines = Vec::new();
        let restarted = self
            .tailer
            .as_mut()
            .is_some_and(|tailer| tailer.read_lines(&mut lines));
        let mut changed = restarted && !self.ring.is_empty();
        if restarted {
            self.ring.clear();
            self.roster.clear();
            self.selected = 0;
            self.open = None;
        }
        let harness = self
            .task
            .as_ref()
            .and_then(|task| task.binding.as_ref().map(|binding| binding.harness));
        for line in lines {
            changed = true;
            if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                self.roster.ingest(&value, harness);
            }
            for entry in transcript::parse(&line) {
                self.ring.push(entry);
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
    /// focus. A bar that cannot take text keeps the `? help` key. The
    /// panel focus names its own steering keys. A released focus names
    /// the session and pipeline keys, and a view with no task falls back
    /// to the shell view keys.
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
        if self.panel_focus {
            return "ctrl-a release · Up Down select · enter open · esc close".to_string();
        }
        if self.input_enabled() {
            return "i enter focus · h l · ctrl-a panel · esc pipeline · ? help".to_string();
        }
        "h l session · ctrl-a panel · esc pipeline · ? help".to_string()
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
    /// `ctrl-a` takes or releases the subagent panel focus, whatever the
    /// chat bar does. While the panel holds the focus, `Up` and `Down`
    /// move the selection, Enter opens the selected row, and `esc` closes
    /// the open drill-in or releases the panel focus.
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
            (KeyCode::Char('a'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.set_panel_focus(!self.panel_focus);
                None
            }
            _ if self.panel_focus => {
                self.panel_key(key, page);
                None
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

    /// Apply one key press while the subagents panel holds the focus.
    ///
    /// `Up` and `Down` move the selection in the order the panel shows
    /// the rows: the agent rows first, then the bash rows. Enter opens
    /// the drill-in of the selected row. `esc` closes an open drill-in
    /// first and releases the panel focus second. The scroll keys keep
    /// working on the transcript pane.
    fn panel_key(&mut self, key: KeyEvent, page: usize) {
        let last = self.panel_order().len().saturating_sub(1);
        self.selected = self.selected.min(last);
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(last);
            }
            KeyCode::Enter => {
                let index = self.panel_order().get(self.selected).copied();
                if let Some(index) = index {
                    self.open = Some(self.roster.rows()[index].task_id.clone());
                }
            }
            KeyCode::Esc => {
                if self.open.is_some() {
                    self.open = None;
                } else {
                    self.panel_focus = false;
                }
            }
            KeyCode::PageUp => {
                self.scroll_up = self.scroll_up.saturating_add(page);
            }
            KeyCode::PageDown => {
                self.scroll_up = self.scroll_up.saturating_sub(page);
            }
            KeyCode::End => {
                self.scroll_up = 0;
            }
            _ => {}
        }
    }

    /// The roster row indices in panel display order.
    ///
    /// The panel shows the agent rows first, then the bash rows, and
    /// each group keeps its creation order. The selection and the
    /// highlight follow this order, not the creation order, so the
    /// keys move the highlight the way the panel draws it.
    fn panel_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = self
            .roster
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == AgentKind::Agent)
            .map(|(index, _)| index)
            .collect();
        order.extend(
            self.roster
                .rows()
                .iter()
                .enumerate()
                .filter(|(_, row)| row.kind == AgentKind::Bash)
                .map(|(index, _)| index),
        );
        order
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

    /// The content of the transcript pane: the drill-in when one is open,
    /// the main transcript otherwise.
    fn pane_lines(&self, width: u16) -> Vec<Line<'static>> {
        match &self.open {
            Some(task_id) => self.drill_lines(task_id, width),
            None => self.transcript_lines(width),
        }
    }

    /// Build the drill-in lines of one open roster row.
    ///
    /// The pane header names the row. For an opencode row the pane shows
    /// the single output entry and states that opencode streams no child
    /// transcript; a claude row streams its full child transcript.
    fn drill_lines(&self, task_id: &str, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let Some(index) = self
            .roster
            .rows()
            .iter()
            .position(|row| row.task_id == task_id)
        else {
            lines.push(Line::styled(
                "the subagent is gone from the roster",
                transcript::dim_style(),
            ));
            return lines;
        };
        let row = &self.roster.rows()[index];
        let mut header = format!(
            "— {} — ",
            if row.description.is_empty() {
                "subagent"
            } else {
                &row.description
            }
        );
        match &row.status {
            AgentStatus::Running => header.push_str("running"),
            AgentStatus::Done => header.push_str("done"),
            AgentStatus::Failed(text) => header.push_str(text),
        }
        lines.push(Line::styled(header, transcript::ask_style()));
        let entries = self.roster.child_entries(task_id);
        if entries.is_empty() {
            lines.push(Line::styled("no child output yet", transcript::dim_style()));
        }
        for entry in entries {
            lines.extend(transcript::render(entry, width));
        }
        if self.binding_harness() == Some(Harness::Opencode) {
            lines.push(Line::styled(
                "opencode streams no child transcript",
                transcript::dim_style(),
            ));
        }
        lines
    }

    /// The harness of the shown task's binding, when one is bound.
    fn binding_harness(&self) -> Option<Harness> {
        self.task
            .as_ref()
            .and_then(|task| task.binding.as_ref())
            .map(|binding| binding.harness)
    }

    /// Build the subagents panel lines for one full-height left pane.
    ///
    /// The panel holds three parts: the session meters and quota rows of
    /// the billed identity, one row per agent subagent, and one
    /// row per backgrounded bash task. Every line is cut to the panel
    /// width; no input can make a line wider.
    fn panel_lines(&self, usage: &[UsageView]) -> Vec<Line<'static>> {
        let dim = transcript::dim_style();
        let title = Style::default()
            .fg(THEME.accent)
            .add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        lines.push(Line::styled("session", title));
        lines.extend(self.panel_session_lines(usage));
        lines.push(Line::styled(String::new(), dim));
        lines.push(Line::styled("subagents", title));
        if self.binding_harness() == Some(Harness::Opencode) {
            lines.push(Line::styled("(opencode: no live progress)", dim));
        }
        let agent_rows: Vec<&crate::tui::agents::AgentRow> = self
            .roster
            .rows()
            .iter()
            .filter(|row| row.kind == AgentKind::Agent)
            .collect();
        if agent_rows.is_empty() {
            lines.push(Line::styled("no subagents", dim));
        }
        let selected = self
            .selected
            .min(self.panel_order().len().saturating_sub(1));
        for (position, &row) in agent_rows.iter().enumerate() {
            lines.extend(self.agent_row_lines(row, position == selected));
        }
        let bash_rows: Vec<&crate::tui::agents::AgentRow> = self
            .roster
            .rows()
            .iter()
            .filter(|row| row.kind == AgentKind::Bash)
            .collect();
        if !bash_rows.is_empty() {
            lines.push(Line::styled(String::new(), dim));
            lines.push(Line::styled("background", title));
            for (offset, &row) in bash_rows.iter().enumerate() {
                let selected = agent_rows.len() + offset == selected;
                lines.extend(self.agent_row_lines(row, selected));
            }
        }
        lines
    }

    /// The `session` part: harness, model, context, spend, and the quota
    /// rows of the identity the shown task binds.
    ///
    /// A task with no binding, or an empty usage list, draws no quota
    /// rows and no empty box. The context prints as a token value; the
    /// context line itself appears only when the log carries a count, so
    /// a codex session shows none.
    fn panel_session_lines(&self, usage: &[UsageView]) -> Vec<Line<'static>> {
        let dim = transcript::dim_style();
        let mut lines = Vec::new();
        let binding = self.task.as_ref().and_then(|task| task.binding.as_ref());
        let model = self
            .roster
            .meters()
            .model
            .clone()
            .or_else(|| binding.as_ref().map(|binding| binding.model.clone()));
        let mut summary = String::new();
        if let Some(binding) = binding {
            summary.push_str(binding.harness.program());
            if let Some(model) = &model {
                summary.push_str(" · ");
                summary.push_str(model);
            }
        } else if let Some(model) = &model {
            summary.push_str(model);
        }
        if !summary.is_empty() {
            lines.push(fit_line(summary, dim));
        }
        let meters = self.roster.meters();
        let mut facts: Vec<String> = Vec::new();
        if let Some(tokens) = meters.context_tokens {
            facts.push(format!("ctx {} tok", compact_tokens(tokens)));
        }
        if let Some(spend) = meters.spend_usd {
            facts.push(format!("spend {}", format_usd(spend)));
        }
        if !facts.is_empty() {
            lines.push(fit_line(facts.join(" · "), dim));
        }
        let Some(binding) = binding else {
            return lines;
        };
        let identity = usage::identity_of(binding.harness, &binding.model);
        for row in usage.iter().filter(|row| row.identity == identity) {
            let plan = match (&row.mode, &row.plan) {
                (usage::UsageMode::Plan, Some(plan)) => format!("{identity} · {plan} plan"),
                (usage::UsageMode::Plan, None) => format!("{identity} · plan"),
                (usage::UsageMode::Api, _) => format!("{identity} · api"),
                (usage::UsageMode::Unknown, _) => identity.clone(),
            };
            lines.push(fit_line(plan, dim));
            for window in &row.windows {
                let mut text = format!("{} {}% left", window.label, window.used_percent as u64);
                if let Some(resets_at_ms) = window.resets_at_ms {
                    text.push_str(&format!(" · resets {}", utc_hhmm(resets_at_ms)));
                }
                lines.push(fit_line(text, dim));
            }
            if row.factory_spend_usd > 0.0 {
                lines.push(fit_line(
                    format!("factory {}", format_usd(row.factory_spend_usd)),
                    dim,
                ));
            }
            if let Some(org) = &row.org_spend {
                lines.push(fit_line(format!("org {}", format_usd(org.amount_usd)), dim));
            }
            if let Some(credits) = &row.credits {
                lines.push(fit_line(
                    format!("{} {}", credits.label, format_amount(credits.remaining)),
                    dim,
                ));
            }
            if let Some(error) = &row.error {
                lines.push(fit_line(format!("probe: {error}"), dim));
            }
        }
        lines
    }

    /// The panel lines of one roster row.
    fn agent_row_lines(
        &self,
        row: &crate::tui::agents::AgentRow,
        selected: bool,
    ) -> Vec<Line<'static>> {
        let style = match (&row.status, selected) {
            (_, true) => THEME.selected(),
            (AgentStatus::Running, false) => Style::default().fg(THEME.accent),
            (AgentStatus::Done, false) => transcript::dim_style(),
            (AgentStatus::Failed(_), false) => Style::default().fg(THEME.error),
        };
        let marker = match &row.status {
            AgentStatus::Running => "•",
            AgentStatus::Done => "✓",
            AgentStatus::Failed(_) => "✗",
        };
        let mut head = String::from(marker);
        if let Some(kind) = &row.subagent_type {
            head.push(' ');
            head.push_str(kind);
        }
        if let Some(name) = &row.name {
            head.push_str(&format!(" {name}"));
        }
        if !row.description.is_empty() {
            head.push_str(&format!(": {}", row.description));
        }
        let mut detail = String::new();
        match &row.status {
            AgentStatus::Running => {}
            AgentStatus::Done => {
                detail.push_str("done");
                if let Some(duration_ms) = row.duration_ms {
                    detail.push_str(&format!(" {}", format_duration(duration_ms)));
                }
            }
            AgentStatus::Failed(text) => detail.push_str(&format!("failed: {text}")),
        }
        if let Some(tokens) = row.total_tokens {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(&format!("{} tok", compact_tokens(tokens)));
        }
        if let Some(uses) = row.tool_uses {
            detail.push_str(&format!(" · {uses} tools"));
        }
        if let Some(last_tool) = &row.last_tool {
            detail.push_str(&format!(" · {last_tool}"));
        }
        let mut lines = vec![fit_line(head, style)];
        // The model gets its own line, because a provider-qualified id
        // alone can fill the panel width.
        if let Some(model) = &row.model {
            lines.push(fit_line(format!("  {model}"), style));
        }
        if !detail.is_empty() {
            lines.push(fit_line(format!("  {detail}"), style));
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
    /// At 64 or more columns the view splits first: a [`PANEL_COLS`]
    /// wide subagents panel on the left, and the session content in the
    /// rest. Below 64 columns the panel hides, so the transcript keeps
    /// at least [`PANEL_COLS`] columns and draws exactly as before. The
    /// split changes no height, so the vertical layout, from the top, is
    /// still a one-row task header, the transcript, the inline ask block
    /// when a decision of this task waits, and the input bar at the
    /// bottom. An open drill-in replaces the transcript pane content.
    pub fn draw(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        decisions: &[Decision],
        usage: &[UsageView],
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (panel_rect, area) = if area.width >= PANEL_MIN_COLS {
            let panel = Rect::new(area.x, area.y, PANEL_COLS, area.height);
            let rest = Rect::new(
                area.x + PANEL_COLS,
                area.y,
                area.width - PANEL_COLS,
                area.height,
            );
            (Some(panel), rest)
        } else {
            (None, area)
        };
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
            let lines = self.pane_lines(transcript_rect.width);
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

        if let Some(panel) = panel_rect {
            frame.render_widget(Paragraph::new(self.panel_lines(usage)), panel);
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

/// Cut one line to at most `cols` terminal display columns.
fn cut_cols(text: &str, cols: usize) -> String {
    let mut end = 0;
    let mut used = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        let width = Span::raw(&text[index..next]).width();
        if used + width > cols {
            break;
        }
        used += width;
        end = next;
    }
    text[..end].to_string()
}

/// One panel line cut to the panel width.
fn fit_line(text: String, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        cut_cols(&text, usize::from(PANEL_COLS)),
        style,
    ))
}

/// Shorten a token count for the narrow panel.
fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// Format a dollar amount with its sign.
fn format_usd(amount: f64) -> String {
    format!("${amount:.2}")
}

/// Format a bare amount: whole when it is whole, two decimals otherwise.
fn format_amount(amount: f64) -> String {
    if amount.fract() == 0.0 {
        format!("{amount:.0}")
    } else {
        format!("{amount:.2}")
    }
}

/// Format one duration in milliseconds for the narrow panel.
fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1_000.0)
    }
}

/// The hour and minute of one Unix-millisecond instant, in UTC.
///
/// The panel has no clock of its own, so a reset time prints as the UTC
/// wall clock it names.
fn utc_hhmm(ms: u64) -> String {
    let minutes = ms / 60_000;
    format!("{:02}:{:02}Z", (minutes / 60) % 24, minutes % 60)
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

        // A released focus offers the focus, panel, session, and pipeline
        // keys.
        view.set_chat_focus(false);
        assert_eq!(
            view.footer_hints(),
            "i enter focus · h l · ctrl-a panel · esc pipeline · ? help"
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
        assert_eq!(
            view.footer_hints(),
            "h l session · ctrl-a panel · esc pipeline · ? help"
        );

        // The panel focus names only its own steering keys.
        view.set_panel_focus(true);
        assert_eq!(
            view.footer_hints(),
            "ctrl-a release · Up Down select · enter open · esc close"
        );

        for hint in [
            "1 2 3 4 5 views · ? help",
            "esc tab release focus",
            "esc tab release focus · ? help",
            "i enter focus · h l · ctrl-a panel · esc pipeline · ? help",
            "h l session · ctrl-a panel · esc pipeline · ? help",
            "ctrl-a release · Up Down select · enter open · esc close",
        ] {
            assert!(hint.chars().count() <= crate::tui::HINT_CAP, "hint {hint}");
        }
    }

    #[test]
    fn ctrl_a_takes_and_releases_the_panel_focus_and_releases_the_chat() {
        let dir = TempDir::new("panel-focus");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);

        // Taking the panel focus releases the chat focus.
        assert!(view.chat_focus());
        assert_eq!(view.handle_key(ctrl_a, 10), None);
        assert!(view.panel_focus());
        assert!(!view.chat_focus());

        // A chat-focused key press types nothing while the panel holds
        // the keyboard, and the scroll keys stay alive.
        assert_eq!(view.handle_key(letter('h'), 10), None);
        assert!(view.input.is_empty());
        view.handle_key(key(KeyCode::PageUp), 10);
        assert!(!view.following());
        view.handle_key(key(KeyCode::End), 10);
        assert!(view.following());

        // The second ctrl-a releases the panel focus again.
        assert_eq!(view.handle_key(ctrl_a, 10), None);
        assert!(!view.panel_focus());

        // Taking the chat focus back releases the panel focus too.
        view.set_panel_focus(true);
        view.set_chat_focus(true);
        assert!(!view.panel_focus(), "the two focuses are exclusive");
        assert!(view.chat_focus());
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

        let backend = TestBackend::new(63, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 63, 12), &[], &[]))
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

        let backend = TestBackend::new(63, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 63, 12), &[], &[]))
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
        terminal
            .draw(|frame| view.draw(frame, area, &[], &[]))
            .unwrap();

        let screen = terminal.backend().to_string();
        assert!(screen.contains("borsuk/implement-i142"), "header: {screen}");
        assert!(screen.contains("running"), "state: {screen}");
        assert!(screen.contains("refine done"), "transcript: {screen}");
        assert!(screen.contains("chat"), "input bar title: {screen}");
        assert!(screen.contains("enter send"), "hint: {screen}");
    }

    /// Render the view at `width` by `height` and return the screen text.
    fn draw_screen(view: &SessionView, width: u16, height: u16, usage: &[UsageView]) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, Rect::new(0, 0, width, height), &[], usage))
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

        // 160 columns leave the transcript pane wide enough for the
        // whole header line beside the 32-column panel.
        let screen = draw_screen(&view, 160, 10, &[]);

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

        let screen = draw_screen(&view, 120, 10, &[]);

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

        let screen = draw_screen(&view, 120, 10, &[]);

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

        let screen = draw_screen(&view, 160, 10, &[]);

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
        let screen = draw_screen(&view, 160, 10, &[]);
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

        let screen = draw_screen(&view, 160, 10, &[]);
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
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 60, 10), &[], &[]))
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
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 60, 20), &decisions, &[]))
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
            .draw(|frame| view.draw(frame, Rect::new(0, 0, 30, 1), &[], &[]))
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

    /// Render the view at 63 by 12 and return the visible text.
    ///
    /// 63 columns hide the panel, so the legacy assertions keep the exact
    /// layout the session view had before the panel existed.
    fn drawn_screen(view: &SessionView) -> String {
        draw_screen(view, 63, 12, &[])
    }

    /// Render the view and return the style of the input bar top border.
    ///
    /// The input bar fills the bottom three rows, so its top border row is
    /// three rows above the bottom edge.
    fn drawn_border_style(view: &SessionView) -> ratatui::style::Style {
        let backend = TestBackend::new(63, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 63, 12);
        terminal
            .draw(|frame| view.draw(frame, area, &[], &[]))
            .unwrap();
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

    // -- subagents panel ------------------------------------------------

    use crate::usage::{Credits, OrgSpend, UsageMode, UsageWindow};

    /// Append `lines` to the log and read them into the view.
    ///
    /// The append keeps the file length monotonic, so the tailer treats
    /// the feed as one growing log and never restarts the roster.
    fn feed(view: &mut SessionView, log: &Path, lines: &[&str]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        view.on_redraw(Instant::now());
    }

    const SPAWN_STARTED: &str = r#"{"type":"system","subtype":"task_started","uuid":"u1","session_id":"s1","task_id":"task-1","tool_use_id":"toolu_a","description":"Refine the spec","task_type":"local_agent","subagent_type":"Explore","is_backgrounded":false,"spawn_depth":1,"prompt":"go"}"#;
    const SPAWN_PROGRESS: &str = r#"{"type":"system","subtype":"task_progress","uuid":"u2","session_id":"s1","task_id":"task-1","tool_use_id":"toolu_a","description":"Refine the spec","subagent_type":"Explore","last_tool_name":"Read","usage":{"total_tokens":1234,"tool_uses":3,"duration_ms":4200}}"#;
    const SPAWN_DONE: &str = r#"{"type":"system","subtype":"task_notification","uuid":"u3","session_id":"s1","task_id":"task-1","tool_use_id":"toolu_a","status":"completed","summary":"spec refined","output_file":"/tmp/out"}"#;
    const CLAUDE_CONTEXT: &str = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"usage":{"input_tokens":183421,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":5}}}"#;
    const OC_TASK: &str = r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"task","callID":"call_1","state":{"status":"completed","title":"Explore the code","input":{"description":"d","prompt":"p","subagent_type":"explore"},"output":"the answer","time":{"start":1000,"end":5000},"metadata":{"parentSessionId":"p","sessionId":"c","model":{"providerID":"zai-coding-plan","modelID":"glm-5.3-flash"},"truncated":false}}}}"#;

    #[test]
    fn the_panel_shows_a_running_claude_subagent_row() {
        let dir = TempDir::new("panel-running");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        feed(&mut view, &log, &[SPAWN_STARTED]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("subagents"), "panel: {screen}");
        assert!(screen.contains("Explore"), "type: {screen}");
        assert!(screen.contains("Refine the spec"), "description: {screen}");
        assert!(screen.contains("•"), "a running row is marked: {screen}");

        feed(&mut view, &log, &[SPAWN_PROGRESS]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("1k tok"), "tokens: {screen}");
        assert!(screen.contains("3 tools"), "tool uses: {screen}");
        assert!(screen.contains("Read"), "last tool: {screen}");
    }

    #[test]
    fn the_spawn_block_name_shows_in_the_panel_row() {
        let dir = TempDir::new("panel-name");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let spawn = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"toolu_a","name":"Agent","input":{"description":"Refine the spec","subagent_type":"Explore","name":"spec-reader","prompt":"go"}}]}}"#;

        feed(&mut view, &log, &[SPAWN_STARTED, spawn, SPAWN_PROGRESS]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("spec-reader"), "name: {screen}");
    }

    #[test]
    fn the_panel_marks_a_done_row_and_a_failed_row() {
        let dir = TempDir::new("panel-status");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let failed_started = SPAWN_STARTED.replace(
            r#""task_id":"task-1","tool_use_id":"toolu_a""#,
            r#""task_id":"task-2","tool_use_id":"toolu_b""#,
        );
        let failed = r#"{"type":"system","subtype":"task_notification","task_id":"task-2","tool_use_id":"toolu_b","status":"stalled","summary":"gave up"}"#;

        feed(
            &mut view,
            &log,
            &[SPAWN_STARTED, SPAWN_DONE, &failed_started, failed],
        );
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("✓"), "a done row is marked: {screen}");
        assert!(screen.contains("✗"), "a failed row is marked: {screen}");
        assert!(
            screen.contains("stalled"),
            "the failure text shows: {screen}"
        );
    }

    #[test]
    fn a_backgrounded_bash_task_shows_under_background_only() {
        let dir = TempDir::new("panel-bash");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let bash = r#"{"type":"system","subtype":"task_started","task_id":"bash-1","tool_use_id":"toolu_c","description":"run the tests","task_type":"local_bash","is_backgrounded":true}"#;

        feed(&mut view, &log, &[SPAWN_STARTED, bash]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("background"), "the bash section: {screen}");
        assert!(screen.contains("run the tests"), "the bash row: {screen}");
    }

    #[test]
    fn an_opencode_task_row_shows_done_with_the_model_and_the_duration() {
        let dir = TempDir::new("panel-opencode");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, None));

        feed(&mut view, &log, &[OC_TASK]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("✓"), "the row is done: {screen}");
        assert!(screen.contains("Explore the code"), "title: {screen}");
        assert!(screen.contains("4.0s"), "duration: {screen}");
        assert!(
            screen.contains("zai-coding-plan/glm-5.3-flash"),
            "model: {screen}"
        );
        assert!(
            screen.contains("(opencode: no live progress)"),
            "the no-progress note: {screen}"
        );
    }

    #[test]
    fn a_codex_session_shows_the_session_part_and_no_subagent_rows() {
        let dir = TempDir::new("panel-codex");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        let mut task = sample_task(&log);
        task.binding = Some(RoleBindingView {
            harness: crate::config::Harness::Codex,
            model: "gpt-5.6-sol".to_string(),
            effort: None,
        });
        view.show(&task);

        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("codex · gpt-5.6-sol"), "session: {screen}");
        assert!(screen.contains("no subagents"), "rows: {screen}");
        assert!(!screen.contains("ctx "), "codex has no context: {screen}");
    }

    #[test]
    fn the_panel_shows_context_tokens_for_claude_and_for_opencode() {
        let dir = TempDir::new("panel-context");
        let claude_log = dir.path().join("claude.jsonl");
        let opencode_log = dir.path().join("opencode.jsonl");

        let mut view = SessionView::new();
        view.show(&sample_task(&claude_log));
        feed(&mut view, &claude_log, &[CLAUDE_CONTEXT]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("ctx 183k tok"), "claude context: {screen}");

        let mut view = SessionView::new();
        view.show(&bound_task(&opencode_log, None));
        let step = r#"{"type":"step_finish","part":{"reason":"ok","cost":0.0,"tokens":{"total":100,"input":60,"output":30,"reasoning":10,"cache":{"write":5,"read":40}}}}"#;
        feed(&mut view, &opencode_log, &[step]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("ctx 100 tok"), "opencode context: {screen}");
    }

    #[test]
    fn the_panel_shows_the_session_spend_when_the_log_carries_one() {
        let dir = TempDir::new("panel-spend");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let result =
            r#"{"type":"result","subtype":"success","result":"done","total_cost_usd":0.42}"#;

        feed(&mut view, &log, &[result]);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("spend $0.42"), "spend: {screen}");
    }

    fn usage_row(identity: &str, harness: crate::config::Harness) -> UsageView {
        UsageView {
            identity: identity.to_string(),
            harness,
            mode: UsageMode::Plan,
            plan: Some("Pro".to_string()),
            models: Vec::new(),
            windows: vec![UsageWindow {
                label: "5 hour".to_string(),
                used_percent: 42.0,
                resets_at_ms: Some(52_320_000),
            }],
            factory_spend_usd: 1.25,
            org_spend: Some(OrgSpend {
                label: "org this month".to_string(),
                amount_usd: 12.0,
            }),
            credits: Some(Credits {
                label: "credits".to_string(),
                remaining: 8500.0,
            }),
            updated_ms: 1_000,
            error: None,
        }
    }

    #[test]
    fn the_panel_joins_the_quota_rows_of_the_bound_identity() {
        let dir = TempDir::new("panel-quota");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, None));
        let usage = vec![
            usage_row("claude", crate::config::Harness::Claude),
            usage_row("zai-coding-plan", crate::config::Harness::Opencode),
        ];

        let screen = draw_screen(&view, 100, 32, &usage);
        assert!(
            screen.contains("zai-coding-plan · Pro plan"),
            "the bound identity row: {screen}"
        );
        assert!(screen.contains("5 hour 42% left"), "window: {screen}");
        assert!(screen.contains("resets 14:32Z"), "reset time: {screen}");
        assert!(screen.contains("factory $1.25"), "factory spend: {screen}");
        assert!(screen.contains("org $12.00"), "org spend: {screen}");
        assert!(screen.contains("credits 8500"), "credits: {screen}");
        assert!(
            !screen.contains("claude · Pro plan"),
            "the unbound identity stays out: {screen}"
        );
    }

    #[test]
    fn no_binding_or_empty_usage_draws_no_quota_rows() {
        let dir = TempDir::new("panel-no-quota");
        let unbound = dir.path().join("unbound.jsonl");
        let bound = dir.path().join("bound.jsonl");
        let usage = vec![usage_row(
            "zai-coding-plan",
            crate::config::Harness::Opencode,
        )];

        let mut view = SessionView::new();
        view.show(&sample_task(&unbound));
        let screen = draw_screen(&view, 100, 24, &usage);
        assert!(
            !screen.contains("Pro plan"),
            "no binding, no quota rows: {screen}"
        );
        assert!(
            !screen.contains("factory $1.25"),
            "no binding, no factory row: {screen}"
        );

        let mut view = SessionView::new();
        view.show(&bound_task(&bound, None));
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(
            !screen.contains("Pro plan"),
            "no usage rows, no quota rows: {screen}"
        );
    }

    #[test]
    fn the_panel_hides_below_64_columns_and_the_transcript_draws_as_today() {
        let dir = TempDir::new("panel-width");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        feed(&mut view, &log, &[SPAWN_STARTED]);
        let narrow = draw_screen(&view, 63, 24, &[]);
        assert!(
            !narrow.contains("subagents"),
            "the panel hides below 64 columns: {narrow}"
        );
        assert!(
            narrow.contains("system/task_started"),
            "the transcript draws exactly as today: {narrow}"
        );
        assert!(narrow.contains("attempt 1"), "narrow: {narrow}");

        let wide = draw_screen(&view, 64, 24, &[]);
        assert!(wide.contains("subagents"), "wide: {wide}");
        assert!(wide.contains("Explore"), "wide: {wide}");
        assert!(
            wide.contains("system/task_started"),
            "the transcript still draws: {wide}"
        );
    }

    #[test]
    fn the_panel_keys_select_open_and_close() {
        let dir = TempDir::new("panel-keys");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let second = SPAWN_STARTED.replace(
            r#""task_id":"task-1","tool_use_id":"toolu_a""#,
            r#""task_id":"task-2","tool_use_id":"toolu_b""#,
        );
        feed(&mut view, &log, &[SPAWN_STARTED, &second]);

        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(view.handle_key(ctrl_a, 10), None);
        assert!(view.panel_focus());

        // The first row is selected. Enter opens it.
        assert_eq!(view.selected, 0);
        assert_eq!(view.handle_key(key(KeyCode::Enter), 10), None);
        assert_eq!(view.open.as_deref(), Some("task-1"));

        // Esc closes the drill-in and keeps the panel focus.
        assert_eq!(view.handle_key(key(KeyCode::Esc), 10), None);
        assert!(view.open.is_none());
        assert!(view.panel_focus());

        // Down moves the selection; Enter opens the second row.
        view.handle_key(key(KeyCode::Down), 10);
        assert_eq!(view.selected, 1);
        view.handle_key(key(KeyCode::Enter), 10);
        assert_eq!(view.open.as_deref(), Some("task-2"));

        // Up moves back, and the ends hold the selection in range.
        view.handle_key(key(KeyCode::Up), 10);
        assert_eq!(view.selected, 0);
        view.handle_key(key(KeyCode::Up), 10);
        assert_eq!(view.selected, 0, "the top holds the selection");

        // The drill-in of the second row is still open. Esc closes it
        // first and keeps the panel focus.
        assert_eq!(view.open.as_deref(), Some("task-2"));
        assert_eq!(view.handle_key(key(KeyCode::Esc), 10), None);
        assert!(view.open.is_none());
        assert!(view.panel_focus());

        // A second esc releases the panel focus.
        assert_eq!(view.handle_key(key(KeyCode::Esc), 10), None);
        assert!(!view.panel_focus());

        // While the chat bar holds the focus again, ctrl-a still takes
        // the panel.
        view.set_chat_focus(true);
        assert_eq!(view.handle_key(ctrl_a, 10), None);
        assert!(view.panel_focus());
        assert!(!view.chat_focus());
    }

    #[test]
    fn the_selection_walks_the_visible_order_across_the_sections() {
        let dir = TempDir::new("panel-order");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        // The bash task is spawned first, so the roster holds the bash
        // row first while the panel shows the agent row above it.
        let bash = r#"{"type":"system","subtype":"task_started","task_id":"bash-1","tool_use_id":"toolu_c","description":"run the tests","task_type":"local_bash","is_backgrounded":true}"#;

        feed(&mut view, &log, &[bash, SPAWN_STARTED]);

        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(view.handle_key(ctrl_a, 10), None);
        assert_eq!(view.selected, 0, "the top visible row starts selected");

        // Enter opens the agent row, the first row the panel shows.
        view.handle_key(key(KeyCode::Enter), 10);
        assert_eq!(view.open.as_deref(), Some("task-1"));

        // Down walks into the background section, and Up walks back.
        view.handle_key(key(KeyCode::Down), 10);
        assert_eq!(view.selected, 1);
        view.handle_key(key(KeyCode::Enter), 10);
        assert_eq!(view.open.as_deref(), Some("bash-1"));
        view.handle_key(key(KeyCode::Down), 10);
        assert_eq!(view.selected, 1, "the last visible row holds");
        view.handle_key(key(KeyCode::Up), 10);
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn an_open_drill_in_shows_the_child_transcript_in_order() {
        let dir = TempDir::new("panel-drill");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));
        let child_one = r#"{"type":"assistant","parent_tool_use_id":"toolu_a","subagent_type":"Explore","task_description":"Refine the spec","message":{"content":[{"type":"text","text":"child prose one"}]}}"#;
        let child_result = r#"{"type":"user","parent_tool_use_id":"toolu_a","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_b","content":"the tool result"}]}}"#;
        let child_two = r#"{"type":"assistant","parent_tool_use_id":"toolu_a","message":{"content":[{"type":"text","text":"child prose two"}]}}"#;

        feed(
            &mut view,
            &log,
            &[SPAWN_STARTED, child_one, child_result, child_two],
        );
        view.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        view.handle_key(key(KeyCode::Enter), 10);

        let screen = draw_screen(&view, 100, 24, &[]);
        let prose_one = screen.find("child prose one").expect("first prose");
        let result = screen.find("the tool result").expect("tool result");
        let prose_two = screen.find("child prose two").expect("second prose");
        assert!(prose_one < result && result < prose_two, "order: {screen}");
        assert!(
            screen.contains("Refine the spec"),
            "the pane header names the row: {screen}"
        );
        assert!(
            !screen.contains("no child output"),
            "the child entries exist: {screen}"
        );

        // The main transcript never gained the child lines.
        view.handle_key(key(KeyCode::Esc), 10);
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(
            !screen.contains("child prose one"),
            "the main transcript excludes subagent output: {screen}"
        );
    }

    #[test]
    fn an_opencode_drill_in_names_the_missing_child_transcript() {
        let dir = TempDir::new("panel-drill-oc");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&bound_task(&log, None));

        feed(&mut view, &log, &[OC_TASK]);
        view.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        view.handle_key(key(KeyCode::Enter), 10);

        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(screen.contains("the answer"), "the output entry: {screen}");
        assert!(
            screen.contains("opencode streams no child transcript"),
            "the note: {screen}"
        );
    }

    #[test]
    fn a_task_switch_clears_the_roster_the_selection_and_the_drill_in() {
        let dir = TempDir::new("panel-switch");
        let first = dir.path().join("first.jsonl");
        let second = dir.path().join("second.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&first));

        feed(&mut view, &first, &[SPAWN_STARTED, SPAWN_PROGRESS]);
        view.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        view.handle_key(key(KeyCode::Down), 10);
        view.handle_key(key(KeyCode::Enter), 10);
        assert!(view.open.is_some());

        let mut other = sample_task(&second);
        other.id = "borsuk/refine-i7".to_string();
        view.show(&other);

        assert!(view.roster.rows().is_empty(), "the roster resets");
        assert_eq!(view.selected, 0, "the selection resets");
        assert!(view.open.is_none(), "the drill-in closes");
        assert!(!view.panel_focus(), "the panel focus releases");
    }

    #[test]
    fn a_line_that_is_not_json_never_reaches_the_roster_or_panics() {
        let dir = TempDir::new("panel-garbage");
        let log = dir.path().join("task.jsonl");
        let mut view = SessionView::new();
        view.show(&sample_task(&log));

        feed(
            &mut view,
            &log,
            &["this is not json {", "{\"type\":\"mystery\"", ""],
        );
        assert!(view.roster.rows().is_empty(), "garbage adds no rows");
        assert_eq!(
            view.roster.meters(),
            &crate::tui::agents::SessionMeters::default()
        );
        let screen = draw_screen(&view, 100, 24, &[]);
        assert!(
            screen.contains("no subagents"),
            "the panel survives: {screen}"
        );
    }
}
