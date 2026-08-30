//! Draws the decisions inbox and handles its keys.
//!
//! The inbox is the one place where everything that needs a human waits.
//! It lists every open decision across the repositories, and each key of
//! the selected row sends exactly one [`Action::Answer`] whose response
//! fits the kind of the row. The key map per kind:
//!
//! | Kind | Keys |
//! |---|---|
//! | `Permission` | `y` allow, `n` type a deny reason, `enter` open the session |
//! | `Question` | `1`..`9` pick, `enter` submit, `i` type a free answer |
//! | `Stuck` | `r` retry, `c` cancel, `enter` open the session |
//! | `NeedsHuman` | `t` type a comment, `c` cancel the label |
//! | `ReleaseGate` | `1`..`9` toggle one pull request, `space` all or none, `g` fire |
//!
//! Global keys: `j`, `k`, and the arrow keys move the selection, and `!`
//! selects the oldest decision. `enter` submits on a `Question` row and
//! jumps to the session view on the other rows that carry a task.
//!
//! Two contracts shape this module. The age of a row comes from the
//! `opened_ms` of the decision, so a re-push never resets it. A gate row
//! is a snapshot, so the checkboxes live here and show the optimistic
//! local change; the next push corrects the list.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::decisions::{Decision, DecisionKind, Response};
use crate::model::ItemKind;
use crate::sock::{Action, StateView};

/// The local UI state of the inbox view.
///
/// The state holds only what the pushed state does not carry: which row
/// the human selected, the picks and checkboxes of the expanded rows, and
/// a text input in progress. Every field survives a re-push, because the
/// keys name decision ids and not list positions.
#[derive(Debug, Default)]
pub struct Inbox {
    /// The id of the selected decision, if any.
    selected_id: Option<String>,
    /// The picked options of each question row, keyed by decision id.
    /// The inner vector runs parallel to the parsed questions, and each
    /// set holds the indexes of the picked options.
    picks: BTreeMap<String, Vec<BTreeSet<usize>>>,
    /// The checked pull requests of each gate row, keyed by decision id.
    checks: BTreeMap<String, BTreeSet<u64>>,
    /// The text input in progress, if any.
    input: Option<TextInput>,
    /// A short hint that explains a blocked intent.
    hint: Option<&'static str>,
}

/// What the human is typing, and the response it will produce.
#[derive(Debug)]
struct TextInput {
    /// The text typed so far.
    buffer: String,
    /// The label shown in front of the buffer.
    label: &'static str,
    /// The response the finished text produces.
    kind: InputKind,
}

/// The response one text input produces on `enter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    /// The reason of a `Deny` for a `Permission` row.
    DenyReason,
    /// A free `Text` answer for a `Question` or `NeedsHuman` row.
    FreeText,
}

/// What a key did beyond the local inbox state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxOutcome {
    /// The key changed only the local state.
    None,
    /// The key asked for the session view of one task id.
    OpenSession(String),
}

/// Where the inbox hands the actions it produces.
///
/// The shell passes its daemon client; the tests pass a fake sender.
pub trait ActionSink {
    /// Send one action to the daemon.
    fn send_action(&mut self, action: Action);
}

impl ActionSink for Vec<Action> {
    fn send_action(&mut self, action: Action) {
        self.push(action);
    }
}

impl ActionSink for Sender<Action> {
    fn send_action(&mut self, action: Action) {
        if let Err(error) = self.send(action) {
            eprintln!("inbox: the action receiver is gone: {error}");
        }
    }
}

impl Inbox {
    /// An inbox with no selection and no input.
    pub fn new() -> Self {
        Inbox::default()
    }

    /// Apply one pushed state.
    ///
    /// The call drops the local state of rows that closed and re-anchors
    /// the selection to the first row when its row is gone. The age of a
    /// row needs no bookkeeping here: it derives from `opened_ms`, which
    /// the daemon preserves across re-pushes.
    pub fn observe(&mut self, state: &StateView) {
        self.picks
            .retain(|id, _| state.decisions.iter().any(|d| d.id == *id));
        self.checks
            .retain(|id, _| state.decisions.iter().any(|d| d.id == *id));
        let selected_is_gone = self
            .selected_id
            .as_deref()
            .is_none_or(|id| !state.decisions.iter().any(|d| d.id == id));
        if selected_is_gone {
            self.selected_id = state.decisions.first().map(|d| d.id.clone());
        }
    }

    /// Select the row at `index`, clamped to the open rows.
    pub fn select_index(&mut self, state: &StateView, index: usize) {
        self.selected_id = state
            .decisions
            .get(index)
            .or_else(|| state.decisions.last())
            .map(|d| d.id.clone());
    }

    /// Select the oldest decision, as the `!` key promises.
    ///
    /// The oldest row is the one with the smallest `opened_ms`; a tie
    /// keeps the earlier push order.
    pub fn select_oldest(&mut self, state: &StateView) {
        let oldest = state
            .decisions
            .iter()
            .enumerate()
            .min_by_key(|(position, decision)| (decision.opened_ms, *position));
        self.selected_id = oldest.map(|(_, decision)| decision.id.clone());
    }

    /// Handle one key while the inbox view is open.
    ///
    /// The call sends at most one action to `sink` and returns what the
    /// shell must do beyond the inbox. A key that the kind of the
    /// selected row does not accept changes nothing, so the UI can never
    /// produce a response that the decision refuses.
    pub fn handle_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        self.hint = None;
        if self.input.is_some() {
            return self.typing_key(state, key, sink);
        }
        if state.decisions.is_empty() {
            return InboxOutcome::None;
        }
        match key.code {
            KeyCode::Char('!') => {
                self.select_oldest(state);
                InboxOutcome::None
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(state, 1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(state, -1),
            KeyCode::Esc => InboxOutcome::None,
            _ => self.row_key(state, key, sink),
        }
    }

    /// Move the selection by `step` rows and clamp at the ends.
    fn move_selection(&mut self, state: &StateView, step: isize) -> InboxOutcome {
        let Some(current) = self.selected_index(state) else {
            return InboxOutcome::None;
        };
        let last = state.decisions.len() - 1;
        let next = (current as isize + step).clamp(0, last as isize) as usize;
        self.select_index(state, next);
        InboxOutcome::None
    }

    /// The index of the selected row, or 0 when the row is gone.
    fn selected_index(&self, state: &StateView) -> Option<usize> {
        if state.decisions.is_empty() {
            return None;
        }
        let id = self.selected_id.as_deref();
        Some(
            state
                .decisions
                .iter()
                .position(|d| Some(d.id.as_str()) == id)
                .unwrap_or(0),
        )
    }

    /// Handle one key that names the selected row.
    fn row_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        let Some(index) = self.selected_index(state) else {
            return InboxOutcome::None;
        };
        let Some(decision) = state.decisions.get(index) else {
            return InboxOutcome::None;
        };
        let decision = decision.clone();
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return InboxOutcome::None;
        }
        match (&decision.kind, key.code) {
            (DecisionKind::Permission { .. }, KeyCode::Char('y')) => {
                self.answer(&decision.id, Response::Allow, sink);
                InboxOutcome::None
            }
            (DecisionKind::Permission { .. }, KeyCode::Char('n')) => {
                self.open_input("reason", InputKind::DenyReason);
                InboxOutcome::None
            }
            (DecisionKind::Permission { task, .. }, KeyCode::Enter) => {
                InboxOutcome::OpenSession(task.clone())
            }
            (DecisionKind::Question { .. }, KeyCode::Char(digit @ '1'..='9')) => {
                self.pick_option(&decision, digit);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Char('i')) => {
                self.open_input("answer", InputKind::FreeText);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Enter) => {
                self.submit_question(&decision, sink);
                InboxOutcome::None
            }
            (DecisionKind::Stuck { .. }, KeyCode::Char('r')) => {
                self.answer(&decision.id, Response::Retry, sink);
                InboxOutcome::None
            }
            (DecisionKind::Stuck { .. }, KeyCode::Char('c')) => {
                self.answer(&decision.id, Response::Cancel, sink);
                InboxOutcome::None
            }
            (DecisionKind::Stuck { task, .. }, KeyCode::Enter) => {
                InboxOutcome::OpenSession(task.clone())
            }
            (DecisionKind::NeedsHuman { .. }, KeyCode::Char('t')) => {
                self.open_input("comment", InputKind::FreeText);
                InboxOutcome::None
            }
            (DecisionKind::NeedsHuman { .. }, KeyCode::Char('c')) => {
                self.answer(&decision.id, Response::Cancel, sink);
                InboxOutcome::None
            }
            (DecisionKind::ReleaseGate { .. }, KeyCode::Char(digit @ '1'..='9')) => {
                self.toggle_gate_digit(&decision, digit);
                InboxOutcome::None
            }
            (DecisionKind::ReleaseGate { .. }, KeyCode::Char(' ')) => {
                self.toggle_all_gate_prs(&decision);
                InboxOutcome::None
            }
            (DecisionKind::ReleaseGate { .. }, KeyCode::Char('g')) => {
                self.fire_gate(&decision, sink);
                InboxOutcome::None
            }
            _ => InboxOutcome::None,
        }
    }

    /// Handle one key while the text input holds the keyboard.
    fn typing_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        let Some(index) = self.selected_index(state) else {
            return InboxOutcome::None;
        };
        let Some(decision) = state.decisions.get(index) else {
            return InboxOutcome::None;
        };
        let decision = decision.clone();
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return InboxOutcome::None;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                InboxOutcome::None
            }
            KeyCode::Backspace => {
                if let Some(input) = self.input.as_mut() {
                    input.buffer.pop();
                }
                InboxOutcome::None
            }
            KeyCode::Enter => self.submit_text(&decision, sink),
            KeyCode::Char(character) => {
                if let Some(input) = self.input.as_mut() {
                    input.buffer.push(character);
                }
                InboxOutcome::None
            }
            _ => InboxOutcome::None,
        }
    }

    /// Send one answer for the decision.
    fn answer(&self, id: &str, response: Response, sink: &mut impl ActionSink) {
        sink.send_action(Action::Answer {
            decision_id: id.to_string(),
            response,
        });
    }

    /// Start a text input.
    fn open_input(&mut self, label: &'static str, kind: InputKind) {
        self.input = Some(TextInput {
            buffer: String::new(),
            label,
            kind,
        });
    }

    /// Finish the text input and send its response.
    fn submit_text(&mut self, decision: &Decision, sink: &mut impl ActionSink) -> InboxOutcome {
        let Some(input) = self.input.take() else {
            return InboxOutcome::None;
        };
        let text = input.buffer.trim().to_string();
        if text.is_empty() {
            self.hint = Some("type the text first, or press esc");
            return InboxOutcome::None;
        }
        let response = match input.kind {
            InputKind::DenyReason => Response::Deny { message: text },
            InputKind::FreeText => Response::Text { text },
        };
        self.answer(&decision.id, response, sink);
        InboxOutcome::None
    }

    /// Apply one digit key to the options of a question row.
    ///
    /// The options are numbered across the whole expansion. On a
    /// multi-select question the digit toggles one option; on a
    /// single-select question it replaces the pick.
    fn pick_option(&mut self, decision: &Decision, digit: char) {
        let DecisionKind::Question { questions, .. } = &decision.kind else {
            return;
        };
        let parsed = parse_questions(questions);
        let Some((question_index, option_index)) = flattened_option(&parsed, digit) else {
            return;
        };
        let picks = self.picks.entry(decision.id.clone()).or_default();
        if picks.len() != parsed.len() {
            let empty = vec![BTreeSet::new(); parsed.len()];
            *picks = empty;
        }
        let chosen = &mut picks[question_index];
        if parsed[question_index].multi_select {
            if chosen.contains(&option_index) {
                chosen.remove(&option_index);
            } else {
                chosen.insert(option_index);
            }
        } else {
            *chosen = [option_index].into_iter().collect();
        }
    }

    /// Submit the picks of a question row as one `Answers` response.
    ///
    /// Every question must carry a pick. The answers use the verified
    /// wire shape `{"answers": {header: label}}`; a multi-select answer
    /// is a list of labels.
    fn submit_question(&mut self, decision: &Decision, sink: &mut impl ActionSink) {
        let DecisionKind::Question { questions, .. } = &decision.kind else {
            return;
        };
        let parsed = parse_questions(questions);
        if parsed.is_empty() {
            self.hint = Some("the options are unreadable; press i to answer in text");
            return;
        }
        let empty = Vec::new();
        let picks = self.picks.get(&decision.id).unwrap_or(&empty);
        let mut answers = serde_json::Map::new();
        for (index, question) in parsed.iter().enumerate() {
            let chosen = picks.get(index);
            if question.multi_select {
                let labels: Vec<String> = question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(option_index, _)| {
                        chosen.is_some_and(|set| set.contains(option_index))
                    })
                    .map(|(_, (label, _))| label.clone())
                    .collect();
                answers.insert(question.key.clone(), serde_json::json!(labels));
            } else {
                let Some(pick) = chosen.and_then(|set| set.iter().next()) else {
                    self.hint = Some("answer every question, or press i for a free answer");
                    return;
                };
                let Some((label, _)) = question.options.get(*pick) else {
                    self.hint = Some("answer every question, or press i for a free answer");
                    return;
                };
                answers.insert(question.key.clone(), serde_json::json!(label));
            }
        }
        self.answer(
            &decision.id,
            Response::Answers {
                updated_input: serde_json::json!({ "answers": answers }),
            },
            sink,
        );
    }

    /// The pull requests of a gate row that the checkboxes mark.
    fn checked_prs(&self, decision: &Decision) -> Vec<u64> {
        let DecisionKind::ReleaseGate { prs } = &decision.kind else {
            return Vec::new();
        };
        match self.checks.get(&decision.id) {
            Some(checked) => prs
                .iter()
                .copied()
                .filter(|pr| checked.contains(pr))
                .collect(),
            None => prs.clone(),
        }
    }

    /// Toggle one gate pull request by its digit.
    fn toggle_gate_digit(&mut self, decision: &Decision, digit: char) {
        let DecisionKind::ReleaseGate { prs } = &decision.kind else {
            return;
        };
        let index = match digit.to_digit(10) {
            Some(index @ 1..=9) => index as usize - 1,
            _ => return,
        };
        let Some(pr) = prs.get(index) else {
            return;
        };
        let pr = *pr;
        let checked = self
            .checks
            .entry(decision.id.clone())
            .or_insert_with(|| prs.iter().copied().collect());
        if !checked.remove(&pr) {
            checked.insert(pr);
        }
    }

    /// Toggle the gate checkboxes between all and none.
    fn toggle_all_gate_prs(&mut self, decision: &Decision) {
        let DecisionKind::ReleaseGate { prs } = &decision.kind else {
            return;
        };
        let all: BTreeSet<u64> = prs.iter().copied().collect();
        let checked = self.checked_prs(decision);
        let next = if checked.len() < all.len() {
            all
        } else {
            BTreeSet::new()
        };
        self.checks.insert(decision.id.clone(), next);
    }

    /// Fire the gate with the checked pull requests.
    fn fire_gate(&mut self, decision: &Decision, sink: &mut impl ActionSink) {
        let checked = self.checked_prs(decision);
        if checked.is_empty() {
            self.hint = Some("select at least one pull request");
            return;
        }
        self.answer(&decision.id, Response::Go { prs: checked }, sink);
    }
}

/// One parsed question of a `Question` decision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionView {
    /// The answer key: the header, or the question text without one.
    key: String,
    /// The text shown to the human.
    text: String,
    /// The options as label and description pairs.
    options: Vec<(String, String)>,
    /// True when the human may pick several options.
    multi_select: bool,
}

/// Parse the `questions` array of a `Question` decision.
///
/// Entries that miss both a question text and a header, or that name no
/// readable option labels, still parse with what they carry; a value
/// that is no array parses as no questions at all.
fn parse_questions(value: &serde_json::Value) -> Vec<QuestionView> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries.iter().filter_map(parse_question).collect()
}

/// Parse one question object of the array.
fn parse_question(value: &serde_json::Value) -> Option<QuestionView> {
    let object = value.as_object()?;
    let question = object
        .get("question")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let header = object
        .get("header")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let text = if question.is_empty() {
        header
    } else {
        question
    };
    if text.is_empty() {
        return None;
    }
    let key = if header.is_empty() { text } else { header };
    let mut options = Vec::new();
    if let Some(list) = object.get("options").and_then(serde_json::Value::as_array) {
        for option in list {
            let Some(label) = option.get("label").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let description = option
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            options.push((label.to_string(), description.to_string()));
        }
    }
    let multi_select = object
        .get("multiSelect")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(QuestionView {
        key: key.to_string(),
        text: text.to_string(),
        options,
        multi_select,
    })
}

/// Map one digit to its flattened option: question index and option index.
///
/// The numbering runs across every option of every question, so a
/// question with one option and a question with two options use the
/// digits `1`, `2`, and `3`.
fn flattened_option(questions: &[QuestionView], digit: char) -> Option<(usize, usize)> {
    let index = match digit.to_digit(10) {
        Some(index @ 1..=9) => index as usize - 1,
        _ => return None,
    };
    let mut flat = 0;
    for (question_index, question) in questions.iter().enumerate() {
        for option_index in 0..question.options.len() {
            if flat == index {
                return Some((question_index, option_index));
            }
            flat += 1;
        }
    }
    None
}

/// How many decisions wait for a human.
pub fn open_count(state: &StateView) -> usize {
    state.decisions.len()
}

/// The status bar badge with the open count.
///
/// The shell renders this text in every view. The `!` in the badge names
/// the key that jumps to the oldest decision.
pub fn badge(state: &StateView) -> String {
    format!("! {} open", state.decisions.len())
}

/// The oldest open decision, if any.
pub fn oldest_decision(state: &StateView) -> Option<&Decision> {
    state
        .decisions
        .iter()
        .enumerate()
        .min_by_key(|(position, decision)| (decision.opened_ms, *position))
        .map(|(_, decision)| decision)
}

/// The current time in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The age of one row as a short text.
///
/// The age derives from `opened_ms` alone, so a re-push of the same
/// condition never resets it. A clock in the past clamps to zero.
pub fn age_text(opened_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(opened_ms) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// The one-line summary of a decision.
fn summary(decision: &Decision) -> String {
    match &decision.kind {
        DecisionKind::Permission { tool, input, .. } => {
            format!("{tool} {}", input_summary(input))
        }
        DecisionKind::Question { questions, .. } => question_summary(questions),
        DecisionKind::Stuck { reason, .. } => reason.clone(),
        DecisionKind::NeedsHuman {
            kind,
            number,
            title,
        } => format!("#{number} {title} ({})", item_word(*kind)),
        DecisionKind::ReleaseGate { prs } => {
            format!("release {} pull requests: {}", prs.len(), pr_list(prs))
        }
    }
}

/// The short summary of one tool input.
fn input_summary(input: &serde_json::Value) -> String {
    if let Some(path) = input.get("file_path").and_then(serde_json::Value::as_str) {
        return path.chars().take(60).collect();
    }
    if let Some(command) = input.get("command").and_then(serde_json::Value::as_str) {
        return command.chars().take(60).collect();
    }
    input.to_string().chars().take(60).collect()
}

/// The short summary of the questions of one row.
fn question_summary(questions: &serde_json::Value) -> String {
    let parsed = parse_questions(questions);
    let Some(first) = parsed.first() else {
        return "a question with unreadable options".to_string();
    };
    let total_options: usize = parsed.iter().map(|q| q.options.len()).sum();
    if parsed.len() == 1 {
        format!("{} ({} options)", first.text, total_options)
    } else {
        format!(
            "{} questions, starting with: {} ({} options)",
            parsed.len(),
            first.text,
            total_options
        )
    }
}

/// The human word for one item kind.
fn item_word(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Issue => "issue",
        ItemKind::Pr => "pull request",
    }
}

/// The pull request list as `#7 #9`, at most five numbers.
fn pr_list(prs: &[u64]) -> String {
    let named: Vec<String> = prs.iter().take(5).map(|pr| format!("#{pr}")).collect();
    let mut text = named.join(" ");
    if prs.len() > 5 {
        text.push_str(&format!(" and {} more", prs.len() - 5));
    }
    text
}

/// The stage column text of a row.
fn stage_text(decision: &Decision) -> String {
    decision
        .stage
        .map(|stage| stage.as_str().to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Draw the inbox into `area`.
///
/// The shell calls this from its render closure when the inbox view is
/// open. `now_ms` stamps the age column; the shell passes [`now_ms`].
pub fn draw(f: &mut Frame, area: Rect, state: &StateView, inbox: &Inbox, now_ms: u64) {
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let mut lines: Vec<Line> = Vec::new();
    if state.decisions.is_empty() {
        lines.push(Line::from(
            "No open decisions. The factory asks here when an agent needs you.",
        ));
    }
    for decision in &state.decisions {
        let selected = inbox.selected_id.as_deref() == Some(decision.id.as_str());
        lines.push(row_line(decision, now_ms, selected));
        if selected {
            lines.extend(detail_lines(decision, inbox));
        }
    }
    let title = format!("decisions - {} open", state.decisions.len());
    let list = Paragraph::new(Text::from(lines)).block(Block::bordered().title(title));
    f.render_widget(list, rows[0]);
    f.render_widget(Paragraph::new(footer_text(state, inbox)), rows[1]);
}

/// Build one row line of the list.
fn row_line(decision: &Decision, now_ms: u64, selected: bool) -> Line<'static> {
    let mut style = Style::new();
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::styled(marker.to_string(), style),
        Span::styled(
            format!("{:>4}  ", age_text(decision.opened_ms, now_ms)),
            style,
        ),
        Span::styled(format!("{:<10} ", decision.repo), style),
        Span::styled(format!("{:<9} ", stage_text(decision)), style),
        Span::styled(summary(decision), style),
    ])
}

/// Build the detail lines of the selected row.
fn detail_lines(decision: &Decision, inbox: &Inbox) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let dim_line = move |text: String| Line::from(Span::styled(text, dim));
    match &decision.kind {
        DecisionKind::Permission {
            task, tool, input, ..
        } => vec![
            dim_line(format!("  task {task} asks to use {tool}")),
            dim_line(format!("  input {}", input_summary(input))),
        ],
        DecisionKind::Question { questions, .. } => {
            let mut lines = Vec::new();
            let mut flat = 0;
            for (question_index, question) in parse_questions(questions).into_iter().enumerate() {
                lines.push(dim_line(format!("  {}", question.text)));
                if question.multi_select {
                    lines.push(dim_line("  (pick any number of options)".to_string()));
                }
                for (option_index, (label, description)) in question.options.iter().enumerate() {
                    flat += 1;
                    let mark = if option_picked(inbox, &decision.id, question_index, option_index) {
                        "x"
                    } else {
                        " "
                    };
                    let text = if description.is_empty() {
                        label.clone()
                    } else {
                        format!("{label} {description}")
                    };
                    if flat <= 9 {
                        lines.push(dim_line(format!("  {flat}. [{mark}] {text}")));
                    } else {
                        lines.push(dim_line(format!("     [{mark}] {text}")));
                    }
                }
            }
            lines
        }
        DecisionKind::Stuck { task, .. } => {
            vec![dim_line(format!(
                "  task {task} failed on its last attempt"
            ))]
        }
        DecisionKind::NeedsHuman { kind, number, .. } => vec![dim_line(format!(
            "  the {} #{number} carries the needs-human label",
            item_word(*kind)
        ))],
        DecisionKind::ReleaseGate { prs } => {
            let checked = inbox.checked_prs(decision);
            let mut lines = Vec::new();
            for (index, pr) in prs.iter().enumerate() {
                let mark = if checked.contains(pr) { "x" } else { " " };
                lines.push(dim_line(format!("  {}. [{mark}] #{pr}", index + 1)));
            }
            lines.push(dim_line(
                "  the list is a snapshot; the next push corrects it".to_string(),
            ));
            lines
        }
    }
}

/// Whether one option of a question row carries a pick.
fn option_picked(inbox: &Inbox, id: &str, question_index: usize, option_index: usize) -> bool {
    inbox
        .picks
        .get(id)
        .and_then(|all| all.get(question_index))
        .is_some_and(|set| set.contains(&option_index))
}

/// Build the footer line: the key map, a hint, or the text input.
fn footer_text(state: &StateView, inbox: &Inbox) -> String {
    if let Some(input) = &inbox.input {
        return format!(
            "{}: {}_  (enter sends, esc cancels)",
            input.label, input.buffer
        );
    }
    if let Some(hint) = inbox.hint {
        return hint.to_string();
    }
    let Some(index) = inbox.selected_index(state) else {
        return "j k move · ! oldest".to_string();
    };
    let Some(decision) = state.decisions.get(index) else {
        return "j k move · ! oldest".to_string();
    };
    match &decision.kind {
        DecisionKind::Permission { .. } => {
            "j k move · y allow · n deny · enter session".to_string()
        }
        DecisionKind::Question { .. } => {
            "j k move · 1-9 pick · enter submit · i free answer".to_string()
        }
        DecisionKind::Stuck { .. } => "j k move · r retry · c cancel · enter session".to_string(),
        DecisionKind::NeedsHuman { .. } => {
            "j k move · t comment · c cancel · no retry: the label can outlive its task".to_string()
        }
        DecisionKind::ReleaseGate { .. } => {
            "j k move · 1-9 toggle · space all or none · g fire".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::model::Stage;
    use crate::sock::PausedView;
    use crate::tasks::Task;

    /// The epoch time every test decision opens at.
    const OPENED: u64 = 10_000_000;

    /// One fresh worker task.
    fn worker() -> Task {
        Task::new(
            "borsuk",
            Stage::Implement,
            ItemKind::Issue,
            142,
            PathBuf::from("log.jsonl"),
            OPENED,
        )
    }

    /// One decision of every kind, in a fixed order.
    fn every_decision() -> Vec<Decision> {
        let worker = worker();
        vec![
            Decision::permission(
                &worker,
                "req-1",
                "Write",
                serde_json::json!({"file_path": "src/main.rs"}),
                OPENED,
            ),
            Decision::question(
                &worker,
                "req-2",
                serde_json::json!([
                    {
                        "question": "Which database?",
                        "header": "Storage",
                        "options": [
                            {"label": "SQLite", "description": "embedded"},
                            {"label": "postgres", "description": "server"},
                        ],
                        "multiSelect": false,
                    }
                ]),
                OPENED,
            ),
            Decision::stuck(&worker, "3 failures", OPENED),
            Decision::needs_human("borsuk", ItemKind::Issue, 142, "Fix the flake", OPENED),
            Decision::release_gate("borsuk", vec![7, 9], OPENED),
        ]
    }

    /// A state view that carries only `decisions`.
    fn state_with(decisions: Vec<Decision>) -> StateView {
        StateView {
            repos: Vec::new(),
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions,
            trains: Vec::new(),
            paused: PausedView {
                global: false,
                stages: Vec::new(),
                repos: Vec::new(),
            },
        }
    }

    /// A full state view with one decision of every kind.
    fn full_state() -> StateView {
        state_with(every_decision())
    }

    /// A key event for one character.
    fn press(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
    }

    /// A key event for one non-character key.
    fn press_code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Type one string as a run of character keys.
    fn type_text(inbox: &mut Inbox, state: &StateView, text: &str, tx: &mut mpsc::Sender<Action>) {
        for character in text.chars() {
            inbox.handle_key(state, press(character), tx);
        }
    }

    /// An inbox selected onto the row at `index`.
    fn selected(state: &StateView, index: usize) -> Inbox {
        let mut inbox = Inbox::new();
        inbox.observe(state);
        inbox.select_index(state, index);
        inbox
    }

    /// A fake sender and its receiver.
    fn fake_sink() -> (mpsc::Sender<Action>, mpsc::Receiver<Action>) {
        mpsc::channel()
    }

    /// Render the inbox and return the whole screen as text.
    fn render(state: &StateView, inbox: &Inbox, now_ms: u64) -> String {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), state, inbox, now_ms))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    /// Render only the badge line.
    fn render_badge(state: &StateView) -> String {
        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(badge(state)), f.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn rows_render_age_repo_stage_and_summary() {
        let state = full_state();
        let inbox = Inbox::new();
        let screen = render(&state, &inbox, OPENED + 90_000);

        assert!(screen.contains("decisions - 5 open"), "screen: {screen}");
        assert!(screen.contains("  1m"), "age: {screen}");
        assert!(screen.contains("borsuk"), "repo: {screen}");
        assert!(screen.contains("implement"), "stage: {screen}");
        assert!(screen.contains("Write src/main.rs"), "summary: {screen}");
        assert!(
            screen.contains("Which database? (2 options)"),
            "summary: {screen}"
        );
        assert!(screen.contains("3 failures"), "summary: {screen}");
        assert!(
            screen.contains("#142 Fix the flake (issue)"),
            "summary: {screen}"
        );
        assert!(
            screen.contains("release 2 pull requests: #7 #9"),
            "summary: {screen}"
        );
        // A row without a stage shows a placeholder.
        assert!(screen.contains("-        "), "stage gap: {screen}");
    }

    #[test]
    fn an_empty_state_renders_a_placeholder_and_no_rows() {
        let state = state_with(Vec::new());
        let inbox = Inbox::new();
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("No open decisions"), "screen: {screen}");
        assert!(!screen.contains("borsuk"), "screen: {screen}");
    }

    #[test]
    fn the_age_derives_from_opened_ms_so_a_late_connect_and_a_repush_never_reset_it() {
        assert_eq!(age_text(OPENED, OPENED + 45_000), "45s");
        assert_eq!(age_text(OPENED, OPENED + 90_000), "1m");
        assert_eq!(age_text(OPENED, OPENED + 3_600_000), "1h");
        assert_eq!(age_text(OPENED, OPENED + 2 * 86_400_000), "2d");
        assert_eq!(age_text(OPENED, OPENED - 5_000), "0s");

        // The UI connects an hour after the row opened. The first pushed
        // state already shows the true age, not an age from arrival time.
        let state = full_state();
        let inbox = Inbox::new();
        let first = render(&state, &inbox, OPENED + 3_600_000);
        assert!(first.contains("  1h"), "screen: {first}");

        // The daemon re-pushes the row five minutes later. The age grows
        // from opened_ms and never falls back to zero.
        let repushed = full_state();
        let second = render(&repushed, &inbox, OPENED + 3_900_000);
        assert!(second.contains("  1h"), "screen: {second}");
    }

    #[test]
    fn the_selected_question_row_expands_to_numbered_options() {
        let state = full_state();
        let inbox = selected(&state, 1);
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("Which database?"), "screen: {screen}");
        assert!(
            screen.contains("1. [ ] SQLite embedded"),
            "screen: {screen}"
        );
        assert!(
            screen.contains("2. [ ] postgres server"),
            "screen: {screen}"
        );
        assert!(
            screen.contains("1-9 pick · enter submit · i free answer"),
            "footer: {screen}"
        );
    }

    #[test]
    fn the_selected_gate_row_expands_to_checkboxes() {
        let state = full_state();
        let inbox = selected(&state, 4);
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("1. [x] #7"), "screen: {screen}");
        assert!(screen.contains("2. [x] #9"), "screen: {screen}");
        assert!(
            screen.contains("1-9 toggle · space all or none · g fire"),
            "footer: {screen}"
        );

        let mut inbox = selected(&state, 4);
        let (mut tx, _rx) = fake_sink();
        inbox.handle_key(&state, press('1'), &mut tx);
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("1. [ ] #7"), "screen: {screen}");
        assert!(screen.contains("2. [x] #9"), "screen: {screen}");
    }

    #[test]
    fn permission_y_sends_allow_a_sends_nothing_and_enter_opens_the_session() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        let outcome = inbox.handle_key(&state, press('y'), &mut tx);
        assert_eq!(outcome, InboxOutcome::None);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-1".to_string(),
                response: Response::Allow,
            }
        );

        // The a key is deliberately unbound: the wire carries no field for
        // the request suggestion, so an a key would promise what it
        // cannot do.
        inbox.handle_key(&state, press('a'), &mut tx);
        assert!(rx.try_recv().is_err());

        let outcome = inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(
            outcome,
            InboxOutcome::OpenSession("borsuk/implement-i142".to_string())
        );
        assert!(rx.try_recv().is_err(), "enter sends no answer");
    }

    #[test]
    fn permission_n_takes_a_reason_and_sends_deny() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('n'), &mut tx);
        assert!(rx.try_recv().is_err(), "opening the input sends nothing");
        type_text(&mut inbox, &state, "not this file", &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-1".to_string(),
                response: Response::Deny {
                    message: "not this file".to_string(),
                },
            }
        );
    }

    #[test]
    fn question_digits_pick_enter_submits_the_answers() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press('1'), &mut tx);
        assert!(rx.try_recv().is_err(), "a pick alone sends nothing");
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-2".to_string(),
                response: Response::Answers {
                    updated_input: serde_json::json!({"answers": {"Storage": "SQLite"}}),
                },
            }
        );
    }

    #[test]
    fn question_enter_without_a_pick_is_blocked_with_a_hint() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert!(
            rx.try_recv().is_err(),
            "an unanswered question sends nothing"
        );
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("answer every question"), "hint: {screen}");
    }

    #[test]
    fn question_i_takes_a_free_answer_and_sends_text() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press('i'), &mut tx);
        type_text(&mut inbox, &state, "use sqlite", &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-2".to_string(),
                response: Response::Text {
                    text: "use sqlite".to_string(),
                },
            }
        );
    }

    #[test]
    fn question_digits_toggle_options_and_answers_carry_lists_when_multi_select() {
        let worker = worker();
        let decision = Decision::question(
            &worker,
            "req-9",
            serde_json::json!([
                {
                    "question": "Which database?",
                    "header": "Storage",
                    "options": [{"label": "SQLite", "description": ""}],
                    "multiSelect": false,
                },
                {
                    "question": "Which caches?",
                    "header": "Caches",
                    "options": [
                        {"label": "redis", "description": ""},
                        {"label": "memcached", "description": ""},
                    ],
                    "multiSelect": true,
                },
            ]),
            OPENED,
        );
        let state = state_with(vec![decision]);
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        // The digits number the options across the questions: 1, 2, 3.
        inbox.handle_key(&state, press('1'), &mut tx);
        inbox.handle_key(&state, press('2'), &mut tx);
        inbox.handle_key(&state, press('3'), &mut tx);
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("1. [x] SQLite"), "screen: {screen}");
        assert!(screen.contains("2. [x] redis"), "screen: {screen}");
        assert!(screen.contains("3. [x] memcached"), "screen: {screen}");

        inbox.handle_key(&state, press('2'), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-9".to_string(),
                response: Response::Answers {
                    updated_input: serde_json::json!({
                        "answers": {"Storage": "SQLite", "Caches": ["memcached"]},
                    }),
                },
            }
        );
    }

    #[test]
    fn the_answer_key_prefers_the_header_and_falls_back_to_the_question_text() {
        let parsed = parse_questions(&serde_json::json!([
            {"question": "Which database?", "header": "Storage", "options": []},
            {"question": "Which cache?", "options": []},
        ]));

        assert_eq!(parsed[0].key, "Storage");
        assert_eq!(parsed[1].key, "Which cache?");
    }

    #[test]
    fn stuck_r_retries_c_cancels_and_enter_opens_the_session() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 2);

        inbox.handle_key(&state, press('r'), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "stuck:borsuk/implement-i142:1".to_string(),
                response: Response::Retry,
            }
        );

        inbox.handle_key(&state, press('c'), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "stuck:borsuk/implement-i142:1".to_string(),
                response: Response::Cancel,
            }
        );

        let outcome = inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(
            outcome,
            InboxOutcome::OpenSession("borsuk/implement-i142".to_string())
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn needs_human_t_comments_c_cancels_and_no_key_retries() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 3);

        inbox.handle_key(&state, press('t'), &mut tx);
        type_text(&mut inbox, &state, "please split the change", &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "human:borsuk:i142".to_string(),
                response: Response::Text {
                    text: "please split the change".to_string(),
                },
            }
        );

        inbox.handle_key(&state, press('c'), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "human:borsuk:i142".to_string(),
                response: Response::Cancel,
            }
        );

        // The row has no task, so enter jumps nowhere.
        let outcome = inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(outcome, InboxOutcome::None);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn gate_space_and_g_fire_the_whole_batch() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 4);

        inbox.handle_key(&state, press(' '), &mut tx);
        assert!(rx.try_recv().is_err(), "space sends nothing");
        inbox.handle_key(&state, press(' '), &mut tx);
        inbox.handle_key(&state, press('g'), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "gate:borsuk".to_string(),
                response: Response::Go { prs: vec![7, 9] },
            }
        );
    }

    #[test]
    fn gate_digits_narrow_the_batch_and_g_refuses_an_empty_one() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 4);

        inbox.handle_key(&state, press('1'), &mut tx);
        inbox.handle_key(&state, press('g'), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "gate:borsuk".to_string(),
                response: Response::Go { prs: vec![9] },
            }
        );

        // Uncheck the last pull request: g is then a no-op with a hint.
        inbox.handle_key(&state, press('2'), &mut tx);
        inbox.handle_key(&state, press('g'), &mut tx);
        assert!(rx.try_recv().is_err());
        let screen = render(&state, &inbox, OPENED);
        assert!(
            screen.contains("select at least one pull request"),
            "hint: {screen}"
        );

        // Space brings the whole batch back.
        inbox.handle_key(&state, press(' '), &mut tx);
        inbox.handle_key(&state, press('g'), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "gate:borsuk".to_string(),
                response: Response::Go { prs: vec![7, 9] },
            }
        );
    }

    #[test]
    fn no_key_ever_produces_a_response_the_kind_refuses() {
        let state = full_state();
        let mut alphabet: Vec<KeyCode> = "ynarcgti !123456789".chars().map(KeyCode::Char).collect();
        alphabet.extend([
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Tab,
        ]);

        for (index, decision) in every_decision().into_iter().enumerate() {
            for code in alphabet.clone() {
                let mut inbox = selected(&state, index);
                let (mut tx, rx) = fake_sink();
                inbox.handle_key(&state, press_code(code), &mut tx);
                while let Ok(action) = rx.try_recv() {
                    let Action::Answer {
                        decision_id,
                        response,
                    } = action
                    else {
                        panic!("the inbox sent a non-answer action: {action:?}");
                    };
                    assert_eq!(decision_id, decision.id);
                    crate::decisions::validate(&decision, &response).unwrap_or_else(|error| {
                        panic!("key {code:?} produced a response the kind refuses: {error}")
                    });
                }
            }
        }
    }

    #[test]
    fn typing_edits_the_buffer_and_esc_cancels() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('n'), &mut tx);
        type_text(&mut inbox, &state, "ab", &mut tx);
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("reason: ab_"), "screen: {screen}");

        inbox.handle_key(&state, press_code(KeyCode::Backspace), &mut tx);
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("reason: a_"), "screen: {screen}");

        inbox.handle_key(&state, press_code(KeyCode::Esc), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert!(rx.try_recv().is_err(), "a cancelled input sends nothing");
    }

    #[test]
    fn an_empty_text_input_is_blocked_with_a_hint() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('n'), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert!(rx.try_recv().is_err());
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("type the text first"), "hint: {screen}");
    }

    #[test]
    fn the_selection_follows_its_row_across_a_repush_and_prunes_gone_rows() {
        let state = full_state();
        let mut inbox = selected(&state, 4);
        let (mut tx, _rx) = fake_sink();
        inbox.handle_key(&state, press('1'), &mut tx);
        assert!(inbox.checks.contains_key("gate:borsuk"));

        // The gate row closes. Its local state goes away and the
        // selection re-anchors to the first row.
        let without_gate = state_with(every_decision()[..4].to_vec());
        inbox.observe(&without_gate);
        assert!(inbox.checks.is_empty());
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("perm:borsuk/implement-i142:req-1")
        );

        // A surviving row keeps its selection and its picks.
        let mut inbox = selected(&state, 1);
        inbox.handle_key(&state, press('1'), &mut tx);
        let repushed = full_state();
        inbox.observe(&repushed);
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("perm:borsuk/implement-i142:req-2")
        );
        assert!(inbox.picks.contains_key("perm:borsuk/implement-i142:req-2"));
    }

    #[test]
    fn j_k_and_the_arrow_keys_move_the_selection() {
        let state = full_state();
        let (mut tx, _rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('j'), &mut tx);
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("perm:borsuk/implement-i142:req-2")
        );
        inbox.handle_key(&state, press_code(KeyCode::Down), &mut tx);
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("stuck:borsuk/implement-i142:1")
        );
        inbox.handle_key(&state, press('k'), &mut tx);
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("perm:borsuk/implement-i142:req-2")
        );

        // The ends clamp.
        for _ in 0..10 {
            inbox.handle_key(&state, press('k'), &mut tx);
        }
        assert_eq!(
            inbox.selected_id.as_deref(),
            Some("perm:borsuk/implement-i142:req-1")
        );
    }

    #[test]
    fn exclamation_selects_the_oldest_row() {
        // The gate opened an hour before the rest.
        let mut decisions = every_decision();
        decisions[4] = Decision::release_gate("borsuk", vec![7, 9], OPENED - 3_600_000);
        let state = state_with(decisions);
        let (mut tx, _rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('!'), &mut tx);

        assert_eq!(inbox.selected_id.as_deref(), Some("gate:borsuk"));
    }

    #[test]
    fn the_badge_shows_the_pushed_open_count() {
        let state = full_state();
        assert_eq!(open_count(&state), 5);
        let text = render_badge(&state);
        assert!(text.contains("! 5 open"), "badge: {text}");

        let empty = state_with(Vec::new());
        assert_eq!(open_count(&empty), 0);
        let text = render_badge(&empty);
        assert!(text.contains("! 0 open"), "badge: {text}");
    }

    #[test]
    fn oldest_decision_names_the_row_with_the_smallest_opened_ms() {
        let empty = state_with(Vec::new());
        assert!(oldest_decision(&empty).is_none());

        let mut decisions = every_decision();
        decisions[3] =
            Decision::needs_human("borsuk", ItemKind::Issue, 142, "Fix the flake", OPENED - 1);
        let state = state_with(decisions);
        assert_eq!(
            oldest_decision(&state).map(|d| d.id.as_str()),
            Some("human:borsuk:i142")
        );
    }

    #[test]
    fn the_footer_shows_the_key_map_of_the_selected_kind() {
        let state = full_state();
        let cases: [(usize, &str); 5] = [
            (0, "y allow · n deny"),
            (1, "1-9 pick · enter submit · i free answer"),
            (2, "r retry · c cancel"),
            (3, "t comment · c cancel · no retry"),
            (4, "1-9 toggle · space all or none · g fire"),
        ];
        for (index, expected) in cases {
            let inbox = selected(&state, index);
            let screen = render(&state, &inbox, OPENED);
            assert!(screen.contains(expected), "row {index}: {screen}");
        }
    }

    #[test]
    fn a_digit_beyond_the_options_changes_nothing() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press('5'), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert!(
            rx.try_recv().is_err(),
            "screen: {}",
            render(&state, &inbox, OPENED)
        );
    }
}
