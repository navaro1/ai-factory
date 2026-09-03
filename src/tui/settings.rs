//! The terminal editor for execution role settings.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::catalog::{self, ListField};
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

    fn is_toggle(self) -> bool {
        matches!(self, Self::StrictMcp | Self::AutoApprove)
    }

    /// The catalog field of one value list field. `None` for the fields
    /// that use another editor.
    fn list_field(self) -> Option<ListField> {
        match self {
            Self::Harness => Some(ListField::Harness),
            Self::Program => Some(ListField::Program),
            Self::Model => Some(ListField::Model),
            Self::Effort => Some(ListField::Effort),
            Self::Agent => Some(ListField::Agent),
            Self::Profile => Some(ListField::Profile),
            Self::PermissionMode => Some(ListField::PermissionMode),
            Self::PermissionHandler => Some(ListField::PermissionHandler),
            Self::ApprovalPolicy => Some(ListField::ApprovalPolicy),
            Self::Sandbox => Some(ListField::Sandbox),
            _ => None,
        }
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

/// The state of the one `opencode models` probe per shell start.
#[derive(Debug, Clone, Default)]
enum ModelDiscovery {
    /// The probe thread has not reported yet.
    #[default]
    Pending,
    /// The probe parsed this model list.
    Ready(Vec<String>),
    /// The probe failed with this reason.
    Failed(String),
}

/// One row of a value list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListRow {
    /// Clear the optional field.
    Empty,
    /// Apply this value.
    Value(String),
    /// A dim, non-selectable status line.
    Note(String),
    /// Open the text box with the current value.
    Custom,
}

impl ListRow {
    /// True when `Enter` may apply the row.
    fn selectable(&self) -> bool {
        !matches!(self, Self::Note(_))
    }

    /// The value the row applies. `None` for rows without a write.
    fn applied_value(&self) -> Option<String> {
        match self {
            Self::Empty => Some(String::new()),
            Self::Value(value) => Some(value.clone()),
            Self::Custom | Self::Note(_) => None,
        }
    }
}

/// The value list overlay of one field.
#[derive(Debug, Clone)]
struct ValueList {
    field: Field,
    list_field: ListField,
    /// The typed filter. A row matches when it contains the text.
    filter: String,
    /// The marked row index into `rows`.
    cursor: usize,
    /// The current value of the field. The cursor returns to it.
    current: String,
    /// The visible rows, rebuilt after every filter change.
    rows: Vec<ListRow>,
    /// The fixed and state candidates of the field.
    base: Vec<String>,
    /// The probe state of the OpenCode model list, for `model`.
    models: Option<ModelDiscovery>,
}

impl ValueList {
    /// True when the list carries the OpenCode model discovery state.
    fn discovers_models(&self) -> bool {
        self.list_field == ListField::Model && self.models.is_some()
    }

    /// The joined candidate values of the list.
    fn values(&self) -> Vec<String> {
        let mut sources = vec![self.base.clone()];
        if let Some(ModelDiscovery::Ready(models)) = &self.models {
            sources.push(models.clone());
        }
        catalog::join_candidates(sources)
    }

    /// Rebuild the visible rows from the filter and restore the cursor.
    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if catalog::optional(self.list_field) {
            rows.push(ListRow::Empty);
        }
        let needle = self.filter.to_lowercase();
        for value in self.values() {
            if needle.is_empty() || value.to_lowercase().contains(&needle) {
                rows.push(ListRow::Value(value));
            }
        }
        match &self.models {
            Some(ModelDiscovery::Pending) => {
                rows.push(ListRow::Note("discovering models...".to_string()));
            }
            Some(ModelDiscovery::Failed(reason)) => rows.push(ListRow::Note(reason.clone())),
            Some(ModelDiscovery::Ready(_)) | None => {}
        }
        if catalog::open(self.list_field) {
            rows.push(ListRow::Custom);
        }
        self.rows = rows;
        self.restore_cursor();
    }

    /// Put the cursor back on the current value, or on the first row
    /// `Enter` may apply.
    fn restore_cursor(&mut self) {
        let on_current = self
            .rows
            .iter()
            .position(|row| row.applied_value().as_deref() == Some(self.current_value()));
        self.cursor = on_current
            .or_else(|| self.rows.iter().position(ListRow::selectable))
            .unwrap_or(0);
    }

    /// The value the cursor returns to after a filter change.
    fn current_value(&self) -> &str {
        self.current.as_str()
    }

    /// Move the cursor one row. The step clamps at the ends and skips the
    /// status rows.
    fn step(&mut self, delta: isize) {
        let mut next = self.cursor as isize + delta;
        while next >= 0 && next < self.rows.len() as isize {
            self.cursor = next as usize;
            if self.rows[next as usize].selectable() {
                return;
            }
            next += delta;
        }
    }

    /// Replace the discovery state and refresh an open model list.
    fn observe_models(&mut self, models: &ModelDiscovery) {
        if !self.discovers_models() {
            return;
        }
        self.models = Some(models.clone());
        self.rebuild();
    }
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
    value_list: Option<ValueList>,
    /// The state of the one `opencode models` probe per shell start.
    models: ModelDiscovery,
    /// The notice line that a harness change leaves under the form.
    notice: Option<String>,
    errors: BTreeMap<Field, String>,
    pending_request: Option<String>,
    status: Option<(SettingsResultStatus, String)>,
    discard_confirm: bool,
}

impl Settings {
    /// True while the settings view owns typed characters.
    pub fn typing(&self) -> bool {
        self.text_editor.is_some() || self.list_editor.is_some() || self.value_list.is_some()
    }

    /// Store the result of the `opencode models` probe and refresh an
    /// open model list.
    pub fn observe_models(&mut self, result: Result<Vec<String>, String>) {
        self.models = match result {
            Ok(models) => ModelDiscovery::Ready(models),
            Err(reason) => ModelDiscovery::Failed(reason),
        };
        if let Some(list) = self.value_list.as_mut() {
            let models = self.models.clone();
            list.observe_models(&models);
        }
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
            self.notice = None;
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
        if self.value_list.is_some() {
            self.handle_value_list_key(state, key);
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
                    self.notice = None;
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
        if let Some(list_field) = field.list_field() {
            self.open_value_list(state, field, list_field);
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
        if field.is_toggle() {
            self.toggle_choice(state, field);
            return;
        }
        let original = self.field_value(state, field);
        self.text_editor = Some(TextEditor {
            field,
            buffer: original,
        });
    }

    /// Open the value list of one field.
    ///
    /// The rows start on the current value. A filter narrows the rows, and
    /// `Enter` applies the marked row.
    fn open_value_list(&mut self, state: &StateView, field: Field, list_field: ListField) {
        let Some(settings) = self.current_settings_ref(state) else {
            return;
        };
        let harness = settings.harness;
        let current = self.field_value(state, field);
        let base = self.candidates(state, harness, list_field);
        let models = match (list_field, harness) {
            (ListField::Model, Harness::Opencode) => Some(self.models.clone()),
            _ => None,
        };
        let mut list = ValueList {
            field,
            list_field,
            filter: String::new(),
            cursor: 0,
            current,
            rows: Vec::new(),
            base,
            models,
        };
        list.rebuild();
        self.value_list = Some(list);
    }

    /// The sorted, deduplicated candidate values of one field.
    ///
    /// The join takes the fixed table of the harness and every value that
    /// the pushed settings state holds for the same field and harness.
    fn candidates(
        &self,
        state: &StateView,
        harness: Harness,
        list_field: ListField,
    ) -> Vec<String> {
        let fixed = catalog::fixed_values(harness, list_field);
        let state_values = self
            .same_harness_settings(state, harness)
            .filter_map(|settings| scalar_value(settings, list_field))
            .collect();
        catalog::join_candidates([fixed, state_values])
    }

    /// Every pushed settings value that uses `harness`, from all global
    /// roles and all repository roles.
    fn same_harness_settings<'a>(
        &'a self,
        state: &'a StateView,
        harness: Harness,
    ) -> impl Iterator<Item = &'a RoleSettings> {
        state
            .settings
            .global
            .iter()
            .map(|value| &value.settings)
            .chain(
                state
                    .settings
                    .repositories
                    .iter()
                    .map(|value| &value.settings),
            )
            .filter(move |settings| settings.harness == harness)
    }

    /// Apply one key while a value list is open.
    fn handle_value_list_key(&mut self, state: &StateView, key: KeyEvent) {
        let Some(editor) = self.value_list.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.value_list = None,
            KeyCode::Char('j') | KeyCode::Down => editor.step(1),
            KeyCode::Char('k') | KeyCode::Up => editor.step(-1),
            KeyCode::Enter => {
                let row = editor.rows.get(editor.cursor).cloned();
                self.apply_value_row(state, row);
            }
            KeyCode::Backspace => {
                editor.filter.pop();
                editor.rebuild();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                editor.filter.push(character);
                editor.rebuild();
            }
            _ => {}
        }
    }

    /// Apply one marked value list row.
    fn apply_value_row(&mut self, state: &StateView, row: Option<ListRow>) {
        let Some(editor) = self.value_list.as_ref() else {
            return;
        };
        match row {
            Some(ListRow::Custom) => {
                let field = editor.field;
                let buffer = self.field_value(state, field);
                self.value_list = None;
                self.text_editor = Some(TextEditor { field, buffer });
            }
            Some(ListRow::Value(value)) => {
                let field = editor.field;
                self.value_list = None;
                if field == Field::Harness {
                    if let Some(harness) = catalog::harness_value(&value) {
                        self.apply_harness(state, harness);
                    }
                } else {
                    self.set_text(state, field, value);
                }
            }
            Some(ListRow::Empty) => {
                let field = editor.field;
                self.value_list = None;
                self.set_text(state, field, String::new());
            }
            _ => {}
        }
    }

    /// Apply one harness choice to the complete form.
    ///
    /// The change sets the program, picks a default model, clears every
    /// harness-specific field, and leaves one notice line under the form.
    fn apply_harness(&mut self, state: &StateView, next: Harness) {
        let role = self.selected_role_for();
        let global_model = state
            .settings
            .global
            .iter()
            .find(|value| value.role == role && value.settings.harness == next)
            .map(|value| value.settings.model.clone())
            .filter(|model| !model.is_empty());
        let model = global_model
            .or_else(|| {
                self.candidates(state, next, ListField::Model)
                    .into_iter()
                    .next()
            })
            .unwrap_or_default();
        self.ensure_draft(state);
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let previous = draft.settings().clone();
        let settings = draft.settings_mut();
        settings.harness = next;
        settings.program = next.program().to_string();
        settings.model = model.clone();
        clear_harness_fields(settings);
        if next == Harness::Opencode {
            settings.auto_approve = Some(false);
        }
        if let DraftValue::Repository {
            settings,
            override_settings,
        } = &mut draft.value
        {
            **override_settings = complete_override(settings);
        }
        draft.changed.insert(Field::Harness);
        self.errors.clear();
        self.discard_confirm = false;
        self.field = 0;
        self.notice = Some(harness_notice(&previous, next, &model));
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

    fn toggle_choice(&mut self, state: &StateView, field: Field) {
        self.ensure_draft(state);
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let settings = draft.settings_mut();
        match field {
            Field::StrictMcp => {
                settings.strict_mcp = Some(!settings.strict_mcp.unwrap_or(false));
            }
            Field::AutoApprove => {
                settings.auto_approve = Some(!settings.auto_approve.unwrap_or(false));
            }
            _ => return,
        }
        sync_override(draft, field);
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
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                notice.clone(),
                Style::default().fg(THEME.ok),
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
        } else if let Some(editor) = &self.value_list {
            let height = (editor.rows.len() as u16 + 5).clamp(7, 18);
            let panel = centered(64, height, area);
            frame.render_widget(Clear, panel);
            let mut lines = Vec::new();
            if !editor.filter.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(" filter: {}", editor.filter),
                    THEME.dim(),
                )));
            }
            for (index, row) in editor.rows.iter().enumerate() {
                let marker = if index == editor.cursor { ">" } else { " " };
                let line = match row {
                    ListRow::Empty => {
                        Line::from(Span::styled(format!("{marker} (none)"), THEME.dim()))
                    }
                    ListRow::Value(value) => Line::from(format!("{marker} {value}")),
                    ListRow::Note(reason) => {
                        Line::from(Span::styled(format!("{marker} {reason}"), THEME.dim()))
                    }
                    ListRow::Custom => Line::from(Span::styled(
                        format!("{marker} custom value..."),
                        Style::default().fg(THEME.accent),
                    )),
                };
                lines.push(line);
            }
            lines.push(Line::from(Span::styled(
                "j/k move  type to filter  Enter apply  Esc cancel",
                THEME.dim(),
            )));
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

    #[cfg(test)]
    fn value_list_field(&self) -> Option<Field> {
        self.value_list.as_ref().map(|list| list.field)
    }

    #[cfg(test)]
    fn value_list_rows(&self) -> Vec<String> {
        self.value_list
            .as_ref()
            .map(|list| list.rows.iter().map(row_text).collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn value_list_selected(&self) -> Option<String> {
        let list = self.value_list.as_ref()?;
        list.rows.get(list.cursor).map(row_text)
    }

    #[cfg(test)]
    fn text_editor_buffer(&self) -> Option<String> {
        self.text_editor
            .as_ref()
            .map(|editor| editor.buffer.clone())
    }
}

#[cfg(test)]
fn row_text(row: &ListRow) -> String {
    match row {
        ListRow::Empty => "(none)".to_string(),
        ListRow::Value(value) => value.clone(),
        ListRow::Note(reason) => reason.clone(),
        ListRow::Custom => "custom value...".to_string(),
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
    settings.effort = None;
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

/// One field value that the pushed settings state holds, when present.
fn scalar_value(settings: &RoleSettings, list_field: ListField) -> Option<String> {
    let value = match list_field {
        ListField::Model => Some(settings.model.clone()),
        ListField::Effort => settings.effort.clone(),
        ListField::Agent => settings.agent.clone(),
        ListField::Profile => settings.profile.clone(),
        ListField::PermissionMode => settings.permission_mode.clone(),
        ListField::PermissionHandler => settings.permission_handler.clone(),
        ListField::ApprovalPolicy => settings.approval_policy.clone(),
        ListField::Sandbox => settings.sandbox.clone(),
        ListField::Harness | ListField::Program => None,
    };
    value.filter(|value| !value.trim().is_empty())
}

/// The notice line that one harness change leaves under the form.
///
/// The line names the new harness and every field that the change reset.
fn harness_notice(previous: &RoleSettings, next: Harness, model: &str) -> String {
    let mut parts = vec![format!("switched to {}", next.program())];
    let cleared = [
        ("effort", previous.effort.is_some()),
        ("agent", previous.agent.is_some()),
        ("profile", previous.profile.is_some()),
        ("permission mode", previous.permission_mode.is_some()),
        ("permission handler", previous.permission_handler.is_some()),
        ("tools", !previous.tools.is_empty()),
        ("denied tools", !previous.disallowed_tools.is_empty()),
        ("strict MCP", previous.strict_mcp.is_some()),
        ("approval policy", previous.approval_policy.is_some()),
        ("sandbox", previous.sandbox.is_some()),
        ("auto approve", previous.auto_approve == Some(true)),
    ]
    .into_iter()
    .filter(|(_, cleared)| *cleared)
    .map(|(label, _)| label)
    .collect::<Vec<_>>();
    if !cleared.is_empty() {
        parts.push(format!("cleared {}", cleared.join(", ")));
    }
    if previous.model != model {
        if model.is_empty() {
            parts.push("cleared model".to_string());
        } else {
            parts.push(format!("model {model}"));
        }
    }
    parts.join("; ")
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
    fn enter_opens_the_value_list_and_the_text_box() {
        let state = state();
        let mut settings = Settings::default();
        assert_eq!(settings.selected_field(), Field::Harness);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing());
        assert_eq!(settings.value_list_field(), Some(Field::Harness));
        assert_eq!(
            settings.current_settings(&state).unwrap().harness,
            Harness::Claude
        );
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.typing());
        settings.handle_key(&state, key(KeyCode::Tab));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing());
        assert_eq!(settings.value_list_field(), Some(Field::Program));
        settings.handle_key(&state, key(KeyCode::Char('x')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing(), "the custom row opens the text box");
        settings.handle_key(&state, key(KeyCode::Char('x')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings
            .current_settings(&state)
            .unwrap()
            .program
            .ends_with('x'));
    }

    #[test]
    fn enter_opens_the_value_list_on_the_ten_choice_fields() {
        let state = state();
        let cases = [
            (ExecutionRole::Refine, Field::Harness),
            (ExecutionRole::Refine, Field::Program),
            (ExecutionRole::Refine, Field::Model),
            (ExecutionRole::Refine, Field::Effort),
            (ExecutionRole::Refine, Field::Agent),
            (ExecutionRole::Refine, Field::PermissionMode),
            (ExecutionRole::Refine, Field::PermissionHandler),
            (ExecutionRole::Implement, Field::Harness),
            (ExecutionRole::Implement, Field::Model),
            (ExecutionRole::Implement, Field::Effort),
            (ExecutionRole::Implement, Field::Agent),
            (ExecutionRole::Review, Field::Profile),
            (ExecutionRole::Review, Field::ApprovalPolicy),
            (ExecutionRole::Review, Field::Sandbox),
        ];
        let mut settings = Settings::default();
        for (role, field) in cases {
            settings.set_role(role);
            settings.set_field(field);
            settings.handle_key(&state, key(KeyCode::Enter));
            assert_eq!(settings.value_list_field(), Some(field), "{field:?}");
            assert!(settings.typing(), "{field:?} must hold the keyboard");
            settings.handle_key(&state, key(KeyCode::Esc));
            assert!(!settings.typing(), "{field:?} must close on esc");
        }
    }

    #[test]
    fn closed_native_values_use_value_selection() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::PermissionMode);
        settings.handle_key(&state, key(KeyCode::Enter));
        for character in "dont".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        settings.handle_key(&state, key(KeyCode::Char('j')));
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
        for character in "nev".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        settings.handle_key(&state, key(KeyCode::Char('j')));
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
        for character in "dang".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        settings.handle_key(&state, key(KeyCode::Char('j')));
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
    fn the_value_list_moves_filters_and_applies_with_the_documented_keys() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_selected(),
            Some("model-one".to_string()),
            "the cursor starts on the current value"
        );
        settings.handle_key(&state, key(KeyCode::Char('s')));
        settings.handle_key(&state, key(KeyCode::Char('o')));
        settings.handle_key(&state, key(KeyCode::Char('n')));
        assert_eq!(
            settings.value_list_rows(),
            ["sonnet", "custom value..."],
            "a printable character extends the filter"
        );
        for _ in 0..3 {
            settings.handle_key(&state, key(KeyCode::Backspace));
        }
        assert_eq!(
            settings.value_list_rows(),
            ["fable", "model-one", "opus", "sonnet", "custom value..."],
            "backspace shortens the filter"
        );
        settings.handle_key(&state, key(KeyCode::Char('j')));
        assert_eq!(settings.value_list_selected(), Some("opus".to_string()));
        settings.handle_key(&state, key(KeyCode::Down));
        assert_eq!(settings.value_list_selected(), Some("sonnet".to_string()));
        settings.handle_key(&state, key(KeyCode::Char('k')));
        settings.handle_key(&state, key(KeyCode::Up));
        settings.handle_key(&state, key(KeyCode::Char('k')));
        assert_eq!(settings.value_list_selected(), Some("fable".to_string()));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.current_settings(&state).unwrap().model, "fable");
        assert!(!settings.typing());

        settings.set_field(Field::Effort);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('z')));
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.typing());
        assert_eq!(
            settings.current_settings(&state).unwrap().effort,
            Some("high".to_string()),
            "esc cancels and changes nothing"
        );
    }

    #[test]
    fn the_candidate_set_joins_the_fixed_and_state_values() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_rows(),
            ["fable", "model-one", "opus", "sonnet", "custom value..."]
        );
        settings.handle_key(&state, key(KeyCode::Esc));
        settings.set_field(Field::Effort);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_rows(),
            [
                "(none)",
                "high",
                "low",
                "max",
                "medium",
                "xhigh",
                "custom value..."
            ]
        );
        settings.handle_key(&state, key(KeyCode::Esc));
        settings.set_field(Field::Harness);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.value_list_rows(), ["claude", "codex", "opencode"]);
    }

    #[test]
    fn the_model_list_shows_the_discovery_state() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_role(ExecutionRole::Implement);
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_rows(),
            ["model-one", "discovering models...", "custom value..."]
        );
        settings.observe_models(Ok(vec![
            "zai-coding-plan/glm-5.3".to_string(),
            "zai-coding-plan/glm-5.3-flash".to_string(),
        ]));
        assert_eq!(
            settings.value_list_rows(),
            [
                "model-one",
                "zai-coding-plan/glm-5.3",
                "zai-coding-plan/glm-5.3-flash",
                "custom value..."
            ]
        );

        settings.handle_key(&state, key(KeyCode::Esc));
        settings.observe_models(Err("opencode models exited with status 1".to_string()));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_rows(),
            [
                "model-one",
                "opencode models exited with status 1",
                "custom value..."
            ]
        );
        settings.handle_key(&state, key(KeyCode::Down));
        assert_eq!(
            settings.value_list_selected(),
            Some("custom value...".to_string()),
            "the cursor skips the dim failure row"
        );
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("opencode models exited with status 1"));
        settings.handle_key(&state, key(KeyCode::Esc));
    }

    #[test]
    fn optional_fields_start_with_a_none_row() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Effort);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.value_list_rows().first(),
            Some(&"(none)".to_string())
        );
        settings.handle_key(&state, key(KeyCode::Char('k')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.current_settings(&state).unwrap().effort, None);

        settings.set_field(Field::PermissionMode);
        settings.handle_key(&state, key(KeyCode::Enter));
        let rows = settings.value_list_rows();
        assert_eq!(rows.first(), Some(&"(none)".to_string()));
        assert_eq!(rows.last(), Some(&"plan".to_string()));
    }

    #[test]
    fn open_fields_end_with_a_custom_row_that_opens_the_text_box() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        let rows = settings.value_list_rows();
        assert_eq!(rows.last(), Some(&"custom value...".to_string()));
        for _ in 0..(rows.len() - 2) {
            settings.handle_key(&state, key(KeyCode::Char('j')));
        }
        assert_eq!(
            settings.value_list_selected(),
            Some("custom value...".to_string())
        );
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.typing());
        assert_eq!(settings.text_editor_buffer(), Some("model-one".to_string()));
        settings.handle_key(&state, key(KeyCode::Esc));

        settings.set_field(Field::Harness);
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.value_list_rows(), ["claude", "codex", "opencode"]);
    }

    #[test]
    fn a_value_choice_writes_through_the_text_path_and_marks_the_repository() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.current_settings(&state).unwrap().model, "opus");
        assert!(matches!(
            settings.field_source(&state, Field::Model),
            Some(SettingsSource::Repository { .. })
        ));
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("+ model"), "{output}");
    }

    #[test]
    fn a_harness_change_sets_the_program_picks_a_model_and_clears_the_rest() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).expect("the draft exists");
        assert_eq!(current.harness, Harness::Opencode);
        assert_eq!(current.program, "opencode");
        assert_eq!(current.model, "model-one");
        assert_eq!(current.effort, None);
        assert_eq!(current.agent, None);
        assert_eq!(current.profile, None);
        assert_eq!(current.permission_mode, None);
        assert_eq!(current.permission_handler, None);
        assert_eq!(current.tools, Vec::<String>::new());
        assert_eq!(current.disallowed_tools, Vec::<String>::new());
        assert_eq!(current.strict_mcp, None);
        assert_eq!(current.approval_policy, None);
        assert_eq!(current.sandbox, None);
        assert_eq!(current.auto_approve, Some(false));
        assert_eq!(settings.selected_field(), Field::Harness);
    }

    #[test]
    fn a_harness_change_takes_the_model_of_the_same_global_role() {
        let mut state = state();
        state.settings.global[0].settings.harness = Harness::Codex;
        state.settings.global[0].settings.program = "codex".to_string();
        state.settings.global[0].settings.model = "codex-primary".to_string();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).expect("the draft exists");
        assert_eq!(current.harness, Harness::Codex);
        assert_eq!(current.model, "codex-primary");
    }

    #[test]
    fn the_harness_change_notice_names_the_resets_and_disappears() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let output = text(&settings, &state, 140, 30);
        assert!(output.contains("switched to opencode"), "{output}");
        assert!(output.contains("cleared effort"), "{output}");
        assert!(output.contains("agent"), "{output}");
        assert!(output.contains("permission mode"), "{output}");
        assert!(output.contains("tools"), "{output}");

        let save = settings
            .handle_key(&state, key(KeyCode::Char('s')))
            .expect("save action");
        let Action::SaveSettings { request, .. } = save else {
            panic!("wrong save action");
        };
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::Save,
            status: SettingsResultStatus::Saved,
            revision: "rev-two".to_string(),
            message: None,
        });
        assert!(!text(&settings, &state, 140, 30).contains("switched to opencode"));

        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(text(&settings, &state, 140, 30).contains("switched to opencode"));
        settings.handle_key(&state, key(KeyCode::Esc));
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!text(&settings, &state, 140, 30).contains("switched to opencode"));
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
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
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
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Char('j')));
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
