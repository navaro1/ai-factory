//! The terminal editor for execution role settings.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::config::{
    validate_extra_args, ExecutionRole, Harness, RoleOverride, RoleSettings, SettingsEdit,
    SettingsSource, CLAUDE_PERMISSION_MODES, CODEX_APPROVAL_POLICIES, CODEX_SANDBOXES,
};
use crate::sock::{Action, RoleFieldSources, SettingsResult, SettingsResultStatus, StateView};

use super::theme::THEME;

/// One editable field in its stable display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Harness,
    Program,
    Model,
    Effort,
    ExtraArgs,
    Agent,
    Profile,
    PermissionMode,
    PermissionHandler,
    Tools,
    DisallowedTools,
    StrictMcp,
    AutoApprove,
    ApprovalPolicy,
    Sandbox,
    Limit,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Program => "program",
            Self::Model => "model",
            Self::Effort => "effort",
            Self::ExtraArgs => "extra args",
            Self::Agent => "agent",
            Self::Profile => "profile",
            Self::PermissionMode => "permission mode",
            Self::PermissionHandler => "permission handler",
            Self::Tools => "tools",
            Self::DisallowedTools => "denied tools",
            Self::StrictMcp => "strict MCP",
            Self::AutoApprove => "auto approve",
            Self::ApprovalPolicy => "approval policy",
            Self::Sandbox => "sandbox",
            Self::Limit => "limit",
        }
    }

    fn is_list(self) -> bool {
        matches!(self, Self::ExtraArgs | Self::Tools | Self::DisallowedTools)
    }

    fn is_choice(self) -> bool {
        matches!(
            self,
            Self::Harness
                | Self::StrictMcp
                | Self::AutoApprove
                | Self::PermissionMode
                | Self::ApprovalPolicy
                | Self::Sandbox
        )
    }
}

#[derive(Debug, Clone)]
enum DraftValue {
    Global {
        settings: RoleSettings,
        limit: Option<usize>,
    },
    Repository {
        settings: RoleSettings,
        override_settings: Box<RoleOverride>,
    },
}

#[derive(Debug, Clone)]
struct Draft {
    scope: usize,
    role: ExecutionRole,
    base_revision: String,
    value: DraftValue,
    changed: BTreeSet<Field>,
}

impl Draft {
    fn settings(&self) -> &RoleSettings {
        match &self.value {
            DraftValue::Global { settings, .. } | DraftValue::Repository { settings, .. } => {
                settings
            }
        }
    }

    fn settings_mut(&mut self) -> &mut RoleSettings {
        match &mut self.value {
            DraftValue::Global { settings, .. } | DraftValue::Repository { settings, .. } => {
                settings
            }
        }
    }
}

#[derive(Debug, Clone)]
struct TextEditor {
    field: Field,
    buffer: String,
}

#[derive(Debug, Clone)]
struct ListEditor {
    field: Field,
    rows: Vec<String>,
    selected: usize,
    row_editor: Option<String>,
}

/// The Settings view state.
#[derive(Debug, Default)]
pub struct Settings {
    scope: usize,
    role: usize,
    field: usize,
    draft: Option<Draft>,
    text_editor: Option<TextEditor>,
    list_editor: Option<ListEditor>,
    errors: BTreeMap<Field, String>,
    pending_request: Option<String>,
    status: Option<(SettingsResultStatus, String)>,
    discard_confirm: bool,
}

impl Settings {
    /// True while the settings view owns typed characters.
    pub fn typing(&self) -> bool {
        self.text_editor.is_some() || self.list_editor.is_some()
    }

    /// Unlock a settings request after a failed send or socket disconnect.
    pub fn delivery_failed(&mut self, action: Option<&Action>) {
        let matches = match action {
            Some(Action::SaveSettings { request, .. } | Action::ReloadSettings { request }) => {
                self.pending_request.as_deref() == Some(request.as_str())
            }
            Some(_) => false,
            None => self.pending_request.is_some(),
        };
        if matches {
            self.pending_request = None;
            self.status = Some((
                SettingsResultStatus::Failed,
                "the settings request was not delivered".to_string(),
            ));
        }
    }

    /// Apply a daemon response only when its request matches the active request.
    pub fn observe_result(&mut self, result: SettingsResult) {
        if self.pending_request.as_deref() != Some(result.request.as_str()) {
            return;
        }
        self.pending_request = None;
        let message = result
            .message
            .clone()
            .unwrap_or_else(|| match result.status {
                SettingsResultStatus::Saved => "settings saved".to_string(),
                SettingsResultStatus::Reloaded => "settings reloaded".to_string(),
                SettingsResultStatus::Stale => "the file changed; reload before save".to_string(),
                SettingsResultStatus::Invalid => "the settings are invalid".to_string(),
                SettingsResultStatus::RestartRequired => {
                    "repository changes require a daemon restart".to_string()
                }
                SettingsResultStatus::Failed => "the settings request failed".to_string(),
            });
        if matches!(
            result.status,
            SettingsResultStatus::Saved | SettingsResultStatus::Reloaded
        ) {
            self.draft = None;
            self.errors.clear();
            self.discard_confirm = false;
        } else if result.status == SettingsResultStatus::Invalid {
            let field = field_from_error(&message).unwrap_or_else(|| self.selected_field_for(None));
            self.errors.insert(field, message.clone());
        }
        self.status = Some((result.status, message));
    }

    /// Apply one Settings key and return a daemon action when needed.
    pub fn handle_key(&mut self, state: &StateView, key: KeyEvent) -> Option<Action> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return None;
        }
        if self.text_editor.is_some() {
            self.handle_text_key(state, key);
            return None;
        }
        if self.list_editor.is_some() {
            self.handle_list_key(state, key);
            return None;
        }
        self.clamp(state);
        if self.draft.is_some()
            && matches!(
                key.code,
                KeyCode::Char('h' | 'j' | 'k' | 'l')
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
            )
        {
            self.status = Some((
                SettingsResultStatus::Failed,
                "save or discard the current draft before navigation".to_string(),
            ));
            return None;
        }
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                self.scope = self.scope.saturating_sub(1);
                self.field = 0;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.scope = (self.scope + 1).min(self.repositories(state).len());
                self.field = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.role = (self.role + 1).min(ExecutionRole::ALL.len() - 1);
                self.field = 0;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.role = self.role.saturating_sub(1);
                self.field = 0;
            }
            KeyCode::Tab => {
                let count = self.visible_fields(state).len();
                if count > 0 {
                    self.field = (self.field + 1) % count;
                }
            }
            KeyCode::BackTab => {
                let count = self.visible_fields(state).len();
                if count > 0 {
                    self.field = (self.field + count - 1) % count;
                }
            }
            KeyCode::Enter => self.enter_field(state),
            KeyCode::Char('s') => return self.save(state),
            KeyCode::Char('r') => return self.reload(),
            KeyCode::Char('d') if self.scope > 0 => return self.remove_override(state),
            KeyCode::Esc if self.draft.is_some() => {
                if self.discard_confirm {
                    self.draft = None;
                    self.errors.clear();
                    self.discard_confirm = false;
                } else {
                    self.discard_confirm = true;
                    self.status = Some((
                        SettingsResultStatus::Failed,
                        "press Esc again to discard the draft".to_string(),
                    ));
                }
            }
            _ => self.discard_confirm = false,
        }
        None
    }

    fn enter_field(&mut self, state: &StateView) {
        let field = self.selected_field_for(Some(state));
        if field.is_choice() {
            self.cycle_choice(state, field);
            return;
        }
        if field.is_list() {
            let rows = self.list_value(state, field);
            self.list_editor = Some(ListEditor {
                field,
                selected: 0,
                rows,
                row_editor: None,
            });
            return;
        }
        let original = self.field_value(state, field);
        self.text_editor = Some(TextEditor {
            field,
            buffer: original,
        });
    }

    fn handle_text_key(&mut self, state: &StateView, key: KeyEvent) {
        let Some(editor) = self.text_editor.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.text_editor = None;
            }
            KeyCode::Enter => {
                let editor = self.text_editor.take().expect("the editor exists");
                self.set_text(state, editor.field, editor.buffer);
            }
            KeyCode::Backspace => {
                editor.buffer.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                editor.buffer.push(character);
            }
            _ => {}
        }
    }

    fn handle_list_key(&mut self, state: &StateView, key: KeyEvent) {
        let Some(editor) = self.list_editor.as_mut() else {
            return;
        };
        if let Some(buffer) = editor.row_editor.as_mut() {
            match key.code {
                KeyCode::Esc => editor.row_editor = None,
                KeyCode::Enter => {
                    let value = buffer.clone();
                    if let Some(row) = editor.rows.get_mut(editor.selected) {
                        *row = value;
                    }
                    editor.row_editor = None;
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    buffer.push(character);
                }
                _ => {}
            }
            return;
        }
        let mut apply = None;
        match key.code {
            KeyCode::Esc => {
                self.list_editor = None;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                editor.selected = (editor.selected + 1).min(editor.rows.len());
            }
            KeyCode::Char('k') | KeyCode::Up => editor.selected = editor.selected.saturating_sub(1),
            KeyCode::Char('a') => {
                editor.rows.push(String::new());
                editor.selected = editor.rows.len() - 1;
            }
            KeyCode::Char('d') => {
                if editor.selected < editor.rows.len() {
                    editor.rows.remove(editor.selected);
                    editor.selected = editor.selected.min(editor.rows.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if editor.selected == editor.rows.len() {
                    apply = Some((editor.field, editor.rows.clone()));
                } else {
                    let current = editor.rows[editor.selected].clone();
                    editor.row_editor = Some(current);
                }
            }
            _ => {}
        }
        if let Some((field, rows)) = apply {
            self.list_editor = None;
            self.set_list(state, field, rows);
        }
    }

    fn save(&mut self, state: &StateView) -> Option<Action> {
        if self.request_pending() {
            return None;
        }
        let Some(draft) = self.draft.clone() else {
            self.status = Some((SettingsResultStatus::Failed, "no draft to save".to_string()));
            return None;
        };
        self.errors = validate_draft(&draft);
        if !self.errors.is_empty() {
            self.status = Some((
                SettingsResultStatus::Invalid,
                "fix the marked fields before save".to_string(),
            ));
            return None;
        }
        let request = request_code();
        self.pending_request = Some(request.clone());
        let edit = match draft.value {
            DraftValue::Global { settings, limit } => SettingsEdit::Global {
                role: draft.role,
                settings,
                limit,
            },
            DraftValue::Repository {
                override_settings, ..
            } => SettingsEdit::Repository {
                repository: self.repositories(state)[draft.scope - 1].clone(),
                role: draft.role,
                settings: Some(*override_settings),
            },
        };
        Some(Action::SaveSettings {
            request,
            base_revision: draft.base_revision,
            edit,
        })
    }

    fn reload(&mut self) -> Option<Action> {
        if self.request_pending() {
            return None;
        }
        let request = request_code();
        self.pending_request = Some(request.clone());
        Some(Action::ReloadSettings { request })
    }

    fn remove_override(&mut self, state: &StateView) -> Option<Action> {
        if self.request_pending() {
            return None;
        }
        let repository = self.repositories(state).get(self.scope - 1)?.clone();
        let request = request_code();
        self.pending_request = Some(request.clone());
        Some(Action::SaveSettings {
            request,
            base_revision: state.settings.revision.clone(),
            edit: SettingsEdit::Repository {
                repository,
                role: self.selected_role_for(),
                settings: None,
            },
        })
    }

    fn request_pending(&mut self) -> bool {
        if self.pending_request.is_none() {
            return false;
        }
        self.status = Some((
            SettingsResultStatus::Failed,
            "a settings request is pending".to_string(),
        ));
        true
    }

    fn cycle_choice(&mut self, state: &StateView, field: Field) {
        self.ensure_draft(state);
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        match field {
            Field::Harness => {
                let next = match draft.settings().harness {
                    Harness::Claude => Harness::Opencode,
                    Harness::Opencode => Harness::Codex,
                    Harness::Codex => Harness::Claude,
                };
                let settings = draft.settings_mut();
                settings.harness = next;
                settings.program = next.program().to_string();
                clear_harness_fields(settings);
                match next {
                    Harness::Claude => {}
                    Harness::Opencode => settings.auto_approve = Some(false),
                    Harness::Codex => {}
                }
                if let DraftValue::Repository {
                    settings,
                    override_settings,
                } = &mut draft.value
                {
                    **override_settings = complete_override(settings);
                }
                self.field = 0;
            }
            Field::StrictMcp => {
                let settings = draft.settings_mut();
                settings.strict_mcp = Some(!settings.strict_mcp.unwrap_or(false));
                sync_override(draft, field);
            }
            Field::AutoApprove => {
                let settings = draft.settings_mut();
                settings.auto_approve = Some(!settings.auto_approve.unwrap_or(false));
                sync_override(draft, field);
            }
            Field::PermissionMode => {
                let settings = draft.settings_mut();
                settings.permission_mode = Some(next_choice(
                    settings.permission_mode.as_deref(),
                    CLAUDE_PERMISSION_MODES,
                ));
                sync_override(draft, field);
            }
            Field::ApprovalPolicy => {
                let settings = draft.settings_mut();
                settings.approval_policy = Some(next_choice(
                    settings.approval_policy.as_deref(),
                    CODEX_APPROVAL_POLICIES,
                ));
                sync_override(draft, field);
            }
            Field::Sandbox => {
                let settings = draft.settings_mut();
                settings.sandbox = Some(next_choice(settings.sandbox.as_deref(), CODEX_SANDBOXES));
                sync_override(draft, field);
            }
            _ => {}
        }
        draft.changed.insert(field);
        self.errors.remove(&field);
        self.discard_confirm = false;
    }

    fn set_text(&mut self, state: &StateView, field: Field, value: String) {
        self.ensure_draft(state);
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let settings = draft.settings_mut();
        match field {
            Field::Program => settings.program = value,
            Field::Model => settings.model = value,
            Field::Effort => settings.effort = optional(value),
            Field::Agent => settings.agent = optional(value),
            Field::Profile => settings.profile = optional(value),
            Field::PermissionMode => settings.permission_mode = optional(value),
            Field::PermissionHandler => settings.permission_handler = optional(value),
            Field::ApprovalPolicy => settings.approval_policy = optional(value),
            Field::Sandbox => settings.sandbox = optional(value),
            Field::Limit => {
                if let DraftValue::Global { limit, .. } = &mut draft.value {
                    *limit = value.parse::<usize>().ok();
                }
            }
            _ => return,
        }
        sync_override(draft, field);
        draft.changed.insert(field);
        self.errors.remove(&field);
        self.discard_confirm = false;
    }

    fn set_list(&mut self, state: &StateView, field: Field, rows: Vec<String>) {
        self.ensure_draft(state);
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        match field {
            Field::ExtraArgs => draft.settings_mut().extra_args = rows,
            Field::Tools => draft.settings_mut().tools = rows,
            Field::DisallowedTools => draft.settings_mut().disallowed_tools = rows,
            _ => return,
        }
        sync_override(draft, field);
        draft.changed.insert(field);
        self.errors.remove(&field);
    }

    fn ensure_draft(&mut self, state: &StateView) {
        let scope = self.scope;
        let role = self.selected_role_for();
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.scope == scope && draft.role == role)
        {
            return;
        }
        let value = if scope == 0 {
            let Some(source) = state
                .settings
                .global
                .iter()
                .find(|value| value.role == role)
            else {
                return;
            };
            DraftValue::Global {
                settings: source.settings.clone(),
                limit: source.limit,
            }
        } else {
            let repositories = self.repositories(state);
            let Some(alias) = repositories.get(scope - 1) else {
                return;
            };
            let Some(source) = state
                .settings
                .repositories
                .iter()
                .find(|value| value.repository == *alias && value.role == role)
            else {
                return;
            };
            DraftValue::Repository {
                settings: source.settings.clone(),
                override_settings: Box::new(override_from_sources(
                    &source.settings,
                    &source.sources,
                )),
            }
        };
        self.draft = Some(Draft {
            scope,
            role,
            base_revision: state.settings.revision.clone(),
            value,
            changed: BTreeSet::new(),
        });
        self.errors.clear();
    }

    fn current_settings_ref<'a>(&'a self, state: &'a StateView) -> Option<&'a RoleSettings> {
        let role = self.selected_role_for();
        if let Some(draft) = self
            .draft
            .as_ref()
            .filter(|draft| draft.scope == self.scope && draft.role == role)
        {
            return Some(draft.settings());
        }
        if self.scope == 0 {
            state
                .settings
                .global
                .iter()
                .find(|value| value.role == role)
                .map(|value| &value.settings)
        } else {
            let repositories = self.repositories(state);
            let alias = repositories.get(self.scope - 1)?;
            state
                .settings
                .repositories
                .iter()
                .find(|value| value.repository == *alias && value.role == role)
                .map(|value| &value.settings)
        }
    }

    fn visible_fields(&self, state: &StateView) -> Vec<Field> {
        let Some(settings) = self.current_settings_ref(state) else {
            return Vec::new();
        };
        let mut fields = vec![
            Field::Harness,
            Field::Program,
            Field::Model,
            Field::Effort,
            Field::ExtraArgs,
        ];
        match settings.harness {
            Harness::Claude => fields.extend([
                Field::Agent,
                Field::PermissionMode,
                Field::PermissionHandler,
                Field::Tools,
                Field::DisallowedTools,
                Field::StrictMcp,
            ]),
            Harness::Opencode => fields.extend([Field::Agent, Field::AutoApprove]),
            Harness::Codex => {
                fields.extend([Field::Profile, Field::ApprovalPolicy, Field::Sandbox])
            }
        }
        if self.scope == 0 && self.selected_role_for().stage().is_some() {
            fields.push(Field::Limit);
        }
        fields
    }

    fn selected_role_for(&self) -> ExecutionRole {
        ExecutionRole::ALL[self.role.min(ExecutionRole::ALL.len() - 1)]
    }

    fn selected_field_for(&self, state: Option<&StateView>) -> Field {
        state
            .map(|state| self.visible_fields(state))
            .and_then(|fields| {
                fields
                    .get(self.field.min(fields.len().saturating_sub(1)))
                    .copied()
            })
            .unwrap_or(Field::Harness)
    }

    fn repositories(&self, state: &StateView) -> Vec<String> {
        let mut names = state
            .settings
            .repositories
            .iter()
            .map(|value| value.repository.clone())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }

    fn clamp(&mut self, state: &StateView) {
        self.scope = self.scope.min(self.repositories(state).len());
        self.role = self.role.min(ExecutionRole::ALL.len() - 1);
        self.field = self
            .field
            .min(self.visible_fields(state).len().saturating_sub(1));
    }

    fn field_value(&self, state: &StateView, field: Field) -> String {
        let Some(settings) = self.current_settings_ref(state) else {
            return String::new();
        };
        match field {
            Field::Harness => settings.harness.program().to_string(),
            Field::Program => settings.program.clone(),
            Field::Model => settings.model.clone(),
            Field::Effort => settings.effort.clone().unwrap_or_default(),
            Field::Agent => settings.agent.clone().unwrap_or_default(),
            Field::Profile => settings.profile.clone().unwrap_or_default(),
            Field::PermissionMode => settings.permission_mode.clone().unwrap_or_default(),
            Field::PermissionHandler => settings.permission_handler.clone().unwrap_or_default(),
            Field::StrictMcp => settings.strict_mcp.unwrap_or(false).to_string(),
            Field::AutoApprove => settings.auto_approve.unwrap_or(false).to_string(),
            Field::ApprovalPolicy => settings.approval_policy.clone().unwrap_or_default(),
            Field::Sandbox => settings.sandbox.clone().unwrap_or_default(),
            Field::ExtraArgs => format!("[{} rows]", settings.extra_args.len()),
            Field::Tools => format!("[{} rows]", settings.tools.len()),
            Field::DisallowedTools => format!("[{} rows]", settings.disallowed_tools.len()),
            Field::Limit => {
                if let Some(draft) = self.draft.as_ref().filter(|draft| {
                    draft.scope == self.scope && draft.role == self.selected_role_for()
                }) {
                    match draft.value {
                        DraftValue::Global { limit, .. } => {
                            limit.map(|value| value.to_string()).unwrap_or_default()
                        }
                        DraftValue::Repository { .. } => String::new(),
                    }
                } else {
                    state
                        .settings
                        .global
                        .iter()
                        .find(|value| value.role == self.selected_role_for())
                        .and_then(|value| value.limit)
                        .map(|value| value.to_string())
                        .unwrap_or_default()
                }
            }
        }
    }

    fn list_value(&self, state: &StateView, field: Field) -> Vec<String> {
        let Some(settings) = self.current_settings_ref(state) else {
            return Vec::new();
        };
        match field {
            Field::ExtraArgs => settings.extra_args.clone(),
            Field::Tools => settings.tools.clone(),
            Field::DisallowedTools => settings.disallowed_tools.clone(),
            _ => Vec::new(),
        }
    }

    /// Draw the complete Settings view.
    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        let narrow = area.width < 72;
        let scope = self.scope_line(state);
        let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
        frame.render_widget(Paragraph::new(scope), chunks[0]);
        let panes = settings_panes(chunks[1], narrow);
        self.draw_roles(frame, panes[0]);
        self.draw_form(frame, panes[1], state);
        self.draw_editor(frame, area);
    }

    fn scope_line(&self, state: &StateView) -> Line<'static> {
        let mut spans = vec![Span::styled(" scopes ", Style::default().fg(THEME.dim))];
        let labels = std::iter::once("global".to_string()).chain(self.repositories(state));
        for (index, label) in labels.enumerate() {
            let style = if index == self.scope {
                Style::default()
                    .fg(if index == 0 { THEME.accent } else { THEME.repo })
                    .add_modifier(Modifier::BOLD)
            } else {
                THEME.dim()
            };
            spans.push(Span::styled(format!("< {label} > "), style));
        }
        spans.push(Span::styled(
            format!("revision {}", state.settings.revision),
            THEME.dim(),
        ));
        Line::from(spans)
    }

    fn draw_roles(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = ExecutionRole::ALL
            .iter()
            .enumerate()
            .map(|(index, role)| {
                let marker = if index == self.role { ">" } else { " " };
                let style = if index == self.role {
                    Style::default()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    THEME.dim()
                };
                Line::from(Span::styled(
                    format!("{marker} {}", role_label(*role)),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" roles ")),
            area,
        );
    }

    fn draw_form(&self, frame: &mut Frame<'_>, area: Rect, state: &StateView) {
        let fields = self.visible_fields(state);
        let mut lines = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            let cursor = if index == self.field { ">" } else { " " };
            let source = self.field_source(state, *field);
            let (owner, owner_style) = match source {
                Some(SettingsSource::Global) if self.scope > 0 => ("~", THEME.dim()),
                Some(SettingsSource::Repository { .. }) => (
                    "+",
                    Style::default().fg(THEME.repo).add_modifier(Modifier::BOLD),
                ),
                _ => (" ", THEME.dim()),
            };
            let label_style = if index == self.field {
                Style::default()
                    .fg(THEME.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                THEME.dim()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{cursor}{owner} "), owner_style),
                Span::styled(format!("{:<19}", field.label()), label_style),
                Span::raw(self.field_value(state, *field)),
            ]));
            if let Some(error) = self.errors.get(field) {
                lines.push(Line::from(Span::styled(
                    format!("   {}: {error}", field.label()),
                    Style::default().fg(THEME.error),
                )));
            }
        }
        if self.scope > 0 {
            lines.push(Line::from(Span::styled(
                "~ inherited   + repository value",
                THEME.dim(),
            )));
        }
        for warning in self.warnings(state) {
            lines.push(Line::from(Span::styled(
                format!("WARNING: {warning}"),
                Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
            )));
        }
        if let Some((status, message)) = &self.status {
            let color = match status {
                SettingsResultStatus::Saved | SettingsResultStatus::Reloaded => THEME.ok,
                SettingsResultStatus::Stale
                | SettingsResultStatus::Invalid
                | SettingsResultStatus::RestartRequired
                | SettingsResultStatus::Failed => THEME.error,
            };
            lines.push(Line::from(Span::styled(
                message.clone(),
                Style::default().fg(color),
            )));
        }
        if self.discard_confirm {
            lines.push(Line::from(Span::styled(
                "Esc discard draft   any key keep draft",
                Style::default().fg(THEME.warn),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "h/l scope  j/k role  Tab field  Enter edit  s save  r reload  d remove",
                THEME.dim(),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" role settings ")),
            area,
        );
    }

    fn draw_editor(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(editor) = &self.text_editor {
            let panel = centered(58, 5, area);
            frame.render_widget(Clear, panel);
            let lines = vec![
                Line::from(editor.buffer.clone()),
                Line::from(Span::styled("Enter apply   Esc cancel", THEME.dim())),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(format!(" {} ", editor.field.label()))),
                panel,
            );
        } else if let Some(editor) = &self.list_editor {
            let height = (editor.rows.len() as u16 + 5).clamp(7, 18);
            let panel = centered(64, height, area);
            frame.render_widget(Clear, panel);
            let mut lines = editor
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let marker = if index == editor.selected { ">" } else { " " };
                    Line::from(format!("{marker} {row}"))
                })
                .collect::<Vec<_>>();
            if editor.rows.is_empty() {
                lines.push(Line::from(Span::styled("  (no rows)", THEME.dim())));
            }
            let marker = if editor.selected == editor.rows.len() {
                ">"
            } else {
                " "
            };
            lines.push(Line::from(Span::styled(
                format!("{marker} apply list"),
                Style::default().fg(THEME.accent),
            )));
            if let Some(buffer) = &editor.row_editor {
                lines.push(Line::from(format!("> edit: {buffer}")));
                lines.push(Line::from(Span::styled(
                    "Enter apply   Esc cancel",
                    THEME.dim(),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "j/k row  a add  d delete  Enter edit/apply  Esc cancel",
                    THEME.dim(),
                )));
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(format!(" {} rows ", editor.field.label()))),
                panel,
            );
        }
    }

    fn field_source(&self, state: &StateView, field: Field) -> Option<SettingsSource> {
        if self.scope == 0 {
            return Some(SettingsSource::Global);
        }
        if self.draft.as_ref().is_some_and(|draft| {
            draft.scope == self.scope
                && draft.role == self.selected_role_for()
                && matches!(
                    &draft.value,
                    DraftValue::Repository {
                        override_settings,
                        ..
                    } if override_settings.harness.is_some()
                )
        }) {
            return Some(SettingsSource::Repository {
                alias: self.repositories(state).get(self.scope - 1)?.clone(),
            });
        }
        if self.draft.as_ref().is_some_and(|draft| {
            draft.scope == self.scope
                && draft.role == self.selected_role_for()
                && draft.changed.contains(&field)
        }) {
            return Some(SettingsSource::Repository {
                alias: self.repositories(state).get(self.scope - 1)?.clone(),
            });
        }
        let repositories = self.repositories(state);
        let alias = repositories.get(self.scope - 1)?;
        let sources = &state
            .settings
            .repositories
            .iter()
            .find(|value| value.repository == *alias && value.role == self.selected_role_for())?
            .sources;
        Some(source_for_field(sources, field).clone())
    }

    fn warnings(&self, state: &StateView) -> Vec<&'static str> {
        let Some(settings) = self.current_settings_ref(state) else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        if settings.permission_mode.as_deref() == Some("bypassPermissions") {
            warnings.push("Claude permission checks are disabled");
        }
        if settings.auto_approve == Some(true) {
            warnings.push("OpenCode approval checks are disabled");
        }
        if settings.approval_policy.as_deref() == Some("never") {
            warnings.push("Codex approval checks are disabled");
        }
        if settings.sandbox.as_deref() == Some("danger-full-access") {
            warnings.push("Codex sandbox protection is disabled");
        }
        warnings
    }

    #[cfg(test)]
    fn scope_label(&self, state: &StateView) -> String {
        if self.scope == 0 {
            "global".to_string()
        } else {
            self.repositories(state)[self.scope - 1].clone()
        }
    }

    #[cfg(test)]
    fn selected_role(&self) -> ExecutionRole {
        self.selected_role_for()
    }

    #[cfg(test)]
    fn selected_field(&self) -> Field {
        [
            Field::Harness,
            Field::Program,
            Field::Model,
            Field::Effort,
            Field::ExtraArgs,
        ]
        .get(self.field)
        .copied()
        .unwrap_or(Field::Harness)
    }

    #[cfg(test)]
    fn set_role(&mut self, role: ExecutionRole) {
        self.role = ExecutionRole::ALL
            .iter()
            .position(|value| *value == role)
            .unwrap();
        self.field = 0;
    }

    #[cfg(test)]
    fn set_field(&mut self, field: Field) {
        self.field = match field {
            Field::Harness => 0,
            Field::Program => 1,
            Field::Model => 2,
            Field::Effort => 3,
            Field::ExtraArgs => 4,
            Field::Agent | Field::Profile => 5,
            Field::PermissionMode | Field::AutoApprove | Field::ApprovalPolicy => 6,
            Field::PermissionHandler | Field::Sandbox => 7,
            Field::Tools => 8,
            Field::DisallowedTools => 9,
            Field::StrictMcp => 10,
            Field::Limit => 11,
        };
    }

    #[cfg(test)]
    fn replace_selected_text(&mut self, state: &StateView, value: &str) {
        let field = self.selected_field_for(Some(state));
        self.set_text(state, field, value.to_string());
    }

    #[cfg(test)]
    fn current_settings<'a>(&'a self, state: &'a StateView) -> Option<&'a RoleSettings> {
        self.current_settings_ref(state)
    }

    #[cfg(test)]
    fn field_error(&self, field: Field) -> Option<&str> {
        self.errors.get(&field).map(String::as_str)
    }

    #[cfg(test)]
    fn discard_confirmation(&self) -> bool {
        self.discard_confirm
    }

    #[cfg(test)]
    fn dirty(&self) -> bool {
        self.draft.is_some()
    }
}

fn optional(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn request_code() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn role_label(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Refine => "refine",
        ExecutionRole::Implement => "implement",
        ExecutionRole::Review => "review",
        ExecutionRole::Release => "release",
        ExecutionRole::TicketCreate => "ticket creation",
        ExecutionRole::TicketChat => "ticket chat",
    }
}

fn clear_harness_fields(settings: &mut RoleSettings) {
    settings.agent = None;
    settings.profile = None;
    settings.permission_mode = None;
    settings.permission_handler = None;
    settings.tools.clear();
    settings.disallowed_tools.clear();
    settings.strict_mcp = None;
    settings.auto_approve = None;
    settings.approval_policy = None;
    settings.sandbox = None;
}

fn complete_override(settings: &RoleSettings) -> RoleOverride {
    let mut value = RoleOverride {
        harness: Some(settings.harness),
        program: Some(settings.program.clone()),
        model: Some(settings.model.clone()),
        effort: settings.effort.clone(),
        extra_args: Some(settings.extra_args.clone()),
        ..RoleOverride::default()
    };
    match settings.harness {
        Harness::Claude => {
            value.agent = settings.agent.clone();
            value.permission_mode = settings.permission_mode.clone();
            value.permission_handler = settings.permission_handler.clone();
            value.tools = Some(settings.tools.clone());
            value.disallowed_tools = Some(settings.disallowed_tools.clone());
            value.strict_mcp = settings.strict_mcp;
        }
        Harness::Opencode => {
            value.agent = settings.agent.clone();
            value.auto_approve = settings.auto_approve;
        }
        Harness::Codex => {
            value.profile = settings.profile.clone();
            value.approval_policy = settings.approval_policy.clone();
            value.sandbox = settings.sandbox.clone();
        }
    }
    value
}

fn override_from_sources(settings: &RoleSettings, sources: &RoleFieldSources) -> RoleOverride {
    if is_repository(&sources.harness) {
        return complete_override(settings);
    }
    RoleOverride {
        harness: None,
        program: is_repository(&sources.program).then(|| settings.program.clone()),
        model: is_repository(&sources.model).then(|| settings.model.clone()),
        effort: is_repository(&sources.effort)
            .then(|| settings.effort.clone())
            .flatten(),
        extra_args: is_repository(&sources.extra_args).then(|| settings.extra_args.clone()),
        agent: is_repository(&sources.agent)
            .then(|| settings.agent.clone())
            .flatten(),
        profile: is_repository(&sources.profile)
            .then(|| settings.profile.clone())
            .flatten(),
        permission_mode: is_repository(&sources.permission_mode)
            .then(|| settings.permission_mode.clone())
            .flatten(),
        permission_handler: is_repository(&sources.permission_handler)
            .then(|| settings.permission_handler.clone())
            .flatten(),
        tools: is_repository(&sources.tools).then(|| settings.tools.clone()),
        disallowed_tools: is_repository(&sources.disallowed_tools)
            .then(|| settings.disallowed_tools.clone()),
        strict_mcp: is_repository(&sources.strict_mcp)
            .then_some(settings.strict_mcp)
            .flatten(),
        auto_approve: is_repository(&sources.auto_approve)
            .then_some(settings.auto_approve)
            .flatten(),
        approval_policy: is_repository(&sources.approval_policy)
            .then(|| settings.approval_policy.clone())
            .flatten(),
        sandbox: is_repository(&sources.sandbox)
            .then(|| settings.sandbox.clone())
            .flatten(),
    }
}

fn sync_override(draft: &mut Draft, field: Field) {
    let DraftValue::Repository {
        settings,
        override_settings,
    } = &mut draft.value
    else {
        return;
    };
    if override_settings.harness.is_some() {
        **override_settings = complete_override(settings);
        return;
    }
    match field {
        Field::Program => override_settings.program = Some(settings.program.clone()),
        Field::Model => override_settings.model = Some(settings.model.clone()),
        Field::Effort => override_settings.effort = settings.effort.clone(),
        Field::ExtraArgs => override_settings.extra_args = Some(settings.extra_args.clone()),
        Field::Agent => override_settings.agent = settings.agent.clone(),
        Field::Profile => override_settings.profile = settings.profile.clone(),
        Field::PermissionMode => {
            override_settings.permission_mode = settings.permission_mode.clone()
        }
        Field::PermissionHandler => {
            override_settings.permission_handler = settings.permission_handler.clone()
        }
        Field::Tools => override_settings.tools = Some(settings.tools.clone()),
        Field::DisallowedTools => {
            override_settings.disallowed_tools = Some(settings.disallowed_tools.clone())
        }
        Field::StrictMcp => override_settings.strict_mcp = settings.strict_mcp,
        Field::AutoApprove => override_settings.auto_approve = settings.auto_approve,
        Field::ApprovalPolicy => {
            override_settings.approval_policy = settings.approval_policy.clone()
        }
        Field::Sandbox => override_settings.sandbox = settings.sandbox.clone(),
        Field::Harness | Field::Limit => {}
    }
}

fn validate_draft(draft: &Draft) -> BTreeMap<Field, String> {
    let settings = draft.settings();
    let mut errors = BTreeMap::new();
    if settings.program.trim().is_empty() {
        errors.insert(Field::Program, "must not be empty".to_string());
    }
    if settings.model.trim().is_empty() {
        errors.insert(Field::Model, "must not be empty".to_string());
    }
    for (field, value) in [
        (Field::Effort, settings.effort.as_deref()),
        (Field::Agent, settings.agent.as_deref()),
        (Field::Profile, settings.profile.as_deref()),
        (Field::PermissionMode, settings.permission_mode.as_deref()),
        (
            Field::PermissionHandler,
            settings.permission_handler.as_deref(),
        ),
        (Field::ApprovalPolicy, settings.approval_policy.as_deref()),
        (Field::Sandbox, settings.sandbox.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            errors.insert(field, "must not be empty".to_string());
        }
    }
    for (field, values) in [
        (Field::Tools, settings.tools.as_slice()),
        (Field::DisallowedTools, settings.disallowed_tools.as_slice()),
    ] {
        if values.iter().any(|value| value.trim().is_empty()) {
            errors.insert(field, "rows must not be empty".to_string());
        }
    }
    if let Err(error) = validate_extra_args(&settings.extra_args, settings.harness, "settings") {
        errors.insert(Field::ExtraArgs, format!("unsupported argument: {error}"));
    }
    for (field, value, choices) in [
        (
            Field::PermissionMode,
            settings.permission_mode.as_deref(),
            CLAUDE_PERMISSION_MODES,
        ),
        (
            Field::ApprovalPolicy,
            settings.approval_policy.as_deref(),
            CODEX_APPROVAL_POLICIES,
        ),
        (Field::Sandbox, settings.sandbox.as_deref(), CODEX_SANDBOXES),
    ] {
        if value.is_some_and(|value| !choices.contains(&value)) {
            errors.insert(field, format!("must be one of {}", choices.join(", ")));
        }
    }
    if let DraftValue::Global { limit, .. } = draft.value {
        if draft.role.stage().is_some() && limit.unwrap_or(0) == 0 {
            errors.insert(Field::Limit, "must be at least 1".to_string());
        }
    }
    errors
}

fn next_choice(current: Option<&str>, choices: &[&str]) -> String {
    let index = current
        .and_then(|value| choices.iter().position(|choice| *choice == value))
        .map_or(0, |index| (index + 1) % choices.len());
    choices[index].to_string()
}

fn source_for_field(sources: &RoleFieldSources, field: Field) -> &SettingsSource {
    match field {
        Field::Harness => &sources.harness,
        Field::Program => &sources.program,
        Field::Model => &sources.model,
        Field::Effort => &sources.effort,
        Field::ExtraArgs => &sources.extra_args,
        Field::Agent => &sources.agent,
        Field::Profile => &sources.profile,
        Field::PermissionMode => &sources.permission_mode,
        Field::PermissionHandler => &sources.permission_handler,
        Field::Tools => &sources.tools,
        Field::DisallowedTools => &sources.disallowed_tools,
        Field::StrictMcp => &sources.strict_mcp,
        Field::AutoApprove => &sources.auto_approve,
        Field::ApprovalPolicy => &sources.approval_policy,
        Field::Sandbox => &sources.sandbox,
        Field::Limit => &sources.harness,
    }
}

fn is_repository(source: &SettingsSource) -> bool {
    matches!(source, SettingsSource::Repository { .. })
}

fn field_from_error(message: &str) -> Option<Field> {
    [
        ("extra_args", Field::ExtraArgs),
        ("permission_mode", Field::PermissionMode),
        ("permission_handler", Field::PermissionHandler),
        ("disallowed_tools", Field::DisallowedTools),
        ("strict_mcp", Field::StrictMcp),
        ("auto_approve", Field::AutoApprove),
        ("approval_policy", Field::ApprovalPolicy),
        ("program", Field::Program),
        ("model", Field::Model),
        ("effort", Field::Effort),
        ("agent", Field::Agent),
        ("profile", Field::Profile),
        ("tools", Field::Tools),
        ("sandbox", Field::Sandbox),
        ("limit", Field::Limit),
    ]
    .into_iter()
    .find_map(|(name, field)| message.contains(name).then_some(field))
}

fn centered(width: u16, height: u16, outer: Rect) -> Rect {
    Rect {
        x: outer.x + outer.width.saturating_sub(width) / 2,
        y: outer.y + outer.height.saturating_sub(height) / 2,
        width: width.min(outer.width),
        height: height.min(outer.height),
    }
}

fn settings_panes(area: Rect, narrow: bool) -> [Rect; 2] {
    if narrow {
        let panes = Layout::vertical([Constraint::Length(8), Constraint::Min(1)]).split(area);
        [panes[0], panes[1]]
    } else {
        let panes = Layout::horizontal([Constraint::Length(22), Constraint::Min(30)]).split(area);
        [panes[0], panes[1]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionRole, Harness, RoleSettings, SettingsSource};
    use crate::sock::{
        Action, GlobalRoleSettingsView, RepositoryRoleSettingsView, RoleFieldSources,
        SettingsOperation, SettingsResult, SettingsResultStatus, SettingsView, StateView,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn role(harness: Harness) -> RoleSettings {
        RoleSettings {
            harness,
            program: harness.program().to_string(),
            model: "model-one".to_string(),
            effort: Some("high".to_string()),
            extra_args: vec![],
            agent: matches!(harness, Harness::Claude | Harness::Opencode)
                .then(|| "builder".to_string()),
            profile: (harness == Harness::Codex).then(|| "reviewer".to_string()),
            permission_mode: (harness == Harness::Claude).then(|| "manual".to_string()),
            permission_handler: (harness == Harness::Claude).then(|| "inbox".to_string()),
            tools: if harness == Harness::Claude {
                vec!["Read".to_string()]
            } else {
                Vec::new()
            },
            disallowed_tools: vec![],
            strict_mcp: (harness == Harness::Claude).then_some(false),
            auto_approve: (harness == Harness::Opencode).then_some(false),
            approval_policy: (harness == Harness::Codex).then(|| "on-request".to_string()),
            sandbox: (harness == Harness::Codex).then(|| "workspace-write".to_string()),
        }
    }

    fn sources(alias: &str) -> RoleFieldSources {
        let global = SettingsSource::Global;
        let repo = SettingsSource::Repository {
            alias: alias.to_string(),
        };
        RoleFieldSources {
            harness: global.clone(),
            program: global.clone(),
            model: repo,
            effort: global.clone(),
            extra_args: global.clone(),
            agent: global.clone(),
            profile: global.clone(),
            permission_mode: global.clone(),
            permission_handler: global.clone(),
            tools: global.clone(),
            disallowed_tools: global.clone(),
            strict_mcp: global.clone(),
            auto_approve: global.clone(),
            approval_policy: global.clone(),
            sandbox: global,
        }
    }

    fn state() -> StateView {
        let mut state = crate::tui::pipeline::sample_view();
        let harnesses = [
            Harness::Claude,
            Harness::Opencode,
            Harness::Codex,
            Harness::Claude,
            Harness::Claude,
            Harness::Claude,
        ];
        state.settings = SettingsView {
            revision: "rev-one".to_string(),
            global: ExecutionRole::ALL
                .into_iter()
                .zip(harnesses)
                .map(|(role_name, harness)| GlobalRoleSettingsView {
                    role: role_name,
                    settings: role(harness),
                    limit: role_name.stage().map(|_| 3),
                })
                .collect(),
            repositories: ExecutionRole::ALL
                .into_iter()
                .map(|role_name| RepositoryRoleSettingsView {
                    repository: "borsuk".to_string(),
                    role: role_name,
                    settings: role(Harness::Claude),
                    sources: sources("borsuk"),
                    overridden: true,
                })
                .collect(),
        };
        state
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(settings: &Settings, state: &StateView, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| settings.draw(frame, frame.area(), state))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn wide_and_narrow_layouts_keep_the_stable_role_order() {
        let settings = Settings::default();
        for output in [
            text(&settings, &state(), 100, 28),
            text(&settings, &state(), 50, 34),
        ] {
            let positions = [
                "refine",
                "implement",
                "review",
                "release",
                "ticket creation",
                "ticket chat",
            ]
            .map(|name| output.find(name).expect("the role must be visible"));
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(output.contains("role settings"));
        }
    }

    #[test]
    fn scope_role_and_field_keys_move_the_three_cursors() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Tab));
        assert_eq!(settings.scope_label(&state), "borsuk");
        assert_eq!(settings.selected_role(), ExecutionRole::Implement);
        assert_eq!(settings.selected_field(), Field::Program);
        settings.handle_key(&state, key(KeyCode::Char('h')));
        settings.handle_key(&state, key(KeyCode::Char('k')));
        assert_eq!(settings.scope_label(&state), "global");
        assert_eq!(settings.selected_role(), ExecutionRole::Refine);
    }

    #[test]
    fn enter_edits_text_and_cycles_typed_values() {
        let state = state();
        let mut settings = Settings::default();
        assert_eq!(settings.selected_field(), Field::Harness);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.current_settings(&state).unwrap().harness,
            Harness::Opencode
        );
        settings.handle_key(&state, key(KeyCode::Tab));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing());
        settings.handle_key(&state, key(KeyCode::Char('x')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings
            .current_settings(&state)
            .unwrap()
            .program
            .ends_with('x'));
    }

    #[test]
    fn closed_native_values_use_value_selection() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::PermissionMode);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(!settings.typing());
        assert_eq!(
            settings
                .current_settings(&state)
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("dontAsk")
        );

        settings.set_role(ExecutionRole::Review);
        settings.set_field(Field::ApprovalPolicy);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings
                .current_settings(&state)
                .unwrap()
                .approval_policy
                .as_deref(),
            Some("never")
        );
        settings.set_field(Field::Sandbox);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings
                .current_settings(&state)
                .unwrap()
                .sandbox
                .as_deref(),
            Some("danger-full-access")
        );
    }

    #[test]
    fn settings_reject_the_same_managed_aliases_as_config() {
        let state = state();
        let cases = [
            (ExecutionRole::Refine, "--allowedTools=Read"),
            (ExecutionRole::Implement, "-s"),
            (ExecutionRole::Review, "--full-auto"),
        ];
        for (role, argument) in cases {
            let mut settings = Settings::default();
            settings.set_role(role);
            settings.set_list(&state, Field::ExtraArgs, vec![argument.to_string()]);
            assert!(settings
                .handle_key(&state, key(KeyCode::Char('s')))
                .is_none());
            assert!(settings.field_error(Field::ExtraArgs).is_some());
        }
    }

    #[test]
    fn list_fields_use_a_row_editor() {
        let state = state();
        let mut settings = Settings::default();
        for _ in 0..4 {
            settings.handle_key(&state, key(KeyCode::Tab));
        }
        assert_eq!(settings.selected_field(), Field::ExtraArgs);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing());
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('-')));
        settings.handle_key(&state, key(KeyCode::Char('v')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('-')));
        settings.handle_key(&state, key(KeyCode::Char('q')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('k')));
        settings.handle_key(&state, key(KeyCode::Char('d')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.current_settings(&state).unwrap().extra_args,
            vec!["-q"]
        );
    }

    #[test]
    fn escape_cancels_all_pending_list_changes() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::ExtraArgs);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('x')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("Enter edit/apply"));
        assert!(output.contains("Esc cancel"));
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.dirty());
        assert_eq!(
            settings.current_settings(&state).unwrap().extra_args,
            Vec::<String>::new()
        );
    }

    #[test]
    fn repository_fields_show_inherited_and_owned_markers() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("~ harness"), "{output}");
        assert!(output.contains("+ model"), "{output}");
        assert!(output.contains("~ inherited"), "{output}");
    }

    #[test]
    fn dangerous_native_modes_show_a_text_warning() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_role(ExecutionRole::Review);
        settings.set_field(Field::ApprovalPolicy);
        settings.replace_selected_text(&state, "never");
        settings.set_field(Field::Sandbox);
        settings.replace_selected_text(&state, "danger-full-access");
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("WARNING"));
        assert!(output.contains("approval checks are disabled"));
        assert!(output.contains("sandbox protection is disabled"));
    }

    #[test]
    fn claude_and_opencode_dangerous_modes_also_show_text_warnings() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::PermissionMode);
        settings.replace_selected_text(&state, "bypassPermissions");
        assert!(text(&settings, &state, 100, 28).contains("Claude permission checks are disabled"));

        settings.set_role(ExecutionRole::Implement);
        settings.set_field(Field::AutoApprove);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(text(&settings, &state, 100, 28).contains("OpenCode approval checks are disabled"));
    }

    #[test]
    fn invalid_fields_stay_local_and_block_save() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "");
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .is_none());
        assert_eq!(
            settings.field_error(Field::Model),
            Some("must not be empty")
        );
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("model: must not be empty"));
    }

    #[test]
    fn managed_extra_arguments_show_an_error_on_the_list_field() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_list(&state, Field::ExtraArgs, vec!["--model".to_string()]);
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .is_none());
        assert!(settings
            .field_error(Field::ExtraArgs)
            .is_some_and(|error| error.contains("unsupported argument")));
    }

    #[test]
    fn an_invalid_stage_limit_stays_visible_as_an_invalid_field() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Limit);
        settings.replace_selected_text(&state, "not-a-number");
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .is_none());
        assert_eq!(settings.field_value(&state, Field::Limit), "");
        assert_eq!(
            settings.field_error(Field::Limit),
            Some("must be at least 1")
        );
    }

    #[test]
    fn save_reload_and_remove_use_request_identities() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let save = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action");
        let Action::SaveSettings {
            request,
            base_revision,
            ..
        } = save
        else {
            panic!("wrong save action");
        };
        assert!(!request.is_empty());
        assert_eq!(base_revision, "rev-one");
        settings.observe_result(SettingsResult {
            request: request.clone(),
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });

        let reload = settings
            .handle_key(&state, key(KeyCode::Char('r')))
            .expect("reload action");
        let Action::ReloadSettings { request: reload_id } = reload else {
            panic!("wrong reload action");
        };
        assert_ne!(request, reload_id);
        settings.observe_result(SettingsResult {
            request: reload_id,
            operation: SettingsOperation::Reload,
            status: SettingsResultStatus::Reloaded,
            revision: "rev-one".to_string(),
            message: None,
        });

        settings.handle_key(&state, key(KeyCode::Char('l')));
        let remove = settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .expect("remove action");
        assert!(matches!(
            remove,
            Action::SaveSettings {
                edit: crate::config::SettingsEdit::Repository { settings: None, .. },
                ..
            }
        ));
    }

    #[test]
    fn a_failed_delivery_unlocks_a_later_settings_action() {
        let mut settings = Settings::default();
        let first = settings.reload().expect("the first reload starts");
        assert!(settings.reload().is_none());
        settings.delivery_failed(Some(&first));
        assert!(settings.reload().is_some());
    }

    #[test]
    fn a_socket_disconnect_clears_any_pending_settings_action() {
        let mut settings = Settings::default();
        settings.reload().unwrap();
        settings.delivery_failed(None);
        assert!(settings.reload().is_some());
    }

    #[test]
    fn two_quick_save_keys_send_once_and_out_of_order_results_do_not_replace_it() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("first save")
        else {
            panic!("wrong action");
        };
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .is_none());
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('r')))
            .is_none());
        settings.observe_result(SettingsResult {
            request: "later-unrelated-request".to_string(),
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Saved,
            revision: "wrong-revision".to_string(),
            message: None,
        });
        assert!(settings.dirty());
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Saved,
            revision: "rev-two".to_string(),
            message: None,
        });
        assert!(!settings.dirty());
    }

    #[test]
    fn a_pending_save_blocks_override_removal_until_its_result_arrives() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action")
        else {
            panic!("wrong action");
        };
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        settings.observe_result(SettingsResult {
            request: "unrelated".to_string(),
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_some());
    }

    #[test]
    fn a_pending_save_blocks_reload_until_its_result_arrives() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action")
        else {
            panic!("wrong action");
        };
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('r')))
            .is_none());
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });
        assert!(matches!(
            settings.handle_key(&state, key(KeyCode::Char('r'))),
            Some(Action::ReloadSettings { .. })
        ));
    }

    #[test]
    fn a_pending_override_removal_blocks_save_until_its_result_arrives() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .expect("remove action")
        else {
            panic!("wrong action");
        };
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .is_none());
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('r')))
            .is_none());
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });
        assert!(matches!(
            settings.handle_key(&state, key(KeyCode::Char('s'))),
            Some(Action::SaveSettings { .. })
        ));
    }

    #[test]
    fn a_pending_reload_blocks_every_other_settings_request() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::ReloadSettings { request } = settings
            .handle_key(&state, key(KeyCode::Char('r')))
            .expect("reload action")
        else {
            panic!("wrong action");
        };
        for code in [KeyCode::Char('s'), KeyCode::Char('r'), KeyCode::Char('d')] {
            assert!(settings.handle_key(&state, key(code)).is_none());
        }
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Reload,
            status: SettingsResultStatus::Failed,
            revision: "rev-one".to_string(),
            message: None,
        });
        assert!(matches!(
            settings.handle_key(&state, key(KeyCode::Char('s'))),
            Some(Action::SaveSettings { .. })
        ));
    }

    #[test]
    fn escape_cancels_an_editor_then_confirms_draft_removal() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('x')));
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.typing());
        settings.replace_selected_text(&state, "model-two");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(settings.discard_confirmation());
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.dirty());
    }

    #[test]
    fn settings_results_require_the_current_request_and_show_stale_or_field_errors() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action")
        else {
            panic!("wrong action");
        };
        settings.observe_result(SettingsResult {
            request: "old-request".to_string(),
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Invalid,
            revision: "rev-one".to_string(),
            message: Some("stage.refine.model must not be empty".to_string()),
        });
        assert_eq!(settings.field_error(Field::Model), None);

        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Stale,
            revision: "rev-two".to_string(),
            message: None,
        });
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("file changed; reload before save"));

        let Action::SaveSettings { request, .. } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("second save action")
        else {
            panic!("wrong action");
        };
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Invalid,
            revision: "rev-two".to_string(),
            message: Some("stage.refine.model is rejected".to_string()),
        });
        assert_eq!(
            settings.field_error(Field::Model),
            Some("stage.refine.model is rejected")
        );
    }

    #[test]
    fn a_repository_harness_change_omits_foreign_empty_fields() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let Action::SaveSettings {
            edit:
                crate::config::SettingsEdit::Repository {
                    settings: Some(replacement),
                    ..
                },
            ..
        } = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action")
        else {
            panic!("wrong action");
        };
        assert_eq!(replacement.harness, Some(Harness::Opencode));
        assert!(replacement.model.is_some());
        assert_eq!(replacement.extra_args, Some(Vec::new()));
        assert_eq!(replacement.auto_approve, Some(false));
        assert_eq!(replacement.tools, None);
        assert_eq!(replacement.disallowed_tools, None);
        assert_eq!(replacement.strict_mcp, None);
        assert_eq!(replacement.profile, None);
        assert_eq!(replacement.approval_policy, None);
        assert_eq!(replacement.sandbox, None);
    }

    #[test]
    fn a_repository_harness_change_marks_the_complete_form_as_repository_owned() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.handle_key(&state, key(KeyCode::Enter));
        for field in settings.visible_fields(&state) {
            assert!(matches!(
                settings.field_source(&state, field),
                Some(SettingsSource::Repository { .. })
            ));
        }
        let output = text(&settings, &state, 100, 28);
        assert!(!output.contains("~ program"), "{output}");
        assert!(output.contains("+ program"), "{output}");
    }

    #[test]
    fn narrow_layout_places_the_form_below_the_role_pane() {
        let [roles, form] = settings_panes(Rect::new(0, 2, 50, 30), true);
        assert!(form.y >= roles.y + roles.height);
        assert_eq!(roles.x, form.x);
        let [roles, form] = settings_panes(Rect::new(0, 2, 100, 24), false);
        assert!(form.x >= roles.x + roles.width);
        assert_eq!(roles.y, form.y);
    }

    #[test]
    fn claude_tool_lists_keep_their_field_order_and_apply_independently() {
        let state = state();
        let mut settings = Settings::default();
        let fields = settings.visible_fields(&state);
        let tools = fields
            .iter()
            .position(|field| *field == Field::Tools)
            .unwrap();
        let denied = fields
            .iter()
            .position(|field| *field == Field::DisallowedTools)
            .unwrap();
        assert_eq!(denied, tools + 1);

        settings.set_field(Field::Tools);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Enter));
        for character in "Write".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.current_settings(&state).unwrap().tools,
            vec!["Read", "Write"]
        );

        settings.set_field(Field::DisallowedTools);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Enter));
        for character in "Bash".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).unwrap();
        assert_eq!(current.tools, vec!["Read", "Write"]);
        assert_eq!(current.disallowed_tools, vec!["Bash"]);
    }

    #[test]
    fn role_and_scope_navigation_cannot_replace_an_unsaved_draft() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "model-two");
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(settings.selected_role(), ExecutionRole::Refine);
        assert_eq!(settings.scope_label(&state), "global");
        assert!(text(&settings, &state, 100, 28).contains("save or discard the current draft"));

        settings.handle_key(&state, key(KeyCode::Esc));
        settings.handle_key(&state, key(KeyCode::Esc));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        assert_eq!(settings.selected_role(), ExecutionRole::Implement);
    }
}
