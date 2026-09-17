//! The Theory view: what the governor knows about each repository.
//!
//! The view draws one block per repository: the header strip with the
//! governor state and the counts, then the AREAS panel with the tier each
//! area reaches, then the HOLDS panel with the items the governor holds,
//! then the DELTAS panel with the deltas the reviews reported. The MAP
//! panel at the bottom of the body draws one area of the marked
//! repository as a picture. The operator moves the cursor with `j` and
//! `k`. The keys `h` and `l` cycle the area of the map. On a repository
//! row `v` asks for the run skill of one surface. On an area row `t`
//! asks the agent to teach that area. On a repository row `e` opens
//! `theory/model.toml` in the operator's editor. On a hold row that
//! names an area with no entries `b` starts the bootstrap chat of that
//! area and opens its session view in place of the panels.

mod map;

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

/// The filled block of the window gauge, one per open record.
const WINDOW_MARK: char = '\u{25ae}';

/// The empty block of the window gauge, one per free record.
const WINDOW_ROOM: char = '\u{25af}';

/// The mark between the window gauge and the pause word.
const PAUSE_MARK: char = '\u{25b8}';

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
/// Each governed repository contributes its header row, one stop per map
/// entry of the area the map shows, one row per area, and one row per
/// delta, in draw order. A delta that draws several miss rows is one
/// stop. An ungoverned repository draws no panel, so it contributes its
/// header row alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stop {
    /// The header row of one repository.
    Repo(String),
    /// One map entry of one area of one repository.
    Entry(String, String, String),
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
            | Stop::Entry(alias, _, _)
            | Stop::Area(alias, _)
            | Stop::Hold(alias, _)
            | Stop::Delta(alias, _) => alias,
        }
    }
}

/// The area the map shows of one repository: the one the marked entry or
/// area row names, else the stored area, else the first area.
fn shown_area<'a>(
    row: &'a TheoryView,
    marked: Option<&Stop>,
    stored: Option<&String>,
) -> Option<&'a AreaView> {
    if !row.governor || row.areas.is_empty() {
        return None;
    }
    let named = marked.and_then(|stop| match stop {
        Stop::Entry(_, area, _) | Stop::Area(_, area) => Some(area),
        _ => None,
    });
    if let Some(id) = named.or(stored) {
        if let Some(one) = row.areas.iter().find(|one| &one.id == id) {
            return Some(one);
        }
    }
    row.areas.first()
}

/// The Theory view state.
///
/// It holds no repository data. The marked stop survives a state push, and
/// the view falls back to the first stop when the marked one is gone.
#[derive(Debug, Default)]
pub(super) struct Theory {
    /// The stop the operator marked.
    marked: Option<Stop>,
    /// The area the map shows of the marked repository.
    ///
    /// The mark names the area itself on an entry or area row, so this
    /// only carries the choice of `h` and `l` past rows that name no
    /// area, such as the header, a hold, or a delta.
    map_area: Option<String>,
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

    /// The stop the keys act on within `all`: the marked one when `all`
    /// still holds it, else the first one.
    fn at_in(all: &[Stop], marked: Option<&Stop>) -> Option<Stop> {
        match marked.filter(|stop| all.contains(stop)) {
            Some(stop) => Some(stop.clone()),
            None => all.first().cloned(),
        }
    }

    /// The stops of the state view, in draw order.
    ///
    /// The repository the mark acts on also contributes one stop per map
    /// entry of the area the map shows, between its header row and its
    /// area rows. The marked stop names that repository itself, so an
    /// entry mark keeps its own repository; a view with no mark takes the
    /// first one.
    fn stops(&self, state: &StateView) -> Vec<Stop> {
        let marked = self.marked.as_ref();
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
        let repo = match marked {
            Some(stop) => Some(stop.repo().to_string()),
            None => all.first().map(|stop| stop.repo().to_string()),
        };
        if let Some(repo) = repo {
            if let Some(view) = state.theory.get(&repo) {
                if let Some(one) = shown_area(view, marked, self.map_area.as_ref()) {
                    let head = all.iter().position(|one| one.repo() == repo.as_str());
                    for (offset, line) in map::lines(&view.model, &one.boundary)
                        .into_iter()
                        .enumerate()
                    {
                        all.insert(
                            head.unwrap_or(0) + 1 + offset,
                            Stop::Entry(repo.clone(), one.id.clone(), line.id),
                        );
                    }
                }
            }
        }
        all
    }

    /// The stop the keys act on: the marked one, else the first one.
    fn at(&self, state: &StateView) -> Option<Stop> {
        Self::at_in(&self.stops(state), self.marked.as_ref())
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
                "1-6 view{DOT}j/k row{DOT}h/l area{DOT}v run skill{DOT}t teach{DOT}b bootstrap{DOT}e model{DOT}esc home"
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
            KeyCode::Char('h') => {
                self.cycle_area(state, -1);
                Outcome::None
            }
            KeyCode::Char('l') => {
                self.cycle_area(state, 1);
                Outcome::None
            }
            KeyCode::Char('v') => match self.at(state) {
                Some(Stop::Repo(_)) => {
                    self.input = Some(String::new());
                    Outcome::None
                }
                Some(_) => Outcome::Reject("move to the repository row for v".to_string()),
                None => Outcome::None,
            },
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

    /// Send the teach action of the marked area or delta row.
    ///
    /// A repository, map entry, or hold row names neither, so it sends
    /// nothing.
    fn send_teach(&self, state: &StateView) -> Outcome {
        match self.at(state) {
            Some(Stop::Area(repo, id)) => {
                let toast = format!("asked to teach {repo}/{id}");
                Outcome::Send(
                    Box::new(Action::Theory(TheoryAction::Teach {
                        repo,
                        key: TeachKey::Area(id),
                    })),
                    toast,
                )
            }
            Some(Stop::Delta(repo, number)) => {
                let toast = format!("asked to teach {repo}/#{number}");
                Outcome::Send(
                    Box::new(Action::Theory(TheoryAction::Teach {
                        repo,
                        key: TeachKey::Delta(number),
                    })),
                    toast,
                )
            }
            _ => Outcome::None,
        }
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
    /// An area or map entry row names one area, not the repository model,
    /// so it sends nothing. The identity of the request stays here until
    /// the reply arrives.
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
    ///
    /// A move into another repository drops the stored area of the map,
    /// so the map of the new repository starts at its own first area.
    fn move_mark(&mut self, state: &StateView, delta: isize) {
        let all = self.stops(state);
        if all.is_empty() {
            return;
        }
        let at = self
            .at(state)
            .and_then(|stop| all.iter().position(|one| *one == stop))
            .unwrap_or(0);
        let next = (at as isize + delta).clamp(0, all.len() as isize - 1) as usize;
        let stop = all[next].clone();
        if self.marked.as_ref().map(|one| one.repo()) != Some(stop.repo()) {
            self.map_area = None;
        }
        self.marked = Some(stop);
    }

    /// Show the previous or the next area of the marked repository.
    ///
    /// The index moves without wrapping, and the mark moves onto the
    /// area row of the area the map now shows.
    fn cycle_area(&mut self, state: &StateView, delta: isize) {
        let Some(alias) = self.current(state) else {
            return;
        };
        let Some(row) = state.theory.get(&alias) else {
            return;
        };
        if !row.governor || row.areas.is_empty() {
            return;
        }
        let marked = self
            .marked
            .as_ref()
            .filter(|stop| stop.repo() == alias.as_str());
        let at = shown_area(row, marked, self.map_area.as_ref())
            .and_then(|one| row.areas.iter().position(|area| area.id == one.id))
            .unwrap_or(0);
        let next = (at as isize + delta).clamp(0, row.areas.len() as isize - 1) as usize;
        let id = row.areas[next].id.clone();
        self.map_area = Some(id.clone());
        self.marked = Some(Stop::Area(alias, id));
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
/// interview and the panels do not compete with it for rows. The map
/// pane of the marked repository takes the rows the panels leave free at
/// the bottom of the body.
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
    let body = block.inner(area);
    f.render_widget(block, area);
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
    let used = lines.len().min(body.height as usize) as u16;
    let pane = map_pane(state, at.as_ref(), view, body, body.height - used);
    let top = match &pane {
        Some(pane) => Rect {
            height: body.height - pane.rect.height,
            ..body
        },
        None => body,
    };
    f.render_widget(Paragraph::new(lines), top);
    if let Some(pane) = pane {
        map::draw(
            f,
            pane.rect,
            &pane.area,
            &pane.lines,
            pane.cursor.as_deref(),
            pane.strip.as_deref(),
        );
    }
}

/// The map pane of the marked repository, ready to draw.
struct MapPane {
    /// The rect the pane draws in, at the bottom of the body.
    rect: Rect,
    /// The id of the area the map shows.
    area: String,
    /// The lines of the map, in draw order.
    lines: Vec<map::MapLine>,
    /// The id of the map entry under the cursor.
    cursor: Option<String>,
    /// The statement strip of that entry.
    strip: Option<String>,
}

/// The map pane of the marked repository.
///
/// The pane renders only when the marked repository is governed and has
/// at least one area, and only on the rows the panels above leave free.
fn map_pane(
    state: &StateView,
    at: Option<&Stop>,
    view: &Theory,
    body: Rect,
    free: u16,
) -> Option<MapPane> {
    let stop = at?;
    let row = state.theory.get(stop.repo())?;
    if !row.governor {
        return None;
    }
    let one = shown_area(row, Some(stop), view.map_area.as_ref())?;
    let lines = map::lines(&row.model, &one.boundary);
    let cursor = match stop {
        Stop::Entry(_, area, entry) if *area == one.id => Some(entry.clone()),
        _ => None,
    };
    let strip = cursor.as_ref().and_then(|id| {
        row.model
            .entries
            .iter()
            .find(|candidate| candidate.id() == id.as_str())
            .map(|entry| format!("{}: {}", entry.id(), entry.statement()))
    });
    let needed = 2 + lines.len() + usize::from(strip.is_some());
    let height = needed.min(free as usize);
    if height == 0 {
        return None;
    }
    Some(MapPane {
        rect: Rect {
            x: body.x,
            y: body.y + body.height - height as u16,
            width: body.width,
            height: height as u16,
        },
        area: one.id.clone(),
        lines,
        cursor,
        strip,
    })
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
        if view.window.1 > 0 {
            spans.push(Span::styled(DOT, THEME.dim()));
            spans.push(Span::styled(
                window_gauge(view.window),
                Style::default().fg(THEME.text),
            ));
            if view.window.0 >= view.window.1 {
                spans.push(Span::styled(DOT, THEME.dim()));
                spans.push(Span::styled(
                    format!("IMPLEMENT {PAUSE_MARK} PAUSED"),
                    Style::default().fg(THEME.warn),
                ));
            }
        }
    } else {
        spans.push(Span::styled(
            view.error.clone(),
            Style::default().fg(THEME.error),
        ));
    }
    Line::from(spans)
}

/// The window gauge of one repository: one filled block per open record
/// up to the cap, empty blocks for the rest, then the count.
fn window_gauge(window: (usize, usize)) -> String {
    let (open, cap) = window;
    let mut gauge = String::from("WINDOW ");
    for _ in 0..open.min(cap) {
        gauge.push(WINDOW_MARK);
    }
    for _ in open..cap {
        gauge.push(WINDOW_ROOM);
    }
    gauge.push_str(&format!(" {open}/{cap}"));
    gauge
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
                calibration: None,
                rungs: [0; 3],
                events_per_day: 0,
                stale_entries: Vec::new(),
                merged: Vec::new(),
                deltas: Vec::new(),
                window: (0, 0),
                cards: Vec::new(),
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
        render_at(state, view, 70)
    }

    /// The render at a width the strip under test needs. A full window
    /// strip outgrows the default 70 columns.
    fn render_at(state: &StateView, view: &mut Theory, width: u16) -> String {
        let backend = TestBackend::new(width, 16);
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

    /// The strip draws the window gauge after the counts, and the pause
    /// word when the open records reach the cap.
    #[test]
    fn the_strip_draws_the_window_gauge_and_the_pause_when_full() {
        let mut state = view(Vec::new(), 0);
        state.theory.get_mut("borsuk").unwrap().window = (3, 3);

        let text = render_at(&state, &mut Theory::default(), 90);

        assert!(
            text.contains("WINDOW \u{25ae}\u{25ae}\u{25ae} 3/3 · IMPLEMENT \u{25b8} PAUSED"),
            "screen was:\n{text}"
        );

        // Room in the window draws the open gauge and no pause word.
        state.theory.get_mut("borsuk").unwrap().window = (1, 3);
        let text = render_at(&state, &mut Theory::default(), 90);
        assert!(
            text.contains("WINDOW \u{25ae}\u{25af}\u{25af} 1/3"),
            "screen was:\n{text}"
        );
        assert!(!text.contains("PAUSED"), "screen was:\n{text}");
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
            hold: None,
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

        // The cursor does not wrap, and v on an area row asks for the
        // repository row.
        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('v'))),
            Outcome::Reject("move to the repository row for v".to_string())
        );
        assert!(!pane.typing(), "v on an area row opens no input");
    }

    #[test]
    fn v_asks_for_the_repository_row_on_an_area_a_hold_and_a_delta() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().holds = vec![HoldView {
            number: 142,
            reason: "awaits full prediction".to_string(),
            stage: crate::model::Stage::Implement,
        }];
        state.theory.get_mut("borsuk").unwrap().deltas =
            vec![delta(142, &[("sure-miss", "INV-3")], 4, 0)];
        let mut pane = Theory::default();

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('v'))),
            Outcome::Reject("move to the repository row for v".to_string()),
            "the area row refuses v"
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('v'))),
            Outcome::Reject("move to the repository row for v".to_string()),
            "the hold row refuses v"
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('v'))),
            Outcome::Reject("move to the repository row for v".to_string()),
            "the delta row refuses v"
        );
        assert!(!pane.typing(), "v opens the input on no marked row");
    }

    /// `t` on a DELTAS row teaches the pull request the delta belongs to.
    #[test]
    fn t_on_a_delta_row_teaches_the_pull_request_of_the_delta() {
        let mut state = view(
            vec![area("web-checkout", Tier::Browser, Tier::None, false)],
            1,
        );
        state.theory.get_mut("borsuk").unwrap().deltas =
            vec![delta(142, &[("sure-miss", "INV-3")], 4, 0)];
        let mut pane = Theory::default();

        // Two steps walk past the repository and area rows.
        pane.handle_key(&state, press(KeyCode::Char('j')));
        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.at(&state),
            Some(Stop::Delta("borsuk".to_string(), 142))
        );

        assert_eq!(
            pane.handle_key(&state, press(KeyCode::Char('t'))),
            Outcome::Send(
                Box::new(Action::Theory(TheoryAction::Teach {
                    repo: "borsuk".to_string(),
                    key: TeachKey::Delta(142),
                })),
                "asked to teach borsuk/#142".to_string()
            )
        );
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

    /// A wide window gauge renders at a narrow width without panic: the
    /// strip clips inside its width, and the clipped count and the pause
    /// word stay off the screen until the width fits them.
    #[test]
    fn a_wide_window_gauge_clips_at_a_narrow_width_without_panic() {
        let mut state = view(Vec::new(), 0);
        state.theory.get_mut("borsuk").unwrap().window = (0, 50);

        let text = render_at(&state, &mut Theory::default(), 70);

        assert!(
            text.contains("GOVERNOR ON · ENTRIES 0 · AREAS 0 · WINDOW "),
            "screen was:\n{text}"
        );
        assert!(!text.contains("0/50"), "screen was:\n{text}");
        assert!(!text.contains("PAUSED"), "screen was:\n{text}");

        // A full window of fifty records clips the same way: no count,
        // no pause word, no panic.
        state.theory.get_mut("borsuk").unwrap().window = (50, 50);
        let text = render_at(&state, &mut Theory::default(), 70);
        assert!(
            text.contains("GOVERNOR ON · ENTRIES 0 · AREAS 0 · WINDOW "),
            "screen was:\n{text}"
        );
        assert!(!text.contains("50/50"), "screen was:\n{text}");
        assert!(!text.contains("PAUSED"), "screen was:\n{text}");

        let text = render_at(&state, &mut Theory::default(), 130);
        let gauge = format!("WINDOW {} 50/50", "\u{25ae}".repeat(50));
        assert!(
            text.contains(&format!("{gauge} · IMPLEMENT \u{25b8} PAUSED")),
            "screen was:\n{text}"
        );
    }

    // --- The map panel. ---

    /// One area row bound to one boundary of the model.
    fn area_on(id: &str, boundary: &str) -> AreaView {
        AreaView {
            id: id.to_string(),
            boundary: boundary.to_string(),
            tier: Tier::Browser,
            min_tier: Tier::None,
            lint: false,
        }
    }

    /// One state entry of the map model.
    fn box_state(id: &str, statement: &str) -> Entry {
        Entry::State {
            id: id.to_string(),
            title: id.to_string(),
            statement: statement.to_string(),
        }
    }

    /// The model of the map tests: the checkout boundary with the IDLE
    /// and BUSY states, one `poll` transition, and one crossing failure.
    fn map_model() -> crate::theory::model::Model {
        crate::theory::model::Model {
            entries: vec![
                Entry::Boundary {
                    id: "B-checkout".to_string(),
                    title: "checkout".to_string(),
                    statement: "the cart pays".to_string(),
                    sides: vec!["IDLE".to_string(), "BUSY".to_string()],
                    paths: vec!["web/**".to_string()],
                },
                box_state("IDLE", "the cart waits"),
                box_state("BUSY", "the cart pays"),
                Entry::Transition {
                    id: "T-poll".to_string(),
                    title: "poll".to_string(),
                    statement: "the poller starts the cart".to_string(),
                    from: "IDLE".to_string(),
                    to: "BUSY".to_string(),
                },
                Entry::Failure {
                    id: "FM-2".to_string(),
                    title: "replay".to_string(),
                    statement: "a replay pays twice".to_string(),
                    crosses: "B-checkout".to_string(),
                },
            ],
        }
    }

    /// A state whose repository carries `model` on the given areas.
    fn map_view(model: crate::theory::model::Model, areas: Vec<AreaView>) -> StateView {
        let mut state = view(areas, 0);
        state.theory.get_mut("borsuk").unwrap().model = model;
        state
    }

    /// The map draws the transition inside a double-line frame titled
    /// with the area id in uppercase.
    #[test]
    fn the_map_draws_the_transition_in_a_frame_titled_by_the_area() {
        let state = map_view(map_model(), vec![area_on("web-checkout", "B-checkout")]);

        let screen = render(&state);

        assert!(screen.contains('╔'), "screen was:\n{screen}");
        assert!(screen.contains('╚'), "screen was:\n{screen}");
        assert!(screen.contains("╔ WEB-CHECKOUT"), "screen was:\n{screen}");
        assert!(
            screen.contains("[IDLE]──poll──▶[BUSY]"),
            "screen was:\n{screen}"
        );
    }

    /// The failure that crosses the shown boundary draws on a line under
    /// the frame.
    #[test]
    fn the_map_draws_a_crossing_failure_under_the_frame() {
        let state = map_view(map_model(), vec![area_on("web-checkout", "B-checkout")]);

        let screen = render(&state);

        let rows: Vec<&str> = screen.lines().collect();
        let frame_bottom = rows
            .iter()
            .position(|row| row.contains('╚'))
            .expect("the frame draws a bottom border");
        let failure = rows
            .iter()
            .position(|row| row.contains("⚠ FM-2 CROSSES B-checkout"))
            .expect("the failure draws");
        assert!(failure > frame_bottom, "screen was:\n{screen}");
    }

    /// A map wider than the pane truncates with `…` and renders without
    /// a panic.
    #[test]
    fn a_map_wider_than_the_pane_truncates_with_an_ellipsis_without_panic() {
        let model = crate::theory::model::Model {
            entries: vec![
                Entry::Boundary {
                    id: "B-checkout".to_string(),
                    title: "checkout".to_string(),
                    statement: "the cart pays".to_string(),
                    sides: vec!["CHECKOUT-START".to_string(), "PAYMENT-FINISH".to_string()],
                    paths: vec!["web/**".to_string()],
                },
                box_state("CHECKOUT-START", "the cart opens"),
                box_state("PAYMENT-FINISH", "the cart closes"),
                Entry::Transition {
                    id: "T-refresh".to_string(),
                    title: "overnight-refresh".to_string(),
                    statement: "the nightly refresh restarts the cart".to_string(),
                    from: "CHECKOUT-START".to_string(),
                    to: "PAYMENT-FINISH".to_string(),
                },
            ],
        };
        let state = map_view(model, vec![area_on("web-checkout", "B-checkout")]);

        let screen = render_at(&state, &mut Theory::default(), 40);

        assert!(
            screen.contains("[CHECKOUT-START]──overnig…"),
            "screen was:\n{screen}"
        );
        assert!(screen.contains('…'), "screen was:\n{screen}");
    }

    /// From the repository header, `j` walks the map entries, and the
    /// strip shows the statement of the entry under the cursor.
    #[test]
    fn j_walks_the_map_entries_and_the_strip_shows_their_statements() {
        let state = map_view(map_model(), vec![area_on("web-checkout", "B-checkout")]);
        let mut pane = Theory::default();
        assert_eq!(
            pane.at(&state),
            Some(Stop::Repo("borsuk".to_string())),
            "the mark starts on the repository header"
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert!(
            render_with(&state, &mut pane).contains("T-poll: the poller starts the cart"),
            "screen was:\n{}",
            render_with(&state, &mut pane)
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert!(
            render_with(&state, &mut pane).contains("FM-2: a replay pays twice"),
            "screen was:\n{}",
            render_with(&state, &mut pane)
        );
    }

    /// `l` shows the next area of the marked repository in the frame
    /// title, and `h` shows the previous one again.
    #[test]
    fn h_and_l_cycle_the_area_of_the_map() {
        let state = map_view(
            map_model(),
            vec![
                area_on("web-checkout", "B-checkout"),
                area_on("api-orders", "B-api"),
            ],
        );
        let mut pane = Theory::default();

        pane.handle_key(&state, press(KeyCode::Char('l')));
        assert!(
            render_with(&state, &mut pane).contains("╔ API-ORDERS "),
            "screen was:\n{}",
            render_with(&state, &mut pane)
        );

        pane.handle_key(&state, press(KeyCode::Char('h')));
        assert!(
            render_with(&state, &mut pane).contains("╔ WEB-CHECKOUT "),
            "screen was:\n{}",
            render_with(&state, &mut pane)
        );
    }

    /// `j` walks the map entries of the marked repository, even when it
    /// is not the first one the state view lists.
    #[test]
    fn j_walks_the_entries_of_the_marked_repository_not_the_first_one() {
        let mut state = map_view(map_model(), vec![area_on("web-checkout", "B-checkout")]);
        let second = state.theory["borsuk"].clone();
        state.theory.insert("zulu".to_string(), second);
        let mut pane = Theory::default();

        // Four steps walk from the borsuk header past its two entries
        // and its area row onto the zulu header.
        for _ in 0..4 {
            pane.handle_key(&state, press(KeyCode::Char('j')));
        }
        assert_eq!(
            pane.at(&state),
            Some(Stop::Repo("zulu".to_string())),
            "the mark reaches the second repository"
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.at(&state),
            Some(Stop::Entry(
                "zulu".to_string(),
                "web-checkout".to_string(),
                "T-poll".to_string()
            )),
            "the first entry of the second repository"
        );

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert_eq!(
            pane.at(&state),
            Some(Stop::Entry(
                "zulu".to_string(),
                "web-checkout".to_string(),
                "FM-2".to_string()
            )),
            "the mark stays in the second repository"
        );
    }
}
