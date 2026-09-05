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
use crate::prompts;
use crate::sock::{
    Action, PromptSource, PromptView, RoleFieldSources, SettingsOperation, SettingsResult,
    SettingsResultStatus, StateView,
};

use super::theme::THEME;

/// The line step of `PageUp` and `PageDown` in the prompt editor.
const PROMPT_PAGE_LINES: usize = 20;

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
    Prompt,
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
            Self::Prompt => "prompt",
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

/// The multi-line editor of one role prompt.
///
/// The buffer holds one string per line and a cursor as a line index and
/// a character column. `ctrl-s` sends the joined lines to the daemon
/// against the revision the edit started from.
#[derive(Debug, Clone)]
struct PromptEditor {
    role: ExecutionRole,
    /// Where the shown prompt came from when the editor opened.
    source: PromptSource,
    /// The prompt revision the edit started from. A stale result replaces
    /// it, so the next `ctrl-s` overwrites the file.
    base_revision: String,
    /// The text when the editor opened, for the change check.
    original: String,
    lines: Vec<String>,
    row: usize,
    /// The character column inside the current line.
    col: usize,
    /// True after one `Esc` on a changed buffer.
    discard_confirm: bool,
    /// The identity of this editor's in-flight save, if any. A result for
    /// another request never touches this buffer.
    request: Option<String>,
}

impl PromptEditor {
    fn new(view: &PromptView) -> Self {
        Self {
            role: view.role,
            source: view.source,
            base_revision: view.revision.clone(),
            original: view.text.clone(),
            lines: view.text.split('\n').map(str::to_string).collect(),
            row: 0,
            col: 0,
            discard_confirm: false,
            request: None,
        }
    }

    /// The buffer as one text. The split at open and this join keep the
    /// text byte for byte, a final newline included.
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn changed(&self) -> bool {
        self.text() != self.original
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map_or(0, |line| line.chars().count())
    }

    /// The byte index of character `col` in `line`, or the end.
    fn byte_at(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map_or(line.len(), |(index, _)| index)
    }

    fn insert(&mut self, character: char) {
        let line = &mut self.lines[self.row];
        let at = Self::byte_at(line, self.col);
        line.insert(at, character);
        self.col += 1;
    }

    fn newline(&mut self) {
        let line = &mut self.lines[self.row];
        let at = Self::byte_at(line, self.col);
        let rest = line.split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let at = Self::byte_at(line, self.col - 1);
            line.remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len(self.row) {
            let line = &mut self.lines[self.row];
            let at = Self::byte_at(line, self.col);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let line = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&line);
        }
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    fn right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn up(&mut self, lines: usize) {
        self.row = self.row.saturating_sub(lines);
        self.col = self.col.min(self.line_len(self.row));
    }

    fn down(&mut self, lines: usize) {
        self.row = (self.row + lines).min(self.lines.len() - 1);
        self.col = self.col.min(self.line_len(self.row));
    }
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

/// The add-repository form with one alias row and one path row.
#[derive(Debug, Clone)]
struct AddForm {
    /// The alias of the new repository.
    alias: String,
    /// The checkout path of the new repository.
    path: String,
    /// The active row: 0 for the alias, 1 for the path.
    row: usize,
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
    /// The open add-repository form, over the whole view.
    add_form: Option<AddForm>,
    /// The open prompt editor, over the whole view.
    prompt_editor: Option<PromptEditor>,
    /// The state of the one `opencode models` probe per shell start.
    models: ModelDiscovery,
    /// The notice line that a harness change leaves under the form.
    notice: Option<String>,
    errors: BTreeMap<Field, String>,
    pending_request: Option<String>,
    status: Option<(SettingsResultStatus, String)>,
    discard_confirm: bool,
    /// True after one `d` on the prompt row. The next `d` sends the reset.
    reset_confirm: bool,
    /// True after one `X` on a repository scope. The next `X` removes the
    /// repository.
    remove_repo_confirm: bool,
}

impl Settings {
    /// True while the settings view owns typed characters.
    pub fn typing(&self) -> bool {
        self.text_editor.is_some()
            || self.list_editor.is_some()
            || self.value_list.is_some()
            || self.prompt_editor.is_some()
            || self.add_form.is_some()
    }

    /// The bottom-row hints of the settings state.
    ///
    /// An open editor names its own keys and shows no `? help`. The form
    /// names the navigation keys and the keys of the active scope: the
    /// repository scope removes its override and the repository, the
    /// global scope reloads. `a` opens the add-repository form on both
    /// scopes. On the form `j` and `k` step the execution role; inside
    /// the list editor they step one row.
    pub fn footer_hints(&self) -> String {
        if self.prompt_editor.is_some() {
            return "ctrl-s save · esc close · arrows move".to_string();
        }
        if let Some(editor) = self.list_editor.as_ref() {
            if editor.row_editor.is_some() {
                return "enter apply · esc cancel".to_string();
            }
            return "j k row · a add · d delete · enter edit · esc close".to_string();
        }
        if self.text_editor.is_some() {
            return "enter apply · esc cancel".to_string();
        }
        if self.value_list.is_some() {
            return "type filter · enter apply · esc close".to_string();
        }
        if self.scope > 0 {
            return "j k role · tab field · enter open · s save · d remove · X remove repo · a add repo · ? help".to_string();
        }
        "j k role · tab field · enter open · s save · r reload · a add repo · ? help".to_string()
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
            Some(
                Action::SaveSettings { request, .. }
                | Action::ReloadSettings { request }
                | Action::SavePrompt { request, .. }
                | Action::ResetPrompt { request, .. },
            ) => self.pending_request.as_deref() == Some(request.as_str()),
            Some(_) => false,
            None => self.pending_request.is_some(),
        };
        if matches {
            self.pending_request = None;
            if let Some(editor) = self.prompt_editor.as_mut() {
                editor.request = None;
            }
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
        if matches!(
            result.operation,
            SettingsOperation::SavePrompt | SettingsOperation::ResetPrompt
        ) {
            self.observe_prompt_result(result);
            return;
        }
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

    /// Apply the result of one prompt save or reset.
    ///
    /// A save closes the editor that sent it. A stale result keeps that
    /// editor open and takes the current revision, so the next `ctrl-s`
    /// overwrites the file. Every other result keeps the editor and shows
    /// the message.
    ///
    /// A result whose request no editor sent only shows its message. A
    /// reset comes from the form, and an editor of another role holds text
    /// that nobody asked to discard.
    fn observe_prompt_result(&mut self, result: SettingsResult) {
        let message =
            result
                .message
                .clone()
                .unwrap_or_else(|| match (result.operation, result.status) {
                    (SettingsOperation::ResetPrompt, SettingsResultStatus::Saved) => {
                        "built-in prompt restored".to_string()
                    }
                    (_, SettingsResultStatus::Saved) => "prompt saved".to_string(),
                    (_, SettingsResultStatus::Stale) => {
                        "the prompt file changed on disk; repeat the action to overwrite it"
                            .to_string()
                    }
                    (_, SettingsResultStatus::Invalid) => "the prompt is invalid".to_string(),
                    _ => "the prompt request failed".to_string(),
                });
        let mine = self
            .prompt_editor
            .as_ref()
            .is_some_and(|editor| editor.request.as_deref() == Some(result.request.as_str()));
        if mine {
            match result.status {
                SettingsResultStatus::Saved => self.prompt_editor = None,
                SettingsResultStatus::Stale => {
                    if let Some(editor) = self.prompt_editor.as_mut() {
                        editor.base_revision = result.revision.clone();
                        editor.request = None;
                    }
                }
                _ => {
                    if let Some(editor) = self.prompt_editor.as_mut() {
                        editor.request = None;
                    }
                }
            }
        }
        self.status = Some((result.status, message));
    }

    /// Drop every pending confirmation of the form.
    ///
    /// The shell calls this for a key it handles itself, such as a view
    /// switch. Without it a `d` or `X` armed before the switch would
    /// remove the prompt file or the repository on the first key after
    /// the operator comes back.
    pub fn drop_confirmations(&mut self) {
        self.reset_confirm = false;
        self.discard_confirm = false;
        self.remove_repo_confirm = false;
    }

    /// Apply one Settings key and return a daemon action when needed.
    pub fn handle_key(&mut self, state: &StateView, key: KeyEvent) -> Option<Action> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return None;
        }
        if self.prompt_editor.is_some() {
            return self.handle_prompt_key(key);
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
        if self.add_form.is_some() {
            return self.handle_add_key(state, key);
        }
        self.clamp(state);
        // A second `d` on the prompt row confirms the reset, and a second
        // `X` on a repository scope confirms the removal. Any other key
        // drops the pending confirmation.
        let reset_pending = std::mem::take(&mut self.reset_confirm);
        let remove_pending = std::mem::take(&mut self.remove_repo_confirm);
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
            KeyCode::Char('a') => {
                if self.request_pending() {
                    return None;
                }
                self.add_form = Some(AddForm {
                    alias: String::new(),
                    path: String::new(),
                    row: 0,
                });
                self.status = None;
            }
            KeyCode::Char('X') if self.scope > 0 => {
                return self.remove_repository(state, remove_pending)
            }
            KeyCode::Char('d')
                if self.scope == 0 && self.selected_field_for(Some(state)) == Field::Prompt =>
            {
                return self.reset_prompt(state, reset_pending)
            }
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
        if field == Field::Prompt {
            self.open_prompt_editor(state);
            return;
        }
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

    /// The pushed prompt view of the selected role.
    fn prompt_view<'a>(&self, state: &'a StateView) -> Option<&'a PromptView> {
        let role = self.selected_role_for();
        state.settings.prompts.iter().find(|view| view.role == role)
    }

    /// Open the prompt editor on the pushed prompt of the selected role.
    ///
    /// A pending request blocks the open: its result would land on the new
    /// editor and close it, and the typed text would be gone.
    fn open_prompt_editor(&mut self, state: &StateView) {
        if self.request_pending() {
            return;
        }
        let Some(view) = self.prompt_view(state) else {
            self.status = Some((
                SettingsResultStatus::Failed,
                "the daemon sent no prompt for this role; restart the daemon".to_string(),
            ));
            return;
        };
        self.prompt_editor = Some(PromptEditor::new(view));
        self.status = None;
        self.discard_confirm = false;
    }

    /// Apply one key while the prompt editor is open.
    fn handle_prompt_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The banner says any key keeps the prompt, so every key that is
        // not `Esc` cancels the question, `ctrl-s` and an unhandled key
        // included.
        if key.code != KeyCode::Esc {
            if let Some(editor) = self.prompt_editor.as_mut() {
                editor.discard_confirm = false;
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.save_prompt();
        }
        if key.code == KeyCode::Esc {
            let changed = self
                .prompt_editor
                .as_ref()
                .is_some_and(|editor| editor.changed() && !editor.discard_confirm);
            if changed {
                if let Some(editor) = self.prompt_editor.as_mut() {
                    editor.discard_confirm = true;
                }
            } else {
                self.prompt_editor = None;
            }
            return None;
        }
        let editor = self.prompt_editor.as_mut()?;
        match key.code {
            KeyCode::Enter => editor.newline(),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            KeyCode::Up => editor.up(1),
            KeyCode::Down => editor.down(1),
            KeyCode::PageUp => editor.up(PROMPT_PAGE_LINES),
            KeyCode::PageDown => editor.down(PROMPT_PAGE_LINES),
            KeyCode::Home => editor.col = 0,
            KeyCode::End => editor.col = editor.line_len(editor.row),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                editor.insert(character);
            }
            _ => {}
        }
        None
    }

    /// Send the prompt editor buffer to the daemon.
    ///
    /// The placeholder check runs here first, so an unknown placeholder
    /// shows at once and never leaves the shell. The daemon repeats the
    /// check before it writes the file.
    fn save_prompt(&mut self) -> Option<Action> {
        if self.request_pending() {
            return None;
        }
        let editor = self.prompt_editor.as_ref()?;
        let text = editor.text();
        if let Err(error) = prompts::check(editor.role, &text) {
            self.status = Some((
                SettingsResultStatus::Invalid,
                format!("the prompt is invalid: {error:#}"),
            ));
            return None;
        }
        let (role, base_revision) = (editor.role, editor.base_revision.clone());
        let request = request_code();
        self.pending_request = Some(request.clone());
        if let Some(editor) = self.prompt_editor.as_mut() {
            editor.request = Some(request.clone());
        }
        Some(Action::SavePrompt {
            request,
            role,
            base_revision,
            text,
        })
    }

    /// Ask for a second `d`, then send the prompt reset of the selected role.
    fn reset_prompt(&mut self, state: &StateView, confirmed: bool) -> Option<Action> {
        let Some(view) = self.prompt_view(state) else {
            self.status = Some((
                SettingsResultStatus::Failed,
                "the daemon sent no prompt for this role; restart the daemon".to_string(),
            ));
            return None;
        };
        if view.source == PromptSource::Builtin {
            self.status = Some((
                SettingsResultStatus::Failed,
                "the prompt is already the built-in".to_string(),
            ));
            return None;
        }
        if !confirmed {
            self.reset_confirm = true;
            self.status = Some((
                SettingsResultStatus::Failed,
                "press d again to restore the built-in prompt".to_string(),
            ));
            return None;
        }
        if self.request_pending() {
            return None;
        }
        let request = request_code();
        self.pending_request = Some(request.clone());
        Some(Action::ResetPrompt {
            request,
            role: view.role,
            base_revision: view.revision.clone(),
        })
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
        let current_candidate = (!current.trim().is_empty())
            .then(|| current.clone())
            .into_iter()
            .collect();
        let base = catalog::join_candidates([
            self.candidates(state, harness, list_field),
            current_candidate,
        ]);
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
    /// The model comes from the same global role, else from the first row
    /// of the fixed harness table, else from the sorted candidates.
    fn apply_harness(&mut self, state: &StateView, next: Harness) {
        if self
            .current_settings_ref(state)
            .is_some_and(|settings| settings.harness == next)
        {
            return;
        }
        let role = self.selected_role_for();
        let global_model = state
            .settings
            .global
            .iter()
            .find(|value| value.role == role && value.settings.harness == next)
            .map(|value| value.settings.model.clone())
            .filter(|model| !model.is_empty());
        let mut model_candidates = self.candidates(state, next, ListField::Model);
        if next == Harness::Opencode {
            if let ModelDiscovery::Ready(models) = &self.models {
                model_candidates = catalog::join_candidates([model_candidates, models.clone()]);
            }
        }
        let model = global_model
            .or_else(|| {
                catalog::fixed_values(next, ListField::Model)
                    .into_iter()
                    .next()
            })
            .or_else(|| model_candidates.into_iter().next())
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
        // Both approving harnesses start from the safe answer: every
        // request reaches a person until the operator turns it off.
        if matches!(next, Harness::Opencode | Harness::Codex) {
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

    /// Ask for a second `X`, then send the removal of the scoped repository.
    ///
    /// The question names the repository, the active tasks that the
    /// removal stops, and the worktrees that stay on disk.
    fn remove_repository(&mut self, state: &StateView, confirmed: bool) -> Option<Action> {
        let alias = self.repositories(state).get(self.scope - 1).cloned()?;
        if !confirmed {
            if self.request_pending() {
                return None;
            }
            let count = state
                .tasks
                .iter()
                .filter(|task| task.repo == alias && !task.state.is_terminal())
                .count();
            self.remove_repo_confirm = true;
            self.status = Some((
                SettingsResultStatus::Failed,
                format!(
                    "press X again to remove {alias}: it stops {count} active task(s); \
                     worktrees stay"
                ),
            ));
            return None;
        }
        if self.request_pending() {
            return None;
        }
        let request = request_code();
        self.pending_request = Some(request.clone());
        Some(Action::SaveSettings {
            request,
            base_revision: state.settings.revision.clone(),
            edit: SettingsEdit::RemoveRepository { alias },
        })
    }

    /// Apply one key while the add-repository form is open.
    ///
    /// `Enter` moves from the alias row to the path row and sends the add
    /// on the path row. `Esc` closes the form and sends nothing. The
    /// daemon rejects an invalid alias or path, and the result shows the
    /// reason.
    fn handle_add_key(&mut self, state: &StateView, key: KeyEvent) -> Option<Action> {
        let mut close = false;
        let mut send = None;
        let form = self.add_form.as_mut()?;
        match key.code {
            KeyCode::Esc => close = true,
            KeyCode::Up | KeyCode::BackTab => form.row = form.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => form.row = (form.row + 1).min(1),
            KeyCode::Enter => {
                if form.row == 0 {
                    form.row = 1;
                } else {
                    close = true;
                    send = Some((form.alias.trim().to_string(), form.path.trim().to_string()));
                }
            }
            KeyCode::Backspace => match form.row {
                0 => {
                    form.alias.pop();
                }
                _ => {
                    form.path.pop();
                }
            },
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match form.row {
                    0 => form.alias.push(character),
                    _ => form.path.push(character),
                }
            }
            _ => {}
        }
        if close {
            self.add_form = None;
        }
        let (alias, path) = send?;
        if self.request_pending() {
            return None;
        }
        let request = request_code();
        self.pending_request = Some(request.clone());
        Some(Action::SaveSettings {
            request,
            base_revision: state.settings.revision.clone(),
            edit: SettingsEdit::AddRepository { alias, path },
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
            Harness::Codex => fields.extend([
                Field::Profile,
                Field::ApprovalPolicy,
                Field::Sandbox,
                Field::AutoApprove,
            ]),
        }
        if self.scope == 0 && self.selected_role_for().stage().is_some() {
            fields.push(Field::Limit);
        }
        // Prompts have no repository scope; the file is one per role. The
        // theory roles carry no template, so they get no prompt row.
        if self.scope == 0 && prompts::file_name(self.selected_role_for()).is_some() {
            fields.push(Field::Prompt);
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
            Field::Prompt => self.prompt_view(state).map_or_else(
                || "unavailable; restart the daemon".to_string(),
                |view| {
                    format!(
                        "{} · {} lines",
                        prompt_source_label(view.role, view.source),
                        view.text.lines().count()
                    )
                },
            ),
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
        let mut labels = vec!["global".to_string()];
        labels.extend(self.repositories(state));
        // A removal can leave the cursor past the last label. The line
        // clamps the highlight here; `handle_key` clamps the cursor
        // itself.
        let scope = self.scope.min(labels.len() - 1);
        for (index, label) in labels.iter().enumerate() {
            let style = if index == scope {
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
        } else if self.reset_confirm {
            lines.push(Line::from(Span::styled(
                "d restore the built-in prompt   any key keep the file",
                Style::default().fg(THEME.warn),
            )));
        } else if self.selected_field_for(Some(state)) == Field::Prompt {
            lines.push(Line::from(Span::styled(
                "h/l scope  j/k role  Tab field  Enter edit prompt  d restore built-in",
                THEME.dim(),
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
        if let Some(editor) = &self.prompt_editor {
            self.draw_prompt_editor(frame, area, editor);
        } else if let Some(editor) = &self.text_editor {
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
            let row_count = u16::try_from(editor.rows.len()).unwrap_or(u16::MAX);
            let height = row_count.saturating_add(5).clamp(7, 18);
            let panel = centered(64, height, area);
            frame.render_widget(Clear, panel);
            let mut lines = Vec::new();
            if !editor.filter.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(" filter: {}", editor.filter),
                    THEME.dim(),
                )));
            }
            let filter_rows = u16::from(!editor.filter.is_empty());
            let row_capacity = usize::from(panel.height.saturating_sub(3 + filter_rows).max(1));
            let start = editor
                .cursor
                .saturating_sub(row_capacity.saturating_sub(1))
                .min(editor.rows.len().saturating_sub(row_capacity));
            for (index, row) in editor
                .rows
                .iter()
                .enumerate()
                .skip(start)
                .take(row_capacity)
            {
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
        } else if let Some(form) = &self.add_form {
            let panel = centered(58, 6, area);
            frame.render_widget(Clear, panel);
            let rows = [("alias", &form.alias, 0usize), ("path", &form.path, 1)];
            let mut lines = Vec::new();
            for (label, value, index) in rows {
                let active = form.row == index;
                let cursor = if active { ">" } else { " " };
                let label_style = if active {
                    Style::default()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    THEME.dim()
                };
                lines.push(Line::from(vec![
                    Span::raw(cursor),
                    Span::raw(" "),
                    Span::styled(format!("{:<19}", label), label_style),
                    Span::raw(value.as_str()),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "enter next / send · esc cancel",
                THEME.dim(),
            )));
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" add repository ")),
                panel,
            );
        }
    }

    /// Draw the prompt editor over the whole view.
    ///
    /// The text pane shows a window of lines that keeps the cursor line
    /// visible, and a horizontal offset that keeps the cursor column
    /// visible. The cursor cell renders reversed. One status row shows the
    /// last result or the change state, and one hint row lists the keys.
    fn draw_prompt_editor(&self, frame: &mut Frame<'_>, area: Rect, editor: &PromptEditor) {
        frame.render_widget(Clear, area);
        let block = Block::bordered().title(format!(
            " prompt · {} · {} ",
            role_label(editor.role),
            prompt_source_label(editor.role, editor.source)
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
        let visible = usize::from(rows[0].height.max(1));
        let width = usize::from(rows[0].width.max(1));
        // Centre the cursor line, then clamp to the ends. A window pinned
        // to the cursor would hide every line after it.
        let last_start = editor.lines.len().saturating_sub(visible);
        let start = editor.row.saturating_sub(visible / 2).min(last_start);
        let x_offset = editor.col.saturating_sub(width - 1);
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        let mut lines = Vec::new();
        for (index, line) in editor.lines.iter().enumerate().skip(start).take(visible) {
            let chars: Vec<char> = line.chars().skip(x_offset).take(width).collect();
            if index == editor.row {
                let cursor = editor.col - x_offset;
                let before: String = chars.iter().take(cursor).collect();
                let under = chars
                    .get(cursor)
                    .map_or_else(|| " ".to_string(), char::to_string);
                let after: String = chars.iter().skip(cursor + 1).collect();
                lines.push(Line::from(vec![
                    Span::raw(before),
                    Span::styled(under, cursor_style),
                    Span::raw(after),
                ]));
            } else {
                lines.push(Line::from(chars.into_iter().collect::<String>()));
            }
        }
        frame.render_widget(Paragraph::new(lines), rows[0]);
        let status = if editor.discard_confirm {
            Line::from(Span::styled(
                "Esc again discards the changed prompt   any key keeps it",
                Style::default().fg(THEME.warn),
            ))
        } else if let Some((status, message)) = &self.status {
            let color = match status {
                SettingsResultStatus::Saved | SettingsResultStatus::Reloaded => THEME.ok,
                SettingsResultStatus::Stale
                | SettingsResultStatus::Invalid
                | SettingsResultStatus::RestartRequired
                | SettingsResultStatus::Failed => THEME.error,
            };
            Line::from(Span::styled(message.clone(), Style::default().fg(color)))
        } else if editor.changed() {
            Line::from(Span::styled("changed, not saved", THEME.dim()))
        } else {
            Line::from(Span::styled(
                "the next task of this role reads the saved file",
                THEME.dim(),
            ))
        };
        frame.render_widget(Paragraph::new(status), rows[1]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "ctrl-s save   Esc close   arrows Home End PageUp PageDown move   line {}/{} col {}",
                    editor.row + 1,
                    editor.lines.len(),
                    editor.col + 1
                ),
                THEME.dim(),
            ))),
            rows[2],
        );
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
            warnings.push(if settings.harness == Harness::Codex {
                "Codex approval checks are disabled"
            } else {
                "OpenCode approval checks are disabled"
            });
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
            Field::Prompt => 12,
        };
    }

    #[cfg(test)]
    fn prompt_editor_text(&self) -> Option<String> {
        self.prompt_editor.as_ref().map(PromptEditor::text)
    }

    #[cfg(test)]
    fn prompt_editor_cursor(&self) -> Option<(usize, usize)> {
        self.prompt_editor
            .as_ref()
            .map(|editor| (editor.row, editor.col))
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

    #[cfg(test)]
    fn add_form_row(&self) -> Option<usize> {
        self.add_form.as_ref().map(|form| form.row)
    }

    #[cfg(test)]
    fn add_form_buffers(&self) -> Option<(String, String)> {
        self.add_form
            .as_ref()
            .map(|form| (form.alias.clone(), form.path.clone()))
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

/// The source text of the prompt row: `built-in` or the prompt file path
/// under the config directory.
fn prompt_source_label(role: ExecutionRole, source: PromptSource) -> String {
    match (source, prompts::file_name(role)) {
        (PromptSource::File, Some(name)) => format!("prompts/{name}"),
        _ => "built-in".to_string(),
    }
}

fn role_label(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Refine => "refine",
        ExecutionRole::Implement => "implement",
        ExecutionRole::Review => "review",
        ExecutionRole::Release => "release",
        ExecutionRole::TicketCreate => "ticket creation",
        ExecutionRole::TicketChat => "ticket chat",
        ExecutionRole::TheoryAudit => "theory audit",
        ExecutionRole::TheoryChat => "theory chat",
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
            value.auto_approve = settings.auto_approve;
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
        Field::Harness | Field::Limit | Field::Prompt => {}
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
        ListField::Harness => Some(settings.harness.program().to_string()),
        ListField::Program => Some(settings.program.clone()),
        ListField::Model => Some(settings.model.clone()),
        ListField::Effort => settings.effort.clone(),
        ListField::Agent => settings.agent.clone(),
        ListField::Profile => settings.profile.clone(),
        ListField::PermissionMode => settings.permission_mode.clone(),
        ListField::PermissionHandler => settings.permission_handler.clone(),
        ListField::ApprovalPolicy => settings.approval_policy.clone(),
        ListField::Sandbox => settings.sandbox.clone(),
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
        ("auto approve", previous.auto_approve.is_some()),
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
        Field::Limit | Field::Prompt => &sources.harness,
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
        let role_height = u16::try_from(ExecutionRole::ALL.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let panes =
            Layout::vertical([Constraint::Length(role_height), Constraint::Min(1)]).split(area);
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
        Action, GlobalRoleSettingsView, PromptSource, PromptView, RepositoryRoleSettingsView,
        RoleFieldSources, SettingsOperation, SettingsResult, SettingsResultStatus, SettingsView,
        StateView,
    };

    /// The prompt text every test role starts with.
    const PROMPT_TEXT: &str = "line one {number}\nline two\n";

    fn prompt(role_name: ExecutionRole) -> PromptView {
        PromptView {
            role: role_name,
            source: if role_name == ExecutionRole::Refine {
                PromptSource::File
            } else {
                PromptSource::Builtin
            },
            text: PROMPT_TEXT.to_string(),
            revision: format!("prompt-rev-{}", role_name.table_name()),
        }
    }
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
            auto_approve: matches!(harness, Harness::Opencode | Harness::Codex).then_some(false),
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
            prompts: prompts::ROLES.into_iter().map(prompt).collect(),
        };
        state
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    /// Type every character of `text` into the settings view.
    fn type_text(settings: &mut Settings, state: &StateView, text: &str) {
        for character in text.chars() {
            settings.handle_key(state, key(KeyCode::Char(character)));
        }
    }

    /// A settings view with the prompt editor of `role_name` open.
    fn open_prompt(role_name: ExecutionRole, state: &StateView) -> Settings {
        let mut settings = Settings::default();
        settings.set_role(role_name);
        settings.set_field(Field::Prompt);
        assert_eq!(settings.selected_field_for(Some(state)), Field::Prompt);
        settings.handle_key(state, key(KeyCode::Enter));
        assert!(settings.typing(), "the prompt editor owns the keys");
        settings
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
                "theory audit",
                "theory chat",
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
    fn the_program_list_includes_pushed_custom_values_and_selects_the_current_value() {
        let mut state = state();
        state.settings.global[0].settings.program = "/opt/custom-claude".to_string();
        state.settings.global[3].settings.program = "claude-nightly".to_string();
        let mut settings = Settings::default();
        settings.set_field(Field::Program);
        settings.handle_key(&state, key(KeyCode::Enter));
        let rows = settings.value_list_rows();
        assert!(rows.contains(&"/opt/custom-claude".to_string()));
        assert!(rows.contains(&"claude-nightly".to_string()));
        assert_eq!(
            settings.value_list_selected(),
            Some("/opt/custom-claude".to_string())
        );
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
    fn a_long_model_list_keeps_the_marked_row_visible() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_role(ExecutionRole::Implement);
        settings.observe_models(Ok((0..30)
            .map(|index| format!("model-{index:02}"))
            .collect()));
        settings.set_field(Field::Model);
        settings.handle_key(&state, key(KeyCode::Enter));
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("> model-one"), "{output}");

        settings.handle_key(&state, key(KeyCode::Down));
        let output = text(&settings, &state, 100, 28);
        assert!(output.contains("> custom value..."), "{output}");
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
    fn a_reopened_list_selects_the_current_custom_draft_value() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Model);
        settings.replace_selected_text(&state, "provider/custom-draft");
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings
            .value_list_rows()
            .contains(&"provider/custom-draft".to_string()));
        assert_eq!(
            settings.value_list_selected(),
            Some("provider/custom-draft".to_string())
        );
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

    /// A codex role answers approvals the same two ways an opencode role
    /// does: `auto_approve = true` answers them client side, and the field
    /// left off sends each one to the inbox.
    #[test]
    fn a_codex_role_carries_the_auto_approve_row_and_its_own_warning() {
        let mut state = state();
        for value in &mut state.settings.global {
            value.settings = role(Harness::Codex);
        }
        for value in &mut state.settings.repositories {
            value.settings = role(Harness::Codex);
        }
        let settings = Settings::default();
        assert!(
            settings
                .visible_fields(&state)
                .contains(&Field::AutoApprove),
            "a codex role must offer the auto approve row"
        );
        assert_eq!(settings.warnings(&state), Vec::<&str>::new());

        for value in &mut state.settings.global {
            value.settings.auto_approve = Some(true);
        }
        for value in &mut state.settings.repositories {
            value.settings.auto_approve = Some(true);
        }
        assert_eq!(
            settings.warnings(&state),
            vec!["Codex approval checks are disabled"],
            "the warning must name codex, not opencode"
        );
    }

    #[test]
    fn a_change_to_codex_starts_from_the_supervised_auto_approve_value() {
        let mut state = state();
        for value in &mut state.settings.global {
            value.settings = role(Harness::Claude);
        }
        for value in &mut state.settings.repositories {
            value.settings = role(Harness::Claude);
        }
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Enter));
        // The value list sorts by name: claude, codex, opencode.
        settings.handle_key(&state, key(KeyCode::Down));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).expect("the draft exists");
        assert_eq!(current.harness, Harness::Codex);
        assert_eq!(current.auto_approve, Some(false));
        assert!(settings
            .visible_fields(&state)
            .contains(&Field::AutoApprove));
    }

    #[test]
    fn applying_the_current_harness_changes_nothing() {
        let state = state();
        let mut settings = Settings::default();
        let before = settings.current_settings(&state).unwrap().clone();
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(settings.current_settings(&state), Some(&before));
        assert!(!settings.dirty());
        assert!(!text(&settings, &state, 100, 28).contains("switched to"));
    }

    #[test]
    fn an_opencode_harness_change_uses_the_first_discovered_model() {
        let mut state = state();
        for value in &mut state.settings.global {
            value.settings = role(Harness::Claude);
        }
        for value in &mut state.settings.repositories {
            value.settings = role(Harness::Claude);
        }
        let mut settings = Settings::default();
        settings.observe_models(Ok(vec![
            "provider/z-model".to_string(),
            "provider/a-model".to_string(),
        ]));
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Down));
        settings.handle_key(&state, key(KeyCode::Down));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).unwrap();
        assert_eq!(current.harness, Harness::Opencode);
        assert_eq!(current.model, "provider/a-model");
    }

    #[test]
    fn a_codex_harness_change_takes_the_first_fixed_model() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Char('j')));
        settings.handle_key(&state, key(KeyCode::Enter));
        let current = settings.current_settings(&state).expect("the draft exists");
        assert_eq!(current.harness, Harness::Codex);
        assert_eq!(current.model, "gpt-5.6-sol");
        let output = text(&settings, &state, 140, 30);
        assert!(output.contains("model gpt-5.6-sol"), "{output}");
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
    fn the_harness_notice_names_a_cleared_false_auto_approve_value() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_role(ExecutionRole::Implement);
        settings.handle_key(&state, key(KeyCode::Enter));
        settings.handle_key(&state, key(KeyCode::Up));
        settings.handle_key(&state, key(KeyCode::Enter));
        let output = text(&settings, &state, 140, 30);
        assert!(output.contains("auto approve"), "{output}");
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

    // ------------------------------------------------------------------
    // The prompt of a role
    // ------------------------------------------------------------------

    #[test]
    fn the_prompt_row_is_the_last_global_field_of_every_role_and_absent_in_a_repository() {
        let state = state();
        let mut settings = Settings::default();
        for role_name in prompts::ROLES {
            settings.set_role(role_name);
            let fields = settings.visible_fields(&state);
            assert_eq!(fields.last(), Some(&Field::Prompt), "{role_name}");
        }
        settings.set_role(ExecutionRole::Refine);
        settings.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(settings.scope_label(&state), "borsuk");
        assert!(!settings.visible_fields(&state).contains(&Field::Prompt));
    }

    /// A theory role carries no prompt template, so the form shows no
    /// prompt row and `d` on it never sends a reset.
    #[test]
    fn a_theory_role_has_no_prompt_row() {
        let state = state();
        let mut settings = Settings::default();
        for role_name in [ExecutionRole::TheoryAudit, ExecutionRole::TheoryChat] {
            settings.set_role(role_name);
            let fields = settings.visible_fields(&state);
            assert!(!fields.contains(&Field::Prompt), "{role_name}: {fields:?}");
            settings.set_field(Field::Prompt);
            assert_ne!(
                settings.selected_field_for(Some(&state)),
                Field::Prompt,
                "{role_name} must not select the prompt row"
            );
            assert!(
                settings
                    .handle_key(&state, key(KeyCode::Char('d')))
                    .is_none(),
                "{role_name}"
            );
            assert!(settings.handle_key(&state, key(KeyCode::Enter)).is_none());
            assert!(!settings.typing(), "{role_name} opens no prompt editor");
        }
    }

    #[test]
    fn the_prompt_row_names_its_source_and_line_count() {
        let state = state();
        let mut settings = Settings::default();
        let output = text(&settings, &state, 100, 30);
        assert!(
            output.contains("prompts/refine.md · 2 lines"),
            "the refine prompt comes from a file: {output}"
        );
        settings.set_role(ExecutionRole::TicketChat);
        settings.set_field(Field::Prompt);
        let output = text(&settings, &state, 100, 30);
        assert!(
            output.contains("built-in · 2 lines"),
            "the ticket chat prompt is the built-in: {output}"
        );
        assert!(output.contains("Enter edit prompt  d restore built-in"));
    }

    #[test]
    fn enter_opens_the_editor_and_ctrl_s_sends_the_edited_prompt_with_its_revision() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Implement, &state);
        assert_eq!(settings.prompt_editor_text().as_deref(), Some(PROMPT_TEXT));
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 0)));

        type_text(&mut settings, &state, "Go: ");
        settings.handle_key(&state, key(KeyCode::Down));
        settings.handle_key(&state, key(KeyCode::End));
        settings.handle_key(&state, key(KeyCode::Enter));
        type_text(&mut settings, &state, "line three");
        let action = settings.handle_key(&state, ctrl('s'));
        let Some(Action::SavePrompt {
            role: role_name,
            base_revision,
            text: sent,
            ..
        }) = action
        else {
            panic!("ctrl-s must send the prompt save, got {action:?}");
        };
        assert_eq!(role_name, ExecutionRole::Implement);
        assert_eq!(base_revision, "prompt-rev-stage.implement");
        assert_eq!(sent, "Go: line one {number}\nline two\nline three\n");
        assert!(
            settings.handle_key(&state, ctrl('s')).is_none(),
            "a second ctrl-s waits for the pending result"
        );
    }

    #[test]
    fn the_editor_moves_and_edits_at_the_cursor() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Refine, &state);
        settings.handle_key(&state, key(KeyCode::Right));
        settings.handle_key(&state, key(KeyCode::Right));
        settings.handle_key(&state, key(KeyCode::Delete));
        assert_eq!(
            settings.prompt_editor_text().as_deref(),
            Some("lie one {number}\nline two\n")
        );
        settings.handle_key(&state, key(KeyCode::Backspace));
        settings.handle_key(&state, key(KeyCode::Backspace));
        settings.handle_key(&state, key(KeyCode::Backspace));
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 0)));
        settings.handle_key(&state, key(KeyCode::Down));
        settings.handle_key(&state, key(KeyCode::Home));
        settings.handle_key(&state, key(KeyCode::Backspace));
        assert_eq!(
            settings.prompt_editor_text().as_deref(),
            Some("e one {number}line two\n"),
            "backspace at a line start joins the lines"
        );
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 14)));
        settings.handle_key(&state, key(KeyCode::Left));
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.prompt_editor_text().as_deref(),
            Some("e one {number\n}line two\n")
        );
        settings.handle_key(&state, key(KeyCode::PageDown));
        assert_eq!(settings.prompt_editor_cursor(), Some((2, 0)));
        settings.handle_key(&state, key(KeyCode::Up));
        settings.handle_key(&state, key(KeyCode::End));
        settings.handle_key(&state, key(KeyCode::Right));
        assert_eq!(
            settings.prompt_editor_cursor(),
            Some((2, 0)),
            "right at a line end moves to the next line"
        );
        settings.handle_key(&state, key(KeyCode::Left));
        assert_eq!(settings.prompt_editor_cursor(), Some((1, 9)));
        settings.handle_key(&state, key(KeyCode::PageUp));
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 9)));
    }

    #[test]
    fn escape_closes_an_unchanged_editor_at_once_and_a_changed_one_after_two_presses() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Review, &state);
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.typing(), "an unchanged editor closes at once");

        let mut settings = open_prompt(ExecutionRole::Review, &state);
        type_text(&mut settings, &state, "x");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(settings.typing(), "a changed editor asks first");
        assert!(text(&settings, &state, 100, 30).contains("Esc again discards the changed prompt"));
        type_text(&mut settings, &state, "y");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(settings.typing(), "another key cancels the question");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(!settings.typing());
    }

    #[test]
    fn an_unknown_placeholder_never_leaves_the_shell() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Release, &state);
        type_text(&mut settings, &state, "{frobnicate} ");
        assert!(settings.handle_key(&state, ctrl('s')).is_none());
        let output = text(&settings, &state, 100, 30);
        assert!(output.contains("{frobnicate}"), "{output}");
        assert!(settings.typing(), "the editor stays open");
        assert!(
            settings
                .handle_key(&state, key(KeyCode::Backspace))
                .is_none()
                && settings.typing(),
            "the editor keeps taking keys after a local rejection"
        );
    }

    #[test]
    fn a_saved_result_closes_the_editor_and_a_stale_one_keeps_it_with_the_new_revision() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Refine, &state);
        type_text(&mut settings, &state, "x");
        let Some(Action::SavePrompt { request, .. }) = settings.handle_key(&state, ctrl('s'))
        else {
            panic!("ctrl-s must send the prompt save");
        };
        settings.observe_result(SettingsResult {
            request: request.clone(),
            operation: SettingsOperation::SavePrompt,
            status: SettingsResultStatus::Stale,
            revision: "prompt-rev-fresh".to_string(),
            message: Some("refine.md changed on disk".to_string()),
        });
        assert!(settings.typing(), "a stale result keeps the editor open");
        assert!(text(&settings, &state, 100, 30).contains("refine.md changed on disk"));
        let Some(Action::SavePrompt {
            request,
            base_revision,
            ..
        }) = settings.handle_key(&state, ctrl('s'))
        else {
            panic!("the retry must send the prompt save");
        };
        assert_eq!(base_revision, "prompt-rev-fresh");
        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::SavePrompt,
            status: SettingsResultStatus::Saved,
            revision: "prompt-rev-saved".to_string(),
            message: None,
        });
        assert!(!settings.typing(), "a saved result closes the editor");
        assert!(text(&settings, &state, 100, 30).contains("prompt saved"));
    }

    /// A daemon that sends no prompt leaves the row readable and every
    /// prompt key inert. The wire revision refuses such a daemon, so this
    /// is the last guard, not the first.
    #[test]
    fn a_state_without_prompts_shows_the_row_and_refuses_every_prompt_key() {
        let mut state = state();
        state.settings.prompts.clear();
        let mut settings = Settings::default();
        settings.set_field(Field::Prompt);
        assert_eq!(settings.selected_field_for(Some(&state)), Field::Prompt);
        assert!(text(&settings, &state, 100, 30).contains("unavailable; restart the daemon"));

        assert!(settings.handle_key(&state, key(KeyCode::Enter)).is_none());
        assert!(!settings.typing(), "no editor opens without a prompt");
        assert!(text(&settings, &state, 100, 30).contains("the daemon sent no prompt"));

        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        assert!(text(&settings, &state, 100, 30).contains("the daemon sent no prompt"));
    }

    /// A result belongs to the editor that sent it. A result for another
    /// request must never close a buffer full of typed text.
    #[test]
    fn a_result_of_another_request_leaves_the_open_editor_alone() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Prompt);
        settings.handle_key(&state, key(KeyCode::Char('d')));
        let Some(Action::ResetPrompt { request, .. }) =
            settings.handle_key(&state, key(KeyCode::Char('d')))
        else {
            panic!("the second d must send the reset");
        };

        // The reset is in flight, so the row refuses to open an editor.
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(!settings.typing(), "a pending request blocks the editor");
        assert!(text(&settings, &state, 100, 30).contains("a settings request is pending"));

        settings.observe_result(SettingsResult {
            request,
            operation: SettingsOperation::ResetPrompt,
            status: SettingsResultStatus::Saved,
            revision: "prompt-rev-fresh".to_string(),
            message: None,
        });

        // A stale result of a foreign request touches neither the buffer
        // nor the revision the next ctrl-s carries.
        let mut settings = open_prompt(ExecutionRole::Implement, &state);
        type_text(&mut settings, &state, "keep me ");
        settings.observe_result(SettingsResult {
            request: "someone-else".to_string(),
            operation: SettingsOperation::SavePrompt,
            status: SettingsResultStatus::Stale,
            revision: "refine-revision".to_string(),
            message: Some("refine.md changed on disk".to_string()),
        });
        assert!(settings.typing(), "the editor stays open");
        assert_eq!(
            settings.prompt_editor_text().as_deref(),
            Some("keep me line one {number}\nline two\n")
        );
        let Some(Action::SavePrompt { base_revision, .. }) = settings.handle_key(&state, ctrl('s'))
        else {
            panic!("ctrl-s must send the prompt save");
        };
        assert_eq!(base_revision, "prompt-rev-stage.implement");
    }

    /// A failed delivery frees the editor, so the next `ctrl-s` sends.
    #[test]
    fn a_failed_delivery_frees_the_prompt_editor() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Review, &state);
        type_text(&mut settings, &state, "x");
        let action = settings
            .handle_key(&state, ctrl('s'))
            .expect("ctrl-s must send the prompt save");
        settings.delivery_failed(Some(&action));
        assert!(settings.typing(), "the editor keeps the text");
        assert!(text(&settings, &state, 100, 30).contains("was not delivered"));
        assert!(
            settings.handle_key(&state, ctrl('s')).is_some(),
            "the retry sends again"
        );
    }

    /// The banner promises that any key keeps the prompt. A key the editor
    /// does not act on must keep that promise.
    #[test]
    fn an_unhandled_key_cancels_the_discard_question() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Review, &state);
        type_text(&mut settings, &state, "x");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(text(&settings, &state, 100, 30).contains("Esc again discards"));
        settings.handle_key(&state, key(KeyCode::Insert));
        assert!(
            !text(&settings, &state, 100, 30).contains("Esc again discards"),
            "an unhandled key cancels the question"
        );
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(settings.typing(), "the editor asks again");

        // A local ctrl-s rejection also cancels the question.
        type_text(&mut settings, &state, "{frobnicate}");
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(text(&settings, &state, 100, 30).contains("Esc again discards"));
        assert!(settings.handle_key(&state, ctrl('s')).is_none());
        settings.handle_key(&state, key(KeyCode::Esc));
        assert!(settings.typing(), "the editor asks again after ctrl-s");
    }

    /// The editor window shows the lines after the cursor, so a long
    /// prompt stays readable.
    #[test]
    fn the_editor_window_shows_the_lines_after_the_cursor() {
        let mut state = state();
        let body = (1..=40)
            .map(|line| format!("line{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        for view in &mut state.settings.prompts {
            view.text = body.clone();
        }
        let mut settings = open_prompt(ExecutionRole::Refine, &state);
        for _ in 0..20 {
            settings.handle_key(&state, key(KeyCode::Down));
        }
        let output = text(&settings, &state, 40, 20);
        assert!(output.contains("line21"), "the cursor line: {output}");
        assert!(
            output.contains("line25"),
            "a line after the cursor: {output}"
        );
        assert!(
            output.contains("line17"),
            "a line before the cursor: {output}"
        );

        // The top of a prompt starts at its first line, not centred.
        let settings = open_prompt(ExecutionRole::Refine, &state);
        assert!(text(&settings, &state, 40, 20).contains("line01"));
    }

    #[test]
    fn d_on_the_prompt_row_asks_twice_and_then_sends_the_reset() {
        let state = state();
        let mut settings = Settings::default();
        settings.set_field(Field::Prompt);
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        assert!(text(&settings, &state, 100, 30).contains("press d again to restore"));
        let action = settings.handle_key(&state, key(KeyCode::Char('d')));
        assert_eq!(
            action.map(|action| match action {
                Action::ResetPrompt {
                    role: role_name,
                    base_revision,
                    ..
                } => (role_name, base_revision),
                other => panic!("unexpected action {other:?}"),
            }),
            Some((ExecutionRole::Refine, "prompt-rev-stage.refine".to_string()))
        );

        let mut settings = Settings::default();
        settings.set_field(Field::Prompt);
        settings.handle_key(&state, key(KeyCode::Char('d')));
        settings.handle_key(&state, key(KeyCode::Tab));
        settings.handle_key(&state, key(KeyCode::BackTab));
        assert!(
            settings
                .handle_key(&state, key(KeyCode::Char('d')))
                .is_none(),
            "another key drops the pending confirmation"
        );

        let mut settings = Settings::default();
        settings.set_role(ExecutionRole::Implement);
        settings.set_field(Field::Prompt);
        settings.handle_key(&state, key(KeyCode::Char('d')));
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('d')))
            .is_none());
        assert!(text(&settings, &state, 100, 30).contains("already the built-in"));
    }

    /// The editor keeps a template byte for byte through an open, a move,
    /// and a save, and its cursor math counts characters, not bytes.
    #[test]
    fn the_editor_keeps_multibyte_text_byte_for_byte() {
        let mut state = state();
        let text_with_accents = "napraw zażółć #{number}\nkoniec\n";
        for view in &mut state.settings.prompts {
            view.text = text_with_accents.to_string();
        }
        let mut settings = open_prompt(ExecutionRole::Implement, &state);
        settings.handle_key(&state, key(KeyCode::End));
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 23)));
        settings.handle_key(&state, key(KeyCode::Left));
        settings.handle_key(&state, key(KeyCode::Backspace));
        assert_eq!(
            settings.prompt_editor_text().as_deref(),
            Some("napraw zażółć #{numbe}\nkoniec\n")
        );
        settings.handle_key(&state, key(KeyCode::Home));
        for character in "łąka ".chars() {
            settings.handle_key(&state, key(KeyCode::Char(character)));
        }
        assert_eq!(settings.prompt_editor_cursor(), Some((0, 5)));

        let mut settings = open_prompt(ExecutionRole::Implement, &state);
        let Some(Action::SavePrompt { text: sent, .. }) = settings.handle_key(&state, ctrl('s'))
        else {
            panic!("ctrl-s on an unchanged prompt must still send it");
        };
        assert_eq!(sent, text_with_accents, "the round trip changes no byte");
    }

    /// A one-cell pane draws the editor without a panic, and so does a pane
    /// narrower than the cursor column.
    #[test]
    fn the_prompt_editor_draws_in_a_tiny_pane() {
        let mut state = state();
        for view in &mut state.settings.prompts {
            view.text = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbb\ncc\ndd\n".to_string();
        }
        let mut settings = open_prompt(ExecutionRole::Refine, &state);
        settings.handle_key(&state, key(KeyCode::End));
        settings.handle_key(&state, key(KeyCode::PageDown));
        settings.handle_key(&state, key(KeyCode::Up));
        for (width, height) in [(1, 1), (2, 3), (4, 2), (20, 5), (100, 30)] {
            text(&settings, &state, width, height);
        }
    }

    #[test]
    fn the_prompt_editor_draws_the_text_the_source_and_the_keys() {
        let state = state();
        let mut settings = open_prompt(ExecutionRole::Refine, &state);
        let output = text(&settings, &state, 100, 30);
        assert!(
            output.contains("prompt · refine · prompts/refine.md"),
            "{output}"
        );
        assert!(output.contains("line one {number}"));
        assert!(output.contains("line two"));
        assert!(output.contains("ctrl-s save   Esc close"));
        assert!(output.contains("line 1/3 col 1"));
        type_text(&mut settings, &state, "!");
        assert!(text(&settings, &state, 100, 30).contains("changed, not saved"));
    }

    #[test]
    fn the_footer_hints_follow_the_scope_and_the_open_editors() {
        let state = state();
        let mut settings = Settings::default();
        assert_eq!(
            settings.footer_hints(),
            "j k role · tab field · enter open · s save · r reload · a add repo · ? help"
        );

        settings.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(settings.scope, 1);
        assert_eq!(
            settings.footer_hints(),
            "j k role · tab field · enter open · s save · d remove · X remove repo · a add repo · ? help"
        );

        settings.scope = 0;
        settings.handle_key(&state, key(KeyCode::Enter));
        assert!(settings.value_list.is_some());
        assert_eq!(
            settings.footer_hints(),
            "type filter · enter apply · esc close"
        );
        settings.handle_key(&state, key(KeyCode::Esc));

        settings.text_editor = Some(TextEditor {
            field: Field::Program,
            buffer: String::new(),
        });
        assert_eq!(settings.footer_hints(), "enter apply · esc cancel");
        settings.text_editor = None;

        settings.list_editor = Some(ListEditor {
            field: Field::ExtraArgs,
            selected: 0,
            rows: Vec::new(),
            row_editor: None,
        });
        assert_eq!(
            settings.footer_hints(),
            "j k row · a add · d delete · enter edit · esc close"
        );
        settings.list_editor = Some(ListEditor {
            field: Field::ExtraArgs,
            selected: 0,
            rows: vec!["--verbose".to_string()],
            row_editor: Some(String::new()),
        });
        assert_eq!(settings.footer_hints(), "enter apply · esc cancel");

        for hint in [
            "j k role · tab field · enter open · s save · r reload · a add repo · ? help",
            "j k role · tab field · enter open · s save · d remove · X remove repo · a add repo · ? help",
            "j k row · a add · d delete · enter edit · esc close",
            "type filter · enter apply · esc close",
            "enter apply · esc cancel",
        ] {
            assert!(hint.chars().count() <= crate::tui::HINT_CAP, "hint {hint}");
        }
    }

    // ------------------------------------------------------------------
    // Add and remove a repository
    // ------------------------------------------------------------------

    /// `a` opens the add form, the rows take the typed text, and the last
    /// `Enter` sends the add against the revision the view holds.
    #[test]
    fn the_add_form_sends_add_repository_with_the_typed_values() {
        let state = state();
        let mut settings = Settings::default();
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('a')))
            .is_none());
        assert!(settings.typing(), "the add form owns the keys");
        assert_eq!(settings.add_form_row(), Some(0));

        type_text(&mut settings, &state, "hazel");
        settings.handle_key(&state, key(KeyCode::Enter));
        assert_eq!(
            settings.add_form_row(),
            Some(1),
            "enter moves to the path row"
        );
        type_text(&mut settings, &state, "/tmp/hazel");
        assert_eq!(
            settings.add_form_buffers(),
            Some(("hazel".to_string(), "/tmp/hazel".to_string()))
        );

        let action = settings
            .handle_key(&state, key(KeyCode::Enter))
            .expect("the last enter sends the add");
        let Action::SaveSettings {
            request,
            base_revision,
            edit,
        } = action
        else {
            panic!("wrong action");
        };
        assert!(!request.is_empty());
        assert_eq!(base_revision, "rev-one");
        assert_eq!(
            edit,
            SettingsEdit::AddRepository {
                alias: "hazel".to_string(),
                path: "/tmp/hazel".to_string()
            }
        );
        assert_eq!(settings.add_form_row(), None, "the form closes");
        assert!(!settings.typing());
    }

    /// `Esc` closes the add form and sends nothing.
    #[test]
    fn esc_closes_the_add_form_and_sends_nothing() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('a')));
        type_text(&mut settings, &state, "hazel");
        assert!(settings.handle_key(&state, key(KeyCode::Esc)).is_none());
        assert!(!settings.typing());
        assert_eq!(settings.add_form_row(), None);
    }

    /// Up, Down, Tab, and BackTab switch the two rows of the add form.
    #[test]
    fn the_add_form_moves_between_the_alias_and_the_path_row() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('a')));
        settings.handle_key(&state, key(KeyCode::Tab));
        assert_eq!(settings.add_form_row(), Some(1));
        settings.handle_key(&state, key(KeyCode::Down));
        assert_eq!(
            settings.add_form_row(),
            Some(1),
            "the step clamps at the last row"
        );
        settings.handle_key(&state, key(KeyCode::Up));
        settings.handle_key(&state, key(KeyCode::BackTab));
        assert_eq!(
            settings.add_form_row(),
            Some(0),
            "the step clamps at the first row"
        );
    }

    /// The add form draws both rows with their text and the key hint.
    #[test]
    fn the_add_form_draws_the_rows_and_the_hint() {
        let state = state();
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('a')));
        type_text(&mut settings, &state, "hazel");
        settings.handle_key(&state, key(KeyCode::Tab));
        type_text(&mut settings, &state, "/tmp/hazel");
        let output = text(&settings, &state, 100, 30);
        assert!(output.contains("add repository"), "{output}");
        assert!(output.contains("alias"), "{output}");
        assert!(output.contains("path"), "{output}");
        assert!(output.contains("hazel"), "{output}");
        assert!(output.contains("/tmp/hazel"), "{output}");
        assert!(output.contains("enter next / send"), "{output}");
        assert!(output.contains("esc cancel"), "{output}");
    }

    /// `X` on a repository scope asks once with the active task count,
    /// then removes the repository. Any other key drops the question.
    #[test]
    fn x_on_a_repository_scope_asks_then_removes_the_repository() {
        let mut state = state();
        let mut active = state.tasks[0].clone();
        active.state = crate::tasks::TaskState::Running;
        let mut done = state.tasks[1].clone();
        done.state = crate::tasks::TaskState::Done;
        state.tasks = vec![active, done];
        let mut settings = Settings::default();
        settings.handle_key(&state, key(KeyCode::Char('l')));
        assert_eq!(settings.scope_label(&state), "borsuk");

        assert!(settings
            .handle_key(&state, key(KeyCode::Char('X')))
            .is_none());
        let output = text(&settings, &state, 120, 30);
        assert!(
            output.contains("press X again to remove borsuk"),
            "{output}"
        );
        assert!(output.contains("it stops 1 active task(s)"), "{output}");
        assert!(output.contains("worktrees stay"), "{output}");

        settings.handle_key(&state, key(KeyCode::Tab));
        assert!(
            settings
                .handle_key(&state, key(KeyCode::Char('X')))
                .is_none(),
            "another key dropped the confirmation, so X asks again"
        );

        let action = settings
            .handle_key(&state, key(KeyCode::Char('X')))
            .expect("the second X removes the repository");
        let Action::SaveSettings {
            base_revision,
            edit: SettingsEdit::RemoveRepository { alias },
            ..
        } = action
        else {
            panic!("wrong action");
        };
        assert_eq!(alias, "borsuk");
        assert_eq!(base_revision, "rev-one");
    }

    /// The global scope removes nothing: `X` acts only on a repository.
    #[test]
    fn x_on_the_global_scope_does_nothing() {
        let state = state();
        let mut settings = Settings::default();
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('X')))
            .is_none());
        assert!(
            !text(&settings, &state, 100, 30).contains("press X again"),
            "the global scope shows no removal question"
        );
    }

    /// A pending settings request blocks both new keys like every other
    /// request, and the status says so.
    #[test]
    fn a_pending_request_blocks_the_add_and_remove_keys() {
        let state = state();
        let mut settings = Settings::default();
        settings.reload().unwrap();
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('a')))
            .is_none());
        assert_eq!(settings.add_form_row(), None, "the form stays closed");
        assert!(text(&settings, &state, 100, 30).contains("a settings request is pending"));

        settings.handle_key(&state, key(KeyCode::Char('l')));
        assert!(settings
            .handle_key(&state, key(KeyCode::Char('X')))
            .is_none());
        assert!(text(&settings, &state, 100, 30).contains("a settings request is pending"));
        assert!(
            settings
                .handle_key(&state, key(KeyCode::Char('X')))
                .is_none(),
            "the first X never armed, so the second X cannot send"
        );
    }

    /// A removal can shrink the scope list under the cursor. The scope
    /// line clamps the highlight onto the last label.
    #[test]
    fn the_scope_line_clamps_the_highlight_after_a_removal() {
        let state = state();
        let mut settings = Settings {
            scope: 5,
            ..Default::default()
        };
        let bold = |line: Line<'static>| -> Vec<String> {
            line.spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .map(|span| span.content.to_string())
                .collect()
        };
        assert_eq!(
            bold(settings.scope_line(&state)),
            vec!["< borsuk > ".to_string()],
            "the last label takes the highlight"
        );

        settings.scope = 0;
        assert_eq!(
            bold(settings.scope_line(&state)),
            vec!["< global > ".to_string()]
        );
    }
}
