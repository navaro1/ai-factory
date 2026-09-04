//! Draws the decisions inbox and handles its keys.
//!
//! The inbox is the one place where everything that needs a human waits.
//! It shows every open decision as an oldest-first feed. The selected item
//! shows its choices and quick actions. Each answer key sends one
//! [`Action::Answer`] for that item. The key map follows:
//!
//! | Kind | Keys |
//! |---|---|
//! | `Permission` | `y` allow, `n` type a deny reason, `enter` show context |
//! | `Question` | `1`..`9` pick, `s` submit, `i` type text, `enter` show context |
//! | `Stuck` | `r` retry, `c` cancel, `enter` show context |
//! | `NeedsHuman` | `t` type a comment, `c` clear the label, `enter` show the item |
//! | `ReleaseGate` | `1`..`9` toggle one pull request, `space` all or none, `g` release, `enter` show a pull request |
//!
//! Global keys: `j`, `k`, and the arrow keys move the selection. `PageUp` and
//! `PageDown` scroll a selected item that is taller than the viewport. `!`
//! selects the oldest decision. A detail screen uses `esc` to return. It uses
//! `o` to open the full task session when the decision has one.
//!
//! Three contracts shape this module. The age of an item comes from the
//! `opened_ms` of the decision, so a re-push never resets it. A gate row
//! is a snapshot, so the checkboxes live here and show the optimistic
//! local change; the next push corrects the list. Agent context only uses
//! visible transcript entries from the exact task log.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::markdown::{tone_color, MentionStatuses};
use super::theme::THEME;
use super::transcript;
use crate::decisions::{Decision, DecisionKind, Response};
use crate::mentions;
use crate::model::ItemKind;
use crate::sock::{Action, Client, ItemView, StateView, TicketAction, TicketMentions};

/// The maximum task log section that one detail draw reads.
const CONTEXT_LOG_BYTES: u64 = 128 * 1024;

/// The maximum visible transcript entries on one decision detail screen.
const CONTEXT_ENTRIES: usize = 16;

/// The line count that one page key moves.
const SCROLL_STEP: u16 = 8;

/// The local UI state of the inbox view.
///
/// The state holds data that the pushed state does not carry. This data
/// includes the selected row, option picks, checkboxes, and typed text.
/// Each key uses a decision id. Thus, each field survives a new push.
#[derive(Debug, Default)]
pub struct Inbox {
    /// The id of the selected decision, if any.
    selected_id: Option<String>,
    /// The picked options of each question row, keyed by decision id.
    /// Each value stores the exact question snapshot and its option indexes.
    picks: BTreeMap<String, QuestionPicks>,
    /// The checked pull requests of each gate row, keyed by decision id.
    checks: BTreeMap<String, BTreeSet<u64>>,
    /// The text input in progress, if any.
    input: Option<TextInput>,
    /// A short hint that explains a blocked intent.
    hint: Option<&'static str>,
    /// The line offset inside a selected feed item that exceeds the viewport.
    feed_scroll: Cell<u16>,
    /// The largest valid selected-item offset from the last feed draw.
    feed_scroll_max: Cell<u16>,
    /// The focused context screen, when Enter opened one decision.
    detail: Option<DetailState>,
    /// The mention statuses of each seen pull request description, keyed
    /// by the repository alias and the pull request number.
    pr_mentions: BTreeMap<(String, u64), MentionStatuses>,
    /// The pull requests whose statuses one request already went out for.
    requested_pr_mentions: BTreeSet<(String, u64)>,
}

/// Local navigation state for one focused decision screen.
#[derive(Debug)]
struct DetailState {
    /// What the screen follows.
    anchor: DetailAnchor,
    /// The pull request shown inside a release batch.
    item_number: Option<u64>,
    /// How many rendered lines the context screen scrolls down.
    scroll: Cell<u16>,
    /// The largest valid detail offset from the last draw.
    scroll_max: Cell<u16>,
}

/// What one detail screen follows.
#[derive(Debug, Clone)]
enum DetailAnchor {
    /// One open decision row, by its stable id.
    Decision(String),
    /// The release train of one repository, when no gate row is open.
    Train(String),
}

/// The anchor of a detail screen, resolved against one state push.
#[derive(Debug, Clone)]
enum DetailTarget {
    /// One open decision row.
    Decision(Decision),
    /// One repository release train.
    Train(crate::sock::TrainView),
}

impl DetailAnchor {
    /// Resolve the anchor against one state push.
    fn target(&self, state: &StateView) -> Option<DetailTarget> {
        match self {
            DetailAnchor::Decision(id) => state
                .decisions
                .iter()
                .find(|decision| decision.id == *id)
                .cloned()
                .map(DetailTarget::Decision),
            DetailAnchor::Train(repo) => state
                .trains
                .iter()
                .find(|train| train.repo == *repo)
                .cloned()
                .map(DetailTarget::Train),
        }
    }

    /// The pull request list that a release detail screen walks.
    fn release_prs(&self, state: &StateView) -> Option<Vec<u64>> {
        match self.target(state)? {
            DetailTarget::Decision(decision) => match &decision.kind {
                DecisionKind::ReleaseGate { prs } => Some(prs.clone()),
                _ => None,
            },
            DetailTarget::Train(train) => Some(train_detail_prs(&train)),
        }
    }
}

/// The option picks for one exact question snapshot.
#[derive(Debug)]
struct QuestionPicks {
    /// The questions that the option indexes refer to.
    questions: serde_json::Value,
    /// The option indexes for each parsed question.
    selected: Vec<BTreeSet<usize>>,
}

/// What the human is typing, and the response it will produce.
#[derive(Debug)]
struct TextInput {
    /// The decision that opened this input.
    decision_id: String,
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

impl InputKind {
    /// Check that this input can answer the current decision kind.
    fn accepts(self, decision: &DecisionKind) -> bool {
        matches!(
            (self, decision),
            (InputKind::DenyReason, DecisionKind::Permission { .. })
                | (
                    InputKind::FreeText,
                    DecisionKind::Question { .. } | DecisionKind::NeedsHuman { .. }
                )
        )
    }
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
    fn send_action(&mut self, action: Action) -> bool;
}

impl ActionSink for Sender<Action> {
    fn send_action(&mut self, action: Action) -> bool {
        if let Err(error) = self.send(action) {
            eprintln!("inbox: the action receiver is gone: {error}");
            return false;
        }
        true
    }
}

impl ActionSink for Client {
    fn send_action(&mut self, action: Action) -> bool {
        if let Err(error) = self.send(&action) {
            eprintln!("inbox: cannot send the action to the daemon: {error}");
            return false;
        }
        true
    }
}

impl Inbox {
    /// An inbox with no selection and no input.
    pub fn new() -> Self {
        Inbox::default()
    }

    /// Take one mention-status request for the pull request whose detail
    /// just became visible, or `None`.
    ///
    /// One request per pull request identity goes out; the answer merges
    /// through [`Inbox::observe_pr_mentions`].
    pub fn take_pending_pr_mention(&mut self, state: &StateView) -> Option<Action> {
        let detail = self.detail.as_ref()?;
        let (repo, number) = match detail.anchor.target(state)? {
            DetailTarget::Decision(decision) => {
                let (kind, number) = detail_item_key(&decision, detail.item_number)?;
                if kind != ItemKind::Pr {
                    return None;
                }
                (decision.repo, number)
            }
            DetailTarget::Train(train) => {
                let prs = train_detail_prs(&train);
                let number = detail
                    .item_number
                    .filter(|number| prs.contains(number))
                    .or_else(|| prs.first().copied())?;
                (train.repo, number)
            }
        };
        if !self.requested_pr_mentions.insert((repo.clone(), number)) {
            return None;
        }
        Some(Action::Ticket(TicketAction::PrMentions {
            request: uuid::Uuid::new_v4().to_string(),
            repo,
            number,
        }))
    }

    /// Merge one mention-status answer into the pull request store.
    pub fn observe_pr_mentions(&mut self, mentions: &TicketMentions) {
        let entry = self
            .pr_mentions
            .entry((mentions.repo.clone(), mentions.number))
            .or_default();
        for status in &mentions.statuses {
            entry.insert((status.repo.clone(), status.number), status.status);
        }
    }

    /// The known mention statuses of one pull request description.
    fn pr_statuses(&self, repo: &str, number: u64) -> Option<&MentionStatuses> {
        self.pr_mentions.get(&(repo.to_string(), number))
    }

    /// Apply one pushed state.
    ///
    /// The call drops local state for closed or changed rows. It removes
    /// pull requests that left a gate snapshot. It re-anchors a lost
    /// selection to the first row. A row age derives from `opened_ms`.
    pub fn observe(&mut self, state: &StateView) {
        self.picks.retain(|id, picks| {
            state
                .decisions
                .iter()
                .find(|decision| decision.id == *id)
                .is_some_and(|decision| {
                    matches!(
                        &decision.kind,
                        DecisionKind::Question { questions, .. }
                            if questions == &picks.questions
                    )
                })
        });
        self.checks.retain(|id, checked| {
            let Some(decision) = state.decisions.iter().find(|decision| decision.id == *id) else {
                return false;
            };
            let DecisionKind::ReleaseGate { prs } = &decision.kind else {
                return false;
            };
            checked.retain(|pr| prs.contains(pr));
            true
        });
        if self.input.as_ref().is_some_and(|input| {
            state
                .decisions
                .iter()
                .find(|decision| decision.id == input.decision_id)
                .is_none_or(|decision| !input.kind.accepts(&decision.kind))
        }) {
            self.input = None;
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.anchor.target(state).is_none())
        {
            self.detail = None;
        }
        if let Some(detail) = self.detail.as_mut() {
            if let Some(prs) = detail.anchor.release_prs(state) {
                if detail
                    .item_number
                    .is_none_or(|number| !prs.contains(&number))
                {
                    detail.item_number = prs.first().copied();
                    detail.scroll.set(0);
                    detail.scroll_max.set(0);
                }
            }
        }
        let selected_is_gone = self
            .selected_id
            .as_deref()
            .is_none_or(|id| !state.decisions.iter().any(|d| d.id == id));
        if selected_is_gone {
            self.feed_scroll.set(0);
            self.feed_scroll_max.set(0);
            self.selected_id = oldest_decision(state).map(|decision| decision.id.clone());
        }
    }

    /// Select the row at `index`, clamped to the open rows.
    pub fn select_index(&mut self, state: &StateView, index: usize) {
        let ordered = ordered_decisions(state);
        let next = ordered
            .get(index)
            .or_else(|| ordered.last())
            .map(|d| d.id.clone());
        if self.selected_id != next {
            self.feed_scroll.set(0);
            self.feed_scroll_max.set(0);
        }
        self.selected_id = next;
    }

    /// Select the oldest decision, as the `!` key promises.
    ///
    /// The oldest row is the one with the smallest `opened_ms`; a tie
    /// keeps the earlier push order.
    pub fn select_oldest(&mut self, state: &StateView) {
        self.detail = None;
        self.input = None;
        self.hint = None;
        self.feed_scroll.set(0);
        self.feed_scroll_max.set(0);
        self.selected_id = oldest_decision(state).map(|decision| decision.id.clone());
    }

    /// The id of the selected row.
    ///
    /// The shell and the tests read it to check where the selection sits.
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// True while the reason input holds the keyboard.
    ///
    /// The shell keeps every key away from the global handler while the
    /// operator types a reason.
    pub fn typing(&self) -> bool {
        self.input.is_some()
    }

    /// True while one focused decision context screen is open.
    pub fn detail_open(&self) -> bool {
        self.detail.is_some()
    }

    /// Handle one key while the inbox view is open.
    ///
    /// The call sends at most one action to `sink` and returns what the
    /// shell must do beyond the inbox. A key that the kind of the
    /// selected row does not accept changes nothing. Thus, the UI cannot
    /// produce a response that the decision refuses.
    pub fn handle_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        self.hint = None;
        if key.kind != KeyEventKind::Press {
            return InboxOutcome::None;
        }
        if self.input.is_some() {
            return self.typing_key(state, key, sink);
        }
        if !key.modifiers.is_empty() {
            return InboxOutcome::None;
        }
        if self.detail.is_some() {
            return self.detail_key(state, key, sink);
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
            KeyCode::PageDown => self.scroll_feed(i32::from(SCROLL_STEP)),
            KeyCode::PageUp => self.scroll_feed(-i32::from(SCROLL_STEP)),
            KeyCode::Esc => InboxOutcome::None,
            _ => self.row_key(state, key, sink),
        }
    }

    /// Move inside a selected feed item and clamp to its last drawn content.
    fn scroll_feed(&self, step: i32) -> InboxOutcome {
        let current = i32::from(self.feed_scroll.get());
        let maximum = i32::from(self.feed_scroll_max.get());
        let next = (current + step).clamp(0, maximum) as u16;
        self.feed_scroll.set(next);
        InboxOutcome::None
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
            ordered_decisions(state)
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
        let ordered = ordered_decisions(state);
        let Some(decision) = ordered.get(index) else {
            return InboxOutcome::None;
        };
        let decision = (*decision).clone();
        match (&decision.kind, key.code) {
            (DecisionKind::Permission { .. }, KeyCode::Char('y')) => {
                self.answer(&decision.id, Response::Allow, sink);
                InboxOutcome::None
            }
            (DecisionKind::Permission { .. }, KeyCode::Char('n')) => {
                self.open_input(&decision, "reason", InputKind::DenyReason);
                InboxOutcome::None
            }
            (DecisionKind::Permission { .. }, KeyCode::Enter) => {
                self.open_detail(&decision);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Char(digit @ '1'..='9')) => {
                self.pick_option(&decision, digit);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Char('i')) => {
                self.open_input(&decision, "answer", InputKind::FreeText);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Char('s')) => {
                self.submit_question(&decision, sink);
                InboxOutcome::None
            }
            (DecisionKind::Question { .. }, KeyCode::Enter) => {
                self.open_detail(&decision);
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
            (DecisionKind::Stuck { .. }, KeyCode::Enter) => {
                self.open_detail(&decision);
                InboxOutcome::None
            }
            (DecisionKind::NeedsHuman { .. }, KeyCode::Char('t')) => {
                self.open_input(&decision, "comment", InputKind::FreeText);
                InboxOutcome::None
            }
            (DecisionKind::NeedsHuman { .. }, KeyCode::Char('c')) => {
                self.answer(&decision.id, Response::Cancel, sink);
                InboxOutcome::None
            }
            (DecisionKind::NeedsHuman { .. }, KeyCode::Enter) => {
                self.open_detail(&decision);
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
            (DecisionKind::ReleaseGate { .. }, KeyCode::Enter) => {
                self.open_detail(&decision);
                InboxOutcome::None
            }
            _ => InboxOutcome::None,
        }
    }

    /// Open the focused context screen of `decision`.
    fn open_detail(&mut self, decision: &Decision) {
        let item_number = match &decision.kind {
            DecisionKind::ReleaseGate { prs } => prs.first().copied(),
            _ => None,
        };
        self.open_anchor(DetailAnchor::Decision(decision.id.clone()), item_number);
    }

    /// Open the release detail of one release-train pull request.
    ///
    /// The screen follows the open gate row when the repository has one
    /// and the pull request belongs to it. Otherwise the screen follows
    /// the release train itself. The call returns false when neither the
    /// gate nor the train holds the pull request.
    pub fn open_release_pr(&mut self, state: &StateView, repo: &str, pr: u64) -> bool {
        let gate = state.decisions.iter().find(|decision| {
            decision.repo == repo
                && decision.id == format!("gate:{repo}")
                && matches!(decision.kind, DecisionKind::ReleaseGate { .. })
        });
        if let Some(decision) = gate {
            let DecisionKind::ReleaseGate { prs } = &decision.kind else {
                return false;
            };
            if prs.contains(&pr) {
                self.open_anchor(DetailAnchor::Decision(decision.id.clone()), Some(pr));
                return true;
            }
        }
        let in_train = state
            .trains
            .iter()
            .find(|train| train.repo == repo)
            .is_some_and(|train| train_detail_prs(train).contains(&pr));
        if !in_train {
            return false;
        }
        self.open_anchor(DetailAnchor::Train(repo.to_string()), Some(pr));
        true
    }

    /// Show one anchor with one initial pull request and a fresh scroll.
    fn open_anchor(&mut self, anchor: DetailAnchor, item_number: Option<u64>) {
        self.detail = Some(DetailState {
            anchor,
            item_number,
            scroll: Cell::new(0),
            scroll_max: Cell::new(0),
        });
    }

    /// Handle one key while a focused decision context screen is open.
    fn detail_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        let Some(anchor) = self.detail.as_ref().map(|detail| detail.anchor.clone()) else {
            return InboxOutcome::None;
        };
        let Some(target) = anchor.target(state) else {
            self.detail = None;
            return InboxOutcome::None;
        };
        let is_release = match &target {
            DetailTarget::Decision(decision) => {
                matches!(decision.kind, DecisionKind::ReleaseGate { .. })
            }
            DetailTarget::Train(_) => true,
        };
        match key.code {
            KeyCode::Esc => {
                self.detail = None;
                InboxOutcome::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_detail(1);
                InboxOutcome::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_detail(-1);
                InboxOutcome::None
            }
            KeyCode::PageDown => {
                self.scroll_detail(i32::from(SCROLL_STEP));
                InboxOutcome::None
            }
            KeyCode::PageUp => {
                self.scroll_detail(-i32::from(SCROLL_STEP));
                InboxOutcome::None
            }
            KeyCode::Left | KeyCode::Char('h') if is_release => {
                let prs = anchor.release_prs(state).unwrap_or_default();
                self.move_detail_pr(&prs, -1);
                InboxOutcome::None
            }
            KeyCode::Right | KeyCode::Char('l') if is_release => {
                let prs = anchor.release_prs(state).unwrap_or_default();
                self.move_detail_pr(&prs, 1);
                InboxOutcome::None
            }
            KeyCode::Char(' ') if is_release && matches!(target, DetailTarget::Decision(_)) => {
                if let DetailTarget::Decision(decision) = target {
                    self.toggle_detail_pr(&decision);
                }
                InboxOutcome::None
            }
            KeyCode::Char('o') => match &target {
                DetailTarget::Decision(decision) => match task_id(decision) {
                    Some(task) => InboxOutcome::OpenSession(task.to_string()),
                    None => InboxOutcome::None,
                },
                DetailTarget::Train(_) => InboxOutcome::None,
            },
            _ => match target {
                DetailTarget::Decision(_) => self.row_key(state, key, sink),
                DetailTarget::Train(_) => InboxOutcome::None,
            },
        }
    }

    /// Move inside a detail screen and clamp to its last drawn content.
    fn scroll_detail(&self, step: i32) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let current = i32::from(detail.scroll.get());
        let maximum = i32::from(detail.scroll_max.get());
        let next = (current + step).clamp(0, maximum) as u16;
        detail.scroll.set(next);
    }

    /// Move the release detail by one pull request and clamp at both ends.
    fn move_detail_pr(&mut self, prs: &[u64], step: isize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let Some(first) = prs.first() else {
            detail.item_number = None;
            return;
        };
        let current = detail
            .item_number
            .and_then(|number| prs.iter().position(|pr| *pr == number))
            .unwrap_or(0);
        let last = prs.len() - 1;
        let next = (current as isize + step).clamp(0, last as isize) as usize;
        detail.item_number = prs.get(next).copied().or(Some(*first));
        detail.scroll.set(0);
        detail.scroll_max.set(0);
    }

    /// Toggle the pull request that the release detail screen shows.
    fn toggle_detail_pr(&mut self, decision: &Decision) {
        let DecisionKind::ReleaseGate { prs } = &decision.kind else {
            return;
        };
        let Some(number) = self.detail.as_ref().and_then(|detail| detail.item_number) else {
            return;
        };
        if !prs.contains(&number) {
            return;
        }
        let checked = self
            .checks
            .entry(decision.id.clone())
            .or_insert_with(|| prs.iter().copied().collect());
        if !checked.remove(&number) {
            checked.insert(number);
        }
    }

    /// Handle one key while the text input holds the keyboard.
    fn typing_key(
        &mut self,
        state: &StateView,
        key: KeyEvent,
        sink: &mut impl ActionSink,
    ) -> InboxOutcome {
        let Some((decision_id, input_kind)) = self
            .input
            .as_ref()
            .map(|input| (input.decision_id.as_str(), input.kind))
        else {
            return InboxOutcome::None;
        };
        let Some(decision) = state
            .decisions
            .iter()
            .find(|decision| decision.id == decision_id)
        else {
            self.input = None;
            return InboxOutcome::None;
        };
        if !input_kind.accepts(&decision.kind) {
            self.input = None;
            return InboxOutcome::None;
        }
        let decision = decision.clone();
        let allowed_modifiers = match key.code {
            KeyCode::Char(_) => key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT,
            _ => key.modifiers.is_empty(),
        };
        if !allowed_modifiers {
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
    fn open_input(&mut self, decision: &Decision, label: &'static str, kind: InputKind) {
        self.input = Some(TextInput {
            decision_id: decision.id.clone(),
            buffer: String::new(),
            label,
            kind,
        });
    }

    /// Finish the text input and send its response.
    fn submit_text(&mut self, decision: &Decision, sink: &mut impl ActionSink) -> InboxOutcome {
        let Some(input) = self.input.as_ref() else {
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
        self.input = None;
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
        let picks = self
            .picks
            .entry(decision.id.clone())
            .or_insert_with(|| QuestionPicks {
                questions: questions.clone(),
                selected: vec![BTreeSet::new(); parsed.len()],
            });
        if &picks.questions != questions || picks.selected.len() != parsed.len() {
            *picks = QuestionPicks {
                questions: questions.clone(),
                selected: vec![BTreeSet::new(); parsed.len()],
            };
        }
        let chosen = &mut picks.selected[question_index];
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
        let picks = self
            .picks
            .get(&decision.id)
            .filter(|picks| &picks.questions == questions)
            .map(|picks| &picks.selected)
            .unwrap_or(&empty);
        let mut answers = serde_json::Map::new();
        for (index, question) in parsed.iter().enumerate() {
            let Some(chosen) = picks.get(index).filter(|chosen| !chosen.is_empty()) else {
                self.hint = Some("answer every question, or press i for a free answer");
                return;
            };
            if question.multi_select {
                let labels: Vec<String> = question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(option_index, _)| chosen.contains(option_index))
                    .map(|(_, (label, _))| label.clone())
                    .collect();
                answers.insert(question.key.clone(), serde_json::json!(labels));
            } else {
                let Some(pick) = chosen.iter().next() else {
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
            self.hint = Some("select at least one PR");
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

/// Parse the questions of a `Question` decision.
///
/// The payload carries the question list in one of two shapes, and the
/// reader accepts both, whichever harness recorded it. A bare array is the
/// list itself. An object is the whole tool input, so the list sits under
/// its `questions` key. The claude runner stores the tool input verbatim,
/// so the object shape is the one the daemon writes today.
///
/// An entry can omit a question text, a header, or readable option labels.
/// The parser uses the data that the entry contains. Every other value
/// produces no questions.
fn parse_questions(value: &serde_json::Value) -> Vec<QuestionView> {
    let Some(entries) = question_entries(value) else {
        return Vec::new();
    };
    entries.iter().filter_map(parse_question).collect()
}

/// Find the question list inside one `Question` decision payload.
///
/// The list is the value itself when the value is an array. Otherwise the
/// list is the `questions` member of the value.
fn question_entries(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value
        .as_array()
        .or_else(|| value.get("questions").and_then(serde_json::Value::as_array))
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
/// The numbering continues across all questions. One option in the first
/// question and two options in the second question use digits 1 through 3.
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
    format!("! {} open", open_count(state))
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

/// Get the current time in milliseconds since the Unix epoch.
pub fn now_ms() -> Result<u64> {
    millis_since_epoch(SystemTime::now())
}

/// Convert one system time to milliseconds since the Unix epoch.
fn millis_since_epoch(now: SystemTime) -> Result<u64> {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .context("the system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("the system time in milliseconds does not fit in u64")
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

/// The primary message of one feed item.
fn feed_message(decision: &Decision) -> String {
    let text = match &decision.kind {
        DecisionKind::Permission { tool, input, .. } => {
            format!("Allow {tool} for {}?", input_summary(input))
        }
        DecisionKind::Question { questions, .. } => question_message(questions),
        DecisionKind::Stuck { task, reason } => format!("Task {task} stopped: {reason}"),
        DecisionKind::NeedsHuman {
            kind,
            number,
            title,
        } => format!("{} #{number} needs a decision: {title}", kind.title_noun()),
        DecisionKind::ReleaseGate { prs } if prs.len() == 1 => {
            format!("Release PR #{}?", prs[0])
        }
        DecisionKind::ReleaseGate { prs } => {
            format!("Release {} PRs: {}?", prs.len(), pr_list(prs))
        }
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// The primary message of one question decision.
fn question_message(questions: &serde_json::Value) -> String {
    let parsed = parse_questions(questions);
    let Some(first) = parsed.first() else {
        return "The agent sent a question with unreadable options.".to_string();
    };
    if parsed.len() == 1 {
        first.text.clone()
    } else {
        format!("{} questions, starting with: {}", parsed.len(), first.text)
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

/// The open decisions in explicit age order, with push order as the tie break.
fn ordered_decisions(state: &StateView) -> Vec<&Decision> {
    let mut ordered: Vec<(usize, &Decision)> = state.decisions.iter().enumerate().collect();
    ordered.sort_by_key(|(position, decision)| (decision.opened_ms, *position));
    ordered.into_iter().map(|(_, decision)| decision).collect()
}

/// The visible name of one decision kind.
fn kind_label(decision: &Decision) -> &'static str {
    match decision.kind {
        DecisionKind::Permission { .. } => "PERMISSION",
        DecisionKind::Question { .. } => "QUESTION",
        DecisionKind::Stuck { .. } => "STUCK",
        DecisionKind::NeedsHuman { .. } => "NEEDS HUMAN",
        DecisionKind::ReleaseGate { .. } => "RELEASE",
    }
}

/// The task id of a task-backed decision.
fn task_id(decision: &Decision) -> Option<&str> {
    match &decision.kind {
        DecisionKind::Permission { task, .. }
        | DecisionKind::Question { task, .. }
        | DecisionKind::Stuck { task, .. } => Some(task),
        DecisionKind::NeedsHuman { .. } | DecisionKind::ReleaseGate { .. } => None,
    }
}

/// Draw the inbox into `area`.
///
/// The shell calls this from its render closure when the inbox view is
/// open. `now_ms` stamps the age column; the shell passes [`now_ms`].
pub fn draw(f: &mut Frame, area: Rect, state: &StateView, inbox: &Inbox, now_ms: u64) {
    if let Some(detail) = inbox.detail.as_ref() {
        match detail.anchor.target(state) {
            Some(DetailTarget::Decision(decision)) => {
                draw_detail(f, area, state, inbox, &decision, detail, now_ms);
                return;
            }
            Some(DetailTarget::Train(train)) => {
                draw_train_detail(f, area, state, inbox, &train, detail);
                return;
            }
            None => {}
        }
    }
    draw_feed(f, area, state, inbox, now_ms);
}

/// Draw a release-train detail without an open gate row.
fn draw_train_detail(
    f: &mut Frame,
    area: Rect,
    state: &StateView,
    inbox: &Inbox,
    train: &crate::sock::TrainView,
    detail: &DetailState,
) {
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let width = usize::from(rows[0].width.saturating_sub(4)).max(1);
    let prs = train_detail_prs(train);
    let Some(number) = detail
        .item_number
        .filter(|number| prs.contains(number))
        .or_else(|| prs.first().copied())
    else {
        return draw_missing_item_detail(
            f,
            rows[0],
            rows[1],
            state,
            inbox,
            detail,
            format!("PR · {}", train.repo),
            "The train queue is empty.".to_string(),
        );
    };
    draw_item_detail(
        f,
        rows[0],
        rows[1],
        state,
        inbox,
        detail,
        &train.repo,
        number,
        width,
    );
}
/// Draw one repository item detail, or the missing-item notice.
#[allow(clippy::too_many_arguments)]
fn draw_item_detail(
    f: &mut Frame,
    content_area: Rect,
    footer_area: Rect,
    state: &StateView,
    inbox: &Inbox,
    detail: &DetailState,
    repo: &str,
    number: u64,
    width: usize,
) {
    let title = format!("PR #{number} · {repo}");
    let Some((title, lines)) = item_lines(
        state,
        repo,
        ItemKind::Pr,
        number,
        width,
        &title,
        inbox.pr_statuses(repo, number),
    ) else {
        return draw_missing_item_detail(
            f,
            content_area,
            footer_area,
            state,
            inbox,
            detail,
            title,
            "The PR description is unavailable.".to_string(),
        );
    };
    let scroll = clamped_detail_scroll(detail, lines.len(), content_area);
    let content = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    f.render_widget(content, content_area);
    f.render_widget(Paragraph::new(footer_text(state, inbox)), footer_area);
}

/// The pull requests of one train detail, in release lane order.
fn train_detail_prs(train: &crate::sock::TrainView) -> Vec<u64> {
    let batch = super::pipeline::displayed_batch(train);
    let mut prs: Vec<u64> = batch.to_vec();
    prs.extend(train.queue.iter().copied().filter(|pr| !batch.contains(pr)));
    prs
}

/// Draw the oldest-first decision feed.
fn draw_feed(f: &mut Frame, area: Rect, state: &StateView, inbox: &Inbox, now_ms: u64) {
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_span = None;
    let width = usize::from(rows[0].width.saturating_sub(4)).max(1);
    if state.decisions.is_empty() {
        lines.push(Line::from(
            "No open decisions. The factory asks here when an agent needs you.",
        ));
    }
    for decision in ordered_decisions(state) {
        let selected = inbox.selected_id.as_deref() == Some(decision.id.as_str());
        let row_index = lines.len();
        lines.extend(feed_lines(state, decision, inbox, now_ms, width, selected));
        if selected {
            selected_span = Some((row_index, lines.len().saturating_sub(1)));
        }
        lines.push(Line::from(""));
    }
    let title = format!("decisions · {} open · oldest first", state.decisions.len());
    let inner_height = usize::from(rows[0].height.saturating_sub(2));
    let scroll = selected_span
        .filter(|_| inner_height > 0)
        .map(|(start, end)| {
            let item_height = end.saturating_sub(start).saturating_add(1);
            let maximum = item_height
                .saturating_sub(inner_height)
                .min(usize::from(u16::MAX)) as u16;
            inbox.feed_scroll_max.set(maximum);
            let local = inbox.feed_scroll.get().min(maximum);
            inbox.feed_scroll.set(local);
            if maximum > 0 {
                start.saturating_add(usize::from(local))
            } else {
                end.saturating_add(1)
                    .saturating_sub(inner_height)
                    .min(start)
            }
        })
        .unwrap_or_else(|| {
            inbox.feed_scroll.set(0);
            inbox.feed_scroll_max.set(0);
            0
        })
        .min(usize::from(u16::MAX)) as u16;
    let list = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    f.render_widget(list, rows[0]);
    f.render_widget(Paragraph::new(footer_text(state, inbox)), rows[1]);
}

/// Build one content-first feed item.
fn feed_lines(
    state: &StateView,
    decision: &Decision,
    inbox: &Inbox,
    now_ms: u64,
    width: usize,
    selected: bool,
) -> Vec<Line<'static>> {
    let mut style = Style::new().fg(THEME.text);
    if selected {
        style = style.patch(THEME.selected()).add_modifier(Modifier::BOLD);
    }
    let marker = if selected { "> " } else { "  " };
    let mut metadata = format!(
        "{marker}{} · {} · {}",
        kind_label(decision),
        age_text(decision.opened_ms, now_ms),
        decision.repo
    );
    if decision.stage.is_some() {
        metadata.push_str(&format!(" · {}", stage_text(decision)));
    }
    let mut lines = vec![Line::styled(metadata, style)];
    lines.extend(wrapped_lines(&feed_message(decision), width, "  ", style));
    if selected {
        lines.extend(choice_lines(state, decision, inbox, width, style));
        lines.extend(wrapped_lines(&feed_actions(decision), width, "  ", style));
    }
    lines
}

/// Build choices that the selected feed item needs for a quick answer.
fn choice_lines(
    state: &StateView,
    decision: &Decision,
    inbox: &Inbox,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    match &decision.kind {
        DecisionKind::Question { questions, .. } => {
            let mut lines = Vec::new();
            let mut flat = 0;
            let questions = parse_questions(questions);
            let show_group_prompts = questions.len() > 1;
            for (question_index, question) in questions.into_iter().enumerate() {
                if show_group_prompts {
                    lines.extend(wrapped_lines(&question.text, width, "  ", style));
                }
                if question.multi_select {
                    lines.extend(wrapped_lines(
                        "Select all applicable options.",
                        width,
                        "  ",
                        style,
                    ));
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
                    let choice = if flat <= 9 {
                        format!("{flat}. [{mark}] {text}")
                    } else {
                        format!("   [{mark}] {text}")
                    };
                    lines.extend(wrapped_lines(&choice, width, "  ", style));
                }
            }
            lines
        }
        DecisionKind::ReleaseGate { prs } => {
            let checked = inbox.checked_prs(decision);
            let mut lines = Vec::new();
            for (index, pr) in prs.iter().enumerate() {
                let mark = if checked.contains(pr) { "x" } else { " " };
                let title = find_item(state, &decision.repo, ItemKind::Pr, *pr)
                    .map(|item| format!(" {}", item.title))
                    .unwrap_or_default();
                lines.extend(wrapped_lines(
                    &format!("{}. [{mark}] #{pr}{title}", index + 1),
                    width,
                    "  ",
                    style,
                ));
            }
            lines
        }
        DecisionKind::Permission { .. }
        | DecisionKind::Stuck { .. }
        | DecisionKind::NeedsHuman { .. } => Vec::new(),
    }
}

/// The visible quick actions of one feed item.
fn feed_actions(decision: &Decision) -> String {
    match decision.kind {
        DecisionKind::Permission { .. } => "[y] allow · [n] deny · [enter] details".to_string(),
        DecisionKind::Question { .. } => {
            "[1-9] pick · [s] submit · [i] write · [enter] details".to_string()
        }
        DecisionKind::Stuck { .. } => "[r] retry · [c] cancel task · [enter] details".to_string(),
        DecisionKind::NeedsHuman { .. } => {
            "[t] comment · [c] clear label · [enter] details".to_string()
        }
        DecisionKind::ReleaseGate { .. } => {
            "[1-9] include · [space] all/none · [g] release · [enter] details".to_string()
        }
    }
}

/// Draw one focused repository or agent context screen.
fn draw_detail(
    f: &mut Frame,
    area: Rect,
    state: &StateView,
    inbox: &Inbox,
    decision: &Decision,
    detail: &DetailState,
    now_ms: u64,
) {
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let width = usize::from(rows[0].width.saturating_sub(4)).max(1);
    let (title, lines) = match detail_item_key(decision, detail.item_number) {
        Some((kind, number)) => {
            let title = format!("{} #{number} · {}", kind.title_noun(), decision.repo);
            let statuses = if kind == ItemKind::Pr {
                inbox.pr_statuses(&decision.repo, number)
            } else {
                None
            };
            let Some((title, lines)) =
                item_lines(state, &decision.repo, kind, number, width, &title, statuses)
            else {
                return draw_missing_item_detail(
                    f,
                    rows[0],
                    rows[1],
                    state,
                    inbox,
                    detail,
                    title,
                    format!("The {} description is unavailable.", kind.noun()),
                );
            };
            (title, lines)
        }
        None => agent_detail_lines(state, inbox, decision, now_ms, width),
    };
    let scroll = clamped_detail_scroll(detail, lines.len(), rows[0]);
    let content = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    f.render_widget(content, rows[0]);
    f.render_widget(Paragraph::new(footer_text(state, inbox)), rows[1]);
}

/// Draw a repository detail whose latest item snapshot is absent.
#[allow(clippy::too_many_arguments)]
fn draw_missing_item_detail(
    f: &mut Frame,
    content_area: Rect,
    footer_area: Rect,
    state: &StateView,
    inbox: &Inbox,
    detail: &DetailState,
    title: String,
    message: String,
) {
    let scroll = clamped_detail_scroll(detail, 1, content_area);
    let content = Paragraph::new(message)
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    f.render_widget(content, content_area);
    f.render_widget(Paragraph::new(footer_text(state, inbox)), footer_area);
}

/// Clamp a detail offset to the content height from this draw.
fn clamped_detail_scroll(detail: &DetailState, line_count: usize, area: Rect) -> u16 {
    let inner_height = usize::from(area.height.saturating_sub(2));
    let maximum = line_count
        .saturating_sub(inner_height)
        .min(usize::from(u16::MAX)) as u16;
    detail.scroll_max.set(maximum);
    let scroll = detail.scroll.get().min(maximum);
    detail.scroll.set(scroll);
    scroll
}

/// Build the initial task context screen until transcript context is available.
///
/// The screen repeats the choices of the row, so the human can pick an
/// option from the context screen as well as from the feed. The footer
/// names the keys that act on them.
fn agent_detail_lines(
    state: &StateView,
    inbox: &Inbox,
    decision: &Decision,
    now_ms: u64,
    width: usize,
) -> (String, Vec<Line<'static>>) {
    let title = format!(
        "{} · {} · {}",
        kind_label(decision),
        decision.repo,
        age_text(decision.opened_ms, now_ms)
    );
    let mut lines = wrapped_lines(
        &feed_message(decision),
        width,
        "",
        Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
    );
    let choices = choice_lines(
        state,
        decision,
        inbox,
        width,
        Style::default().fg(THEME.text),
    );
    if !choices.is_empty() {
        lines.push(Line::from(""));
        lines.extend(choices);
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("Recent agent context", THEME.dim()));
    let Some(task_id) = task_id(decision) else {
        lines.push(Line::styled(
            "No task session is available for this decision.",
            THEME.dim(),
        ));
        return (title, lines);
    };
    let Some(task) = state.tasks.iter().find(|task| task.id == task_id) else {
        lines.push(Line::styled(
            "The task session is unavailable.",
            THEME.dim(),
        ));
        return (title, lines);
    };
    let entries = match recent_context(&task.log_path, decision_request_id(decision)) {
        Ok(entries) => entries,
        Err(_) => {
            lines.push(Line::styled("The task log is unavailable.", THEME.dim()));
            return (title, lines);
        }
    };
    if entries.is_empty() {
        lines.push(Line::styled(
            "No visible agent context is available.",
            THEME.dim(),
        ));
        return (title, lines);
    }
    let render_width = u16::try_from(width).unwrap_or(u16::MAX);
    for entry in entries {
        lines.extend(transcript::render(&entry, render_width));
    }
    (title, lines)
}

/// Return the request id that bounds permission and question context.
fn decision_request_id(decision: &Decision) -> Option<&str> {
    match &decision.kind {
        DecisionKind::Permission { request_id, .. } | DecisionKind::Question { request_id, .. } => {
            Some(request_id)
        }
        DecisionKind::Stuck { .. }
        | DecisionKind::NeedsHuman { .. }
        | DecisionKind::ReleaseGate { .. } => None,
    }
}

/// Read and parse the newest visible context for one task decision.
fn recent_context(
    path: &Path,
    request_id: Option<&str>,
) -> std::io::Result<Vec<transcript::Entry>> {
    let raw = read_log_tail(path)?;
    let raw_lines: Vec<&str> = raw.lines().collect();
    let end = request_id
        .and_then(|request_id| {
            raw_lines.iter().rposition(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("request_id")
                            .and_then(serde_json::Value::as_str)
                            .map(|found| found == request_id)
                    })
                    .unwrap_or(false)
            })
        })
        .map_or(raw_lines.len(), |index| index + 1);
    let mut entries: Vec<transcript::Entry> = raw_lines[..end]
        .iter()
        .flat_map(|line| transcript::parse(line))
        .collect();
    if entries.len() > CONTEXT_ENTRIES {
        entries.drain(..entries.len() - CONTEXT_ENTRIES);
    }
    Ok(entries)
}

/// Read a bounded tail of one task log without a partial first line.
fn read_log_tail(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(CONTEXT_LOG_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Resolve the repository item that one detail screen shows.
fn detail_item_key(decision: &Decision, item_number: Option<u64>) -> Option<(ItemKind, u64)> {
    match &decision.kind {
        DecisionKind::ReleaseGate { prs } => {
            let number = item_number
                .filter(|number| prs.contains(number))
                .or_else(|| prs.first().copied())?;
            (ItemKind::Pr, number)
        }
        DecisionKind::NeedsHuman { kind, number, .. } => (*kind, *number),
        DecisionKind::Permission { .. }
        | DecisionKind::Question { .. }
        | DecisionKind::Stuck { .. } => return None,
    }
    .into()
}

/// Find one repository item in the decision-specific state projection.
fn find_item<'a>(
    state: &'a StateView,
    repo: &str,
    kind: ItemKind,
    number: u64,
) -> Option<&'a ItemView> {
    state
        .decision_items
        .iter()
        .find(|item| item.repo == repo && item.kind == kind && item.number == number)
}

/// The bordered title and body lines of one repository item detail.
///
/// The call returns None when the state push carries no snapshot for the
/// item. A pull request description decorates its mention statuses.
fn item_lines(
    state: &StateView,
    repo: &str,
    kind: ItemKind,
    number: u64,
    width: usize,
    title: &str,
    statuses: Option<&MentionStatuses>,
) -> Option<(String, Vec<Line<'static>>)> {
    let item = find_item(state, repo, kind, number)?;
    let mut lines = wrapped_lines(
        &item.title,
        width,
        "",
        Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
    );
    lines.push(Line::from(""));
    if kind == ItemKind::Pr {
        let tickets: Vec<u64> = state
            .links
            .iter()
            .filter(|link| link.repo == repo && link.pr == number)
            .map(|link| link.ticket)
            .collect();
        if !tickets.is_empty() {
            lines.push(Line::styled(
                format!("Closes {}", pr_list(&tickets)),
                THEME.dim(),
            ));
        }
    }
    lines.push(Line::styled("Description", THEME.dim()));
    let body = if item.body.trim().is_empty() {
        "No description."
    } else {
        item.body.as_str()
    };
    match statuses {
        Some(statuses) => lines.extend(decorated_description(body, statuses, width)),
        None => lines.extend(wrapped_lines(
            body,
            width,
            "",
            Style::default().fg(THEME.text),
        )),
    }
    Some((title.to_string(), lines))
}

/// Wrap one pull request description and draw one status icon before
/// every mention whose status is known.
///
/// The decoration runs on the raw text before the wrap, so the icons
/// count toward the line width and a mention never splits from its icon.
fn decorated_description(
    body: &str,
    statuses: &MentionStatuses,
    width: usize,
) -> Vec<Line<'static>> {
    let decorated = decorate_text(body, statuses);
    transcript::wrap(&decorated, width)
        .into_iter()
        .map(|line| Line::from(mention_line_spans(&line, statuses)))
        .collect()
}

/// Insert one icon and one space before every mention with a known
/// status. The text itself never changes.
fn decorate_text(body: &str, statuses: &MentionStatuses) -> String {
    let found = mentions::scan(body);
    if found.is_empty() {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    let mut pos = 0usize;
    for mention in found {
        if mention.start > pos {
            out.push_str(&body[pos..mention.start]);
        }
        if let Some(status) = statuses.get(&(mention.repo.clone(), mention.number)) {
            let icon = mentions::glyph(*status);
            if !icon.is_empty() {
                out.push_str(icon);
                out.push(' ');
            }
        }
        out.push_str(&body[mention.start..mention.end]);
        pos = mention.end;
    }
    if pos < body.len() {
        out.push_str(&body[pos..]);
    }
    out
}

/// Split one wrapped line into spans and color the icon before each
/// mention that carries a known status.
fn mention_line_spans(line: &str, statuses: &MentionStatuses) -> Vec<Span<'static>> {
    let plain = Style::default().fg(THEME.text);
    let found = mentions::scan(line);
    if found.is_empty() {
        return vec![Span::styled(line.to_string(), plain)];
    }
    let mut spans = Vec::new();
    let mut pos = 0usize;
    for mention in found {
        let status = statuses
            .get(&(mention.repo.clone(), mention.number))
            .copied();
        let prefix = status
            .map(|status| {
                let icon = mentions::glyph(status);
                if icon.is_empty() {
                    String::new()
                } else {
                    format!("{icon} ")
                }
            })
            .unwrap_or_default();
        // The decorated icon sits immediately before its mention on this
        // line; a boundary check keeps every slice safe.
        let mut decorated_start = mention.start;
        if !prefix.is_empty() {
            if let Some(at) = mention
                .start
                .checked_sub(prefix.len())
                .filter(|&at| line.is_char_boundary(at))
            {
                if line[at..mention.start] == prefix {
                    decorated_start = at;
                }
            }
        }
        if decorated_start > pos {
            spans.push(Span::styled(line[pos..decorated_start].to_string(), plain));
        }
        if let Some(status) = status {
            if decorated_start < mention.start {
                let icon_style = Style::default().fg(tone_color(mentions::tone(status)));
                spans.push(Span::styled(
                    line[decorated_start..mention.start].to_string(),
                    icon_style,
                ));
            }
        }
        spans.push(Span::styled(
            line[mention.start..mention.end].to_string(),
            plain,
        ));
        pos = mention.end;
    }
    if pos < line.len() {
        spans.push(Span::styled(line[pos..].to_string(), plain));
    }
    spans
}

/// Wrap text with a fixed prefix on every display line.
fn wrapped_lines(text: &str, width: usize, prefix: &str, style: Style) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(prefix.chars().count()).max(1);
    transcript::wrap(text, body_width)
        .into_iter()
        .map(|line| Line::styled(format!("{prefix}{line}"), style))
        .collect()
}

/// Whether one option of a question row carries a pick.
fn option_picked(inbox: &Inbox, id: &str, question_index: usize, option_index: usize) -> bool {
    inbox
        .picks
        .get(id)
        .and_then(|picks| picks.selected.get(question_index))
        .is_some_and(|set| set.contains(&option_index))
}

/// Build the footer line: the key map, a hint, or the text input.
fn footer_text(state: &StateView, inbox: &Inbox) -> String {
    if let Some(hint) = inbox.hint {
        return hint.to_string();
    }
    if let Some(input) = &inbox.input {
        return format!(
            "{}: {}_  (enter sends, esc cancels)",
            input.label, input.buffer
        );
    }
    if let Some(detail) = inbox.detail.as_ref() {
        return match &detail.anchor {
            DetailAnchor::Train(_) => "esc back · j k scroll · h l PR".to_string(),
            DetailAnchor::Decision(id) => {
                let Some(decision) = state.decisions.iter().find(|decision| decision.id == *id)
                else {
                    return "esc back".to_string();
                };
                match decision.kind {
                    DecisionKind::ReleaseGate { .. } => {
                        "esc back · j k scroll · h l PR · space include/exclude · g release"
                            .to_string()
                    }
                    DecisionKind::Permission { .. } => {
                        "esc back · j k scroll · y allow · n deny · o session".to_string()
                    }
                    DecisionKind::Question { .. } => {
                        "esc back · j k scroll · 1-9 pick · s submit · i write · o session"
                            .to_string()
                    }
                    DecisionKind::Stuck { .. } => {
                        "esc back · j k scroll · r retry · c cancel · o session".to_string()
                    }
                    DecisionKind::NeedsHuman { .. } => {
                        "esc back · j k scroll · t comment · c clear label".to_string()
                    }
                }
            }
        };
    }
    let Some(index) = inbox.selected_index(state) else {
        return "j k move · ! oldest".to_string();
    };
    let ordered = ordered_decisions(state);
    let Some(decision) = ordered.get(index) else {
        return "j k move · ! oldest".to_string();
    };
    match &decision.kind {
        DecisionKind::Permission { .. } => {
            "PgUp PgDn scroll · j k move · y allow · n deny · enter details".to_string()
        }
        DecisionKind::Question { .. } => {
            "PgUp PgDn scroll · j k move · 1-9 pick · s submit · i write · enter details"
                .to_string()
        }
        DecisionKind::Stuck { .. } => {
            "PgUp PgDn scroll · j k move · r retry · c cancel · enter details".to_string()
        }
        DecisionKind::NeedsHuman { .. } => {
            "PgUp PgDn scroll · j k move · t comment · c clear label · enter details".to_string()
        }
        DecisionKind::ReleaseGate { .. } => {
            "PgUp PgDn scroll · j k move · 1-9 include · space all or none · g release · enter details"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::config::ReleasePolicy;
    use crate::model::Stage;
    use crate::sock::{
        InputMode, ItemView, MentionStatus, PausedView, RepoView, TaskView, TrainView,
    };
    use crate::tasks::{Task, TaskState};

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
            protocol_revision: crate::sock::WIRE_PROTOCOL_REVISION,
            links: Vec::new(),
            repos: Vec::new(),
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions,
            decision_items: Vec::new(),
            tickets: Vec::new(),
            trains: Vec::new(),
            paused: PausedView {
                global: false,
                overrides: Vec::new(),
            },
            settings: crate::sock::SettingsView::default(),
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

    #[test]
    fn the_socket_client_sends_an_inbox_action() {
        let socket = std::env::temp_dir().join(format!("aif-inbox-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&socket).unwrap();
        let mut client = crate::sock::Client::connect(&socket).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let state = full_state();
        let mut inbox = selected(&state, 0);
        let expected = Action::Answer {
            decision_id: "perm:borsuk/implement-i142:req-1".to_string(),
            response: Response::Allow,
        };

        let outcome = inbox.handle_key(&state, press('y'), &mut client);

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert_eq!(outcome, InboxOutcome::None);
        assert_eq!(serde_json::from_str::<Action>(&line).unwrap(), expected);
        drop(client);
        drop(listener);
        fs::remove_file(socket).unwrap();
    }

    /// Render the inbox and return the whole screen as text.
    fn render(state: &StateView, inbox: &Inbox, now_ms: u64) -> String {
        render_with_height(state, inbox, now_ms, 20)
    }

    /// Render the inbox at one test height.
    fn render_with_height(state: &StateView, inbox: &Inbox, now_ms: u64, height: u16) -> String {
        render_at_size(state, inbox, now_ms, 100, height)
    }

    /// Render the inbox at one test width and height.
    fn render_at_size(
        state: &StateView,
        inbox: &Inbox,
        now_ms: u64,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
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
    fn feed_items_render_age_repo_stage_and_primary_message() {
        let state = full_state();
        let inbox = Inbox::new();
        let screen = render(&state, &inbox, OPENED + 90_000);

        assert!(
            screen.contains("decisions · 5 open · oldest first"),
            "screen: {screen}"
        );
        assert!(
            screen.contains("PERMISSION · 1m · borsuk · implement"),
            "metadata: {screen}"
        );
        assert!(
            screen.contains("Allow Write for src/main.rs?"),
            "message: {screen}"
        );
        assert!(screen.contains("Which database?"), "message: {screen}");
        assert!(
            screen.contains("Task borsuk/implement-i142 stopped: 3 failures"),
            "message: {screen}"
        );
        assert!(
            screen.contains("Ticket #142 needs a decision: Fix the flake"),
            "message: {screen}"
        );
        assert!(
            screen.contains("Release 2 PRs: #7 #9?"),
            "message: {screen}"
        );
    }

    #[test]
    fn the_feed_lists_decisions_from_oldest_to_newest() {
        let worker = worker();
        let decisions = vec![
            Decision::permission(
                &worker,
                "newest",
                "Write",
                serde_json::json!({"file_path": "src/main.rs"}),
                OPENED + 2_000,
            ),
            Decision::release_gate("borsuk", vec![7], OPENED),
            Decision::needs_human(
                "borsuk",
                ItemKind::Issue,
                142,
                "Choose the storage",
                OPENED + 1_000,
            ),
        ];
        let state = state_with(decisions);
        let mut inbox = Inbox::new();
        inbox.observe(&state);

        let screen = render(&state, &inbox, OPENED + 4_000);

        let release = screen
            .find("Release PR #7?")
            .expect("the oldest release decision is absent");
        let human = screen
            .find("Ticket #142 needs a decision: Choose the storage")
            .expect("the middle decision is absent");
        let permission = screen
            .find("Allow Write for src/main.rs?")
            .expect("the newest permission decision is absent");
        assert!(release < human && human < permission, "screen: {screen}");
        assert_eq!(inbox.selected_id(), Some("gate:borsuk"));
        assert!(
            screen.contains("decisions · 3 open · oldest first"),
            "screen: {screen}"
        );
    }

    #[test]
    fn the_feed_shows_quick_actions_only_on_the_selected_item() {
        let worker = worker();
        let state = state_with(vec![
            Decision::permission(
                &worker,
                "req-1",
                "Write",
                serde_json::json!({"file_path": "first.rs"}),
                OPENED,
            ),
            Decision::stuck(&worker, "second task failure", OPENED + 1),
        ]);
        let inbox = selected(&state, 0);

        let screen = render(&state, &inbox, OPENED + 2);

        assert_eq!(
            screen.matches("[enter] details").count(),
            1,
            "screen: {screen}"
        );
        assert!(!screen.contains("[r] retry"), "screen: {screen}");
    }

    #[test]
    fn a_pr_description_decorates_its_known_mentions_once_requested() {
        use crate::sock::{ItemView, TicketMentionStatus};

        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "Protect the release gate".to_string(),
            body: "Closes #9 and needs #8.".to_string(),
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        let action = inbox.take_pending_pr_mention(&state);
        assert!(matches!(
            &action,
            Some(Action::Ticket(TicketAction::PrMentions { repo, number, .. }))
                if repo == "borsuk" && *number == 7
        ));
        assert!(
            inbox.take_pending_pr_mention(&state).is_none(),
            "one request per pull request must be enough"
        );

        inbox.observe_pr_mentions(&TicketMentions {
            request: "m".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            statuses: vec![TicketMentionStatus {
                repo: None,
                number: 9,
                status: MentionStatus::OpenIssue,
            }],
        });

        let screen = render(&state, &inbox, OPENED + 15_000);

        assert!(screen.contains("● #9"), "screen: {screen}");
        assert!(
            !screen.contains("○ #8") && !screen.contains("● #8"),
            "an unknown mention must stay plain: {screen}"
        );
        assert!(rx.try_recv().is_err(), "the request must bypass the sink");
    }

    #[test]
    fn an_issue_or_stuck_detail_sends_no_pr_mention_request() {
        let worker = worker();
        let state = state_with(vec![Decision::stuck(&worker, "failure", OPENED)]);
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        assert!(inbox.take_pending_pr_mention(&state).is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn enter_on_a_release_decision_opens_the_pull_request_description() {
        use crate::sock::ItemView;

        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "Protect the release gate".to_string(),
            body: "Require a current release state before the train starts.".to_string(),
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        let outcome = inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED + 15_000);

        assert_eq!(outcome, InboxOutcome::None);
        assert!(rx.try_recv().is_err(), "Enter sent an answer");
        assert!(screen.contains("PR #7"), "screen: {screen}");
        assert!(
            screen.contains("Protect the release gate"),
            "screen: {screen}"
        );
        assert!(
            screen.contains("Require a current release state before the train starts."),
            "screen: {screen}"
        );
        assert!(screen.contains("esc back"), "screen: {screen}");
    }

    #[test]
    fn a_question_detail_shows_only_its_task_context_and_o_opens_that_session() {
        let correct_log =
            std::env::temp_dir().join(format!("aif-inbox-correct-{}.jsonl", uuid::Uuid::new_v4()));
        let other_log =
            std::env::temp_dir().join(format!("aif-inbox-other-{}.jsonl", uuid::Uuid::new_v4()));
        fs::write(
            &correct_log,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",",
                "\"text\":\"I inspected the storage adapter before this question.\"}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",",
                "\"thinking\":\"PRIVATE THOUGHT CONTEXT\"}]}}\n",
                "{\"type\":\"control_request\",\"request_id\":\"req-2\",",
                "\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"AskUserQuestion\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",",
                "\"text\":\"LATER TASK CONTEXT\"}]}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &other_log,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"WRONG TASK CONTEXT\"}]}}\n",
        )
        .unwrap();
        let mut state = state_with(vec![every_decision()[1].clone()]);
        state.tasks = vec![
            task_view("borsuk/implement-i142", &correct_log),
            task_view("borsuk/refine-i999", &other_log),
        ];
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED);
        let outcome = inbox.handle_key(&state, press('o'), &mut tx);

        assert!(
            screen.contains("I inspected the storage adapter before this question."),
            "screen: {screen}"
        );
        assert!(!screen.contains("WRONG TASK CONTEXT"), "screen: {screen}");
        assert!(
            !screen.contains("PRIVATE THOUGHT CONTEXT"),
            "screen: {screen}"
        );
        assert!(!screen.contains("LATER TASK CONTEXT"), "screen: {screen}");
        assert_eq!(
            outcome,
            InboxOutcome::OpenSession("borsuk/implement-i142".to_string())
        );
        assert!(rx.try_recv().is_err(), "detail navigation sent an answer");

        fs::remove_file(correct_log).unwrap();
        fs::remove_file(other_log).unwrap();
    }

    #[test]
    fn a_missing_pull_request_snapshot_has_a_clear_detail_message() {
        let state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("PR #7"), "screen: {screen}");
        assert!(
            screen.contains("The PR description is unavailable."),
            "screen: {screen}"
        );
    }

    #[test]
    fn a_pr_detail_shows_the_tickets_it_closes() {
        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items = vec![ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "First change".to_string(),
            body: "The first pull request body.".to_string(),
        }];
        state.links = vec![
            crate::sock::LinkView {
                repo: "borsuk".to_string(),
                ticket: 142,
                pr: 7,
            },
            crate::sock::LinkView {
                repo: "borsuk".to_string(),
                ticket: 150,
                pr: 7,
            },
        ];
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("Closes #142 #150"), "screen: {screen}");
    }

    #[test]
    fn a_release_detail_moves_between_pull_requests_and_toggles_the_current_one() {
        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7, 9], OPENED)]);
        state.decision_items = vec![
            ItemView {
                repo: "borsuk".to_string(),
                kind: ItemKind::Pr,
                number: 7,
                title: "First change".to_string(),
                body: "The first pull request body.".to_string(),
            },
            ItemView {
                repo: "borsuk".to_string(),
                kind: ItemKind::Pr,
                number: 9,
                title: "Second change".to_string(),
                body: "The second pull request body.".to_string(),
            },
        ];
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert!(
            render(&state, &inbox, OPENED).contains("The first pull request body."),
            "the first pull request did not open"
        );

        inbox.handle_key(&state, press_code(KeyCode::Right), &mut tx);
        let second = render(&state, &inbox, OPENED);
        assert!(second.contains("PR #9"), "screen: {second}");
        assert!(
            second.contains("The second pull request body."),
            "screen: {second}"
        );
        assert!(!second.contains("The first pull request body."));

        inbox.handle_key(&state, press(' '), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Esc), &mut tx);
        let feed = render(&state, &inbox, OPENED);
        assert!(!inbox.detail_open());
        assert_eq!(inbox.selected_id(), Some("gate:borsuk"));
        assert!(feed.contains("1. [x] #7 First change"), "screen: {feed}");
        assert!(feed.contains("2. [ ] #9 Second change"), "screen: {feed}");
        assert!(rx.try_recv().is_err(), "navigation sent an answer");
    }

    #[test]
    fn a_release_repush_keeps_the_current_pull_request_by_number() {
        let mut first = state_with(vec![Decision::release_gate("borsuk", vec![7, 9], OPENED)]);
        first.decision_items = vec![
            ItemView {
                repo: "borsuk".to_string(),
                kind: ItemKind::Pr,
                number: 7,
                title: "First change".to_string(),
                body: "The first pull request body.".to_string(),
            },
            ItemView {
                repo: "borsuk".to_string(),
                kind: ItemKind::Pr,
                number: 9,
                title: "Second change".to_string(),
                body: "The second pull request body.".to_string(),
            },
        ];
        let mut inbox = selected(&first, 0);
        let (mut tx, _rx) = fake_sink();
        inbox.handle_key(&first, press_code(KeyCode::Enter), &mut tx);
        inbox.handle_key(&first, press_code(KeyCode::Right), &mut tx);

        let mut repushed = first.clone();
        repushed.decisions = vec![Decision::release_gate("borsuk", vec![9, 7], OPENED)];
        inbox.observe(&repushed);
        let screen = render(&repushed, &inbox, OPENED);

        assert!(screen.contains("PR #9"), "screen: {screen}");
        assert!(
            screen.contains("The second pull request body."),
            "screen: {screen}"
        );
        assert!(!screen.contains("The first pull request body."));
    }

    #[test]
    fn enter_on_a_needs_human_issue_opens_its_description() {
        let mut state = state_with(vec![Decision::needs_human(
            "borsuk",
            ItemKind::Issue,
            142,
            "Choose the storage",
            OPENED,
        )]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Issue,
            number: 142,
            title: "Choose the storage".to_string(),
            body: "Compare the operational cost of both storage options.".to_string(),
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED);

        assert!(screen.contains("Ticket #142"), "screen: {screen}");
        assert!(
            screen.contains("Compare the operational cost of both storage options."),
            "screen: {screen}"
        );
        assert!(rx.try_recv().is_err(), "Enter sent an answer");
    }

    #[test]
    fn enter_opens_question_context_and_s_submits_the_selected_answer() {
        let state = state_with(vec![every_decision()[1].clone()]);
        let mut inbox = selected(&state, 0);
        let (mut tx, rx) = fake_sink();

        inbox.handle_key(&state, press('1'), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert!(inbox.detail_open());
        assert!(rx.try_recv().is_err(), "Enter submitted the answer");

        inbox.handle_key(&state, press('s'), &mut tx);

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
    fn a_missing_task_log_has_a_clear_context_message() {
        let state = {
            let mut state = state_with(vec![every_decision()[2].clone()]);
            let path = std::env::temp_dir()
                .join(format!("aif-inbox-missing-{}.jsonl", uuid::Uuid::new_v4()));
            state.tasks.push(task_view("borsuk/implement-i142", &path));
            state
        };
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let screen = render(&state, &inbox, OPENED);

        assert!(
            screen.contains("The task log is unavailable."),
            "screen: {screen}"
        );
    }

    #[test]
    fn a_narrow_detail_keeps_the_description_and_back_key_readable() {
        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "Protect releases".to_string(),
            body: "This description explains the release. It stays readable on a narrow terminal."
                .to_string(),
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);

        let screen = render_at_size(&state, &inbox, OPENED, 44, 12);

        assert!(
            screen.contains("This description explains"),
            "screen: {screen}"
        );
        assert!(screen.contains("narrow terminal."), "screen: {screen}");
        assert!(screen.contains("esc back"), "screen: {screen}");
        assert!(
            screen.lines().all(|line| line.chars().count() == 44),
            "screen: {screen}"
        );
    }

    #[test]
    fn detail_page_keys_stop_at_the_last_content_line() {
        let body = (1..=20)
            .map(|number| format!("description line {number:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "Protect releases".to_string(),
            body,
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        render_at_size(&state, &inbox, OPENED, 60, 10);

        for _ in 0..20 {
            inbox.handle_key(&state, press_code(KeyCode::PageDown), &mut tx);
        }
        let bottom = render_at_size(&state, &inbox, OPENED, 60, 10);
        assert!(bottom.contains("description line 20"), "screen: {bottom}");

        inbox.handle_key(&state, press_code(KeyCode::PageUp), &mut tx);
        let previous_page = render_at_size(&state, &inbox, OPENED, 60, 10);
        assert!(
            previous_page.contains("description line 12"),
            "screen: {previous_page}"
        );
    }

    #[test]
    fn a_narrow_feed_wraps_a_long_question_option() {
        let decision = Decision::question(
            &worker(),
            "req-long-option",
            serde_json::json!([{
                "question": "Which option?",
                "header": "Choice",
                "options": [{
                    "label": "Use the option that continues beyond the narrow terminal edge.",
                    "description": "",
                }],
                "multiSelect": false,
            }]),
            OPENED,
        );
        let state = state_with(vec![decision]);
        let inbox = selected(&state, 0);

        let screen = render_at_size(&state, &inbox, OPENED, 44, 14);

        assert!(screen.contains("narrow terminal edge."), "screen: {screen}");
        assert!(
            screen.lines().all(|line| line.chars().count() == 44),
            "screen: {screen}"
        );
    }

    #[test]
    fn narrow_feed_and_detail_wrap_a_long_pull_request_title() {
        let mut state = state_with(vec![Decision::release_gate("borsuk", vec![7], OPENED)]);
        state.decision_items.push(ItemView {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 7,
            title: "Protect the release when its title continues beyond a narrow terminal."
                .to_string(),
            body: "A short description.".to_string(),
        });
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();

        let feed = render_at_size(&state, &inbox, OPENED, 44, 16);
        assert!(feed.contains("title continues"), "feed: {feed}");
        assert!(feed.contains("terminal."), "feed: {feed}");

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        let detail = render_at_size(&state, &inbox, OPENED, 44, 16);
        assert!(detail.contains("terminal."), "detail: {detail}");
    }

    /// One visible task with a chosen log path.
    fn task_view(id: &str, log_path: &std::path::Path) -> TaskView {
        TaskView {
            id: id.to_string(),
            repo: "borsuk".to_string(),
            stage: Stage::Implement,
            kind: ItemKind::Issue,
            number: 142,
            state: TaskState::AwaitingUser,
            attempt: 1,
            log_path: log_path.to_path_buf(),
            input: InputMode::Live,
            queued_messages: 0,
        }
    }

    #[test]
    fn a_row_summary_replaces_line_breaks_with_spaces() {
        let decision = Decision::stuck(&worker(), "line one\nline two", OPENED);
        let state = state_with(vec![decision]);
        let inbox = Inbox::new();

        let screen = render(&state, &inbox, OPENED);

        assert!(
            screen
                .lines()
                .any(|line| line.contains("line one line two")),
            "screen: {screen}"
        );
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
        assert!(first.contains("PERMISSION · 1h"), "screen: {first}");

        // The daemon re-pushes the row five minutes later. The age grows
        // from opened_ms and never falls back to zero.
        let repushed = full_state();
        let second = render(&repushed, &inbox, OPENED + 3_900_000);
        assert!(second.contains("PERMISSION · 1h"), "screen: {second}");
    }

    #[test]
    fn a_time_before_the_unix_epoch_returns_an_error() {
        let before_epoch = UNIX_EPOCH - Duration::from_millis(1);

        let error = millis_since_epoch(before_epoch).unwrap_err();

        assert!(error.to_string().contains("before the Unix epoch"));
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
            screen.contains("1-9 pick · s submit · i write · enter details"),
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
            screen.contains("1-9 include · space all or none · g release · enter details"),
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
    fn permission_y_sends_allow_enter_opens_details_and_o_opens_the_session() {
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
        assert_eq!(outcome, InboxOutcome::None);
        assert!(inbox.detail_open());
        let outcome = inbox.handle_key(&state, press('o'), &mut tx);
        assert_eq!(
            outcome,
            InboxOutcome::OpenSession("borsuk/implement-i142".to_string())
        );
        assert!(rx.try_recv().is_err(), "enter sends no answer");
    }

    #[test]
    fn permission_answers_require_a_plain_key_press() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        let repeat = KeyEvent::new_with_kind(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Repeat,
        );
        let super_key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::SUPER);

        inbox.handle_key(&state, release, &mut tx);
        inbox.handle_key(&state, repeat, &mut tx);
        inbox.handle_key(&state, super_key, &mut tx);

        assert!(rx.try_recv().is_err(), "a modified event sent an answer");
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
    fn question_digits_pick_and_s_submits_the_answers() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press('1'), &mut tx);
        assert!(rx.try_recv().is_err(), "a pick alone sends nothing");
        inbox.handle_key(&state, press('s'), &mut tx);

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
    fn question_s_without_a_pick_is_blocked_with_a_hint() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);

        inbox.handle_key(&state, press('s'), &mut tx);

        assert!(
            rx.try_recv().is_err(),
            "an unanswered question sends nothing"
        );
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("answer every question"), "hint: {screen}");
    }

    #[test]
    fn question_s_without_a_multi_select_pick_is_blocked_with_a_hint() {
        let decision = Decision::question(
            &worker(),
            "req-multi",
            serde_json::json!([{
                "question": "Which caches?",
                "header": "Caches",
                "options": [
                    {"label": "redis", "description": ""},
                    {"label": "memcached", "description": ""},
                ],
                "multiSelect": true,
            }]),
            OPENED,
        );
        let state = state_with(vec![decision]);
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('s'), &mut tx);

        assert!(rx.try_recv().is_err(), "an empty choice sent an answer");
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
        assert!(screen.contains("Which database?"), "screen: {screen}");
        assert!(screen.contains("Which caches?"), "screen: {screen}");
        assert!(screen.contains("1. [x] SQLite"), "screen: {screen}");
        assert!(screen.contains("2. [x] redis"), "screen: {screen}");
        assert!(screen.contains("3. [x] memcached"), "screen: {screen}");

        inbox.handle_key(&state, press('2'), &mut tx);
        inbox.handle_key(&state, press('s'), &mut tx);

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

    /// The `AskUserQuestion` input as the claude runner records it: the
    /// whole tool input, so the question list sits under `questions`.
    fn recorded_tool_input() -> serde_json::Value {
        serde_json::json!({
            "questions": [{
                "question": "A Failed prior task: does it block the next stage?",
                "header": "Gate rule",
                "options": [
                    {"label": "Split by stage", "description": "a terminal refine frees implement"},
                    {"label": "main wins", "description": "a failed refine blocks implement"},
                ],
                "multiSelect": false,
            }]
        })
    }

    #[test]
    fn parse_questions_reads_a_bare_array_and_a_recorded_tool_input() {
        let bare = parse_questions(&serde_json::json!([
            {"question": "Which database?", "header": "Storage", "options": []},
        ]));
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].key, "Storage");

        let recorded = parse_questions(&recorded_tool_input());
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].key, "Gate rule");
        assert_eq!(recorded[0].options.len(), 2);
        assert_eq!(recorded[0].options[0].0, "Split by stage");
        assert!(!recorded[0].multi_select);

        // Every other shape carries no question list.
        for other in [
            serde_json::json!(null),
            serde_json::json!("no"),
            serde_json::json!({"questions": "no"}),
            serde_json::json!({}),
        ] {
            assert!(parse_questions(&other).is_empty(), "shape: {other}");
        }
    }

    #[test]
    fn a_recorded_tool_input_question_shows_its_options_and_submits_a_pick() {
        let decision = Decision::question(&worker(), "req-ask", recorded_tool_input(), OPENED);
        let state = state_with(vec![decision]);
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        let screen = render(&state, &inbox, OPENED);
        assert!(
            !screen.contains("unreadable"),
            "the options must read: {screen}"
        );
        assert!(
            screen.contains("A Failed prior task: does it block the next stage?"),
            "message: {screen}"
        );
        assert!(screen.contains("1. [ ] Split by stage"), "screen: {screen}");
        assert!(screen.contains("2. [ ] main wins"), "screen: {screen}");

        inbox.handle_key(&state, press('1'), &mut tx);
        assert!(
            render(&state, &inbox, OPENED).contains("1. [x] Split by stage"),
            "the pick must show"
        );
        inbox.handle_key(&state, press('s'), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-ask".to_string(),
                response: Response::Answers {
                    updated_input: serde_json::json!({
                        "answers": {"Gate rule": "Split by stage"},
                    }),
                },
            }
        );
    }

    #[test]
    fn the_question_context_screen_shows_the_options_and_takes_a_pick() {
        let decision = Decision::question(&worker(), "req-ask", recorded_tool_input(), OPENED);
        let state = state_with(vec![decision]);
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert!(inbox.detail_open());

        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("QUESTION · borsuk"), "title: {screen}");
        assert!(screen.contains("1. [ ] Split by stage"), "screen: {screen}");
        assert!(screen.contains("2. [ ] main wins"), "screen: {screen}");
        assert!(
            screen.contains("1-9 pick · s submit"),
            "footer names the keys: {screen}"
        );

        inbox.handle_key(&state, press('2'), &mut tx);
        assert!(
            render(&state, &inbox, OPENED).contains("2. [x] main wins"),
            "the context screen shows the pick"
        );
        inbox.handle_key(&state, press('s'), &mut tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-ask".to_string(),
                response: Response::Answers {
                    updated_input: serde_json::json!({"answers": {"Gate rule": "main wins"}}),
                },
            }
        );
    }

    #[test]
    fn stuck_r_retries_c_cancels_enter_opens_details_and_o_opens_the_session() {
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
        assert_eq!(outcome, InboxOutcome::None);
        assert!(inbox.detail_open());
        let outcome = inbox.handle_key(&state, press('o'), &mut tx);
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

        // Enter opens the issue context. The detail has no task session.
        let outcome = inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(outcome, InboxOutcome::None);
        assert!(inbox.detail_open());
        let outcome = inbox.handle_key(&state, press('o'), &mut tx);
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
        assert!(screen.contains("select at least one PR"), "hint: {screen}");

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
    fn each_kind_has_an_exact_immediate_answer_key_map() {
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
                let expected = match (&decision.kind, code) {
                    (DecisionKind::Permission { .. }, KeyCode::Char('y')) => Some(Response::Allow),
                    (DecisionKind::Stuck { .. }, KeyCode::Char('r')) => Some(Response::Retry),
                    (DecisionKind::Stuck { .. }, KeyCode::Char('c'))
                    | (DecisionKind::NeedsHuman { .. }, KeyCode::Char('c')) => {
                        Some(Response::Cancel)
                    }
                    (DecisionKind::ReleaseGate { prs }, KeyCode::Char('g')) => {
                        Some(Response::Go { prs: prs.clone() })
                    }
                    _ => None,
                };
                let expected: Vec<Action> = expected
                    .map(|response| Action::Answer {
                        decision_id: decision.id.clone(),
                        response,
                    })
                    .into_iter()
                    .collect();
                let actual: Vec<Action> = rx.try_iter().collect();

                assert_eq!(actual, expected, "row {index}, key {code:?}");
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

        type_text(&mut inbox, &state, "later", &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::Enter), &mut tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Action::Answer {
                decision_id: "perm:borsuk/implement-i142:req-1".to_string(),
                response: Response::Deny {
                    message: "later".to_string(),
                },
            }
        );
    }

    #[test]
    fn a_push_that_closes_the_input_row_cancels_that_input() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('n'), &mut tx);
        let next_state = state_with(every_decision()[1..].to_vec());
        inbox.observe(&next_state);

        assert!(inbox.input.is_none(), "the closed row kept its input");
        assert!(
            rx.try_recv().is_err(),
            "closing an input row must not send an answer"
        );
    }

    #[test]
    fn a_push_that_changes_the_input_row_kind_cancels_that_input() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 0);

        inbox.handle_key(&state, press('n'), &mut tx);
        let question = Decision::question(
            &worker(),
            "req-1",
            serde_json::json!([{
                "question": "Continue?",
                "header": "Choice",
                "options": [{"label": "yes", "description": ""}],
                "multiSelect": false,
            }]),
            OPENED,
        );
        let next_state = state_with(vec![question]);
        inbox.observe(&next_state);

        assert!(inbox.input.is_none(), "the changed row kept its input");
        assert!(
            rx.try_recv().is_err(),
            "changing an input row must not send an answer"
        );
    }

    #[test]
    fn a_push_that_changes_question_options_clears_stale_picks() {
        let state = full_state();
        let (mut tx, rx) = fake_sink();
        let mut inbox = selected(&state, 1);
        inbox.handle_key(&state, press('2'), &mut tx);

        let changed = Decision::question(
            &worker(),
            "req-2",
            serde_json::json!([{
                "question": "Which database?",
                "header": "Storage",
                "options": [
                    {"label": "duckdb", "description": "embedded"},
                    {"label": "mysql", "description": "server"},
                ],
                "multiSelect": false,
            }]),
            OPENED,
        );
        let changed_state = state_with(vec![changed]);
        inbox.observe(&changed_state);

        inbox.handle_key(&changed_state, press('s'), &mut tx);

        assert!(rx.try_recv().is_err(), "a stale option sent an answer");
        let screen = render(&changed_state, &inbox, OPENED);
        assert!(screen.contains("answer every question"), "hint: {screen}");
    }

    #[test]
    fn a_gate_repush_drops_checks_for_absent_pull_requests() {
        let first = state_with(vec![Decision::release_gate("borsuk", vec![7, 9], OPENED)]);
        let (mut tx, _rx) = fake_sink();
        let mut inbox = selected(&first, 0);
        inbox.handle_key(&first, press('2'), &mut tx);

        let middle = state_with(vec![Decision::release_gate("borsuk", vec![9], OPENED)]);
        inbox.observe(&middle);
        let last = state_with(vec![Decision::release_gate("borsuk", vec![7, 9], OPENED)]);
        inbox.observe(&last);

        let screen = render(&last, &inbox, OPENED);
        assert!(screen.contains("1. [ ] #7"), "stale check: {screen}");
        assert!(screen.contains("2. [ ] #9"), "stale check: {screen}");
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
    fn the_selected_row_remains_visible_below_the_viewport() {
        let worker = worker();
        let decisions = (0..12)
            .map(|index| {
                Decision::permission(
                    &worker,
                    &format!("req-{index}"),
                    "Write",
                    serde_json::json!({"file_path": format!("file-{index}.rs")}),
                    OPENED,
                )
            })
            .collect();
        let state = state_with(decisions);
        let inbox = selected(&state, 11);

        let screen = render_with_height(&state, &inbox, OPENED, 8);

        assert!(screen.contains("> PERMISSION · 0s"), "selection: {screen}");
        assert!(screen.contains("file-11.rs"), "selected row: {screen}");
    }

    #[test]
    fn page_keys_scroll_and_clamp_a_selected_item_that_exceeds_the_viewport() {
        let options: Vec<serde_json::Value> = (1..=9)
            .map(|number| {
                serde_json::json!({
                    "label": format!("option {number}"),
                    "description": "",
                })
            })
            .collect();
        let decision = Decision::question(
            &worker(),
            "req-tall",
            serde_json::json!([{
                "question": "Choose one option.",
                "header": "Choice",
                "options": options,
                "multiSelect": false,
            }]),
            OPENED,
        );
        let state = state_with(vec![decision]);
        let mut inbox = selected(&state, 0);
        let (mut tx, _rx) = fake_sink();

        let top = render_at_size(&state, &inbox, OPENED, 60, 8);
        assert!(top.contains("option 1"), "screen: {top}");
        assert!(!top.contains("option 9"), "screen: {top}");

        inbox.handle_key(&state, press_code(KeyCode::PageDown), &mut tx);
        let bottom = render_at_size(&state, &inbox, OPENED, 60, 8);
        assert!(bottom.contains("option 9"), "screen: {bottom}");

        inbox.handle_key(&state, press_code(KeyCode::PageDown), &mut tx);
        inbox.handle_key(&state, press_code(KeyCode::PageUp), &mut tx);
        let top_again = render_at_size(&state, &inbox, OPENED, 60, 8);
        assert!(top_again.contains("option 1"), "screen: {top_again}");
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
            (1, "1-9 pick · s submit · i write · enter details"),
            (2, "r retry · c cancel"),
            (3, "t comment · c clear label · enter details"),
            (4, "1-9 include · space all or none · g release"),
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
        inbox.handle_key(&state, press('s'), &mut tx);
        assert!(
            rx.try_recv().is_err(),
            "screen: {}",
            render(&state, &inbox, OPENED)
        );
    }

    /// A state with one borsuk train and the content of its pull requests.
    fn train_state(
        decisions: Vec<Decision>,
        queue: Vec<u64>,
        stacked: Vec<u64>,
        batch: Vec<u64>,
    ) -> StateView {
        let mut items: Vec<ItemView> = Vec::new();
        for number in queue.iter().chain(&stacked).chain(&batch) {
            if items.iter().any(|item| item.number == *number) {
                continue;
            }
            items.push(ItemView {
                repo: "borsuk".to_string(),
                kind: ItemKind::Pr,
                number: *number,
                title: format!("pr {number}"),
                body: format!("body {number}"),
            });
        }
        StateView {
            protocol_revision: crate::sock::WIRE_PROTOCOL_REVISION,
            links: Vec::new(),
            repos: vec![RepoView {
                alias: "borsuk".to_string(),
                owner_repo: String::new(),
            }],
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions,
            decision_items: items,
            tickets: Vec::new(),
            trains: vec![TrainView {
                repo: "borsuk".to_string(),
                queue,
                stacked,
                batch,
                policy: ReleasePolicy::Manual,
                next_fire_ms: None,
                in_flight: None,
            }],
            paused: PausedView {
                global: false,
                overrides: Vec::new(),
            },
            settings: crate::sock::SettingsView::default(),
        }
    }

    #[test]
    fn open_release_pr_prefers_the_open_gate_row() {
        let state = train_state(
            vec![Decision::release_gate("borsuk", vec![7, 9], OPENED)],
            vec![7, 9],
            vec![7],
            Vec::new(),
        );
        let mut inbox = Inbox::new();

        assert!(inbox.open_release_pr(&state, "borsuk", 9));

        assert!(inbox.detail_open());
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("PR #9 · borsuk"), "detail:\n{screen}");
        assert!(screen.contains("pr 9"), "body:\n{screen}");
        assert!(
            screen.contains("space include/exclude · g release"),
            "footer:\n{screen}"
        );
    }

    #[test]
    fn open_release_pr_follows_the_train_without_a_gate() {
        let state = train_state(Vec::new(), vec![7, 9], Vec::new(), Vec::new());
        let mut inbox = Inbox::new();

        assert!(inbox.open_release_pr(&state, "borsuk", 9));

        assert!(inbox.detail_open());
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("PR #9 · borsuk"), "detail:\n{screen}");
        assert!(screen.contains("h l PR"), "footer:\n{screen}");

        // Left moves through the train queue; Esc closes the detail.
        let (mut tx, rx) = fake_sink();
        inbox.handle_key(&state, press_code(KeyCode::Left), &mut tx);
        let screen = render(&state, &inbox, OPENED);
        assert!(screen.contains("PR #7 · borsuk"), "moved:\n{screen}");
        assert!(rx.try_recv().is_err(), "a detail key sends no action");

        inbox.handle_key(&state, press_code(KeyCode::Esc), &mut tx);
        assert!(!inbox.detail_open());
    }

    #[test]
    fn open_release_pr_refuses_a_pull_request_outside_the_release_lane() {
        let state = train_state(Vec::new(), vec![7], Vec::new(), Vec::new());
        let mut inbox = Inbox::new();

        assert!(!inbox.open_release_pr(&state, "borsuk", 42));
        assert!(!inbox.detail_open());
        assert!(!inbox.open_release_pr(&state, "ryba", 7));
        assert!(!inbox.detail_open());
    }

    #[test]
    fn observe_reanchors_and_drops_the_train_detail() {
        let state = train_state(Vec::new(), vec![7, 9], Vec::new(), Vec::new());
        let mut inbox = Inbox::new();
        assert!(inbox.open_release_pr(&state, "borsuk", 9));

        // The pull request 9 leaves the queue: the detail re-anchors.
        let shrunken = train_state(Vec::new(), vec![7], Vec::new(), Vec::new());
        inbox.observe(&shrunken);
        assert!(inbox.detail_open());
        let screen = render(&shrunken, &inbox, OPENED);
        assert!(screen.contains("PR #7 · borsuk"), "re-anchored:\n{screen}");

        // The train itself leaves the state: the detail closes.
        let bare = state_with(Vec::new());
        inbox.observe(&bare);
        assert!(!inbox.detail_open());
    }
}
