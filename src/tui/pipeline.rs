//! Draws the pipeline view and handles its keys.
//!
//! The view lists the four stages in pipeline order. Inside a stage it
//! groups the tickets by repository. A stage header row shows the running
//! and queued counts against the limit and marks a limit that differs from
//! the config file with a star. The release stage shows one train row per
//! repository with the queue, the stacked set, the policy, and the
//! countdown. A pause marks the paused stage header, the repository group
//! row, and the queued tickets that cannot start.
//!
//! Chunk 19 adds the interaction keys to this file. The `Row` model and
//! the selection movement exist so those keys can resolve their target.

use super::inbox::ActionSink;
use super::theme::THEME;
use crate::config::ReleasePolicy;
use crate::model::Stage;
use crate::sock::{Action, LaneView, PauseScope, StateView, TaskView};
use crate::tasks::TaskState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{emit, App, Confirm, Selection, View, Wanted};

/// One selectable row of the pipeline view, in draw order.
///
/// The list holds every stage header, every repository header, every
/// ticket, and every release train row. `j` and `k` walk this list, so a
/// later chunk can act on the selected row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Row {
    /// The header row of one stage.
    Stage {
        /// The stage.
        stage: Stage,
    },
    /// The header row of one repository inside one stage.
    Repo {
        /// The stage the group belongs to.
        stage: Stage,
        /// The repository alias.
        repo: String,
    },
    /// One task ticket. The index points into [`StateView::tasks`].
    Ticket {
        /// The index of the task in the state view.
        index: usize,
    },
    /// The release train row of one repository.
    Train {
        /// The repository alias.
        repo: String,
    },
}

/// The selectable rows of the pipeline view, in draw order.
pub(super) fn rows(state: &StateView) -> Vec<Row> {
    let mut rows = Vec::new();
    for stage in Stage::ALL {
        rows.push(Row::Stage { stage });
        for repo in &state.repos {
            let tickets: Vec<usize> = state
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, task)| task.stage == stage && task.repo == repo.alias)
                .map(|(index, _)| index)
                .collect();
            let train = (stage == Stage::Release)
                .then(|| state.trains.iter().find(|t| t.repo == repo.alias))
                .flatten()
                .map(|t| t.repo.clone());
            if tickets.is_empty() && train.is_none() {
                continue;
            }
            rows.push(Row::Repo {
                stage,
                repo: repo.alias.clone(),
            });
            rows.extend(tickets.into_iter().map(|index| Row::Ticket { index }));
            if let Some(repo) = train {
                rows.push(Row::Train { repo });
            }
        }
    }
    rows
}

/// True when the stage shows no repository group and no train.
fn stage_is_empty(state: &StateView, stage: Stage) -> bool {
    let any_task = state.tasks.iter().any(|task| task.stage == stage);
    let any_train = stage == Stage::Release && !state.trains.is_empty();
    !any_task && !any_train
}

/// True when the operator paused the whole factory or this stage.
fn stage_is_paused(state: &StateView, stage: Stage) -> bool {
    state.paused.global || state.paused.stages.contains(&stage)
}

/// True when a pause blocks the start of this task: the factory, its
/// stage, or its repository.
fn task_is_paused(state: &StateView, task: &TaskView) -> bool {
    state.paused.global
        || state.paused.stages.contains(&task.stage)
        || state.paused.repos.iter().any(|repo| repo == &task.repo)
}

/// Move the selection of the app by `delta` rows in the pipeline view.
///
/// `1` moves down and `-1` moves up. The movement clamps at the ends. With
/// no selection, `j` picks the first row and `k` picks the last one.
pub(super) fn move_selection(app: &mut App, delta: isize) {
    let Some(state) = app.state.as_ref() else {
        app.selection = Selection::None;
        return;
    };
    let count = rows(state).len();
    if count == 0 {
        app.selection = Selection::None;
        return;
    }
    let last = count - 1;
    let next = match app.selection {
        Selection::None => {
            if delta < 0 {
                last
            } else {
                0
            }
        }
        Selection::Row(index) => {
            let target = index as isize + delta;
            target.clamp(0, last as isize) as usize
        }
    };
    app.selection = Selection::Row(next);
}

/// The one change `+` or `-` resolved from the selected row.
enum Target {
    /// A new limit for one stage.
    Limit {
        /// The stage.
        stage: Stage,
        /// The clamped new limit.
        limit: usize,
    },
    /// A new reservation for one lane.
    Lane {
        /// The stage of the lane.
        stage: Stage,
        /// The repository alias of the lane.
        repo: String,
        /// The clamped new slot count.
        slots: usize,
    },
}

/// One safe adjustment to a limit or a lane reservation.
#[derive(Clone, Copy)]
enum AmountChange {
    /// Add one unless the value is already the largest value.
    Increase,
    /// Remove one without crossing the given lower bound.
    Decrease,
}

impl AmountChange {
    /// Apply this adjustment to `value` with the given lower bound.
    fn apply(self, value: usize, minimum: usize) -> usize {
        match self {
            AmountChange::Increase => value.saturating_add(1),
            AmountChange::Decrease => value.saturating_sub(1).max(minimum),
        }
    }
}

/// Handle one key press in the pipeline view.
///
/// The call resolves the selected row and sends at most one action through
/// `sink`. A key with no resolvable target changes nothing and toasts
/// nothing.
pub(super) fn handle_key(app: &mut App, key: KeyEvent, sink: &mut impl ActionSink) {
    if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
        return;
    }
    match key.code {
        KeyCode::Char('+') => change_amount(app, sink, AmountChange::Increase),
        KeyCode::Char('-') => change_amount(app, sink, AmountChange::Decrease),
        KeyCode::Char('p') => pause_selected(app, sink),
        KeyCode::Char('P') => pause_all(app, sink),
        KeyCode::Char('r') => refine_ticket(app, sink),
        KeyCode::Char('n') => create_ticket(app, sink),
        KeyCode::Char('x') => ask_abort(app),
        KeyCode::Char('R') => retry_failed(app, sink),
        KeyCode::Char(' ') => stack_head(app, sink),
        KeyCode::Char('g') => ask_release(app),
        KeyCode::Char('s') => cycle_policy(app, sink),
        _ => {}
    }
}

/// The row the operator selected, cloned out of the row list.
fn selected_row(app: &App) -> Option<Row> {
    let state = app.state.as_ref()?;
    let index = match app.selection {
        Selection::Row(index) => index,
        Selection::None => return None,
    };
    rows(state).into_iter().nth(index)
}

/// Change the selected stage limit or the selected repository lane.
///
/// The stage limit clamps at one. The lane reservation clamps at zero,
/// which removes the lane.
fn change_amount(app: &mut App, sink: &mut impl ActionSink, change: AmountChange) {
    let target = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(row) = selected_row(app) else {
            return;
        };
        match row {
            Row::Stage { stage } => {
                let Some(view) = state
                    .stages
                    .iter()
                    .find(|stage_view| stage_view.stage == stage)
                else {
                    return;
                };
                let limit = change.apply(view.limit, 1);
                Target::Limit { stage, limit }
            }
            Row::Repo { stage, repo } => {
                let slots = state
                    .lanes
                    .iter()
                    .find(|lane| lane.stage == stage && lane.repo == repo)
                    .map(|lane| lane.slots)
                    .unwrap_or(0);
                let slots = change.apply(slots, 0);
                Target::Lane { stage, repo, slots }
            }
            Row::Ticket { .. } | Row::Train { .. } => return,
        }
    };
    match target {
        Target::Limit { stage, limit } => {
            emit(
                app,
                sink,
                Action::Limit { stage, limit },
                format!("sent limit {} {limit}", stage.as_str()),
            );
            if let Some(stage_view) = app
                .state
                .as_mut()
                .and_then(|state| state.stages.iter_mut().find(|view| view.stage == stage))
            {
                stage_view.limit = limit;
            }
        }
        Target::Lane { stage, repo, slots } => {
            emit(
                app,
                sink,
                Action::Lane {
                    stage,
                    repo: repo.clone(),
                    slots,
                },
                format!("sent lane {} {repo} {slots}", stage.as_str()),
            );
            if let Some(state) = app.state.as_mut() {
                if let Some(lane) = state
                    .lanes
                    .iter_mut()
                    .find(|lane| lane.stage == stage && lane.repo == repo)
                {
                    lane.slots = slots;
                } else if slots > 0 {
                    state.lanes.push(LaneView { stage, repo, slots });
                }
            }
        }
    }
}

/// Pause or resume the selected scope with `p`.
///
/// A stage row pauses the stage. A repository, ticket, or train row pauses
/// the repository of that row.
fn pause_selected(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(row) = selected_row(app) else {
            return;
        };
        let (scope, label) = match row {
            Row::Stage { stage } => (PauseScope::Stage { stage }, stage.as_str().to_string()),
            Row::Repo { repo, .. } => {
                let label = repo.clone();
                (PauseScope::Repo { repo }, label)
            }
            Row::Ticket { index } => match state.tasks.get(index) {
                Some(task) => {
                    let label = task.repo.clone();
                    (
                        PauseScope::Repo {
                            repo: task.repo.clone(),
                        },
                        label,
                    )
                }
                None => return,
            },
            Row::Train { repo } => {
                let label = repo.clone();
                (PauseScope::Repo { repo }, label)
            }
        };
        let paused = match &scope {
            PauseScope::Stage { stage } => !state.paused.stages.contains(stage),
            PauseScope::Repo { repo } => !state.paused.repos.contains(repo),
            PauseScope::Global => true,
        };
        (scope, paused, label)
    };
    let (scope, paused, label) = found;
    let operation = if paused { "pause" } else { "resume" };
    emit(
        app,
        sink,
        Action::Pause {
            scope: scope.clone(),
            paused,
        },
        format!("sent {operation} {label}"),
    );
    if let Some(state) = app.state.as_mut() {
        match scope {
            PauseScope::Global => state.paused.global = paused,
            PauseScope::Stage { stage } => {
                if paused {
                    if !state.paused.stages.contains(&stage) {
                        state.paused.stages.push(stage);
                    }
                } else {
                    state.paused.stages.retain(|entry| *entry != stage);
                }
            }
            PauseScope::Repo { repo } => {
                if paused {
                    if !state.paused.repos.contains(&repo) {
                        state.paused.repos.push(repo);
                    }
                } else {
                    state.paused.repos.retain(|entry| *entry != repo);
                }
            }
        }
    }
}

/// Pause or resume the whole factory with `P`.
fn pause_all(app: &mut App, sink: &mut impl ActionSink) {
    let paused = match app.state.as_ref() {
        Some(state) => !state.paused.global,
        None => return,
    };
    let operation = if paused { "pause" } else { "resume" };
    emit(
        app,
        sink,
        Action::Pause {
            scope: PauseScope::Global,
            paused,
        },
        format!("sent {operation} all"),
    );
    if let Some(state) = app.state.as_mut() {
        state.paused.global = paused;
    }
}

/// Send `Action::Refine` for the selected ticket and follow the new task.
fn refine_ticket(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Ticket { index }) = selected_row(app) else {
            return;
        };
        let Some(task) = state.tasks.get(index) else {
            return;
        };
        (task.repo.clone(), task.kind, task.number)
    };
    let (repo, kind, number) = found;
    emit(
        app,
        sink,
        Action::Refine {
            repo: repo.clone(),
            kind,
            number,
        },
        format!("sent refine {repo} {}{number}", kind.as_str()),
    );
    wait_for_task(
        app,
        format!(
            "{repo}/{}-{}{number}",
            Stage::Refine.as_str(),
            kind.as_str()
        ),
        Wanted::Refine { repo, kind, number },
    );
}

/// Send `Action::TicketCreate` for the selected repository and follow it.
fn create_ticket(app: &mut App, sink: &mut impl ActionSink) {
    let Some(Row::Repo { repo, .. }) = selected_row(app) else {
        return;
    };
    emit(
        app,
        sink,
        Action::TicketCreate { repo: repo.clone() },
        format!("sent new ticket {repo}"),
    );
    wait_for_task(
        app,
        format!("{repo}/{}-i0", Stage::Refine.as_str()),
        Wanted::Create { repo },
    );
}

/// Show an empty session until the requested task reaches a state push.
fn wait_for_task(app: &mut App, task: String, wanted: Wanted) {
    app.session.clear();
    app.wanted = Some(wanted);
    app.session_task = Some(task);
    app.view = View::Session;
}

/// Ask the operator to confirm the abort of the selected task.
fn ask_abort(app: &mut App) {
    let task = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Ticket { index }) = selected_row(app) else {
            return;
        };
        let Some(task) = state.tasks.get(index) else {
            return;
        };
        task.id.clone()
    };
    app.confirm = Some(Confirm::Abort { task });
}

/// Retry the selected task when it failed.
fn retry_failed(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Ticket { index }) = selected_row(app) else {
            return;
        };
        let Some(task) = state.tasks.get(index) else {
            return;
        };
        let TaskState::Failed(_) = task.state else {
            return;
        };
        (index, task.id.clone())
    };
    let (index, task) = found;
    emit(
        app,
        sink,
        Action::Retry { task: task.clone() },
        format!("sent retry {task}"),
    );
    if let Some(task) = app
        .state
        .as_mut()
        .and_then(|state| state.tasks.get_mut(index))
    {
        task.state = TaskState::Queued;
        task.attempt = 1;
    }
}

/// Stack or unstack the first pull request of the selected train queue.
fn stack_head(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Train { repo }) = selected_row(app) else {
            return;
        };
        let Some(train) = state.trains.iter().find(|train| train.repo == repo) else {
            return;
        };
        let Some(pr) = train.queue.first().copied() else {
            return;
        };
        let on = !train.stacked.contains(&pr);
        (repo, pr, on)
    };
    let (repo, pr, on) = found;
    let word = if on { "stack" } else { "unstack" };
    emit(
        app,
        sink,
        Action::Stack {
            repo: repo.clone(),
            pr,
            on,
        },
        format!("sent {word} #{pr} {repo}"),
    );
    if let Some(train) = app
        .state
        .as_mut()
        .and_then(|state| state.trains.iter_mut().find(|train| train.repo == repo))
    {
        if on {
            if !train.stacked.contains(&pr) {
                train.stacked.push(pr);
            }
        } else {
            train.stacked.retain(|entry| *entry != pr);
        }
    }
}

/// Ask the operator to confirm the release of the stacked batch.
fn ask_release(app: &mut App) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Train { repo }) = selected_row(app) else {
            return;
        };
        let Some(train) = state.trains.iter().find(|train| train.repo == repo) else {
            return;
        };
        if train.stacked.is_empty() {
            return;
        }
        (repo, train.stacked.clone())
    };
    let (repo, prs) = found;
    app.confirm = Some(Confirm::Go { repo, prs });
}

/// Cycle the policy of the selected train and send `Action::Policy`.
fn cycle_policy(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::Train { repo }) = selected_row(app) else {
            return;
        };
        let Some(train) = state.trains.iter().find(|train| train.repo == repo) else {
            return;
        };
        let next = next_policy(&train.policy);
        (repo, next)
    };
    let (repo, policy) = found;
    emit(
        app,
        sink,
        Action::Policy {
            repo: repo.clone(),
            policy: policy.clone(),
        },
        format!("sent policy {repo} {}", policy_label(&policy)),
    );
    if let Some(train) = app
        .state
        .as_mut()
        .and_then(|state| state.trains.iter_mut().find(|train| train.repo == repo))
    {
        train.policy = policy;
    }
}

/// The next policy in the manual, interval, threshold cycle.
fn next_policy(policy: &ReleasePolicy) -> ReleasePolicy {
    match policy {
        ReleasePolicy::Manual => ReleasePolicy::Interval { minutes: 30 },
        ReleasePolicy::Interval { .. } => ReleasePolicy::Threshold { count: 3 },
        ReleasePolicy::Threshold { .. } => ReleasePolicy::Manual,
    }
}

/// Draw the pipeline view into `area` at the given Unix time.
pub(super) fn draw(f: &mut Frame, app: &App, area: Rect, now_ms: u64) {
    let Some(state) = app.state.as_ref() else {
        let hint = Line::from(Span::styled(
            " waiting for the first state push from the daemon",
            THEME.dim(),
        ));
        f.render_widget(Paragraph::new(hint), area);
        return;
    };
    let all = rows(state);
    let selected = match app.selection {
        Selection::Row(index) if index < all.len() => Some(index),
        _ => None,
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in all.iter().enumerate() {
        let is_selected = selected == Some(index);
        let marker = if is_selected {
            Span::styled(
                "▸ ",
                Style::default()
                    .fg(THEME.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let mut spans = vec![marker];
        spans.extend(row_spans(state, row, now_ms));
        let mut line = Line::from(spans);
        if is_selected {
            line = line.style(THEME.selected());
        }
        lines.push(line);
        if let Row::Stage { stage } = row {
            if stage_is_empty(state, *stage) {
                lines.push(Line::from(Span::styled("    no tasks", THEME.dim())));
            }
        }
    }
    if state.stages.iter().any(|stage| stage.overridden) {
        lines.push(Line::from(Span::styled(
            "  * limit differs from the config file",
            THEME.dim(),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// The spans of one row of the pipeline view.
fn row_spans(state: &StateView, row: &Row, now_ms: u64) -> Vec<Span<'static>> {
    match row {
        Row::Stage { stage } => stage_spans(state, *stage),
        Row::Repo { repo, .. } => repo_spans(state, repo),
        Row::Ticket { index } => match state.tasks.get(*index) {
            Some(task) => ticket_spans(state, task),
            None => vec![Span::raw("    (missing task)")],
        },
        Row::Train { repo } => train_spans(state, repo, now_ms),
    }
}

/// The spans of one stage header row: name, running/queued, and limit.
fn stage_spans(state: &StateView, stage: Stage) -> Vec<Span<'static>> {
    let header = Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(stage.as_str().to_string(), header)];
    let Some(view) = state.stages.iter().find(|s| s.stage == stage) else {
        return spans;
    };
    let running_color = if view.running > 0 {
        THEME.ok
    } else {
        THEME.dim
    };
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        view.running.to_string(),
        Style::default().fg(running_color),
    ));
    spans.push(Span::styled("/", THEME.dim()));
    spans.push(Span::raw(view.queued.to_string()));
    spans.push(Span::styled(" of ", THEME.dim()));
    spans.push(Span::raw(view.limit.to_string()));
    if view.overridden {
        spans.push(Span::styled(
            "*",
            Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
        ));
    }
    if stage_is_paused(state, stage) {
        spans.push(paused_span());
    }
    spans
}

/// The bold warn span that marks a paused scope.
fn paused_span() -> Span<'static> {
    Span::styled(
        "  paused",
        Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
    )
}

/// The spans of one repository group header row.
fn repo_spans(state: &StateView, repo: &str) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(repo.to_string(), THEME.dim().add_modifier(Modifier::BOLD)),
    ];
    if state.paused.repos.iter().any(|entry| entry == repo) {
        spans.push(paused_span());
    }
    spans
}

/// The spans of one ticket row: item, state, and attempt.
///
/// A queued task that a pause blocks shows the pause instead of the queue
/// state, because it cannot start. A task in any other state keeps its true
/// state: a pause blocks starts, it does not stop running tasks.
fn ticket_spans(state: &StateView, task: &TaskView) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(
            format!("{}{}", task.kind.as_str(), task.number),
            Style::default().fg(THEME.text),
        ),
        Span::raw(" "),
    ];
    if matches!(task.state, TaskState::Queued) && task_is_paused(state, task) {
        spans.push(Span::styled("paused", Style::default().fg(THEME.warn)));
    } else {
        spans.push(state_span(&task.state));
    }
    if task.attempt > 1 {
        spans.push(Span::styled(
            format!("  attempt {}", task.attempt),
            THEME.dim(),
        ));
    }
    spans
}

/// The colored label of one task state.
fn state_span(state: &TaskState) -> Span<'static> {
    match state {
        TaskState::Queued => Span::styled("queued", THEME.dim()),
        TaskState::Running => Span::styled("running", Style::default().fg(THEME.accent)),
        TaskState::AwaitingUser => Span::styled("awaiting user", Style::default().fg(THEME.warn)),
        TaskState::Done => Span::styled("done", Style::default().fg(THEME.ok)),
        TaskState::Failed(reason) => Span::styled(
            format!("failed: {reason}"),
            Style::default().fg(THEME.error),
        ),
    }
}

/// The spans of one release train row.
fn train_spans(state: &StateView, repo: &str, now_ms: u64) -> Vec<Span<'static>> {
    let header = Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw("  "), Span::styled("train", header)];
    let Some(train) = state.trains.iter().find(|t| t.repo == repo) else {
        return spans;
    };
    spans.push(Span::styled("  queue ", THEME.dim()));
    spans.push(Span::raw(numbers(&train.queue)));
    spans.push(Span::styled("  stacked ", THEME.dim()));
    spans.push(Span::raw(numbers(&train.stacked)));
    spans.push(Span::styled("  policy ", THEME.dim()));
    spans.push(Span::raw(policy_label(&train.policy)));
    if let Some(fire) = train.next_fire_ms {
        let remaining = fire.saturating_sub(now_ms);
        spans.push(Span::styled("  fires in ", THEME.dim()));
        spans.push(Span::raw(format_countdown(remaining)));
    }
    if let Some(batch) = &train.in_flight {
        spans.push(Span::styled("  batch ", THEME.dim()));
        spans.push(Span::raw(batch.clone()));
    }
    spans
}

/// The numbers as `7,9`, or `none` for an empty set.
fn numbers(values: &[u64]) -> String {
    if values.is_empty() {
        return "none".to_string();
    }
    values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The short label of one release policy.
fn policy_label(policy: &ReleasePolicy) -> String {
    match policy {
        ReleasePolicy::Manual => "manual".to_string(),
        ReleasePolicy::Interval { minutes } => format!("every {minutes}m"),
        ReleasePolicy::Threshold { count } => format!("at {count} ready"),
    }
}

/// The remaining time as `4m12s`, `1h02m`, or `59s`.
fn format_countdown(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// One task view with a derived id and a fake log path.
///
/// Test support for this file and for the shell tests in `mod.rs`.
#[cfg(test)]
fn task(
    repo: &str,
    stage: Stage,
    kind: ItemKind,
    number: u64,
    state: TaskState,
    attempt: u32,
) -> TaskView {
    TaskView {
        id: format!("{repo}/{}-{}{number}", stage.as_str(), kind.as_str()),
        repo: repo.to_string(),
        stage,
        kind,
        number,
        state,
        attempt,
        log_path: std::env::temp_dir().join(format!("{repo}-{stage}-{number}.jsonl")),
    }
}

/// A state view with tasks in every stage and two repositories.
///
/// Test support for this file and for the shell tests in `mod.rs`.
#[cfg(test)]
pub(crate) fn sample_view() -> StateView {
    StateView {
        repos: vec![
            RepoView {
                alias: "borsuk".to_string(),
                owner_repo: String::new(),
            },
            RepoView {
                alias: "ryba".to_string(),
                owner_repo: String::new(),
            },
        ],
        stages: vec![
            StageView {
                stage: Stage::Refine,
                limit: 3,
                overridden: false,
                running: 1,
                queued: 1,
            },
            StageView {
                stage: Stage::Implement,
                limit: 5,
                overridden: true,
                running: 1,
                queued: 1,
            },
            StageView {
                stage: Stage::Review,
                limit: 7,
                overridden: false,
                running: 1,
                queued: 0,
            },
            StageView {
                stage: Stage::Release,
                limit: 1,
                overridden: false,
                running: 1,
                queued: 0,
            },
        ],
        lanes: Vec::new(),
        tasks: vec![
            task(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                142,
                TaskState::Queued,
                1,
            ),
            task(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                143,
                TaskState::Running,
                1,
            ),
            task(
                "borsuk",
                Stage::Implement,
                ItemKind::Issue,
                140,
                TaskState::Running,
                2,
            ),
            task(
                "ryba",
                Stage::Implement,
                ItemKind::Issue,
                7,
                TaskState::Queued,
                1,
            ),
            task(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                7,
                TaskState::Running,
                1,
            ),
            task(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                9,
                TaskState::AwaitingUser,
                1,
            ),
            task(
                "borsuk",
                Stage::Release,
                ItemKind::Pr,
                5,
                TaskState::Running,
                1,
            ),
            task(
                "ryba",
                Stage::Refine,
                ItemKind::Issue,
                9,
                TaskState::Failed("exit 1".to_string()),
                3,
            ),
        ],
        decisions: Vec::new(),
        trains: vec![
            TrainView {
                repo: "borsuk".to_string(),
                queue: vec![7, 9],
                stacked: vec![3],
                policy: ReleasePolicy::Manual,
                next_fire_ms: None,
                in_flight: Some("borsuk/release-p5".to_string()),
            },
            TrainView {
                repo: "ryba".to_string(),
                queue: Vec::new(),
                stacked: Vec::new(),
                policy: ReleasePolicy::Interval { minutes: 30 },
                next_fire_ms: Some(super::inbox::now_ms().unwrap() + 90_000),
                in_flight: None,
            },
        ],
        paused: PausedView {
            global: false,
            stages: Vec::new(),
            repos: Vec::new(),
        },
    }
}

/// Render the app into a test backend and return the visible text.
///
/// Test support for this file and for the shell tests in `mod.rs`.
#[cfg(test)]
pub(super) fn render_to_string(app: &mut App) -> String {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| super::render(frame, app).unwrap())
        .unwrap();
    buffer_text(terminal.backend().buffer())
}

/// The text of a buffer, one trimmed line per row.
///
/// Test support for this file and for the shell tests in `mod.rs`.
#[cfg(test)]
fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            let cell = &buffer.content[(y * buffer.area.width + x) as usize];
            line.push_str(cell.symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
use crate::model::ItemKind;
#[cfg(test)]
use crate::sock::{PausedView, RepoView, StageView, TrainView};
#[cfg(test)]
use ratatui::backend::TestBackend;
#[cfg(test)]
use ratatui::Terminal;

#[cfg(test)]
mod tests {
    use super::*;

    /// A state view with no repositories, tasks, and trains.
    fn empty_view() -> StateView {
        StateView {
            repos: Vec::new(),
            stages: Stage::ALL
                .iter()
                .map(|&stage| StageView {
                    stage,
                    limit: 3,
                    overridden: false,
                    running: 0,
                    queued: 0,
                })
                .collect(),
            lanes: Vec::new(),
            tasks: Vec::new(),
            decisions: Vec::new(),
            trains: Vec::new(),
            paused: PausedView {
                global: false,
                stages: Vec::new(),
                repos: Vec::new(),
            },
        }
    }

    #[test]
    fn rows_order_stages_then_repositories_then_tickets() {
        let state = sample_view();
        let rows = rows(&state);
        let stages: Vec<Stage> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Stage { stage } => Some(*stage),
                _ => None,
            })
            .collect();
        assert_eq!(stages, Stage::ALL.to_vec());
        // The refine group of borsuk holds its two tickets, and the ryba
        // group follows with its failed ticket.
        let refine_start = rows
            .iter()
            .position(|row| {
                *row == Row::Stage {
                    stage: Stage::Refine,
                }
            })
            .unwrap();
        assert_eq!(
            rows[refine_start + 1],
            Row::Repo {
                stage: Stage::Refine,
                repo: "borsuk".to_string(),
            }
        );
        assert_eq!(rows[refine_start + 2], Row::Ticket { index: 0 });
        assert_eq!(rows[refine_start + 3], Row::Ticket { index: 1 });
        assert_eq!(
            rows[refine_start + 4],
            Row::Repo {
                stage: Stage::Refine,
                repo: "ryba".to_string(),
            }
        );
        assert_eq!(rows[refine_start + 5], Row::Ticket { index: 7 });
        // The release stage holds the ticket and the train of each repo.
        let release_start = rows
            .iter()
            .position(|row| {
                *row == Row::Stage {
                    stage: Stage::Release,
                }
            })
            .unwrap();
        assert_eq!(rows[release_start + 2], Row::Ticket { index: 6 });
        assert_eq!(
            rows[release_start + 3],
            Row::Train {
                repo: "borsuk".to_string(),
            }
        );
        assert_eq!(
            rows[release_start + 5],
            Row::Train {
                repo: "ryba".to_string(),
            }
        );
    }

    #[test]
    fn move_selection_walks_and_clamps() {
        let mut app = App::default();
        // Without a state there is nothing to select.
        move_selection(&mut app, 1);
        assert_eq!(app.selection, Selection::None);

        app.state = Some(sample_view());
        let last = rows(app.state.as_ref().unwrap()).len() - 1;

        move_selection(&mut app, 1);
        assert_eq!(app.selection, Selection::Row(0));
        for _ in 0..(last + 5) {
            move_selection(&mut app, 1);
        }
        assert_eq!(app.selection, Selection::Row(last));
        move_selection(&mut app, -1);
        assert_eq!(app.selection, Selection::Row(last - 1));
        // A fresh selection with k picks the last row.
        app.selection = Selection::None;
        move_selection(&mut app, -1);
        assert_eq!(app.selection, Selection::Row(last));
    }

    #[test]
    fn empty_state_shows_every_stage_header() {
        let mut app = App {
            state: Some(empty_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        for stage in ["refine", "implement", "review", "release"] {
            assert!(text.contains(stage), "missing stage header {stage}");
        }
        assert!(text.contains("no tasks"));
        assert!(text.contains("0/0 of 3"));
        assert!(!text.contains("limit differs"));
    }

    #[test]
    fn full_state_shows_stage_counts_and_tickets() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("refine  1/1 of 3"));
        assert!(text.contains("implement  1/1 of 5*"));
        assert!(text.contains("limit differs from the config file"));
        assert!(text.contains("borsuk"));
        assert!(text.contains("ryba"));
        assert!(text.contains("i142 queued"));
        assert!(text.contains("i143 running"));
        assert!(text.contains("i140 running"));
        assert!(text.contains("attempt 2"));
        assert!(text.contains("i7 queued"));
        assert!(text.contains("p7 running"));
        assert!(text.contains("p9 awaiting user"));
        assert!(text.contains("p5 running"));
        assert!(text.contains("i9 failed: exit 1"));
    }

    #[test]
    fn full_state_shows_the_release_group() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("train"));
        assert!(text.contains("queue 7,9"));
        assert!(text.contains("stacked 3"));
        assert!(text.contains("policy manual"));
        assert!(text.contains("batch borsuk/release-p5"));
        assert!(text.contains("policy every 30m"));
        assert!(text.contains("fires in 1m"));
    }

    #[test]
    fn the_selected_row_carries_the_marker() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            selection: Selection::Row(2),
            ..App::default()
        };
        let text = render_to_string(&mut app);
        let line = text.lines().find(|line| line.contains("i142")).unwrap();
        assert!(line.starts_with("▸"), "unmarked selected line: {line}");
    }

    #[test]
    fn paused_stages_show_the_pause_mark() {
        let state = StateView {
            paused: PausedView {
                global: true,
                stages: Vec::new(),
                repos: Vec::new(),
            },
            ..sample_view()
        };
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        // A global pause marks the four stage headers, the status bar, and
        // the two queued tickets.
        let marked: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("paused"))
            .collect();
        assert_eq!(marked.len(), 7);
        assert_eq!(
            marked.iter().filter(|line| line.contains(" of ")).count(),
            4
        );
        assert!(text.contains("i142 paused"));
        assert!(text.contains("i7 paused"));
        assert!(!text.contains("i142 queued"));
        assert!(!text.contains("i7 queued"));

        // A stage pause marks only that stage header and its queued ticket.
        let state = StateView {
            paused: PausedView {
                global: false,
                stages: vec![Stage::Refine],
                repos: Vec::new(),
            },
            ..sample_view()
        };
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("paused"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("refine"));
        assert!(lines[1].contains("i142"));
    }

    #[test]
    fn a_paused_repository_marks_its_group_rows_and_queued_tickets() {
        let state = StateView {
            paused: PausedView {
                global: false,
                stages: Vec::new(),
                repos: vec!["borsuk".to_string()],
            },
            ..sample_view()
        };
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        // Every group row of borsuk carries the mark. The train row stays
        // unmarked: the daemon fires trains regardless of a pause.
        let marked = text
            .lines()
            .filter(|line| line.contains("borsuk") && line.contains("paused"))
            .count();
        assert_eq!(marked, 4);
        let train = text
            .lines()
            .find(|line| line.contains("train") && line.contains("queue 7,9"))
            .expect("the borsuk train row must be visible");
        assert!(!train.contains("paused"), "marked train row: {train}");
        assert!(text.contains("i142 paused"));
        assert!(!text.contains("i142 queued"));
        // A running ticket of the paused repository keeps its true state.
        assert!(text.contains("i140 running"));
        // The unpaused repository keeps its queued state.
        assert!(text.contains("i7 queued"));
        assert!(!text.contains("i7 paused"));
    }

    #[test]
    fn an_unpaused_repository_shows_no_pause_mark() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(!text.contains("paused"));
    }

    #[test]
    fn policy_label_covers_every_policy() {
        assert_eq!(policy_label(&ReleasePolicy::Manual), "manual");
        assert_eq!(
            policy_label(&ReleasePolicy::Interval { minutes: 30 }),
            "every 30m"
        );
        assert_eq!(
            policy_label(&ReleasePolicy::Threshold { count: 5 }),
            "at 5 ready"
        );
    }

    #[test]
    fn format_countdown_formats_each_range() {
        assert_eq!(format_countdown(0), "0s");
        assert_eq!(format_countdown(59_000), "59s");
        assert_eq!(format_countdown(60_000), "1m00s");
        assert_eq!(format_countdown(90_000), "1m30s");
        assert_eq!(format_countdown(3_600_000), "1h00m");
        assert_eq!(format_countdown(7_380_000), "2h03m");
    }

    /// An action sink that records what it sent.
    #[derive(Default)]
    struct FakeSink(Vec<Action>);

    impl ActionSink for FakeSink {
        fn send_action(&mut self, action: Action) {
            self.0.push(action);
        }
    }

    /// A plain key press of one character.
    fn pressed(character: char) -> KeyEvent {
        KeyEvent::new(
            KeyCode::Char(character),
            crossterm::event::KeyModifiers::empty(),
        )
    }

    /// The sample app with one selected row.
    fn app_with_selection(index: usize) -> App {
        App {
            state: Some(sample_view()),
            connected: true,
            selection: Selection::Row(index),
            ..App::default()
        }
    }

    #[test]
    fn plus_and_minus_change_the_stage_limit() {
        let mut app = app_with_selection(0);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('+'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Limit {
                stage: Stage::Refine,
                limit: 4
            }]
        );
        assert!(app.visible_toast().is_some());

        // The limit clamps at one and never goes below it.
        app.state.as_mut().unwrap().stages[0].limit = 1;
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('-'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Limit {
                stage: Stage::Refine,
                limit: 1
            }]
        );
    }

    #[test]
    fn repeated_amount_keys_use_the_previous_request_and_saturate() {
        let mut app = app_with_selection(0);
        let mut sink = FakeSink::default();
        for character in ['+', '+', '-'] {
            handle_key(&mut app, pressed(character), &mut sink);
        }
        assert_eq!(
            sink.0,
            vec![
                Action::Limit {
                    stage: Stage::Refine,
                    limit: 4,
                },
                Action::Limit {
                    stage: Stage::Refine,
                    limit: 5,
                },
                Action::Limit {
                    stage: Stage::Refine,
                    limit: 4,
                },
            ]
        );

        app.state.as_mut().unwrap().stages[0].limit = usize::MAX;
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('+'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Limit {
                stage: Stage::Refine,
                limit: usize::MAX,
            }]
        );

        let mut app = app_with_selection(1);
        let mut sink = FakeSink::default();
        for character in ['+', '+', '-'] {
            handle_key(&mut app, pressed(character), &mut sink);
        }
        assert_eq!(
            sink.0,
            vec![
                Action::Lane {
                    stage: Stage::Refine,
                    repo: "borsuk".to_string(),
                    slots: 1,
                },
                Action::Lane {
                    stage: Stage::Refine,
                    repo: "borsuk".to_string(),
                    slots: 2,
                },
                Action::Lane {
                    stage: Stage::Refine,
                    repo: "borsuk".to_string(),
                    slots: 1,
                },
            ]
        );
    }

    #[test]
    fn plus_and_minus_change_the_lane_of_a_repository_row() {
        let mut app = app_with_selection(1);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('+'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Lane {
                stage: Stage::Refine,
                repo: "borsuk".to_string(),
                slots: 1
            }]
        );

        // An existing lane reservation counts first.
        app.state.as_mut().unwrap().lanes = vec![LaneView {
            stage: Stage::Refine,
            repo: "borsuk".to_string(),
            slots: 2,
        }];
        handle_key(&mut app, pressed('-'), &mut sink);
        assert_eq!(
            sink.0.last(),
            Some(&Action::Lane {
                stage: Stage::Refine,
                repo: "borsuk".to_string(),
                slots: 1
            })
        );
    }

    #[test]
    fn p_pauses_the_selected_scope_and_p_capital_pauses_all() {
        let mut app = app_with_selection(0);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Refine
                },
                paused: true
            }]
        );

        // A repository row pauses that repository.
        let mut app = app_with_selection(1);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Repo {
                    repo: "borsuk".to_string()
                },
                paused: true
            }]
        );

        // A ticket row pauses the repository of its task.
        let mut app = app_with_selection(2);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Repo {
                    repo: "borsuk".to_string()
                },
                paused: true
            }]
        );

        let mut app = app_with_selection(0);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('P'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Global,
                paused: true
            }]
        );
    }

    #[test]
    fn pause_keys_toggle_the_current_scope_and_name_the_operation() {
        let mut app = app_with_selection(0);
        app.state
            .as_mut()
            .unwrap()
            .paused
            .stages
            .push(Stage::Refine);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Refine,
                },
                paused: false,
            }]
        );
        assert_eq!(app.visible_toast(), Some("sent resume refine"));

        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0.last(),
            Some(&Action::Pause {
                scope: PauseScope::Stage {
                    stage: Stage::Refine,
                },
                paused: true,
            })
        );
        assert_eq!(app.visible_toast(), Some("sent pause refine"));

        let mut app = app_with_selection(2);
        app.state
            .as_mut()
            .unwrap()
            .paused
            .repos
            .push("borsuk".to_string());
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('p'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Pause {
                scope: PauseScope::Repo {
                    repo: "borsuk".to_string(),
                },
                paused: false,
            }]
        );
        assert_eq!(app.visible_toast(), Some("sent resume borsuk"));

        let mut app = app_with_selection(0);
        app.state.as_mut().unwrap().paused.global = true;
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('P'), &mut sink);
        handle_key(&mut app, pressed('P'), &mut sink);
        assert_eq!(
            sink.0,
            vec![
                Action::Pause {
                    scope: PauseScope::Global,
                    paused: false,
                },
                Action::Pause {
                    scope: PauseScope::Global,
                    paused: true,
                },
            ]
        );
        assert_eq!(app.visible_toast(), Some("sent pause all"));
    }

    #[test]
    fn r_refines_the_selected_ticket_and_follows_the_new_task() {
        let mut app = app_with_selection(8);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('r'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Refine {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 140
            }]
        );
        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/refine-i140"));
    }

    #[test]
    fn n_creates_a_ticket_for_the_selected_repository() {
        let mut app = app_with_selection(4);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('n'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::TicketCreate {
                repo: "ryba".to_string()
            }]
        );
        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("ryba/refine-i0"));
    }

    #[test]
    fn x_only_opens_the_abort_confirmation() {
        let mut app = app_with_selection(8);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('x'), &mut sink);
        assert!(sink.0.is_empty(), "x must not send before the confirm");
        assert_eq!(
            app.confirm,
            Some(Confirm::Abort {
                task: "borsuk/implement-i140".to_string()
            })
        );
    }

    #[test]
    fn r_capital_retries_only_a_failed_task() {
        let mut app = app_with_selection(5);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('R'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Retry {
                task: "ryba/refine-i9".to_string()
            }]
        );

        // A running ticket does not retry.
        let mut app = app_with_selection(8);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('R'), &mut sink);
        assert!(sink.0.is_empty());
    }

    #[test]
    fn repeated_r_capital_sends_one_retry_for_one_failure() {
        let mut app = app_with_selection(5);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('R'), &mut sink);
        handle_key(&mut app, pressed('R'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Retry {
                task: "ryba/refine-i9".to_string(),
            }]
        );
        assert_eq!(app.visible_toast(), Some("sent retry ryba/refine-i9"));
    }

    #[test]
    fn space_stacks_the_first_queue_entry_of_a_train() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Stack {
                repo: "borsuk".to_string(),
                pr: 7,
                on: true
            }]
        );

        // A train with an empty queue stacks nothing.
        let mut app = app_with_selection(20);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        assert!(sink.0.is_empty());
    }

    #[test]
    fn repeated_space_toggles_the_same_queue_entry() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        handle_key(&mut app, pressed(' '), &mut sink);
        assert_eq!(
            sink.0,
            vec![
                Action::Stack {
                    repo: "borsuk".to_string(),
                    pr: 7,
                    on: true,
                },
                Action::Stack {
                    repo: "borsuk".to_string(),
                    pr: 7,
                    on: false,
                },
            ]
        );
        assert_eq!(app.visible_toast(), Some("sent unstack #7 borsuk"));
    }

    #[test]
    fn release_confirmation_includes_a_just_stacked_pull_request() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        handle_key(&mut app, pressed('g'), &mut sink);
        assert_eq!(
            app.confirm,
            Some(Confirm::Go {
                repo: "borsuk".to_string(),
                prs: vec![3, 7],
            })
        );
    }

    #[test]
    fn g_only_opens_the_release_confirmation() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('g'), &mut sink);
        assert!(sink.0.is_empty(), "g must not send before the confirm");
        assert_eq!(
            app.confirm,
            Some(Confirm::Go {
                repo: "borsuk".to_string(),
                prs: vec![3]
            })
        );

        // An empty stacked set opens no confirmation.
        let mut app = app_with_selection(20);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('g'), &mut sink);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn s_cycles_the_release_policy() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('s'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Policy {
                repo: "borsuk".to_string(),
                policy: ReleasePolicy::Interval { minutes: 30 }
            }]
        );

        app.state.as_mut().unwrap().trains[0].policy = ReleasePolicy::Threshold { count: 5 };
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('s'), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Policy {
                repo: "borsuk".to_string(),
                policy: ReleasePolicy::Manual
            }]
        );
    }

    #[test]
    fn repeated_s_completes_the_full_policy_cycle() {
        let mut app = app_with_selection(18);
        let mut sink = FakeSink::default();
        for _ in 0..3 {
            handle_key(&mut app, pressed('s'), &mut sink);
        }
        assert_eq!(
            sink.0,
            vec![
                Action::Policy {
                    repo: "borsuk".to_string(),
                    policy: ReleasePolicy::Interval { minutes: 30 },
                },
                Action::Policy {
                    repo: "borsuk".to_string(),
                    policy: ReleasePolicy::Threshold { count: 3 },
                },
                Action::Policy {
                    repo: "borsuk".to_string(),
                    policy: ReleasePolicy::Manual,
                },
            ]
        );
        assert_eq!(app.visible_toast(), Some("sent policy borsuk manual"));
    }

    #[test]
    fn a_control_modified_action_key_is_a_no_op() {
        let mut app = app_with_selection(0);
        let mut sink = FakeSink::default();
        let key = KeyEvent::new(KeyCode::Char('p'), crossterm::event::KeyModifiers::CONTROL);
        handle_key(&mut app, key, &mut sink);
        assert!(sink.0.is_empty());
        assert!(app.toast.is_none());
    }

    #[test]
    fn every_direct_action_toast_names_what_was_sent() {
        let cases = [
            (0, '+', "sent limit refine 4"),
            (1, '+', "sent lane refine borsuk 1"),
            (0, 'p', "sent pause refine"),
            (0, 'P', "sent pause all"),
            (8, 'r', "sent refine borsuk i140"),
            (4, 'n', "sent new ticket ryba"),
            (5, 'R', "sent retry ryba/refine-i9"),
            (18, ' ', "sent stack #7 borsuk"),
            (18, 's', "sent policy borsuk every 30m"),
        ];
        for (selection, character, expected) in cases {
            let mut app = app_with_selection(selection);
            let mut sink = FakeSink::default();
            handle_key(&mut app, pressed(character), &mut sink);
            assert_eq!(sink.0.len(), 1, "key {character}");
            assert_eq!(app.visible_toast(), Some(expected), "key {character}");
        }
    }

    #[test]
    fn confirmed_actions_toast_the_exact_operation() {
        let mut app = App::default();
        let mut sink = FakeSink::default();
        Confirm::Abort {
            task: "borsuk/implement-i140".to_string(),
        }
        .send(&mut app, &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Abort {
                task: "borsuk/implement-i140".to_string(),
            }]
        );
        assert_eq!(
            app.visible_toast(),
            Some("sent abort borsuk/implement-i140")
        );

        let mut app = App::default();
        let mut sink = FakeSink::default();
        Confirm::Go {
            repo: "borsuk".to_string(),
            prs: vec![3, 7],
        }
        .send(&mut app, &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Go {
                repo: "borsuk".to_string(),
                prs: vec![3, 7],
            }]
        );
        assert_eq!(app.visible_toast(), Some("sent release borsuk #3 #7"));
    }

    #[test]
    fn a_key_without_a_selection_changes_nothing() {
        // P pauses everything by design, so it is not in this list.
        for character in ['+', '-', 'p', 'r', 'n', 'x', 'R', ' ', 'g', 's'] {
            let mut app = App {
                state: Some(sample_view()),
                connected: true,
                ..App::default()
            };
            let mut sink = FakeSink::default();
            handle_key(&mut app, pressed(character), &mut sink);
            assert!(sink.0.is_empty(), "key {character}");
            assert!(app.toast.is_none(), "key {character}");
            assert!(app.confirm.is_none(), "key {character}");
            assert_eq!(app.view, View::Pipeline, "key {character}");
        }
    }
}
