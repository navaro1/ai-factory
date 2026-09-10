//! The Theory view: what the governor knows about each repository.
//!
//! The view draws one block per repository: the header strip with the
//! governor state and the counts, then the AREAS panel with the tier each
//! area reaches, then the HOLDS panel with the items the governor holds,
//! then the DELTAS panel with the deltas the reviews reported. The
//! operator moves the cursor with `j` and `k`. On a repository row `v`
//! asks for the run skill of one surface. On an area row `t` asks the
//! agent to teach that area. On a repository row `e` opens
//! `theory/model.toml` in the operator's editor. On a hold row that names
//! an area with no entries `b` starts the bootstrap chat of that area and
//! opens its session view in place of the panels.

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::sock::{
    Action, AreaView, ChatPurpose, DeltaState, DeltaView, HoldView, ModelPath, StateView,
    TheoryAction, TheoryView,
};
use crate::tasks::TeachKey;
use crate::theory::model;
use crate::theory::records::{empty_area, names_empty_area, MODEL_FILE};
use crate::theory::verify::Tier;

use super::editor::EditorOutcome;
use super::session::SessionView;
use super::theme::THEME;

/// The separator of the header strip and the area rows.
const DOT: &str = " · ";

/// What one key did in the Theory view.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome {
    /// The key changed only the local state.
    None,
    /// The view took no interest in the key, so the shell owns it.
    Pass,
    /// The key produced no action, and the operator must read a reason.
    Reject(String),
    /// The key produced one action and the toast that announces it.
    Send(Box<Action>, String),
}

/// One stop of the Theory cursor.
///
/// Each governed repository contributes its header row, one row per area,
/// and one row per delta, in draw order. A delta that draws several miss
/// rows is one stop. An ungoverned repository draws no panel, so it
/// contributes its header row alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stop {
    /// The header row of one repository.
    Repo(String),
    /// One area row of one repository.
    Area(String, String),
    /// One hold row of one repository, named by its item number.
    Hold(String, u64),
    /// The delta of one pull request of one repository.
    Delta(String, u64),
}

impl Stop {
    /// The repository alias of the stop.
    fn repo(&self) -> &str {
        match self {
            Stop::Repo(alias)
            | Stop::Area(alias, _)
            | Stop::Hold(alias, _)
            | Stop::Delta(alias, _) => alias,
        }
    }
}

/// The stops of one state view, in draw order.
fn stops(state: &StateView) -> Vec<Stop> {
    let mut all = Vec::new();
    for (alias, view) in &state.theory {
        all.push(Stop::Repo(alias.clone()));
        if !view.governor {
            continue;
        }
        for area in &view.areas {
            all.push(Stop::Area(alias.clone(), area.id.clone()));
        }
        for hold in &view.holds {
            all.push(Stop::Hold(alias.clone(), hold.number));
        }
        for delta in &view.deltas {
            all.push(Stop::Delta(alias.clone(), delta.number));
        }
    }
    all
}

/// The Theory view state.
///
/// It holds no repository data. The marked stop survives a state push, and
/// the view falls back to the first stop when the marked one is gone.
#[derive(Debug, Default)]
pub(super) struct Theory {
    /// The stop the operator marked.
    marked: Option<Stop>,
    /// The surface name typed so far, while the input is open.
    input: Option<String>,
    /// The identity of the edit-model request this UI sent.
    ///
    /// The daemon pushes the reply to every connected UI, so only the UI
    /// that holds the matching identity opens an editor.
    pending_edit: Option<String>,
    /// The transcript and the input of the open bootstrap chat.
    chat: SessionView,
    /// The repository and the area of the open bootstrap chat.
    chat_key: Option<(String, String)>,
}

impl Theory {
    /// True while the surface input or the chat input holds the keyboard.
    pub(super) fn typing(&self) -> bool {
        self.input.is_some() || self.chat_key.is_some()
    }

    /// True when the open bootstrap chat has a transcript to poll.
    pub(super) fn needs_poll(&self) -> bool {
        self.chat_key.is_some() && self.chat.task_id().is_some()
    }

    /// Follow the open bootstrap chat task from the daemon state.
    pub(super) fn observe_state(&mut self, state: &StateView) {
        let Some((repo, area)) = self.chat_key.as_ref() else {
            return;
        };
        let id = crate::tasks::bootstrap_id(repo, area);
        if let Some(task) = state.tasks.iter().find(|task| task.id == id) {
            self.chat.show(task);
        } else if self.chat.task_id().is_some() {
            self.chat.clear();
        }
    }

    /// Read new bootstrap chat log data before one draw.
    pub(super) fn on_redraw(&mut self, now: Instant) {
        if self.chat.task_id().is_some() {
            self.chat.on_redraw(now);
        }
    }

    /// Read new bootstrap chat log data at the session poll interval.
    pub(super) fn poll(&mut self, now: Instant) -> bool {
        self.chat.task_id().is_some() && self.chat.poll(now)
    }

    /// The stop the keys act on: the marked one, else the first one.
    fn at(&self, state: &StateView) -> Option<Stop> {
        let all = stops(state);
        match self.marked.as_ref().filter(|stop| all.contains(stop)) {
            Some(stop) => Some(stop.clone()),
            None => all.into_iter().next(),
        }
    }

    /// The alias the keys act on: the repository of the marked stop.
    fn current(&self, state: &StateView) -> Option<String> {
        self.at(state).map(|stop| stop.repo().to_string())
    }

    /// The key hints of the footer.
    pub(super) fn footer_hints(&self) -> String {
        if self.chat_key.is_some() {
            return format!("type the message{DOT}enter send{DOT}esc back to theory");
        }
        match &self.input {
            Some(buffer) => format!("surface: {buffer}_{DOT}enter send{DOT}esc cancel"),
            None => format!(
                "1-6 view{DOT}j/k row{DOT}v run skill{DOT}t teach{DOT}b bootstrap{DOT}e model{DOT}esc home"
            ),
        }
    }

    /// Handle one key while the Theory view is open.
    pub(super) fn handle_key(&mut self, state: &StateView, key: KeyEvent) -> Outcome {
        if self.chat_key.is_some() {
            if key.code == KeyCode::Esc {
                self.chat_key = None;
                self.chat.clear();
                return Outcome::None;
            }
            return match self.chat.handle_key(key, 10) {
                Some(action) => {
                    let toast = match &action {
                        Action::Chat { task, .. } => format!("sent chat {task}"),
                        Action::Abort { task } => format!("sent abort {task}"),
                        _ => "sent".to_string(),
                    };
                    Outcome::Send(Box::new(action), toast)
                }
                None => Outcome::None,
            };
        }
        if self.input.is_some() {
            return self.typing_key(state, key);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_mark(state, 1);
                Outcome::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_mark(state, -1);
                Outcome::None
            }
            KeyCode::Char('v') => {
                if self.current(state).is_some() {
                    self.input = Some(String::new());
                }
                Outcome::None
            }
            KeyCode::Char('t') => self.send_teach(state),
            KeyCode::Char('b') => self.send_bootstrap(state),
            KeyCode::Char('e') => self.send_edit_model(state),
            _ => Outcome::Pass,
        }
    }

    /// Handle one key while the surface input holds the keyboard.
    ///
    /// Escape closes the input. Enter sends the setup action for the marked
    /// repository and the typed surface; an empty surface sends nothing.
    fn typing_key(&mut self, state: &StateView, key: KeyEvent) -> Outcome {
        let allowed = match key.code {
            KeyCode::Char(_) => key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT,
            _ => key.modifiers.is_empty(),
        };
        if !allowed {
            return Outcome::None;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                Outcome::None
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.pop();
                }
                Outcome::None
            }
            KeyCode::Char(character) => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.push(character);
                }
                Outcome::None
            }
            KeyCode::Enter => self.send_setup(state),
            _ => Outcome::None,
        }
    }

    /// Close the input and send the setup action it names.
    ///
    /// A surface that is not a plain directory name sends nothing and
    /// reports the reason, because the surface becomes the directory
    /// `run-<surface>` of the skills checkout.
    fn send_setup(&mut self, state: &StateView) -> Outcome {
        let surface = self.input.take().unwrap_or_default().trim().to_string();
        let Some(repo) = self.current(state) else {
            return Outcome::None;
        };
        if surface.is_empty() {
            return Outcome::None;
        }
        if !plain_name(&surface) {
            return Outcome::Reject(format!(
                "the surface {surface} takes letters, digits, - and _ only"
            ));
        }
        let toast = format!("asked for the run skill of {repo}/{surface}");

        Outcome::Send(
            Box::new(Action::Theory(TheoryAction::Setup { repo, surface })),
            toast,
        )
    }

    /// Send the teach action of the marked area row.
    ///
    /// A repository row names no area, so it sends nothing.
    fn send_teach(&self, state: &StateView) -> Outcome {
        let Some(Stop::Area(repo, id)) = self.at(state) else {
            return Outcome::None;
        };
        let toast = format!("asked to teach {repo}/{id}");
        Outcome::Send(
            Box::new(Action::Theory(TheoryAction::Teach {
                repo,
                key: TeachKey::Area(id),
            })),
            toast,
        )
    }

    /// Start or resume the bootstrap chat of the marked hold row.
    ///
    /// Only a hold that names an area with no entries answers `b`, because
    /// bootstrapping fixes nothing else. The chat opens in place of the
    /// panels, and [`Theory::observe_state`] shows its task as soon as the
    /// daemon pushes it.
    fn send_bootstrap(&mut self, state: &StateView) -> Outcome {
        let Some(Stop::Hold(repo, number)) = self.at(state) else {
            return Outcome::None;
        };
        let Some(area) = state
            .theory
            .get(&repo)
            .and_then(|view| view.holds.iter().find(|hold| hold.number == number))
            .and_then(|hold| empty_area(&hold.reason))
            .map(str::to_string)
        else {
            return Outcome::None;
        };
        self.chat_key = Some((repo.clone(), area.clone()));
        self.chat.clear();
        let id = crate::tasks::bootstrap_id(&repo, &area);
        if let Some(task) = state.tasks.iter().find(|task| task.id == id) {
            self.chat.show(task);
        }
        let toast = format!("asked to bootstrap {repo}/{area}");
        Outcome::Send(
            Box::new(Action::Theory(TheoryAction::Chat {
                request: uuid::Uuid::new_v4().to_string(),
                repo,
                purpose: ChatPurpose::Bootstrap,
                key: area,
            })),
            toast,
        )
    }

    /// Ask the daemon for the model worktree of the marked repository.
    ///
    /// An area row names one area, not the repository model, so it sends
    /// nothing. The identity of the request stays here until the reply
    /// arrives.
    fn send_edit_model(&mut self, state: &StateView) -> Outcome {
        let Some(Stop::Repo(repo)) = self.at(state) else {
            return Outcome::None;
        };
        let request = uuid::Uuid::new_v4().to_string();
        self.pending_edit = Some(request.clone());
        let toast = format!("opening the model of {repo}");
        Outcome::Send(
            Box::new(Action::Theory(TheoryAction::EditModel { request, repo })),
            toast,
        )
    }

    /// Edit the model of one repository and ask the daemon to commit it.
    ///
    /// A reply this UI did not ask for does nothing. `edit` runs the
    /// operator's editor over `theory/model.toml` in the worktree the
    /// daemon prepared. A file that does not parse sends nothing and
    /// reports the entry and the reason.
    pub(super) fn observe_model_path(
        &mut self,
        view: &ModelPath,
        edit: impl FnOnce(&Path) -> Result<EditorOutcome>,
    ) -> Outcome {
        if self.pending_edit.as_deref() != Some(view.request.as_str()) {
            return Outcome::None;
        }
        self.pending_edit = None;
        let file = view.path.join(MODEL_FILE);
        match edit(&file) {
            Err(error) => Outcome::Reject(format!("cannot edit {}: {error:#}", file.display())),
            Ok(EditorOutcome::Failed(reason)) => Outcome::Reject(reason),
            Ok(EditorOutcome::Unchanged) => {
                Outcome::Reject(format!("the model of {} did not change", view.repo))
            }
            Ok(EditorOutcome::Saved) => match fs::read_to_string(&file) {
                Err(error) => Outcome::Reject(format!("cannot read {}: {error}", file.display())),
                Ok(text) => match model::parse(&text) {
                    Err(error) => Outcome::Reject(format!("{MODEL_FILE}: {error}")),
                    Ok(_) => Outcome::Send(
                        Box::new(Action::Theory(TheoryAction::CommitModel {
                            repo: view.repo.clone(),
                        })),
                        format!("asked to commit the model of {}", view.repo),
                    ),
                },
            },
        }
    }

    /// Move the mark by `delta` rows, without wrapping.
    fn move_mark(&mut self, state: &StateView, delta: isize) {
        let all = stops(state);
        if all.is_empty() {
            return;
        }
        let at = self
            .at(state)
            .and_then(|stop| all.iter().position(|one| *one == stop))
            .unwrap_or(0);
        let next = (at as isize + delta).clamp(0, all.len() as isize - 1) as usize;
        self.marked = Some(all[next].clone());
    }
}

/// True when `surface` is a plain directory name.
///
/// The name becomes the directory `run-<surface>`, so a slash, a space,
/// and a dot all stay out of it.
fn plain_name(surface: &str) -> bool {
    surface
        .chars()
        .all(|one| one.is_ascii_alphanumeric() || one == '-' || one == '_')
}

/// Draw the Theory view.
///
/// An open bootstrap chat replaces the panels, so the operator reads the
/// interview and the panels do not compete with it for rows.
pub(super) fn draw(f: &mut Frame, area: Rect, state: &StateView, view: &mut Theory) {
    if let Some((repo, id)) = view.chat_key.clone() {
        let block = Block::bordered().title(format!(" bootstrap {repo}/{id} "));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if view.chat.task_id().is_some() {
            view.chat.draw(f, inner, &[], &state.usage);
        } else {
            f.render_widget(
                Paragraph::new("… pending: the bootstrap chat starts.").style(THEME.dim()),
                inner,
            );
        }
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(THEME.dim())
        .title(Span::styled(
            " theory ",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let at = view.at(state);
    let marked = at.as_ref().map(|stop| stop.repo().to_string());
    let mut lines: Vec<Line> = Vec::new();
    for (alias, row) in &state.theory {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let here = marked.as_deref() == Some(alias.as_str());
        lines.push(Line::from(Span::styled(
            format!("{} {alias}", if here { ">" } else { " " }),
            Style::default().fg(THEME.repo).add_modifier(Modifier::BOLD),
        )));
        lines.push(strip(row));
        if !row.governor {
            continue;
        }
        lines.push(Line::from(Span::styled("AREAS", THEME.dim())));
        if row.areas.is_empty() {
            lines.push(Line::from(Span::styled("no area", THEME.dim())));
        }
        for one in &row.areas {
            let here = at
                .as_ref()
                .is_some_and(|stop| *stop == Stop::Area(alias.clone(), one.id.clone()));
            lines.push(area_row(one, here));
        }
        if !row.holds.is_empty() {
            lines.push(Line::from(Span::styled("HOLDS", THEME.dim())));
            lines.extend(row.holds.iter().map(|hold| {
                let here = at
                    .as_ref()
                    .is_some_and(|stop| *stop == Stop::Hold(alias.clone(), hold.number));
                hold_row(hold, here)
            }));
        }
        if row.deltas.is_empty() {
            continue;
        }
        lines.push(Line::from(Span::styled("DELTAS", THEME.dim())));
        for one in &row.deltas {
            let here = at
                .as_ref()
                .is_some_and(|stop| *stop == Stop::Delta(alias.clone(), one.number));
            lines.extend(delta_rows(one, here));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("no repository", THEME.dim())));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The header strip of one repository: the governor state and the counts,
/// or the theory read error.
fn strip(view: &TheoryView) -> Line<'static> {
    let (word, color) = if view.governor {
        ("GOVERNOR ON", THEME.ok)
    } else {
        ("GOVERNOR OFF", THEME.dim)
    };
    let mut spans = vec![Span::styled(word, Style::default().fg(color))];
    if !view.governor {
        return Line::from(spans);
    }
    spans.push(Span::styled(DOT, THEME.dim()));
    if view.error.is_empty() {
        spans.push(Span::styled(
            format!(
                "ENTRIES {}{DOT}AREAS {}",
                view.model.entries.len(),
                view.areas.len()
            ),
            Style::default().fg(THEME.text),
        ));
    } else {
        spans.push(Span::styled(
            view.error.clone(),
            Style::default().fg(THEME.error),
        ));
    }
    Line::from(spans)
}

/// One row of the AREAS panel: the area id and its tier mark.
fn area_row(row: &AreaView, is_selected: bool) -> Line<'static> {
    let mark = mark(row);
    let color = match mark.as_str() {
        "!" => THEME.error,
        "-" => THEME.dim,
        _ => THEME.accent,
    };
    let marker = if is_selected {
        Span::styled(
            "\u{25b8} ",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![
        marker,
        Span::styled(row.id.clone(), Style::default().fg(THEME.text)),
        Span::styled(DOT, THEME.dim()),
        Span::styled(mark, Style::default().fg(color)),
    ]);
    if is_selected {
        line.style(THEME.selected())
    } else {
        line
    }
}

/// One row of the HOLDS panel: the item, the reason, and the bootstrap
/// key when the reason names an area the model does not cover.
///
/// Bootstrapping writes the missing area, so it answers that hold alone.
fn hold_row(hold: &HoldView, is_selected: bool) -> Line<'static> {
    let marker = if is_selected { "\u{25b8} " } else { "  " };
    let mut text = format!("{marker}#{}{DOT}{}", hold.number, hold.reason);
    if names_empty_area(&hold.reason) {
        text.push_str(DOT);
        text.push_str("b bootstrap");
    }
    let line = Line::from(Span::styled(text, Style::default().fg(THEME.error)));
    if is_selected {
        line.style(THEME.selected())
    } else {
        line
    }
}

/// The rows of one delta: one row per missed entry, else one summary row.
///
/// A closed delta reads `#n ✓ CLOSED`. An open delta with a miss
/// reads one `#n ● <OUTCOME>  <entry>` row per missed entry, so each
/// miss the operator answers stands on its own line. An open delta that
/// missed nothing reads `#n ○ OPEN  <hits> hit <unsure> unsure`.
fn delta_rows(row: &DeltaView, is_selected: bool) -> Vec<Line<'static>> {
    let number = row.number;
    let texts: Vec<(String, Style)> = if row.state == DeltaState::Closed {
        vec![(
            format!("#{number} \u{2713} CLOSED"),
            Style::default().fg(THEME.dim),
        )]
    } else if row.misses.is_empty() {
        vec![(
            format!(
                "#{number} \u{25cb} OPEN  {} hit {} unsure",
                row.hits, row.unsure
            ),
            Style::default().fg(THEME.accent),
        )]
    } else {
        row.misses
            .iter()
            .map(|(outcome, entry)| {
                (
                    format!("#{number} \u{25cf} {}  {entry}", outcome.to_uppercase()),
                    Style::default().fg(THEME.error),
                )
            })
            .collect()
    };
    texts
        .into_iter()
        .enumerate()
        .map(|(at, (text, style))| {
            let marker = if is_selected && at == 0 {
                Span::styled(
                    "\u{25b8} ",
                    Style::default()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            let line = Line::from(vec![marker, Span::styled(text, style)]);
            if is_selected {
                line.style(THEME.selected())
            } else {
                line
            }
        })
        .collect()
}

/// The tier mark of one area: the tier name, `-` when no surface maps to
/// the area, and `!` for a lint finding or a floor above reach.
fn mark(row: &AreaView) -> String {
    if row.lint || row.min_tier > row.tier {
        return "!".to_string();
    }
    if row.tier == Tier::None {
        return "-".to_string();
    }
    row.tier.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;

    use crate::sock::SurfaceView;
    use crate::theory::model::Entry;

    /// One boundary entry of the model the strip counts.
    fn boundary(id: &str) -> Entry {
        Entry::Boundary {
            id: id.to_string(),
            title: "checkout".to_string(),
            statement: "the cart pays".to_string(),
            sides: vec!["web".to_string(), "api".to_string()],
            paths: vec!["web/**".to_string()],
        }
    }

    fn view(areas: Vec<AreaView>, entries: usize) -> StateView {
        let mut theory = BTreeMap::new();
        theory.insert(
            "borsuk".to_string(),
            TheoryView {
                governor: true,
                error: String::new(),
                model: crate::theory::model::Model {
                    entries: vec![boundary("B-checkout"); entries],
                },
                areas,
                skills: BTreeMap::new(),
                holds: Vec::new(),
                records: BTreeMap::new(),
                deltas: Vec::new(),
            },
        );
        StateView {
            theory,
            ..empty_state()
        }
    }

    fn empty_state() -> StateView {
        StateView {
            protocol_revision: 1,
            repos: Vec::new(),
            stages: Vec::new(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions: Vec::new(),
            decision_items: Vec::new(),
            tickets: Vec::new(),
            prs: Vec::new(),
            links: Vec::new(),
            trains: Vec::new(),
            paused: crate::sock::PausedView {
                global: false,
                overrides: Vec::new(),
            },
            settings: crate::sock::SettingsView::default(),
            usage: Vec::new(),
            theory: BTreeMap::new(),
        }
    }

    fn render(state: &StateView) -> String {
        render_with(state, &mut Theory::default())
    }

    fn render_with(state: &StateView, view: &mut Theory) -> String {
        let backend = TestBackend::new(70, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, f.area(), state, view)).unwrap();
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

    fn area(id: &str, tier: Tier, min_tier: Tier, lint: bool) -> AreaView {
        AreaView {
            id: id.to_string(),
            boundary: String::new(),
            tier,
            min_tier,
            lint,
        }
    }

    /// The HOLDS panel names each held item, and offers the bootstrap key
    /// on the hold that names an area the model does not cover.
    #[test]
    fn the_holds_panel_offers_the_bootstrap_action_on_an_empty_area() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().holds = vec![
            HoldView {
                number: 142,
                reason: "area gh has no entries".to_string(),
                stage: crate::model::Stage::Implement,
            },
            HoldView {
                number: 143,
                reason: "awaits full prediction".to_string(),
                stage: crate::model::Stage::Implement,
            },
        ];
        let screen = render(&state);
        assert!(screen.contains("HOLDS"), "{screen}");
        assert!(
            screen.contains("#142 \u{b7} area gh has no entries \u{b7} b bootstrap"),
            "{screen}"
        );
        assert!(
            screen.contains("#143 \u{b7} awaits full prediction"),
            "{screen}"
        );
        assert!(
            !screen.contains("awaits full prediction \u{b7} b bootstrap"),
            "bootstrap answers an empty area alone:\n{screen}"
        );
    }

    /// One delta of pull request `number` with the given misses.
    fn delta(number: u64, misses: &[(&str, &str)], hits: usize, unsure: usize) -> DeltaView {
        DeltaView {
            number,
            state: DeltaState::Open,
            misses: misses
                .iter()
                .map(|(outcome, entry)| (outcome.to_string(), entry.to_string()))
                .collect(),
            hits,
            unsure,
            violations: Vec::new(),
            question: String::new(),
        }
    }

    /// The DELTAS panel names one row per miss, one summary row for a
    /// hit-only delta, and one closed row once the label is gone.
    #[test]
    fn the_deltas_panel_draws_a_miss_row_a_hit_row_and_a_closed_row() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        let mut closed = delta(9, &[], 5, 0);
        closed.state = DeltaState::Closed;
        state.theory.get_mut("borsuk").unwrap().deltas = vec![
            delta(142, &[("sure-miss", "INV-3")], 4, 0),
            delta(150, &[], 4, 1),
            closed,
        ];

        let screen = render(&state);

        assert!(screen.contains("DELTAS"), "{screen}");
        assert!(
            screen.contains("#142 \u{25cf} SURE-MISS  INV-3"),
            "{screen}"
        );
        assert!(
            screen.contains("#150 \u{25cb} OPEN  4 hit 1 unsure"),
            "{screen}"
        );
        assert!(screen.contains("#9 \u{2713} CLOSED"), "{screen}");
    }

    /// The cursor walks the DELTAS rows after the AREAS rows.
    #[test]
    fn the_cursor_walks_from_the_last_area_row_to_the_delta_row() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().deltas =
            vec![delta(142, &[("unsure-miss", "INV-3")], 0, 0)];
        let mut theory = Theory::default();

        theory.move_mark(&state, 1);
        theory.move_mark(&state, 1);

        assert_eq!(
            theory.at(&state),
            Some(Stop::Delta("borsuk".to_string(), 142))
        );
        let screen = render_with(&state, &mut theory);
        assert!(
            screen.contains("\u{25b8} #142 \u{25cf} UNSURE-MISS  INV-3"),
            "{screen}"
        );
    }

    /// A repository with no hold draws no HOLDS panel at all.
    #[test]
    fn a_repository_without_a_hold_draws_no_holds_panel() {
        let screen = render(&view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        ));
        assert!(!screen.contains("HOLDS"), "{screen}");
    }

    #[test]
    fn the_areas_panel_marks_the_tier_of_each_area() {
        let state = view(
            vec![
                area("web-checkout", Tier::Browser, Tier::None, false),
                area("api-orders", Tier::None, Tier::None, false),
            ],
            4,
        );

        let text = render(&state);

        assert!(
            text.contains("web-checkout · browser"),
            "screen was:\n{text}"
        );
        assert!(text.contains("api-orders · -"), "screen was:\n{text}");
        assert!(
            text.contains("GOVERNOR ON · ENTRIES 4 · AREAS 2"),
            "screen was:\n{text}"
        );
    }

    #[test]
    fn a_floor_above_reach_and_a_lint_finding_both_mark_the_row() {
        let state = view(
            vec![
                area("web-checkout", Tier::Http, Tier::Browser, false),
                area("api-orders", Tier::Http, Tier::None, true),
                area("cli-run", Tier::Terminal, Tier::Terminal, false),
            ],
            0,
        );

        let text = render(&state);

        assert!(text.contains("web-checkout · !"), "screen was:\n{text}");
        assert!(text.contains("api-orders · !"), "screen was:\n{text}");
        assert!(text.contains("cli-run · terminal"), "screen was:\n{text}");
    }

    #[test]
    fn the_strip_shows_the_error_and_an_ungoverned_repository_shows_no_panel() {
        let mut state = view(Vec::new(), 0);
        state.theory.get_mut("borsuk").unwrap().error = "theory/model.toml: missing".to_string();

        let text = render(&state);

        assert!(
            text.contains("GOVERNOR ON · theory/model.toml: missing"),
            "screen was:\n{text}"
        );

        let mut off = empty_state();
        off.theory
            .insert("borsuk".to_string(), TheoryView::default());
        let text = render(&off);
        assert!(text.contains("GOVERNOR OFF"), "screen was:\n{text}");
        assert!(!text.contains("AREAS"), "screen was:\n{text}");

        let text = render(&empty_state());
        assert!(text.contains("no repository"), "screen was:\n{text}");
    }

    /// One key press with no modifier.
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A state whose only repository holds one hold on an empty area.
    fn state_with_empty_area_hold() -> StateView {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().holds = vec![HoldView {
            number: 142,
            reason: crate::theory::records::no_entries("gh"),
            stage: crate::model::Stage::Implement,
        }];
        state
    }

    /// The cursor stops on the hold row of `state`.
    fn mark_the_hold(pane: &mut Theory, state: &StateView) {
        for _ in 0..3 {
            pane.handle_key(state, press(KeyCode::Char('j')));
        }
    }

    #[test]
    fn b_on_the_hold_row_starts_the_bootstrap_chat_of_its_area() {
        let state = state_with_empty_area_hold();
        let mut pane = Theory::default();
        mark_the_hold(&mut pane, &state);
        assert!(
            render_with(&state, &mut pane).contains("\u{25b8} #142"),
            "the cursor marks the hold row:\n{}",
            render_with(&state, &mut pane)
        );

        let outcome = pane.handle_key(&state, press(KeyCode::Char('b')));

        let Outcome::Send(action, toast) = outcome else {
            panic!("b on the hold row sends the bootstrap chat, got {outcome:?}");
        };
        let Action::Theory(TheoryAction::Chat {
            repo, purpose, key, ..
        }) = *action
        else {
            panic!("b sends one TheoryAction::Chat");
        };
        assert_eq!(repo, "borsuk");
        assert_eq!(purpose, ChatPurpose::Bootstrap);
        assert_eq!(key, "gh");
        assert_eq!(
            crate::tasks::bootstrap_id(&repo, &key),
            "borsuk/bootstrap-gh"
        );
        assert_eq!(toast, "asked to bootstrap borsuk/gh");
        assert!(pane.typing(), "the session view takes the keyboard");
        assert!(
            render_with(&state, &mut pane).contains("bootstrap borsuk/gh"),
            "the chat replaces the panels:\n{}",
            render_with(&state, &mut pane)
        );
    }

    /// The running bootstrap task of the `gh` area, as the daemon pushes
    /// it.
    fn bootstrap_task() -> crate::sock::TaskView {
        crate::sock::TaskView {
            id: "borsuk/bootstrap-gh".to_string(),
            repo: "borsuk".to_string(),
            stage: crate::model::Stage::Refine,
            kind: crate::model::ItemKind::Issue,
            number: 0,
            state: crate::tasks::TaskState::Running,
            attempt: 1,
            log_path: std::path::PathBuf::from("bootstrap-gh.jsonl"),
            input: crate::sock::InputMode::Live,
            queued_messages: 0,
            binding: None,
        }
    }

    #[test]
    fn b_shows_the_running_bootstrap_task_and_esc_returns_to_the_panels() {
        let mut state = state_with_empty_area_hold();
        state.tasks = vec![bootstrap_task()];
        let mut pane = Theory::default();
        mark_the_hold(&mut pane, &state);

        assert!(matches!(
            pane.handle_key(&state, press(KeyCode::Char('b'))),
            Outcome::Send(_, _)
        ));
        assert_eq!(pane.chat.task_id(), Some("borsuk/bootstrap-gh"));
        assert!(pane.needs_poll());

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Esc)),
            Outcome::None,
            "esc closes the chat"
        );
        assert!(!pane.typing());
        assert!(render_with(&state, &mut pane).contains("HOLDS"));
    }

    #[test]
    fn a_letter_and_enter_in_the_bootstrap_chat_send_the_typed_message() {
        let mut state = state_with_empty_area_hold();
        state.tasks = vec![bootstrap_task()];
        let mut pane = Theory::default();
        mark_the_hold(&mut pane, &state);
        assert!(matches!(
            pane.handle_key(&state, press(KeyCode::Char('b'))),
            Outcome::Send(_, _)
        ));

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('h'))),
            Outcome::None,
            "a letter types into the bar and sends nothing"
        );
        let outcome = pane.handle_key(&state, press(KeyCode::Enter));

        let Outcome::Send(action, toast) = outcome else {
            panic!("enter sends the typed message, got {outcome:?}");
        };
        let Action::Chat { task, text } = *action else {
            panic!("the chat bar sends one Action::Chat");
        };
        assert_eq!(task, "borsuk/bootstrap-gh");
        assert_eq!(text, "h");
        assert_eq!(toast, "sent chat borsuk/bootstrap-gh");
    }

    #[test]
    fn b_on_a_hold_that_names_no_empty_area_sends_nothing() {
        let mut state = state_with_empty_area_hold();
        state.theory.get_mut("borsuk").unwrap().holds = vec![HoldView {
            number: 143,
            reason: "awaits full prediction".to_string(),
            stage: crate::model::Stage::Implement,
        }];
        let mut pane = Theory::default();
        mark_the_hold(&mut pane, &state);

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('b'))),
            Outcome::None
        );
        assert!(!pane.typing());
    }

    #[test]
    fn v_then_a_surface_name_then_enter_sends_the_setup_action() {
        let state = view(vec![area("web-checkout", Tier::None, Tier::None, false)], 0);
        let mut pane = Theory::default();

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('v'))),
            Outcome::None
        );
        assert!(pane.typing(), "v opens the surface input");
        for character in "web".chars() {
            assert_eq!(
                pane.handle_key(&state, press(KeyCode::Char(character))),
                Outcome::None
            );
        }
        assert!(
            pane.footer_hints().contains("surface: web_"),
            "footer was: {}",
            pane.footer_hints()
        );

        let outcome = pane.handle_key(&state, press(KeyCode::Enter));

        assert_eq!(
            outcome,
            Outcome::Send(
                Box::new(Action::Theory(TheoryAction::Setup {
                    repo: "borsuk".to_string(),
                    surface: "web".to_string(),
                })),
                "asked for the run skill of borsuk/web".to_string()
            )
        );
        assert!(!pane.typing(), "the input closes after the send");
    }

    #[test]
    fn esc_cancels_the_input_and_an_empty_surface_sends_nothing() {
        let state = view(Vec::new(), 0);
        let mut pane = Theory::default();

        pane.handle_key(&state, press(KeyCode::Char('v')));
        pane.handle_key(&state, press(KeyCode::Char('w')));
        pane.handle_key(&state, press(KeyCode::Backspace));
        assert!(pane.footer_hints().contains("surface: _"));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Enter)),
            Outcome::None,
            "an empty surface sends nothing"
        );

        pane.handle_key(&state, press(KeyCode::Char('v')));
        pane.handle_key(&state, press(KeyCode::Char('w')));
        assert_eq!(pane.handle_key(&state, press(KeyCode::Esc)), Outcome::None);
        assert!(!pane.typing(), "esc closes the input");
        assert!(pane.footer_hints().contains("v run skill"));
    }

    #[test]
    fn a_surface_that_is_not_a_plain_directory_name_sends_nothing() {
        let state = view(Vec::new(), 0);
        let mut pane = Theory::default();

        pane.handle_key(&state, press(KeyCode::Char('v')));
        for character in "a/b".chars() {
            pane.handle_key(&state, press(KeyCode::Char(character)));
        }

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Enter)),
            Outcome::Reject("the surface a/b takes letters, digits, - and _ only".to_string())
        );
        assert!(!pane.typing(), "a rejected surface closes the input");

        for bad in ["a b", "a.b", "../x"] {
            pane.handle_key(&state, press(KeyCode::Char('v')));
            for character in bad.chars() {
                pane.handle_key(&state, press(KeyCode::Char(character)));
            }
            assert!(
                matches!(
                    pane.handle_key(&state, press(KeyCode::Enter)),
                    Outcome::Reject(_)
                ),
                "{bad} must not reach the daemon"
            );
        }

        pane.handle_key(&state, press(KeyCode::Char('v')));
        for character in "web-2_x".chars() {
            pane.handle_key(&state, press(KeyCode::Char(character)));
        }
        assert!(matches!(
            pane.handle_key(&state, press(KeyCode::Enter)),
            Outcome::Send(_, _)
        ));
    }

    #[test]
    fn j_and_k_move_the_mark_and_the_marked_repository_takes_the_key() {
        let mut state = view(Vec::new(), 0);
        state
            .theory
            .insert("qubitsok".to_string(), TheoryView::default());
        let mut pane = Theory::default();

        assert!(render_with(&state, &mut pane).contains("> borsuk"));

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert!(render_with(&state, &mut pane).contains("> qubitsok"));
        // The mark does not wrap at the last row.
        pane.handle_key(&state, press(KeyCode::Char('j')));
        pane.handle_key(&state, press(KeyCode::Char('v')));
        pane.handle_key(&state, press(KeyCode::Char('a')));

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Enter)),
            Outcome::Send(
                Box::new(Action::Theory(TheoryAction::Setup {
                    repo: "qubitsok".to_string(),
                    surface: "a".to_string(),
                })),
                "asked for the run skill of qubitsok/a".to_string()
            )
        );

        pane.handle_key(&state, press(KeyCode::Char('k')));
        pane.handle_key(&state, press(KeyCode::Char('v')));
        pane.handle_key(&state, press(KeyCode::Char('a')));
        let Outcome::Send(action, _) = pane.handle_key(&state, press(KeyCode::Enter)) else {
            panic!("the marked repository must take the key");
        };
        assert_eq!(
            *action,
            Action::Theory(TheoryAction::Setup {
                repo: "borsuk".to_string(),
                surface: "a".to_string(),
            })
        );
    }

    #[test]
    fn the_cursor_walks_the_repository_row_then_its_areas_and_t_teaches_one() {
        let state = view(
            vec![
                area("api-orders", Tier::Http, Tier::None, false),
                area("web-checkout", Tier::Browser, Tier::None, false),
            ],
            0,
        );
        let mut pane = Theory::default();

        // The cursor starts on the repository row, where t sends nothing.
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('t'))),
            Outcome::None
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        pane.handle_key(&state, press(KeyCode::Char('j')));
        let screen = render_with(&state, &mut pane);
        assert!(
            screen.contains("\u{25b8} web-checkout"),
            "screen was:\n{screen}"
        );

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('t'))),
            Outcome::Send(
                Box::new(Action::Theory(TheoryAction::Teach {
                    repo: "borsuk".to_string(),
                    key: TeachKey::Area("web-checkout".to_string()),
                })),
                "asked to teach borsuk/web-checkout".to_string()
            )
        );

        // The cursor does not wrap, and v still names the repository of
        // the marked area row.
        pane.handle_key(&state, press(KeyCode::Char('j')));
        pane.handle_key(&state, press(KeyCode::Char('v')));
        pane.handle_key(&state, press(KeyCode::Char('a')));
        assert!(matches!(
            pane.handle_key(&state, press(KeyCode::Enter)),
            Outcome::Send(_, _)
        ));
    }

    // --- The edit-model flow. ---

    /// A fresh temporary directory for one test.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-theory-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write an executable POSIX shell script into `dir`.
    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("editor");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        drop(file);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// The model worktree of one test, with `theory/model.toml` in it.
    fn model_worktree(dir: &Path, text: &str) -> std::path::PathBuf {
        let worktree = dir.join("model");
        fs::create_dir_all(worktree.join("theory")).unwrap();
        fs::write(worktree.join(MODEL_FILE), text).unwrap();
        worktree
    }

    /// An editor closure that runs the fake editor `body` over one file.
    ///
    /// The test writes its fake editor and executes it at once, so the
    /// exec can lose against the write-count release of the just-closed
    /// file and report `Text file busy` for a few microseconds. Production
    /// never executes a file it just wrote, so the retry lives here.
    fn fake_editor(dir: &Path, body: &str) -> impl FnOnce(&Path) -> Result<EditorOutcome> {
        let editor = vec![script(dir, body).to_string_lossy().into_owned()];
        move |file: &Path| {
            for _ in 0..100 {
                match super::super::editor::edit_file_with(file, &editor, || Ok(()), || Ok(())) {
                    Ok(EditorOutcome::Failed(reason)) if reason.contains("Text file busy") => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    outcome => return outcome,
                }
            }
            panic!("the fake editor did not start after 100 attempts");
        }
    }

    /// Press `e` on the marked row and return the request it minted.
    fn press_edit(pane: &mut Theory, state: &StateView) -> ModelPath {
        let Outcome::Send(action, toast) = pane.handle_key(state, press(KeyCode::Char('e'))) else {
            panic!("e on a repository row must send the edit-model action");
        };
        assert_eq!(toast, "opening the model of borsuk");
        let Action::Theory(TheoryAction::EditModel { request, repo }) = *action else {
            panic!("e must send TheoryAction::EditModel");
        };
        ModelPath {
            request,
            repo,
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn an_editor_that_saves_a_valid_model_sends_the_commit_action() {
        let dir = temp_dir("model-saved");
        let worktree = model_worktree(&dir, "# Write one [[entry]] table per model entry.\n");
        let state = view(vec![area("web-checkout", Tier::None, Tier::None, false)], 0);
        let mut pane = Theory::default();
        let mut reply = press_edit(&mut pane, &state);
        reply.path = worktree;
        let editor = fake_editor(
            &dir,
            "#!/bin/sh\nprintf '[[entry]]\\nid = \"pay\"\\nkind = \"state\"\\n             title = \"Pay\"\\nstatement = \"The buyer pays.\"\\n' > \"$1\"\n",
        );

        let outcome = pane.observe_model_path(&reply, editor);

        assert_eq!(
            outcome,
            Outcome::Send(
                Box::new(Action::Theory(TheoryAction::CommitModel {
                    repo: "borsuk".to_string(),
                })),
                "asked to commit the model of borsuk".to_string()
            )
        );
        assert!(
            pane.footer_hints().contains("e model"),
            "footer was: {}",
            pane.footer_hints()
        );
    }

    #[test]
    fn an_editor_that_breaks_the_model_reports_the_entry_and_sends_nothing() {
        let dir = temp_dir("model-broken");
        let worktree = model_worktree(&dir, "# Write one [[entry]] table per model entry.\n");
        let state = view(Vec::new(), 0);
        let mut pane = Theory::default();
        let mut reply = press_edit(&mut pane, &state);
        reply.path = worktree;
        let editor = fake_editor(
            &dir,
            "#!/bin/sh\nprintf '[[entry]]\\nid = \"pay\"\\nkind = \"state\"\\n' > \"$1\"\n",
        );

        let outcome = pane.observe_model_path(&reply, editor);

        assert_eq!(
            outcome,
            Outcome::Reject("theory/model.toml: pay: title is required".to_string()),
            "a broken model names the entry and the reason"
        );
    }

    #[test]
    fn a_failed_editor_and_a_reply_this_ui_never_asked_for_send_nothing() {
        let dir = temp_dir("model-failed");
        let worktree = model_worktree(&dir, "# Write one [[entry]] table per model entry.\n");
        let state = view(Vec::new(), 0);
        let mut pane = Theory::default();
        let mut reply = press_edit(&mut pane, &state);
        reply.path = worktree.clone();

        // Another UI asked for its own edit, so this reply opens no editor.
        let stranger = ModelPath {
            request: "someone-else".to_string(),
            repo: "borsuk".to_string(),
            path: worktree.clone(),
        };
        assert_eq!(
            pane.observe_model_path(&stranger, |_| panic!("no editor may run")),
            Outcome::None
        );

        let outcome = pane.observe_model_path(&reply, fake_editor(&dir, "#!/bin/sh\nexit 1\n"));

        assert!(
            matches!(&outcome, Outcome::Reject(reason) if reason.contains("exit")),
            "a failed editor reports its reason, was {outcome:?}"
        );
        // The reply is spent, so a second copy of it opens no editor.
        assert_eq!(
            pane.observe_model_path(&reply, |_| panic!("no editor may run")),
            Outcome::None
        );
    }

    #[test]
    fn e_on_an_area_row_sends_nothing() {
        let state = view(vec![area("web-checkout", Tier::None, Tier::None, false)], 0);
        let mut pane = Theory::default();

        pane.handle_key(&state, press(KeyCode::Char('j')));

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('e'))),
            Outcome::None,
            "an area row names no repository model"
        );
    }

    #[test]
    fn the_view_keys_pass_through_and_the_input_swallows_them() {
        let state = view(Vec::new(), 0);
        let mut pane = Theory::default();

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('1'))),
            Outcome::Pass
        );
        assert_eq!(pane.handle_key(&state, press(KeyCode::Esc)), Outcome::Pass);

        pane.handle_key(&state, press(KeyCode::Char('v')));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('1'))),
            Outcome::None
        );
        assert!(pane.footer_hints().contains("surface: 1_"));
    }

    #[test]
    fn a_surface_view_reaches_the_panel_through_the_state_view() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().skills.insert(
            "web".to_string(),
            SurfaceView {
                tier: Tier::Browser,
                features: vec!["checkout".to_string()],
                lint: vec!["features/x.md: area nope unknown".to_string()],
            },
        );

        let shipped = &state.theory["borsuk"].skills["web"];

        assert_eq!(shipped.tier, Tier::Browser);
        assert_eq!(shipped.features, vec!["checkout".to_string()]);
        assert_eq!(shipped.lint.len(), 1);
        assert!(render(&state).contains("web-checkout · browser"));
    }
}
