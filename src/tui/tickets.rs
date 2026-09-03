//! The Tickets list and its nested issue views.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::time::Instant;

use crate::sock::{
    Action, StateView, TicketAction, TicketConflict, TicketContent, TicketContentSource,
    TicketDetails, TicketGroup, TicketLabels, TicketMentions, TicketResult, TicketResultKind,
    TicketSummary,
};

use super::markdown::{markdown_lines_with_mentions, MentionStatuses};
use super::session::SessionView;
use super::theme::THEME;

/// The active field of the direct editor.
#[derive(Debug, Clone, Copy, Default)]
enum EditorField {
    /// The title field.
    #[default]
    Title,
    /// The description field.
    Body,
}

/// One direct title and description draft.
#[derive(Debug, Clone)]
struct Editor {
    /// The confirmed content used for conflict protection.
    original: TicketContent,
    /// The pending title.
    title: String,
    /// The pending description.
    body: String,
    /// The field that receives text.
    field: EditorField,
    /// True while the editor is the visible nested view.
    open: bool,
}

/// The active field of the new-label form.
#[derive(Debug, Clone, Copy, Default)]
enum NewLabelField {
    /// The label name field.
    #[default]
    Name,
    /// The six-digit color field.
    Color,
}

/// One pending repository label.
#[derive(Debug, Clone, Default)]
struct NewLabelForm {
    /// The requested label name.
    name: String,
    /// The requested hexadecimal color.
    color: String,
    /// The field that receives text.
    field: NewLabelField,
    /// The current validation error.
    error: Option<String>,
}

/// The Tickets view state.
#[derive(Debug, Default)]
pub struct Tickets {
    /// The repository tab that the list shows.
    tab: Option<String>,
    /// The active search text.
    query: String,
    /// True while key presses edit the search text.
    searching: bool,
    /// The selected filtered row.
    selected: usize,
    /// True while one issue focus is open.
    focus: bool,
    /// The issue identity of the open focus.
    focus_key: Option<(String, u64)>,
    /// The full confirmed issue data for the open focus.
    details: Option<TicketDetails>,
    /// The last repository label catalog.
    labels: Option<TicketLabels>,
    /// True while the label picker is visible.
    label_picker_open: bool,
    /// The selected repository label.
    label_selected: usize,
    /// The open new-label form.
    new_label_form: Option<NewLabelForm>,
    /// The last mutation result.
    result: Option<TicketResult>,
    /// The retained direct edit draft.
    editor: Option<Editor>,
    /// The retained content comparison.
    conflict: Option<TicketConflict>,
    /// True while the comparison is the visible nested view.
    conflict_open: bool,
    /// The transcript and input for the focused issue conversation.
    chat: SessionView,
    /// True while the focused chat input owns the keyboard.
    chat_active: bool,
    /// The current request that applies a shown proposal.
    proposal_request: Option<String>,
    /// The focused issue body, rendered once per update.
    body_lines: Vec<Line<'static>>,
    /// The focused proposal body, rendered once per update.
    proposal_lines: Vec<Line<'static>>,
    /// The mention statuses of the focused body, keyed by scan identity.
    mentions: MentionStatuses,
}

impl Tickets {
    /// True when the focus view is open.
    pub fn focus_open(&self) -> bool {
        self.focus
    }

    /// True while this view owns text input.
    pub fn typing(&self) -> bool {
        self.searching
            || self.editor.as_ref().is_some_and(|editor| editor.open)
            || self.new_label_form.is_some()
            || self.chat_active
    }

    /// True when the open focus has a ticket transcript to poll.
    pub fn needs_poll(&self) -> bool {
        self.focus && self.chat.task_id().is_some()
    }

    /// Follow the focused issue conversation from the daemon state.
    pub fn observe_state(&mut self, state: &StateView) {
        let Some((repo, number)) = self.focus_key.as_ref() else {
            return;
        };
        let id = crate::tasks::ticket_chat_id(repo, *number);
        if let Some(task) = state.tasks.iter().find(|task| task.id == id) {
            self.chat.show(task);
        } else if self.chat.task_id().is_some() {
            self.chat.clear();
        }
    }

    /// Read new ticket chat log data before one draw.
    pub fn on_redraw(&mut self, now: Instant) {
        if self.chat.task_id().is_some() {
            self.chat.on_redraw(now);
        }
    }

    /// Read new ticket chat log data at the session poll interval.
    pub fn poll(&mut self, now: Instant) -> bool {
        self.chat.task_id().is_some() && self.chat.poll(now)
    }

    /// Apply one full issue response.
    pub fn observe_details(&mut self, details: TicketDetails) {
        let current = self
            .focus_key
            .as_ref()
            .is_some_and(|(repo, number)| repo == &details.repo && *number == details.issue.number);
        if current {
            self.details = Some(details);
            self.refresh_body_render();
        }
    }

    /// Rebuild the rendered markdown caches from the current details.
    fn refresh_body_render(&mut self) {
        let Some(details) = self.details.as_ref() else {
            self.body_lines.clear();
            self.proposal_lines.clear();
            return;
        };
        self.body_lines = markdown_lines_with_mentions(&details.issue.body, &self.mentions);
        self.proposal_lines = details
            .proposal
            .as_ref()
            .map(|proposal| markdown_lines_with_mentions(&proposal.body, &self.mentions))
            .unwrap_or_default();
    }

    /// Apply one mention-status response for the open focus.
    ///
    /// The statuses merge into the known set and the body re-renders, so
    /// icons appear without any key press.
    pub fn observe_mentions(&mut self, mentions: TicketMentions) {
        let current = self
            .focus_key
            .as_ref()
            .is_some_and(|(repo, number)| repo == &mentions.repo && *number == mentions.number);
        if !current {
            return;
        }
        for status in mentions.statuses {
            self.mentions
                .insert((status.repo, status.number), status.status);
        }
        self.refresh_body_render();
    }

    /// Apply one label catalog response.
    pub fn observe_labels(&mut self, labels: TicketLabels) {
        let current = self
            .focus_key
            .as_ref()
            .is_some_and(|(repo, _)| repo == &labels.repo);
        if current {
            self.label_selected = self
                .label_selected
                .min(labels.labels.len().saturating_sub(1));
            self.labels = Some(labels);
        }
    }

    /// Apply one ticket mutation response.
    pub fn observe_result(&mut self, result: TicketResult) {
        let current = self
            .focus_key
            .as_ref()
            .is_some_and(|(repo, number)| repo == &result.repo && *number == result.number);
        if !current {
            return;
        }
        if let Some(issue) = result.issue.as_ref() {
            if let Some(details) = self.details.as_mut() {
                details.issue = issue.clone();
            }
            self.refresh_body_render();
        }
        match result.kind {
            TicketResultKind::Conflict => {
                if let Some(conflict) = result.conflict.clone() {
                    self.editor = Some(Editor {
                        original: TicketContent {
                            title: conflict.remote.title.clone(),
                            body: conflict.remote.body.clone(),
                        },
                        title: conflict.pending.title.clone(),
                        body: conflict.pending.body.clone(),
                        field: EditorField::Title,
                        open: false,
                    });
                    self.conflict = Some(conflict);
                    self.conflict_open = true;
                }
            }
            TicketResultKind::Success => {
                if self.proposal_request.as_deref() == Some(result.request.as_str()) {
                    if let Some(details) = self.details.as_mut() {
                        details.proposal = None;
                    }
                    self.refresh_body_render();
                    self.proposal_request = None;
                }
                self.editor = None;
                self.conflict = None;
                self.conflict_open = false;
                self.finish_new_label();
            }
            TicketResultKind::PartialFailure => self.finish_new_label(),
            TicketResultKind::Pending | TicketResultKind::Failure => {}
        }
        self.result = Some(result);
    }

    /// The repositories that own at least one open issue, in tab order.
    ///
    /// The tab order follows the configured alias order. Repositories that
    /// the configuration misses keep their summary order.
    fn tabs(state: &StateView) -> Vec<String> {
        let mut tabs: Vec<String> = Vec::new();
        for repo in state.repos.iter().map(|repo| repo.alias.as_str()) {
            if state.tickets.iter().any(|ticket| ticket.repo == repo)
                && !tabs.iter().any(|tab| tab == repo)
            {
                tabs.push(repo.to_string());
            }
        }
        for repo in state.tickets.iter().map(|ticket| ticket.repo.as_str()) {
            if !tabs.iter().any(|tab| tab == repo) {
                tabs.push(repo.to_string());
            }
        }
        tabs
    }

    /// The repository tab that the list shows.
    fn tab(&self, state: &StateView) -> Option<String> {
        let tabs = Self::tabs(state);
        match self.tab.as_deref() {
            Some(tab) if tabs.iter().any(|current| current == tab) => Some(tab.to_string()),
            _ => tabs.first().cloned(),
        }
    }

    /// Switch to the previous or next repository tab and reset the selection.
    fn switch_tab(&mut self, state: &StateView, step: isize) {
        let tabs = Self::tabs(state);
        if tabs.len() < 2 {
            return;
        }
        let current = self
            .tab
            .as_deref()
            .and_then(|tab| tabs.iter().position(|candidate| candidate == tab))
            .unwrap_or(0) as isize;
        let next = (current + step).rem_euclid(tabs.len() as isize) as usize;
        self.tab = Some(tabs[next].clone());
        self.selected = 0;
    }

    /// The active-tab summaries that match the active search text.
    fn filtered<'a>(&self, state: &'a StateView) -> Vec<&'a TicketSummary> {
        let Some(tab) = self.tab(state) else {
            return Vec::new();
        };
        let query = self.query.trim().to_ascii_lowercase();
        state
            .tickets
            .iter()
            .filter(|ticket| ticket.repo == tab)
            .filter(|ticket| {
                query.is_empty() || {
                    let number = ticket.number.to_string();
                    let marked_number = format!("#{number}");
                    number.contains(&query)
                        || marked_number.contains(&query)
                        || ticket.title.to_ascii_lowercase().contains(&query)
                        || ticket
                            .labels
                            .iter()
                            .any(|label| label.to_ascii_lowercase().contains(&query))
                }
            })
            .collect()
    }

    /// Apply one key and return one daemon action when needed.
    pub fn handle_key(&mut self, state: &StateView, key: KeyEvent) -> Option<Action> {
        if self.focus {
            if self.chat_active {
                if key.code == KeyCode::Esc {
                    self.chat_active = false;
                    return None;
                }
                return self.chat.handle_key(key, 10);
            }
            if self.conflict_open {
                return self.handle_conflict_key(key);
            }
            if self.editor.as_ref().is_some_and(|editor| editor.open) {
                return self.handle_editor_key(key);
            }
            if self.label_picker_open {
                if self.new_label_form.is_some() {
                    return self.handle_new_label_key(key);
                }
                return self.handle_label_key(key);
            }
            match key.code {
                KeyCode::Char('e') => {
                    if let Some(details) = self.details.as_ref() {
                        let content = TicketContent {
                            title: details.issue.title.clone(),
                            body: details.issue.body.clone(),
                        };
                        match self.editor.as_mut() {
                            Some(editor) => editor.open = true,
                            None => {
                                self.editor = Some(Editor {
                                    original: content.clone(),
                                    title: content.title,
                                    body: content.body,
                                    field: EditorField::Title,
                                    open: true,
                                });
                            }
                        }
                    }
                }
                KeyCode::Char('l') => {
                    let (repo, _) = self.focus_key.clone()?;
                    self.label_picker_open = true;
                    self.label_selected = 0;
                    return Some(Action::Ticket(TicketAction::Labels {
                        request: request_code(),
                        repo,
                    }));
                }
                KeyCode::Char('c') => {
                    if self
                        .details
                        .as_ref()
                        .and_then(|details| details.chat_error.as_ref())
                        .is_some()
                    {
                        return None;
                    }
                    let (repo, number) = self.focus_key.clone()?;
                    self.chat_active = true;
                    let id = crate::tasks::ticket_chat_id(&repo, number);
                    if let Some(task) = state.tasks.iter().find(|task| task.id == id) {
                        self.chat.show(task);
                        return None;
                    }
                    return Some(Action::Ticket(TicketAction::Chat {
                        request: request_code(),
                        repo,
                        number,
                    }));
                }
                KeyCode::Char('a') => {
                    let proposal = self.details.as_ref()?.proposal.clone()?;
                    let (repo, number) = self.focus_key.clone()?;
                    let request = request_code();
                    self.proposal_request = Some(request.clone());
                    return Some(Action::Ticket(TicketAction::UpdateContent {
                        request,
                        repo,
                        number,
                        expected: TicketContent {
                            title: proposal.original_title,
                            body: proposal.original_body,
                        },
                        desired: TicketContent {
                            title: proposal.title,
                            body: proposal.body,
                        },
                        source: TicketContentSource::Proposal {
                            proposal_id: proposal.id,
                        },
                    }));
                }
                KeyCode::Esc => {
                    self.focus = false;
                    self.chat_active = false;
                }
                _ => {}
            }
            return None;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.selected = 0;
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.selected = 0;
                }
                KeyCode::Char(character) => {
                    self.query.push(character);
                    self.selected = 0;
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('h') | KeyCode::Left => self.switch_tab(state, -1),
            KeyCode::Char('l') | KeyCode::Right => self.switch_tab(state, 1),
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.filtered(state).len();
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                let selected = self.filtered(state).get(self.selected).copied().cloned()?;
                let key = (selected.repo.clone(), selected.number);
                if self.focus_key.as_ref() != Some(&key) {
                    self.clear_issue_state();
                }
                self.focus = true;
                self.focus_key = Some(key);
                self.details = None;
                self.result = None;
                return Some(Action::Ticket(TicketAction::Details {
                    request: request_code(),
                    repo: selected.repo,
                    number: selected.number,
                }));
            }
            _ => {}
        }
        None
    }

    /// Clear all nested data that belongs to the previous focused issue.
    fn clear_issue_state(&mut self) {
        self.details = None;
        self.labels = None;
        self.label_picker_open = false;
        self.label_selected = 0;
        self.new_label_form = None;
        self.result = None;
        self.editor = None;
        self.conflict = None;
        self.conflict_open = false;
        self.chat.clear();
        self.chat_active = false;
        self.proposal_request = None;
        self.body_lines.clear();
        self.proposal_lines.clear();
        self.mentions.clear();
    }

    /// Apply one key inside the direct editor.
    fn handle_editor_key(&mut self, key: KeyEvent) -> Option<Action> {
        let editor = self.editor.as_mut()?;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let (repo, number) = self.focus_key.clone()?;
            let expected = editor.original.clone();
            let desired = TicketContent {
                title: editor.title.clone(),
                body: editor.body.clone(),
            };
            return Some(Action::Ticket(TicketAction::UpdateContent {
                request: request_code(),
                repo,
                number,
                expected,
                desired,
                source: TicketContentSource::Direct,
            }));
        }
        match key.code {
            KeyCode::Esc => editor.open = false,
            KeyCode::Tab => {
                editor.field = match editor.field {
                    EditorField::Title => EditorField::Body,
                    EditorField::Body => EditorField::Title,
                };
            }
            KeyCode::Backspace => match editor.field {
                EditorField::Title => {
                    editor.title.pop();
                }
                EditorField::Body => {
                    editor.body.pop();
                }
            },
            KeyCode::Enter if matches!(editor.field, EditorField::Body) => editor.body.push('\n'),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match editor.field {
                    EditorField::Title => editor.title.push(character),
                    EditorField::Body => editor.body.push(character),
                }
            }
            _ => {}
        }
        None
    }

    /// Apply one key inside the conflict comparison.
    fn handle_conflict_key(&mut self, key: KeyEvent) -> Option<Action> {
        let conflict = self.conflict.clone()?;
        match key.code {
            KeyCode::Char('g') => {
                if let Some(details) = self.details.as_mut() {
                    details.issue = conflict.remote;
                }
                self.refresh_body_render();
                self.editor = None;
                self.conflict = None;
                self.conflict_open = false;
                None
            }
            KeyCode::Char('p') => {
                let (repo, number) = self.focus_key.clone()?;
                let request = request_code();
                if matches!(&conflict.source, TicketContentSource::Proposal { .. }) {
                    self.proposal_request = Some(request.clone());
                }
                Some(Action::Ticket(TicketAction::UpdateContent {
                    request,
                    repo,
                    number,
                    expected: TicketContent {
                        title: conflict.remote.title,
                        body: conflict.remote.body,
                    },
                    desired: conflict.pending,
                    source: conflict.source,
                }))
            }
            KeyCode::Esc => {
                self.conflict_open = false;
                None
            }
            _ => None,
        }
    }

    /// Apply one key inside the label picker.
    fn handle_label_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => {
                self.label_picker_open = false;
                None
            }
            KeyCode::Char('n') => {
                self.new_label_form = Some(NewLabelForm::default());
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.labels.as_ref()?.labels.len();
                self.label_selected = (self.label_selected + 1).min(count.saturating_sub(1));
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.label_selected = self.label_selected.saturating_sub(1);
                None
            }
            KeyCode::Char(' ') => {
                let labels = self.labels.as_ref()?;
                let label = labels.labels.get(self.label_selected)?.name.clone();
                let details = self.details.as_ref()?;
                let on = !details.issue.labels.iter().any(|current| current == &label);
                let (repo, number) = self.focus_key.clone()?;
                Some(Action::Ticket(TicketAction::ToggleLabel {
                    request: request_code(),
                    repo,
                    number,
                    label,
                    on,
                }))
            }
            _ => None,
        }
    }

    /// Apply one key inside the new-label form.
    fn handle_new_label_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let form = self.new_label_form.as_mut()?;
            let name = form.name.trim().to_string();
            if name.is_empty() {
                form.error = Some("The label name must not be empty.".to_string());
                return None;
            }
            let color = match crate::ticket::normalize_label_color(&form.color) {
                Ok(color) => color,
                Err(error) => {
                    form.error = Some(error);
                    return None;
                }
            };
            let (repo, number) = self.focus_key.clone()?;
            return Some(Action::Ticket(TicketAction::CreateLabel {
                request: request_code(),
                repo,
                number,
                name,
                color,
            }));
        }

        let form = self.new_label_form.as_mut()?;
        match key.code {
            KeyCode::Esc => self.new_label_form = None,
            KeyCode::Tab => {
                form.field = match form.field {
                    NewLabelField::Name => NewLabelField::Color,
                    NewLabelField::Color => NewLabelField::Name,
                };
                form.error = None;
            }
            KeyCode::Backspace => {
                match form.field {
                    NewLabelField::Name => {
                        form.name.pop();
                    }
                    NewLabelField::Color => {
                        form.color.pop();
                    }
                }
                form.error = None;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match form.field {
                    NewLabelField::Name => form.name.push(character),
                    NewLabelField::Color => form.color.push(character),
                }
                form.error = None;
            }
            _ => {}
        }
        None
    }

    /// Add the created label to the visible catalog and close its form.
    fn finish_new_label(&mut self) {
        let Some(form) = self.new_label_form.take() else {
            return;
        };
        let Ok(color) = crate::ticket::normalize_label_color(&form.color) else {
            return;
        };
        let name = form.name.trim().to_string();
        let Some(catalog) = self.labels.as_mut() else {
            return;
        };
        if !catalog
            .labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(&name))
        {
            catalog.labels.push(crate::sock::RepoLabel { name, color });
            catalog
                .labels
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
    }

    /// Draw the list or the focused issue.
    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        if self.focus {
            self.draw_focus(frame, area, state);
        } else {
            self.draw_list(frame, area, state);
        }
    }

    /// Draw grouped ticket rows with one tab row and one search line.
    fn draw_list(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        let block = Block::bordered().title(" tickets // open ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let query = if self.searching {
            format!(" /{}▌", self.query)
        } else if self.query.is_empty() {
            " / search".to_string()
        } else {
            format!(" / {}", self.query)
        };
        let tabbed = Self::tabs(state).len() > 1;
        let mut constraints = vec![Constraint::Length(1)];
        if tabbed {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));
        let rows = Layout::vertical(constraints).split(inner);
        frame.render_widget(Paragraph::new(query).style(THEME.dim()), rows[0]);
        if tabbed {
            self.draw_tabs(frame, rows[1], state);
        }
        let list = rows[rows.len() - 1];

        let filtered = self.filtered(state);
        if filtered.is_empty() {
            frame.render_widget(
                Paragraph::new("No open ticket matches the search.").style(THEME.dim()),
                list,
            );
            return;
        }
        let mut lines = Vec::new();
        let mut previous = None;
        for (ticket_index, ticket) in filtered.into_iter().enumerate() {
            if previous != Some(ticket.group) {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(group_line(ticket.group));
                previous = Some(ticket.group);
            }
            let selected = ticket_index == self.selected;
            let marker = if selected { "›" } else { " " };
            let mut spans = vec![
                Span::styled(
                    format!("{marker} {} ", ticket.repo),
                    Style::default().fg(THEME.repo).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("#{} ", ticket.number), THEME.dim()),
                Span::raw(ticket.title.clone()),
            ];
            if !ticket.labels.is_empty() {
                spans.push(Span::styled(
                    format!("  [{}]", ticket.labels.join(" · ")),
                    THEME.dim(),
                ));
            }
            let mut line = Line::from(spans);
            if selected {
                line = line.style(
                    Style::default()
                        .fg(THEME.accent)
                        .bg(THEME.selected_bg)
                        .add_modifier(Modifier::BOLD),
                );
            }
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), list);
    }

    /// Draw one tab per repository that owns open issues.
    fn draw_tabs(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        let active = self.tab(state);
        let mut spans = Vec::new();
        for (index, tab) in Self::tabs(state).iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" │ ", THEME.dim()));
            }
            let count = state
                .tickets
                .iter()
                .filter(|ticket| ticket.repo == *tab)
                .count();
            let style = if active.as_deref() == Some(tab.as_str()) {
                Style::default()
                    .fg(THEME.accent)
                    .bg(THEME.selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                THEME.dim()
            };
            spans.push(Span::styled(format!(" {tab} {count} "), style));
        }
        spans.push(Span::styled("  h l repo", THEME.dim()));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Draw the issue data and the configured ticket chat pane.
    fn draw_focus(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        if self.conflict_open {
            self.draw_conflict(frame, area);
            return;
        }
        if self.editor.as_ref().is_some_and(|editor| editor.open) {
            self.draw_editor(frame, area);
            return;
        }
        if self.label_picker_open {
            if self.new_label_form.is_some() {
                self.draw_new_label(frame, area);
                return;
            }
            self.draw_labels(frame, area);
            return;
        }
        let panes = if area.width >= 104 {
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(area)
        } else {
            Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)]).split(area)
        };
        let repo = self
            .focus_key
            .as_ref()
            .map_or("", |(repo, _)| repo.as_str());
        let harness = ticket_chat_harness(state, repo);
        let details = match self.details.as_ref() {
            Some(details) => {
                let issue = &details.issue;
                let assignees = if issue.assignees.is_empty() {
                    "none".to_string()
                } else {
                    issue.assignees.join(", ")
                };
                let labels = if issue.labels.is_empty() {
                    "none".to_string()
                } else {
                    issue.labels.join(" · ")
                };
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!("{}  #{}", details.repo, issue.number),
                        Style::default().fg(THEME.repo).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        issue.title.clone(),
                        Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                lines.extend(self.body_lines.iter().cloned());
                lines.extend([
                    Line::from(""),
                    Line::from(format!("Labels: {labels}")),
                    Line::from(format!("Author: {}", issue.author)),
                    Line::from(format!("Assignees: {assignees}")),
                    Line::from(format!("Updated: {}", issue.updated_at)),
                    Line::from(format!("GitHub: {}", issue.github_url)),
                ]);
                if let Some(proposal) = details.proposal.as_ref() {
                    lines.extend([
                        Line::from(""),
                        Line::from(Span::styled(
                            format!("◇ {harness} proposal · a apply"),
                            Style::default()
                                .fg(THEME.accent)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Line::from(format!("Title: {}", proposal.title)),
                    ]);
                    lines.extend(self.proposal_lines.iter().cloned());
                }
                lines.push(result_line(self.result.as_ref()));
                lines
            }
            None => vec![Line::from(Span::styled(
                "Loading the confirmed ticket data…",
                THEME.dim(),
            ))],
        };
        frame.render_widget(
            Paragraph::new(details)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::bordered().title(" ticket // esc back ")),
            panes[0],
        );
        let (chat_title, chat_ready) = ticket_chat_identity(state, repo);
        let chat_block = Block::bordered().title(format!(" {chat_title} "));
        let chat_inner = chat_block.inner(panes[1]);
        frame.render_widget(chat_block, panes[1]);
        if self.chat.task_id().is_some() {
            self.chat.draw(frame, chat_inner, &[]);
        } else {
            let chat_text = self
                .details
                .as_ref()
                .and_then(|details| details.chat_error.as_deref())
                .map_or_else(
                    || {
                        if self.chat_active {
                            "… pending: the configured chat session starts.".to_string()
                        } else {
                            format!("{chat_ready}\n\nc  start or resume chat")
                        }
                    },
                    |error| format!("× chat needs configuration.\n\n{error}"),
                );
            frame.render_widget(
                Paragraph::new(chat_text).wrap(ratatui::widgets::Wrap { trim: false }),
                chat_inner,
            );
        }
    }

    /// Draw the direct title and description editor.
    fn draw_editor(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let rows = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);
        let title_style = if matches!(editor.field, EditorField::Title) {
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.text)
        };
        let body_style = if matches!(editor.field, EditorField::Body) {
            Style::default().fg(THEME.accent)
        } else {
            Style::default().fg(THEME.text)
        };
        frame.render_widget(
            Paragraph::new(editor.title.clone())
                .style(title_style)
                .block(Block::bordered().title(" title ")),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(editor.body.clone())
                .style(body_style)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::bordered().title(" description ")),
            rows[1],
        );
        frame.render_widget(
            Paragraph::new("tab field · ctrl-s save · esc ticket").style(THEME.dim()),
            rows[2],
        );
    }

    /// Draw the remote and pending versions in one comparison.
    fn draw_conflict(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(conflict) = self.conflict.as_ref() else {
            return;
        };
        let panes = if area.width >= 104 {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
        };
        let remote = format!("{}\n\n{}", conflict.remote.title, conflict.remote.body);
        let pending = format!("{}\n\n{}", conflict.pending.title, conflict.pending.body);
        frame.render_widget(
            Paragraph::new(remote)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::bordered().title(" g GitHub version ")),
            panes[0],
        );
        frame.render_widget(
            Paragraph::new(pending)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::bordered().title(" p pending version ")),
            panes[1],
        );
    }

    /// Draw the repository label catalog and confirmed issue state.
    fn draw_labels(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(catalog) = self.labels.as_ref() else {
            frame.render_widget(
                Paragraph::new("Loading repository labels…")
                    .style(THEME.dim())
                    .block(Block::bordered().title(" labels // esc ticket ")),
                area,
            );
            return;
        };
        let applied = self
            .details
            .as_ref()
            .map(|details| details.issue.labels.as_slice())
            .unwrap_or(&[]);
        let mut lines = Vec::new();
        if let Some(error) = catalog.error.as_deref() {
            lines.push(Line::from(Span::styled(
                format!("× catalog failure: {error}"),
                Style::default().fg(THEME.error),
            )));
            lines.push(Line::from(""));
        }
        for (index, label) in catalog.labels.iter().enumerate() {
            let is_applied = applied.iter().any(|current| current == &label.name);
            let state = if is_applied {
                "✓ applied"
            } else {
                "○ absent"
            };
            let marker = if index == self.label_selected {
                "›"
            } else {
                " "
            };
            let mut line = Line::from(vec![
                Span::styled(
                    format!("{marker} {state} "),
                    Style::default().fg(if is_applied { THEME.ok } else { THEME.dim }),
                ),
                Span::styled(
                    label.name.clone(),
                    Style::default().fg(repo_label_color(&label.color)),
                ),
                Span::styled(format!("  #{}", label.color), THEME.dim()),
            ]);
            if index == self.label_selected {
                line = line.style(THEME.selected());
            }
            lines.push(line);
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "No repository labels are available.",
                THEME.dim(),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(" labels // space toggle · n new · esc ticket ")),
            area,
        );
    }

    /// Draw the repository label creation form.
    fn draw_new_label(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(form) = self.new_label_form.as_ref() else {
            return;
        };
        let rows = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(area);
        let name_style = if matches!(form.field, NewLabelField::Name) {
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.text)
        };
        let color_style = if matches!(form.field, NewLabelField::Color) {
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.text)
        };
        frame.render_widget(
            Paragraph::new(form.name.clone())
                .style(name_style)
                .block(Block::bordered().title(" label name ")),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(form.color.clone())
                .style(color_style)
                .block(Block::bordered().title(" color // RRGGBB ")),
            rows[1],
        );
        let status = form
            .error
            .as_deref()
            .unwrap_or("tab field · ctrl-s create and attach · esc labels");
        let color = if form.error.is_some() {
            THEME.error
        } else {
            THEME.dim
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(color)),
            rows[2],
        );
    }
}

/// Convert a repository label color to one terminal true color.
fn repo_label_color(color: &str) -> Color {
    if color.len() != 6 || !color.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return THEME.text;
    }
    let red = u8::from_str_radix(&color[0..2], 16).unwrap_or_default();
    let green = u8::from_str_radix(&color[2..4], 16).unwrap_or_default();
    let blue = u8::from_str_radix(&color[4..6], 16).unwrap_or_default();
    Color::Rgb(red, green, blue)
}

/// Create one globally unique ticket request code.
fn request_code() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The word and symbol for one mutation result.
fn result_line(result: Option<&TicketResult>) -> Line<'static> {
    let Some(result) = result else {
        return Line::from("");
    };
    let (symbol, word, color) = match result.kind {
        TicketResultKind::Pending => ("…", "pending", THEME.warn),
        TicketResultKind::Success => ("✓", "success", THEME.ok),
        TicketResultKind::Conflict => ("!", "conflict", THEME.warn),
        TicketResultKind::PartialFailure => ("!", "partial failure", THEME.error),
        TicketResultKind::Failure => ("×", "failure", THEME.error),
    };
    Line::from(vec![
        Span::styled(format!("{symbol} {word}: "), Style::default().fg(color)),
        Span::raw(result.message.clone()),
    ])
}

fn ticket_chat_identity(state: &StateView, repo: &str) -> (String, String) {
    let harness = ticket_chat_harness(state, repo);
    (
        format!("{harness} // configured access"),
        format!("The configured {harness} role is ready for ticket analysis."),
    )
}

fn ticket_chat_harness<'a>(state: &'a StateView, repo: &str) -> &'a str {
    let settings = state
        .settings
        .repositories
        .iter()
        .find(|value| {
            value.repository == repo && value.role == crate::config::ExecutionRole::TicketChat
        })
        .map(|value| &value.settings)
        .or_else(|| {
            state
                .settings
                .global
                .iter()
                .find(|value| value.role == crate::config::ExecutionRole::TicketChat)
                .map(|value| &value.settings)
        });
    settings.map_or("chat", |settings| settings.harness.program())
}

/// The word and symbol for one workflow group.
fn group_line(group: TicketGroup) -> Line<'static> {
    let (symbol, word, color) = match group {
        TicketGroup::Untouched => ("○", "untouched", THEME.text),
        TicketGroup::ToRefine => ("◇", "to-refine", THEME.warn),
        TicketGroup::Refined => ("◆", "refined", THEME.ok),
    };
    Line::from(vec![
        Span::styled(format!(" {symbol} "), Style::default().fg(color)),
        Span::styled(
            word,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sock::{
        MentionStatus, PausedView, StateView, TicketContent, TicketContentSource, TicketGroup,
        TicketMentions, TicketSummary,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn state() -> StateView {
        let mut state = StateView {
            protocol_revision: crate::sock::WIRE_PROTOCOL_REVISION,
            links: Vec::new(),
            repos: Vec::new(),
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions: Vec::new(),
            decision_items: Vec::new(),
            tickets: vec![
                TicketSummary {
                    repo: "borsuk".to_string(),
                    number: 7,
                    title: "Improve the ticket list".to_string(),
                    labels: vec!["ui".to_string()],
                    updated_at: "2026-08-30T12:00:00Z".to_string(),
                    group: TicketGroup::Untouched,
                },
                TicketSummary {
                    repo: "qubitsok".to_string(),
                    number: 42,
                    title: "Refine analysis".to_string(),
                    labels: vec!["to-refine".to_string(), "analysis".to_string()],
                    updated_at: "2026-08-29T12:00:00Z".to_string(),
                    group: TicketGroup::ToRefine,
                },
            ],
            trains: Vec::new(),
            paused: PausedView {
                global: false,
                overrides: Vec::new(),
            },
            settings: crate::sock::SettingsView::default(),
        };
        state
            .settings
            .global
            .push(ticket_chat_role(crate::config::Harness::Claude));
        state
    }

    fn ticket_chat_role(harness: crate::config::Harness) -> crate::sock::GlobalRoleSettingsView {
        crate::sock::GlobalRoleSettingsView {
            role: crate::config::ExecutionRole::TicketChat,
            settings: crate::config::RoleSettings {
                harness,
                program: harness.program().to_string(),
                model: "model".to_string(),
                effort: None,
                extra_args: Vec::new(),
                agent: None,
                profile: None,
                permission_mode: (harness == crate::config::Harness::Claude)
                    .then(|| "manual".to_string()),
                permission_handler: None,
                tools: if harness == crate::config::Harness::Claude {
                    vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()]
                } else {
                    Vec::new()
                },
                disallowed_tools: Vec::new(),
                strict_mcp: None,
                auto_approve: None,
                approval_policy: None,
                sandbox: None,
            },
            limit: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn details() -> TicketDetails {
        TicketDetails {
            request: "ticket-1".to_string(),
            repo: "borsuk".to_string(),
            issue: crate::model::Issue {
                number: 7,
                node_id: "node-7".to_string(),
                title: "Improve the ticket list".to_string(),
                body: "Show every issue without leaving the terminal.".to_string(),
                labels: vec!["ui".to_string(), "to-refine".to_string()],
                author: "piotr".to_string(),
                assignees: vec!["owner".to_string()],
                updated_at: "2026-08-30T12:00:00Z".to_string(),
                github_url: "https://github.com/acme/borsuk/issues/7".to_string(),
                open: true,
            },
            proposal: None,
            chat_error: None,
        }
    }

    fn ticket_task(repo: &str, number: u64) -> crate::sock::TaskView {
        crate::sock::TaskView {
            id: crate::tasks::ticket_chat_id(repo, number),
            repo: repo.to_string(),
            stage: crate::model::Stage::Refine,
            kind: crate::model::ItemKind::Issue,
            number,
            state: crate::tasks::TaskState::AwaitingUser,
            attempt: 1,
            log_path: format!("/tmp/{repo}-ticket-{number}.jsonl").into(),
            input: crate::sock::InputMode::Live,
            queued_messages: 0,
        }
    }

    fn draw_focus(width: u16) -> Vec<String> {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        let backend = TestBackend::new(width, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| tickets.draw(frame, frame.area(), &state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn enter_opens_the_selected_issue_and_requests_its_details() {
        let mut tickets = Tickets::default();
        let action = tickets.handle_key(&state(), key(KeyCode::Enter));

        let Some(crate::sock::Action::Ticket(crate::sock::TicketAction::Details {
            request,
            repo,
            number,
        })) = action
        else {
            panic!("enter must request ticket details");
        };
        assert!(!request.is_empty());
        assert_eq!(repo, "borsuk");
        assert_eq!(number, 7);
        assert!(tickets.focus_open());
    }

    #[test]
    fn request_codes_are_unique_across_terminal_clients() {
        let action = |mut tickets: Tickets| {
            let Some(Action::Ticket(TicketAction::Details { request, .. })) =
                tickets.handle_key(&state(), key(KeyCode::Enter))
            else {
                panic!("enter must request ticket details");
            };
            request
        };

        assert_ne!(action(Tickets::default()), action(Tickets::default()));
    }

    #[test]
    fn search_matches_number_title_and_label_text_inside_the_active_tab() {
        let state = state();
        for query in ["#7", "ticket list", "ui"] {
            let tickets = Tickets {
                query: query.to_string(),
                ..Tickets::default()
            };
            assert_eq!(tickets.filtered(&state).len(), 1, "query {query}");
        }
        for query in ["#42", "analysis", "borsuk"] {
            let tickets = Tickets {
                query: query.to_string(),
                tab: Some("borsuk".to_string()),
                ..Tickets::default()
            };
            assert!(
                tickets.filtered(&state).is_empty(),
                "query {query} left its tab"
            );
        }
    }

    /// Open the focus, apply one detail body, and render the screen text.
    fn focus_screen(detail: TicketDetails, mentions: Option<TicketMentions>) -> String {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(detail);
        if let Some(mentions) = mentions {
            tickets.observe_mentions(mentions);
        }
        let backend = TestBackend::new(104, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| tickets.draw(frame, frame.area(), &state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol())
            .collect()
    }

    fn mention(number: u64, status: MentionStatus) -> crate::sock::TicketMentionStatus {
        crate::sock::TicketMentionStatus {
            repo: None,
            number,
            status,
        }
    }

    #[test]
    fn mention_statuses_draw_icons_before_known_mentions_only() {
        let mut detail = details();
        detail.issue.body = "Depends on #8 and tracks #9, misses #10.".to_string();
        let mentions = TicketMentions {
            request: "m1".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            statuses: vec![
                mention(8, MentionStatus::ClosedIssue),
                mention(9, MentionStatus::OpenIssue),
            ],
        };

        let screen = focus_screen(detail, Some(mentions));

        assert!(screen.contains("○ #8"), "screen: {screen}");
        assert!(screen.contains("● #9"), "screen: {screen}");
        assert!(
            !screen.contains("#10 ●") && !screen.contains("● #10") && !screen.contains("○ #10"),
            "an unknown mention must stay plain: {screen}"
        );
    }

    #[test]
    fn a_cross_repo_mention_decorates_from_its_canonical_key() {
        let mut detail = details();
        detail.issue.body = "Needs other/repo#5 too.".to_string();
        let mentions = TicketMentions {
            request: "m4".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            statuses: vec![crate::sock::TicketMentionStatus {
                repo: Some("other/repo".to_string()),
                number: 5,
                status: MentionStatus::OpenIssue,
            }],
        };

        let screen = focus_screen(detail, Some(mentions));

        assert!(screen.contains("● other/repo#5"), "screen: {screen}");
    }

    #[test]
    fn a_proposal_body_decorates_its_mentions_too() {
        let mut detail = details();
        detail.proposal = Some(crate::sock::TicketProposal {
            id: "p1".to_string(),
            title: "Proposal".to_string(),
            body: "See #9 for the live check.".to_string(),
            original_title: "Improve the ticket list".to_string(),
            original_body: "Show every issue without leaving the terminal.".to_string(),
        });
        let mentions = TicketMentions {
            request: "m2".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            statuses: vec![mention(9, MentionStatus::OpenIssue)],
        };

        let screen = focus_screen(detail, Some(mentions));

        assert!(screen.contains("● #9"), "screen: {screen}");
    }

    #[test]
    fn a_mention_push_for_another_issue_never_decorates_the_focus() {
        let detail = details();
        let mentions = TicketMentions {
            request: "m3".to_string(),
            repo: "borsuk".to_string(),
            number: 42,
            statuses: vec![mention(9, MentionStatus::OpenIssue)],
        };

        let screen = focus_screen(detail, Some(mentions));

        assert!(!screen.contains("●"), "screen: {screen}");
    }

    #[test]
    fn h_and_l_switch_repo_tabs_and_limit_the_list_to_one_tab() {
        let mut state = state();
        state.tickets.insert(
            1,
            TicketSummary {
                repo: "borsuk".to_string(),
                number: 8,
                title: "Second borsuk issue".to_string(),
                labels: Vec::new(),
                updated_at: "2026-08-31T12:00:00Z".to_string(),
                group: TicketGroup::Untouched,
            },
        );
        let mut tickets = Tickets::default();
        assert_eq!(tickets.tab(&state).as_deref(), Some("borsuk"));

        tickets.handle_key(&state, key(KeyCode::Char('j')));
        assert_eq!(tickets.selected, 1);

        tickets.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(tickets.tab(&state).as_deref(), Some("qubitsok"));

        tickets.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(tickets.tab(&state).as_deref(), Some("borsuk"), "l wraps");

        tickets.handle_key(&state, key(KeyCode::Char('h')));
        assert_eq!(tickets.tab(&state).as_deref(), Some("qubitsok"), "h wraps");
        assert_eq!(tickets.selected, 0, "a tab switch resets the selection");

        let action = tickets.handle_key(&state, key(KeyCode::Enter));
        let Some(Action::Ticket(TicketAction::Details { repo, number, .. })) = action else {
            panic!("enter must open the issue of the active tab");
        };
        assert_eq!(repo, "qubitsok");
        assert_eq!(number, 42);
    }

    #[test]
    fn a_tab_switch_keeps_the_search_text() {
        let state = state();
        let mut tickets = Tickets {
            query: "analysis".to_string(),
            ..Tickets::default()
        };
        assert!(tickets.filtered(&state).is_empty());

        tickets.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(tickets.query, "analysis");
        assert_eq!(tickets.filtered(&state).len(), 1);
    }

    #[test]
    fn body_render_renders_markdown_once_per_update() {
        let mut tickets = Tickets::default();
        tickets.handle_key(&state(), key(KeyCode::Enter));

        let mut incoming = details();
        incoming.issue.body = "## Heading\n\n- [ ] step".to_string();
        incoming.proposal = Some(crate::sock::TicketProposal {
            id: "proposal-1".to_string(),
            title: "Rename".to_string(),
            body: "**bold** plan".to_string(),
            original_title: "Improve the ticket list".to_string(),
            original_body: "Show every issue without leaving the terminal.".to_string(),
        });
        tickets.observe_details(incoming);

        let text_of = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(text_of(&tickets.body_lines[0]), "Heading");
        assert!(tickets.body_lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(text_of(&tickets.proposal_lines[0]), "bold plan");
        assert!(tickets.proposal_lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));

        let mut updated = details();
        updated.issue.body = "Replaced body".to_string();
        tickets.observe_result(TicketResult {
            request: "request-1".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            kind: TicketResultKind::Success,
            message: "saved".to_string(),
            issue: Some(updated.issue),
            conflict: None,
        });
        assert_eq!(
            tickets.body_lines.iter().map(text_of).collect::<Vec<_>>(),
            vec!["Replaced body".to_string()]
        );
    }

    #[test]
    fn focus_renders_all_details_in_a_wide_split_and_a_narrow_stack() {
        let wide = draw_focus(120);
        let narrow = draw_focus(80);
        for screen in [&wide, &narrow] {
            let text = screen.join("\n");
            for required in [
                "Improve the ticket list",
                "Show every issue",
                "Labels: ui · to-refine",
                "Author: piotr",
                "Assignees: owner",
                "2026-08-30T12:00:00Z",
                "https://github.com/acme/borsuk/issues/7",
            ] {
                assert!(text.contains(required), "missing {required}:\n{text}");
            }
        }
        let wide_chat_row = wide
            .iter()
            .position(|row| row.contains("claude // configured access"))
            .unwrap();
        let narrow_chat_row = narrow
            .iter()
            .position(|row| row.contains("claude // configured access"))
            .unwrap();
        assert_eq!(wide_chat_row, 0, "the wide view splits side by side");
        assert!(narrow_chat_row > 10, "the narrow view stacks the chat pane");
    }

    #[test]
    fn ticket_chat_titles_follow_each_selected_harness() {
        for harness in [
            crate::config::Harness::Claude,
            crate::config::Harness::Opencode,
            crate::config::Harness::Codex,
        ] {
            let mut state = state();
            state.settings.global = vec![ticket_chat_role(harness)];
            let (title, message) = ticket_chat_identity(&state, "borsuk");
            assert_eq!(title, format!("{} // configured access", harness.program()));
            assert!(message.contains(harness.program()));
            assert!(!message.contains("read-only"));
        }
        let mut writable = state();
        writable.settings.global[0].settings.tools = vec!["Read".to_string(), "Bash".to_string()];
        let (title, message) = ticket_chat_identity(&writable, "borsuk");
        assert_eq!(title, "claude // configured access");
        assert!(!message.contains("read-only"));
    }

    #[test]
    fn editor_ctrl_s_sends_one_conflict_protected_content_update() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        tickets.handle_key(&state, key(KeyCode::Char('e')));
        let editor = tickets.editor.as_mut().expect("e must open the editor");
        editor.title = "A direct title".to_string();
        editor.body = "A direct description".to_string();

        let action = tickets.handle_key(
            &state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        let Some(Action::Ticket(TicketAction::UpdateContent {
            expected,
            desired,
            source,
            ..
        })) = action
        else {
            panic!("ctrl-s must send one content update");
        };
        assert_eq!(expected.title, "Improve the ticket list");
        assert_eq!(
            expected.body,
            "Show every issue without leaving the terminal."
        );
        assert_eq!(desired.title, "A direct title");
        assert_eq!(desired.body, "A direct description");
        assert_eq!(source, TicketContentSource::Direct);
    }

    #[test]
    fn conflict_g_keeps_github_and_p_reapplies_the_pending_version() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        let mut remote = details().issue;
        remote.title = "Remote title".to_string();
        remote.body = "Remote description".to_string();
        let pending = TicketContent {
            title: "Pending title".to_string(),
            body: "Pending description".to_string(),
        };
        tickets.observe_result(TicketResult {
            request: "save-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            kind: crate::sock::TicketResultKind::Conflict,
            message: "GitHub content changed.".to_string(),
            issue: None,
            conflict: Some(crate::sock::TicketConflict {
                remote: remote.clone(),
                pending: pending.clone(),
                source: TicketContentSource::Direct,
            }),
        });

        let reapply = tickets.handle_key(&state, key(KeyCode::Char('p')));
        let Some(Action::Ticket(TicketAction::UpdateContent {
            expected, desired, ..
        })) = reapply
        else {
            panic!("p must reapply the pending version");
        };
        assert_eq!(expected.title, "Remote title");
        assert_eq!(desired, pending);

        tickets.observe_result(TicketResult {
            request: "save-8".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            kind: crate::sock::TicketResultKind::Conflict,
            message: "GitHub content changed again.".to_string(),
            issue: None,
            conflict: Some(crate::sock::TicketConflict {
                remote: remote.clone(),
                pending: TicketContent {
                    title: "Pending title".to_string(),
                    body: "Pending description".to_string(),
                },
                source: TicketContentSource::Direct,
            }),
        });
        assert!(tickets
            .handle_key(&state, key(KeyCode::Char('g')))
            .is_none());
        assert_eq!(
            tickets.details.as_ref().unwrap().issue.title,
            "Remote title"
        );
        assert!(tickets.conflict.is_none());
    }

    #[test]
    fn label_picker_space_sends_one_immediate_toggle() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());

        let catalog_action = tickets.handle_key(&state, key(KeyCode::Char('l')));
        assert!(matches!(
            catalog_action,
            Some(Action::Ticket(TicketAction::Labels { ref repo, .. })) if repo == "borsuk"
        ));
        tickets.observe_labels(TicketLabels {
            request: "labels-1".to_string(),
            repo: "borsuk".to_string(),
            labels: vec![crate::sock::RepoLabel {
                name: "ui".to_string(),
                color: "55e6ff".to_string(),
            }],
            error: None,
        });

        let toggle = tickets.handle_key(&state, key(KeyCode::Char(' ')));
        let Some(Action::Ticket(TicketAction::ToggleLabel {
            repo,
            number,
            label,
            on,
            ..
        })) = toggle
        else {
            panic!("space must apply one label toggle");
        };
        assert_eq!(repo, "borsuk");
        assert_eq!(number, 7);
        assert_eq!(label, "ui");
        assert!(!on, "an applied label must be removed");
    }

    #[test]
    fn repository_label_color_uses_the_catalog_hex_value() {
        assert_eq!(
            repo_label_color("55e6ff"),
            ratatui::style::Color::Rgb(0x55, 0xe6, 0xff)
        );
        assert_eq!(repo_label_color("not-a-color"), THEME.text);
    }

    #[test]
    fn only_an_open_ticket_chat_requests_a_log_poll() {
        let mut state = state();
        state.tasks.push(ticket_task("borsuk", 7));
        let mut tickets = Tickets::default();
        assert!(!tickets.needs_poll());

        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_state(&state);
        assert!(tickets.needs_poll());

        tickets.handle_key(&state, key(KeyCode::Esc));
        assert!(!tickets.needs_poll());
    }

    #[test]
    fn opening_another_issue_clears_the_previous_issue_state() {
        let mut state = state();
        state.tasks.push(ticket_task("borsuk", 7));
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        tickets.observe_state(&state);
        tickets.observe_labels(TicketLabels {
            request: "labels-7".to_string(),
            repo: "borsuk".to_string(),
            labels: vec![crate::sock::RepoLabel {
                name: "ui".to_string(),
                color: "55e6ff".to_string(),
            }],
            error: None,
        });
        tickets.handle_key(&state, key(KeyCode::Char('e')));
        tickets.handle_key(&state, key(KeyCode::Char('X')));
        tickets.handle_key(&state, key(KeyCode::Esc));
        tickets.handle_key(&state, key(KeyCode::Esc));

        tickets.handle_key(&state, key(KeyCode::Char('l')));
        tickets.handle_key(&state, key(KeyCode::Enter));

        assert_eq!(tickets.focus_key, Some(("qubitsok".to_string(), 42)));
        assert!(tickets.editor.is_none());
        assert!(tickets.labels.is_none());
        assert!(tickets.chat.task_id().is_none());
    }

    #[test]
    fn a_removed_ticket_task_clears_the_focused_chat() {
        let mut state = state();
        state.tasks.push(ticket_task("borsuk", 7));
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_state(&state);
        assert_eq!(tickets.chat.task_id(), Some("borsuk/ticket-i7"));

        state.tasks.clear();
        tickets.observe_state(&state);

        assert!(tickets.chat.task_id().is_none());
    }

    #[test]
    fn reopening_the_same_issue_keeps_its_pending_editor() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        tickets.handle_key(&state, key(KeyCode::Char('e')));
        tickets.handle_key(&state, key(KeyCode::Char('X')));
        tickets.handle_key(&state, key(KeyCode::Esc));
        let pending_title = tickets.editor.as_ref().unwrap().title.clone();
        tickets.handle_key(&state, key(KeyCode::Esc));

        tickets.handle_key(&state, key(KeyCode::Enter));

        assert_eq!(tickets.editor.as_ref().unwrap().title, pending_title);
    }

    #[test]
    fn new_label_form_validates_and_normalizes_the_color() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        tickets.handle_key(&state, key(KeyCode::Char('l')));
        tickets.observe_labels(TicketLabels {
            request: "labels-1".to_string(),
            repo: "borsuk".to_string(),
            labels: Vec::new(),
            error: None,
        });
        tickets.handle_key(&state, key(KeyCode::Char('n')));

        for character in "needs-review".chars() {
            tickets.handle_key(&state, key(KeyCode::Char(character)));
        }
        tickets.handle_key(&state, key(KeyCode::Tab));
        for character in "bad".chars() {
            tickets.handle_key(&state, key(KeyCode::Char(character)));
        }
        let invalid = tickets.handle_key(
            &state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        assert!(invalid.is_none());

        for _ in 0..3 {
            tickets.handle_key(&state, key(KeyCode::Backspace));
        }
        for character in "#55E6FF".chars() {
            tickets.handle_key(&state, key(KeyCode::Char(character)));
        }
        let action = tickets.handle_key(
            &state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        let Some(Action::Ticket(TicketAction::CreateLabel {
            repo,
            number,
            name,
            color,
            ..
        })) = action
        else {
            panic!("ctrl-s must create and attach the valid label");
        };
        assert_eq!(repo, "borsuk");
        assert_eq!(number, 7);
        assert_eq!(name, "needs-review");
        assert_eq!(color, "55e6ff");
    }

    #[test]
    fn a_concurrent_label_with_different_case_stays_one_catalog_entry() {
        let mut tickets = Tickets {
            focus_key: Some(("borsuk".to_string(), 7)),
            new_label_form: Some(NewLabelForm {
                name: "triage".to_string(),
                color: "55e6ff".to_string(),
                ..NewLabelForm::default()
            }),
            ..Tickets::default()
        };
        tickets.observe_labels(TicketLabels {
            request: "labels-7".to_string(),
            repo: "borsuk".to_string(),
            labels: vec![crate::sock::RepoLabel {
                name: "Triage".to_string(),
                color: "ff0000".to_string(),
            }],
            error: None,
        });

        tickets.observe_result(TicketResult {
            request: "create-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            kind: TicketResultKind::Success,
            message: "GitHub confirmed the label.".to_string(),
            issue: None,
            conflict: None,
        });

        let labels = &tickets.labels.as_ref().unwrap().labels;
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].name, "Triage");
        assert_eq!(labels[0].color, "ff0000");
    }

    #[test]
    fn c_starts_chat_for_an_unlabeled_focused_issue() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        let mut issue_details = details();
        issue_details.issue.labels.clear();
        tickets.observe_details(issue_details);

        let action = tickets.handle_key(&state, key(KeyCode::Char('c')));
        let Some(Action::Ticket(TicketAction::Chat { repo, number, .. })) = action else {
            panic!("c must start the issue conversation");
        };
        assert_eq!(repo, "borsuk");
        assert_eq!(number, 7);
        assert!(tickets.typing(), "the chat input must own the keyboard");
    }

    #[test]
    fn a_applies_the_exact_shown_proposal_without_a_label_action() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        let mut issue_details = details();
        issue_details.proposal = Some(crate::sock::TicketProposal {
            id: "proposal-7".to_string(),
            title: "Proposed title".to_string(),
            body: "Proposed body".to_string(),
            original_title: "Original title".to_string(),
            original_body: "Original body".to_string(),
        });
        tickets.observe_details(issue_details);

        let action = tickets.handle_key(&state, key(KeyCode::Char('a')));
        let Some(Action::Ticket(TicketAction::UpdateContent {
            expected,
            desired,
            source,
            ..
        })) = action
        else {
            panic!("a must use the shared content updater");
        };
        assert_eq!(expected.title, "Original title");
        assert_eq!(expected.body, "Original body");
        assert_eq!(desired.title, "Proposed title");
        assert_eq!(desired.body, "Proposed body");
        assert_eq!(
            source,
            TicketContentSource::Proposal {
                proposal_id: "proposal-7".to_string()
            }
        );
    }

    #[test]
    fn a_result_for_another_issue_cannot_change_the_focused_issue() {
        let state = state();
        let mut tickets = Tickets::default();
        tickets.handle_key(&state, key(KeyCode::Enter));
        tickets.observe_details(details());
        let mut remote = details().issue;
        remote.number = 42;

        tickets.observe_result(TicketResult {
            request: "other-42".to_string(),
            repo: "qubitsok".to_string(),
            number: 42,
            kind: TicketResultKind::Conflict,
            message: "another issue changed".to_string(),
            issue: None,
            conflict: Some(TicketConflict {
                remote,
                pending: TicketContent {
                    title: "Other".to_string(),
                    body: "Other".to_string(),
                },
                source: TicketContentSource::Direct,
            }),
        });

        assert!(tickets.result.is_none());
        assert!(tickets.conflict.is_none());
        assert!(!tickets.conflict_open);
    }
}
