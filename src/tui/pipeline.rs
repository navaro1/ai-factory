//! Draws the pipeline view and handles its keys.
//!
//! The view draws the four stages as side-by-side lanes. Inside a stage it
//! groups the tickets by repository. The release lane draws the selected,
//! active, or retry batch inside one border. The waiting queue shows its
//! oldest pull request first and its newest pull request last.
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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::{emit, App, Confirm, Selection, View, Wanted};

/// One selectable item in the logical pipeline board.
///
/// The list holds every stage, repository, ticket, train, and release pull
/// request. Vertical movement uses one stage subset of this list.
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
    /// One pull request in a release batch or waiting queue.
    ReleasePr {
        /// The repository alias.
        repo: String,
        /// The pull request number.
        pr: u64,
    },
}

/// The selectable items of the pipeline board, in stage order.
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
            if stage != Stage::Release {
                rows.extend(tickets.into_iter().map(|index| Row::Ticket { index }));
                continue;
            }
            let Some(train_repo) = train else {
                rows.extend(tickets.into_iter().map(|index| Row::Ticket { index }));
                continue;
            };
            let Some(train) = state.trains.iter().find(|train| train.repo == train_repo) else {
                rows.extend(tickets.into_iter().map(|index| Row::Ticket { index }));
                continue;
            };
            rows.push(Row::Train {
                repo: train_repo.clone(),
            });
            let batch = displayed_batch(train);
            rows.extend(batch.iter().copied().map(|pr| Row::ReleasePr {
                repo: train_repo.clone(),
                pr,
            }));
            let batch_task = release_batch_task_id(train).and_then(|id| {
                tickets
                    .iter()
                    .copied()
                    .find(|index| state.tasks[*index].id == id)
            });
            if let Some(index) = batch_task {
                rows.push(Row::Ticket { index });
            }
            rows.extend(
                train
                    .queue
                    .iter()
                    .copied()
                    .filter(|pr| !batch.contains(pr))
                    .map(|pr| Row::ReleasePr {
                        repo: train_repo.clone(),
                        pr,
                    }),
            );
            rows.extend(
                tickets
                    .into_iter()
                    .filter(|index| Some(*index) != batch_task)
                    .map(|index| Row::Ticket { index }),
            );
        }
    }
    rows
}

/// The pull requests shown inside the release batch border.
fn displayed_batch(train: &crate::sock::TrainView) -> &[u64] {
    if train.batch.is_empty() {
        &train.stacked
    } else {
        &train.batch
    }
}

/// The task that executes the active or saved retry batch.
fn release_batch_task_id(train: &crate::sock::TrainView) -> Option<String> {
    if let Some(id) = &train.in_flight {
        return Some(id.clone());
    }
    train
        .batch
        .iter()
        .min()
        .map(|first| format!("{}/release-p{first}", train.repo))
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

/// Move the selection by `delta` items inside one stage lane.
///
/// `1` moves down and `-1` moves up. The movement stops at the lane ends.
/// With no selection, `j` selects the first item and `k` selects the last item.
pub(super) fn move_selection(app: &mut App, delta: isize) {
    let Some(state) = app.state.as_ref() else {
        app.selection = Selection::None;
        return;
    };
    let all = rows(state);
    let count = all.len();
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
            let Some(stage) = all.get(index).and_then(|row| row_stage(state, row)) else {
                app.selection = Selection::None;
                return;
            };
            let lane: Vec<usize> = all
                .iter()
                .enumerate()
                .filter(|(_, row)| row_stage(state, row) == Some(stage))
                .map(|(index, _)| index)
                .collect();
            let position = lane.iter().position(|entry| *entry == index).unwrap_or(0);
            let target = (position as isize + delta).clamp(0, lane.len() as isize - 1) as usize;
            lane[target]
        }
    };
    app.selection = Selection::Row(next);
}

/// Move between stage lanes and keep the nearest vertical row position.
pub(super) fn move_horizontal(app: &mut App, delta: isize) {
    let Some(state) = app.state.as_ref() else {
        app.selection = Selection::None;
        return;
    };
    let all = rows(state);
    if all.is_empty() {
        app.selection = Selection::None;
        return;
    }
    let Selection::Row(index) = app.selection else {
        let stage = if delta < 0 {
            Stage::Release
        } else {
            Stage::Refine
        };
        app.selection = all
            .iter()
            .position(|row| *row == Row::Stage { stage })
            .map_or(Selection::None, Selection::Row);
        return;
    };
    let Some(source_stage) = all.get(index).and_then(|row| row_stage(state, row)) else {
        app.selection = Selection::None;
        return;
    };
    let source_stage_index = Stage::ALL
        .iter()
        .position(|stage| *stage == source_stage)
        .unwrap_or(0);
    let target_stage_index =
        (source_stage_index as isize + delta).clamp(0, Stage::ALL.len() as isize - 1) as usize;
    let target_stage = Stage::ALL[target_stage_index];
    let source_lane: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, row)| row_stage(state, row) == Some(source_stage))
        .map(|(index, _)| index)
        .collect();
    let target_lane: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, row)| row_stage(state, row) == Some(target_stage))
        .map(|(index, _)| index)
        .collect();
    let source_position = source_lane
        .iter()
        .position(|entry| *entry == index)
        .unwrap_or(0);
    let target_position = source_position.min(target_lane.len().saturating_sub(1));
    if let Some(target) = target_lane.get(target_position) {
        app.selection = Selection::Row(*target);
    }
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
        KeyCode::Char(' ') => stack_selected_pr(app, sink),
        KeyCode::Char('g') => ask_release(app),
        KeyCode::Char('s') => cycle_policy(app, sink),
        KeyCode::Enter => open_selected_task(app),
        _ => {}
    }
}

/// Open the session of the selected ticket.
///
/// A ticket in any state opens its session: a done or failed task keeps
/// its log file, so its transcript stays readable. A stage, repository,
/// or train row opens nothing.
fn open_selected_task(app: &mut App) {
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
    app.session_task = Some(task);
    app.wanted = None;
    app.view = View::Session;
    app.show_session_task();
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
            Row::Ticket { .. } | Row::Train { .. } | Row::ReleasePr { .. } => return,
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
            Row::ReleasePr { repo, .. } => {
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

/// Stack or unstack the selected pull request in a waiting release queue.
fn stack_selected_pr(app: &mut App, sink: &mut impl ActionSink) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(Row::ReleasePr { repo, pr }) = selected_row(app) else {
            return;
        };
        let Some(train) = state.trains.iter().find(|train| train.repo == repo) else {
            return;
        };
        if train.batch.contains(&pr) || !train.queue.contains(&pr) {
            return;
        }
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
        let batch_order = train.batch.clone();
        let queue_order = train.queue.clone();
        train.stacked.sort_by_key(|entry| {
            batch_order
                .iter()
                .position(|candidate| candidate == entry)
                .or_else(|| {
                    queue_order
                        .iter()
                        .position(|candidate| candidate == entry)
                        .map(|position| batch_order.len() + position)
                })
                .unwrap_or(usize::MAX)
        });
    }
    let selected = Row::ReleasePr { repo, pr };
    if let Some(state) = app.state.as_ref() {
        if let Some(index) = rows(state).iter().position(|row| row == &selected) {
            app.selection = Selection::Row(index);
        }
    }
}

/// Ask the operator to confirm the release of the stacked batch.
fn ask_release(app: &mut App) {
    let found = {
        let Some(state) = app.state.as_ref() else {
            return;
        };
        let Some(row) = selected_row(app) else {
            return;
        };
        let Some(repo) = selected_release_repo(state, &row) else {
            return;
        };
        let Some(train) = state.trains.iter().find(|train| train.repo == repo) else {
            return;
        };
        if train.in_flight.is_some() || !train.batch.is_empty() || train.stacked.is_empty() {
            return;
        }
        (repo, train.stacked.clone())
    };
    let (repo, prs) = found;
    app.confirm = Some(Confirm::Go { repo, prs });
}

/// The release repository that owns one selected release row.
fn selected_release_repo(state: &StateView, row: &Row) -> Option<String> {
    match row {
        Row::Repo {
            stage: Stage::Release,
            repo,
        }
        | Row::Train { repo }
        | Row::ReleasePr { repo, .. } => Some(repo.clone()),
        Row::Ticket { index } => state
            .tasks
            .get(*index)
            .filter(|task| task.stage == Stage::Release)
            .map(|task| task.repo.clone()),
        Row::Stage { .. } | Row::Repo { .. } => None,
    }
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
    let lanes = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(area);
    for (stage, lane) in Stage::ALL.into_iter().zip(lanes.iter().copied()) {
        let inner_width = lane.width.saturating_sub(2);
        let mut lines = if stage == Stage::Release {
            release_lane_lines(state, &all, selected, now_ms, inner_width)
        } else {
            ordinary_lane_lines(state, &all, selected, stage, now_ms)
        };
        if state
            .stages
            .iter()
            .any(|view| view.stage == stage && view.overridden)
        {
            lines.push(Line::from(Span::styled("  * config limit", THEME.dim())));
        }
        let block = Block::bordered().title(Span::styled(
            stage.as_str().to_string(),
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        ));
        let scroll = lane_scroll(&lines, lane.height);
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), lane);
    }
}

/// The smallest vertical scroll that keeps the selected item visible.
fn lane_scroll(lines: &[Line<'_>], lane_height: u16) -> u16 {
    let visible = lane_height.saturating_sub(2) as usize;
    let selected = lines.iter().position(|line| {
        line.spans
            .first()
            .is_some_and(|span| span.content.as_ref() == "▸ ")
    });
    selected
        .map(|line| line.saturating_sub(visible.saturating_sub(1)))
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16
}

/// Lines for a refine, implement, or review lane.
fn ordinary_lane_lines(
    state: &StateView,
    all: &[Row],
    selected: Option<usize>,
    stage: Stage,
    now_ms: u64,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, row) in all.iter().enumerate() {
        if row_stage(state, row) != Some(stage) {
            continue;
        }
        lines.push(selectable_line(
            selected == Some(index),
            row_spans(state, row, now_ms),
        ));
        if matches!(row, Row::Stage { .. }) {
            if stage_is_paused(state, stage) {
                lines.push(Line::from(Span::styled(
                    "  paused",
                    Style::default().fg(THEME.warn),
                )));
            }
            if stage_is_empty(state, stage) {
                lines.push(Line::from(Span::styled("  no tasks", THEME.dim())));
            }
        }
    }
    lines
}

/// Lines for the release lane, including each repository's batch border.
fn release_lane_lines(
    state: &StateView,
    all: &[Row],
    selected: Option<usize>,
    now_ms: u64,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let stage_row = Row::Stage {
        stage: Stage::Release,
    };
    push_row_line(state, all, selected, &stage_row, now_ms, &mut lines);
    if stage_is_paused(state, Stage::Release) {
        lines.push(Line::from(Span::styled(
            "  paused",
            Style::default().fg(THEME.warn),
        )));
    }
    if stage_is_empty(state, Stage::Release) {
        lines.push(Line::from(Span::styled("  no tasks", THEME.dim())));
        return lines;
    }

    for repo in &state.repos {
        let repo_row = Row::Repo {
            stage: Stage::Release,
            repo: repo.alias.clone(),
        };
        if !all.contains(&repo_row) {
            continue;
        }
        let train = state.trains.iter().find(|train| train.repo == repo.alias);
        push_custom_row_line(
            all,
            selected,
            &repo_row,
            release_repo_spans(state, &repo.alias, train),
            &mut lines,
        );
        let Some(train) = train else {
            for (index, _) in state
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, task)| task.stage == Stage::Release && task.repo == repo.alias)
            {
                let row = Row::Ticket { index };
                push_row_line(state, all, selected, &row, now_ms, &mut lines);
            }
            continue;
        };
        if let Some(fire) = train.next_fire_ms {
            let remaining = fire.saturating_sub(now_ms);
            lines.push(Line::from(Span::styled(
                format!("  fires {}", format_countdown(remaining)),
                THEME.dim(),
            )));
        }

        let train_row = Row::Train {
            repo: repo.alias.clone(),
        };
        let box_width = width.saturating_sub(2).max(2);
        let (title, border_style) = release_batch_title(train);
        push_custom_row_line(
            all,
            selected,
            &train_row,
            vec![Span::styled(box_top(title, box_width), border_style)],
            &mut lines,
        );

        let batch = displayed_batch(train);
        if batch.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(box_line("select below", box_width), THEME.dim()),
            ]));
        } else {
            for pr in batch {
                let row = Row::ReleasePr {
                    repo: repo.alias.clone(),
                    pr: *pr,
                };
                push_custom_row_line(
                    all,
                    selected,
                    &row,
                    vec![Span::styled(
                        box_line(&format!("#{pr}"), box_width),
                        Style::default().fg(THEME.text),
                    )],
                    &mut lines,
                );
            }
        }
        if let Some(task_id) = release_batch_task_id(train) {
            if let Some((index, task)) = state
                .tasks
                .iter()
                .enumerate()
                .find(|(_, task)| task.id == task_id)
            {
                let row = Row::Ticket { index };
                push_custom_row_line(
                    all,
                    selected,
                    &row,
                    vec![Span::styled(
                        box_line(&task_label(state, task), box_width),
                        state_style(&task.state),
                    )],
                    &mut lines,
                );
            }
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(box_bottom(box_width), border_style),
        ]));

        let waiting: Vec<u64> = train
            .queue
            .iter()
            .copied()
            .filter(|pr| !batch.contains(pr))
            .collect();
        for (position, pr) in waiting.iter().enumerate() {
            let row = Row::ReleasePr {
                repo: repo.alias.clone(),
                pr: *pr,
            };
            let status = if train.stacked.contains(pr) {
                "next batch"
            } else if position == 0 {
                "next"
            } else if position + 1 == waiting.len() {
                "new"
            } else {
                "ready"
            };
            push_custom_row_line(
                all,
                selected,
                &row,
                vec![
                    Span::styled(format!("#{pr}"), Style::default().fg(THEME.text)),
                    Span::styled(format!(" {status}"), THEME.dim()),
                ],
                &mut lines,
            );
        }

        let batch_task = release_batch_task_id(train);
        for (index, _) in state.tasks.iter().enumerate().filter(|(_, task)| {
            task.stage == Stage::Release
                && task.repo == repo.alias
                && Some(task.id.as_str()) != batch_task.as_deref()
        }) {
            let row = Row::Ticket { index };
            push_row_line(state, all, selected, &row, now_ms, &mut lines);
        }
    }
    lines
}

/// Add a row with its standard spans and selection marker.
fn push_row_line(
    state: &StateView,
    all: &[Row],
    selected: Option<usize>,
    row: &Row,
    now_ms: u64,
    lines: &mut Vec<Line<'static>>,
) {
    push_custom_row_line(all, selected, row, row_spans(state, row, now_ms), lines);
}

/// Add a row with custom spans and its selection marker.
fn push_custom_row_line(
    all: &[Row],
    selected: Option<usize>,
    row: &Row,
    spans: Vec<Span<'static>>,
    lines: &mut Vec<Line<'static>>,
) {
    let is_selected = all.iter().position(|entry| entry == row) == selected;
    lines.push(selectable_line(is_selected, spans));
}

/// One line with the board selection marker.
fn selectable_line(is_selected: bool, spans: Vec<Span<'static>>) -> Line<'static> {
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
    let mut all = vec![marker];
    all.extend(spans);
    let mut line = Line::from(all);
    if is_selected {
        line = line.style(THEME.selected());
    }
    line
}

/// The batch border title and color for one train state.
fn release_batch_title(train: &crate::sock::TrainView) -> (&'static str, Style) {
    if train.in_flight.is_some() {
        ("RELEASING NOW", Style::default().fg(THEME.ok))
    } else if !train.batch.is_empty() {
        ("RETRY REQUIRED", Style::default().fg(THEME.error))
    } else {
        ("NEXT RELEASE", Style::default().fg(THEME.accent))
    }
}

/// The top line of a box with a title clipped to the available width.
fn box_top(title: &str, width: u16) -> String {
    let inside = width.saturating_sub(2) as usize;
    let title: String = title.chars().take(inside).collect();
    format!(
        "┌{title}{}┐",
        "─".repeat(inside.saturating_sub(title.chars().count()))
    )
}

/// One padded content line of a box.
fn box_line(text: &str, width: u16) -> String {
    let inside = width.saturating_sub(2) as usize;
    let text: String = text.chars().take(inside).collect();
    format!(
        "│{text}{}│",
        " ".repeat(inside.saturating_sub(text.chars().count()))
    )
}

/// The bottom line of a box.
fn box_bottom(width: u16) -> String {
    format!("└{}┘", "─".repeat(width.saturating_sub(2) as usize))
}

/// The stage column that owns one selectable row.
fn row_stage(state: &StateView, row: &Row) -> Option<Stage> {
    match row {
        Row::Stage { stage } | Row::Repo { stage, .. } => Some(*stage),
        Row::Ticket { index } => state.tasks.get(*index).map(|task| task.stage),
        Row::Train { .. } | Row::ReleasePr { .. } => Some(Stage::Release),
    }
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
        Row::ReleasePr { pr, .. } => vec![Span::styled(
            format!("#{pr}"),
            Style::default().fg(THEME.text),
        )],
    }
}

/// The count spans below one stage lane title.
fn stage_spans(state: &StateView, stage: Stage) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let Some(view) = state.stages.iter().find(|s| s.stage == stage) else {
        return spans;
    };
    let running_color = if view.running > 0 {
        THEME.ok
    } else {
        THEME.dim
    };
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
    let mut spans = vec![Span::styled(
        repo.to_string(),
        THEME.dim().add_modifier(Modifier::BOLD),
    )];
    if state.paused.repos.iter().any(|entry| entry == repo) {
        spans.push(paused_span());
    }
    spans
}

/// The repository, pause, and policy spans of one release section.
fn release_repo_spans(
    state: &StateView,
    repo: &str,
    train: Option<&crate::sock::TrainView>,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        repo.to_string(),
        THEME.dim().add_modifier(Modifier::BOLD),
    )];
    if state.paused.repos.iter().any(|entry| entry == repo) {
        spans.push(Span::styled(" paused", Style::default().fg(THEME.warn)));
    }
    if let Some(train) = train {
        spans.push(Span::styled(
            format!(" {}", policy_label(&train.policy)),
            THEME.dim(),
        ));
    }
    spans
}

/// The spans of one ticket row: item, state, attempt, and queued messages.
///
/// A queued task that a pause blocks shows the pause instead of the queue
/// state, because it cannot start. A task in any other state keeps its true
/// state: a pause blocks starts, it does not stop running tasks. A count
/// above zero of queued messages adds a badge, so a waiting message stays
/// visible from the board.
fn ticket_spans(state: &StateView, task: &TaskView) -> Vec<Span<'static>> {
    let mut spans = vec![
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
    if task.queued_messages > 0 {
        spans.push(Span::styled(
            format!(" m{}", task.queued_messages),
            THEME.dim(),
        ));
    }
    if task.attempt > 1 {
        spans.push(Span::styled(format!(" a{}", task.attempt), THEME.dim()));
    }
    spans
}

/// The compact text of one task inside a nested release border.
fn task_label(state: &StateView, task: &TaskView) -> String {
    let status = if matches!(task.state, TaskState::Queued) && task_is_paused(state, task) {
        "paused"
    } else {
        state_label(&task.state)
    };
    let mut label = format!("{}{} {status}", task.kind.as_str(), task.number);
    if task.queued_messages > 0 {
        label.push_str(&format!(" m{}", task.queued_messages));
    }
    if task.attempt > 1 {
        label.push_str(&format!(" a{}", task.attempt));
    }
    label
}

/// The colored label of one task state.
fn state_span(state: &TaskState) -> Span<'static> {
    Span::styled(state_label(state), state_style(state))
}

/// The compact board label of one task state.
fn state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Running => "running",
        TaskState::AwaitingUser => "needs input",
        TaskState::Done => "done",
        TaskState::Failed(_) => "failed",
    }
}

/// The board color of one task state.
fn state_style(state: &TaskState) -> Style {
    match state {
        TaskState::Queued => THEME.dim(),
        TaskState::Running => Style::default().fg(THEME.accent),
        TaskState::AwaitingUser => Style::default().fg(THEME.warn),
        TaskState::Done => Style::default().fg(THEME.ok),
        TaskState::Failed(_) => Style::default().fg(THEME.error),
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
        input: crate::sock::InputMode::Live,
        queued_messages: 0,
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
                stacked: vec![5],
                batch: vec![5],
                policy: ReleasePolicy::Manual,
                next_fire_ms: None,
                in_flight: Some("borsuk/release-p5".to_string()),
            },
            TrainView {
                repo: "ryba".to_string(),
                queue: Vec::new(),
                stacked: Vec::new(),
                batch: Vec::new(),
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
    render_to_size(app, 80, 24)
}

/// Render the app into a test backend with an explicit terminal size.
#[cfg(test)]
fn render_to_size(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
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
        // The release lane puts the active batch above the waiting queue.
        let release_start = rows
            .iter()
            .position(|row| {
                *row == Row::Stage {
                    stage: Stage::Release,
                }
            })
            .unwrap();
        assert_eq!(
            rows[release_start + 2],
            Row::Train {
                repo: "borsuk".to_string(),
            }
        );
        assert_eq!(
            rows[release_start + 3],
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 5,
            }
        );
        assert_eq!(rows[release_start + 4], Row::Ticket { index: 6 });
        assert_eq!(
            rows[release_start + 5],
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 7,
            }
        );
        assert_eq!(
            rows[release_start + 6],
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 9,
            }
        );
        assert_eq!(
            rows[release_start + 8],
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
        let all = rows(app.state.as_ref().unwrap());
        let last = all.len() - 1;
        let last_refine = all
            .iter()
            .rposition(|row| row_stage(app.state.as_ref().unwrap(), row) == Some(Stage::Refine))
            .unwrap();

        move_selection(&mut app, 1);
        assert_eq!(app.selection, Selection::Row(0));
        for _ in 0..(last + 5) {
            move_selection(&mut app, 1);
        }
        assert_eq!(app.selection, Selection::Row(last_refine));
        move_selection(&mut app, -1);
        assert_eq!(app.selection, Selection::Row(last_refine - 1));
        // A fresh selection with k picks the last row.
        app.selection = Selection::None;
        move_selection(&mut app, -1);
        assert_eq!(app.selection, Selection::Row(last));
    }

    #[test]
    fn vertical_movement_stays_inside_the_selected_stage_lane() {
        let state = sample_view();
        let all = rows(&state);
        let last_refine = all
            .iter()
            .position(|row| *row == Row::Ticket { index: 7 })
            .unwrap();
        let mut app = App {
            state: Some(state),
            connected: true,
            selection: Selection::Row(last_refine),
            ..App::default()
        };

        move_selection(&mut app, 1);

        assert_eq!(app.selection, Selection::Row(last_refine));
    }

    #[test]
    fn horizontal_movement_keeps_the_nearest_vertical_position() {
        let state = sample_view();
        let all = rows(&state);
        let refine_ticket = all
            .iter()
            .position(|row| *row == Row::Ticket { index: 0 })
            .unwrap();
        let implement_ticket = all
            .iter()
            .position(|row| *row == Row::Ticket { index: 2 })
            .unwrap();
        let mut app = App {
            state: Some(state),
            connected: true,
            selection: Selection::Row(refine_ticket),
            ..App::default()
        };

        move_horizontal(&mut app, 1);

        assert_eq!(app.selection, Selection::Row(implement_ticket));
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
    fn stages_render_as_four_side_by_side_lanes() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };

        let text = render_to_size(&mut app, 120, 24);
        let header = text.lines().find(|line| {
            ["refine", "implement", "review", "release"]
                .iter()
                .all(|stage| line.contains(stage))
        });

        assert!(
            header.is_some(),
            "all four stage names must share one board row:\n{text}"
        );
    }

    #[test]
    fn a_tall_lane_scrolls_to_keep_its_selected_task_visible() {
        let mut state = sample_view();
        for number in 200..210 {
            state.tasks.push(task(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                number,
                TaskState::Queued,
                1,
            ));
        }
        let selected_task = state.tasks.len() - 1;
        let mut app = app_with_state_and_row(
            state,
            Row::Ticket {
                index: selected_task,
            },
        );

        let text = render_to_size(&mut app, 80, 10);

        assert!(text.contains("▸ i209 queued"), "board:\n{text}");
    }

    #[test]
    fn full_state_shows_stage_counts_and_tickets() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("1/1 of 3"));
        assert!(text.contains("1/1 of 5*"));
        assert!(text.contains("* config limit"));
        assert!(text.contains("borsuk"));
        assert!(text.contains("ryba"));
        assert!(text.contains("i142 queued"));
        assert!(text.contains("i143 running"));
        assert!(text.contains("i140 running"));
        assert!(text.contains("i140 running a2"));
        assert!(text.contains("i7 queued"));
        assert!(text.contains("p7 running"));
        assert!(text.contains("p9 needs input"));
        assert!(text.contains("p5 running"));
        assert!(text.contains("i9 failed a3"));
    }

    #[test]
    fn full_state_shows_the_release_group() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("borsuk manual"));
        assert!(text.contains("RELEASING NOW"));
        assert!(text.contains("#5"));
        assert!(text.contains("#7 next"));
        assert!(text.contains("#9 new"));
        assert!(text.contains("ryba every 30m"));
        assert!(text.contains("fires 1m"));
    }

    #[test]
    fn release_lane_outlines_the_active_batch_above_its_bottom_up_queue() {
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };

        let text = render_to_string(&mut app);
        assert!(text.contains("RELEASING NOW"), "board:\n{text}");
        assert!(text.contains("#5"), "active pull request:\n{text}");
        let first = text.find("#7").expect("the oldest waiting pull request");
        let second = text.find("#9").expect("the newest waiting pull request");
        assert!(
            first < second,
            "the queue must grow from top to bottom:\n{text}"
        );
    }

    #[test]
    fn release_lane_outlines_the_next_batch_and_removes_it_from_the_queue() {
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked = vec![7];
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };

        let text = render_to_string(&mut app);
        assert!(text.contains("NEXT RELEASE"), "board:\n{text}");
        assert!(text.contains("│#7"), "outlined pull request:\n{text}");
        assert!(text.contains("#9 next"), "waiting queue:\n{text}");
        assert!(!text.contains("#7 next"), "duplicate queue item:\n{text}");
    }

    #[test]
    fn release_lane_marks_a_saved_failed_batch_for_retry() {
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.tasks[6].state = TaskState::Failed("merge failed".to_string());
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };

        let text = render_to_string(&mut app);
        assert!(text.contains("RETRY REQUIRED"), "board:\n{text}");
        assert!(text.contains("#5"), "saved batch:\n{text}");
    }

    #[test]
    fn release_lane_without_a_train_does_not_repeat_other_stage_tasks() {
        let mut state = sample_view();
        state.trains.clear();
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };

        let text = render_to_string(&mut app);
        assert_eq!(text.matches("i142 queued").count(), 1, "board:\n{text}");
        assert_eq!(text.matches("i140 running").count(), 1, "board:\n{text}");
        assert!(text.contains("p5 running"), "release task:\n{text}");
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
        assert!(
            line.contains("│▸ i142 queued"),
            "unmarked selected line: {line}"
        );
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
        assert_eq!(text.matches("paused").count(), 7);
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
        assert_eq!(text.matches("paused").count(), 2);
        assert!(text.contains("i142 paused"));
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
        // Every group row of borsuk carries the mark. The fifth pause mark
        // belongs to its queued refine ticket.
        assert_eq!(text.matches("borsuk").count(), 4);
        assert_eq!(text.matches("paused").count(), 5);
        assert!(text.contains("borsuk paused"));
        assert!(!text.contains("RELEASING NOW paused"));
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
    fn a_ticket_with_queued_messages_shows_the_badge_on_the_board() {
        let mut state = sample_view();
        state.tasks[1].queued_messages = 2;
        let mut app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&mut app);
        assert!(text.contains("i143 running m2"), "board: {text}");

        // A ticket without queued messages shows no badge.
        assert!(!text.contains("i142 queued m"));
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

    /// A plain press of the Enter key.
    fn pressed_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
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

    /// The sample app with one selected logical board row.
    fn app_with_row(row: Row) -> App {
        app_with_state_and_row(sample_view(), row)
    }

    /// One app with the requested state and logical board row selected.
    fn app_with_state_and_row(state: StateView, row: Row) -> App {
        let index = rows(&state)
            .iter()
            .position(|entry| entry == &row)
            .expect("the sample view must contain the selected row");
        App {
            state: Some(state),
            connected: true,
            selection: Selection::Row(index),
            ..App::default()
        }
    }

    /// Select one logical board item in an existing test app.
    fn select_row(app: &mut App, row: Row) {
        let state = app.state.as_ref().expect("the app must hold state");
        let index = rows(state)
            .iter()
            .position(|entry| entry == &row)
            .expect("the state must contain the selected row");
        app.selection = Selection::Row(index);
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
    fn enter_opens_the_session_of_the_selected_ticket() {
        let mut app = app_with_selection(2);
        app.wanted = Some(Wanted::Create {
            repo: "borsuk".to_string(),
        });
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed_enter(), &mut sink);
        assert!(sink.0.is_empty(), "enter must not send an action");
        assert_eq!(app.view, View::Session);
        assert_eq!(app.session_task.as_deref(), Some("borsuk/refine-i142"));
        assert!(app.wanted.is_none());
        assert!(app.session.is_showing("borsuk/refine-i142"));
    }

    #[test]
    fn enter_opens_done_and_failed_tickets() {
        let mut state = sample_view();
        state.tasks[1].state = TaskState::Done;
        let mut app = App {
            state: Some(state),
            connected: true,
            selection: Selection::Row(3),
            ..App::default()
        };
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed_enter(), &mut sink);
        assert_eq!(app.view, View::Session);
        assert!(app.session.is_showing("borsuk/refine-i143"));

        let mut app = app_with_selection(5);
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed_enter(), &mut sink);
        assert_eq!(app.view, View::Session);
        assert!(app.session.is_showing("ryba/refine-i9"));
    }

    #[test]
    fn enter_on_a_non_ticket_row_changes_nothing() {
        let non_ticket_rows = [
            Row::Stage {
                stage: Stage::Refine,
            },
            Row::Repo {
                stage: Stage::Refine,
                repo: "borsuk".to_string(),
            },
            Row::Train {
                repo: "borsuk".to_string(),
            },
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 5,
            },
        ];
        for row in non_ticket_rows {
            let mut app = app_with_row(row.clone());
            let mut sink = FakeSink::default();
            handle_key(&mut app, pressed_enter(), &mut sink);
            assert!(sink.0.is_empty(), "selection {row:?}");
            assert_eq!(app.view, View::Pipeline, "selection {row:?}");
            assert!(app.session_task.is_none(), "selection {row:?}");
            assert!(app.toast.is_none(), "selection {row:?}");
        }

        // A pipeline with no selection also ignores Enter.
        let mut app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed_enter(), &mut sink);
        assert_eq!(app.view, View::Pipeline);
        assert!(app.session_task.is_none());
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
    fn space_toggles_the_selected_waiting_pr_and_blocks_the_active_batch() {
        let mut app = app_with_row(Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 9,
        });
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        assert_eq!(
            sink.0,
            vec![Action::Stack {
                repo: "borsuk".to_string(),
                pr: 9,
                on: true
            }]
        );

        let mut app = app_with_row(Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 5,
        });
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        assert!(sink.0.is_empty(), "the active batch cannot change");
    }

    #[test]
    fn an_active_release_marks_a_future_batch_choice_in_the_queue() {
        let mut app = app_with_row(Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 9,
        });
        let mut sink = FakeSink::default();

        handle_key(&mut app, pressed(' '), &mut sink);

        let text = render_to_string(&mut app);
        assert!(text.contains("#9 next batch"), "board:\n{text}");
    }

    #[test]
    fn space_keeps_the_selected_pr_when_the_row_moves() {
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked.clear();
        let selected = Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 9,
        };
        let mut app = app_with_state_and_row(state, selected.clone());
        let mut sink = FakeSink::default();

        handle_key(&mut app, pressed(' '), &mut sink);

        assert_eq!(selected_row(&app), Some(selected));
    }

    #[test]
    fn selected_batch_prs_keep_the_release_queue_order() {
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked.clear();
        let mut app = app_with_state_and_row(
            state,
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 9,
            },
        );
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        select_row(
            &mut app,
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 7,
            },
        );

        handle_key(&mut app, pressed(' '), &mut sink);
        handle_key(&mut app, pressed('g'), &mut sink);

        assert_eq!(app.state.as_ref().unwrap().trains[0].stacked, vec![7, 9]);
        assert_eq!(
            app.confirm,
            Some(Confirm::Go {
                repo: "borsuk".to_string(),
                prs: vec![7, 9],
            })
        );
    }

    #[test]
    fn repeated_space_toggles_the_same_queue_entry() {
        let mut app = app_with_row(Row::ReleasePr {
            repo: "borsuk".to_string(),
            pr: 7,
        });
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
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked = vec![7];
        let mut app = app_with_state_and_row(
            state,
            Row::ReleasePr {
                repo: "borsuk".to_string(),
                pr: 9,
            },
        );
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed(' '), &mut sink);
        handle_key(&mut app, pressed('g'), &mut sink);
        assert_eq!(
            app.confirm,
            Some(Confirm::Go {
                repo: "borsuk".to_string(),
                prs: vec![7, 9],
            })
        );
    }

    #[test]
    fn g_only_opens_the_release_confirmation() {
        let mut state = sample_view();
        state.trains[0].in_flight = None;
        state.trains[0].batch.clear();
        state.trains[0].stacked = vec![7];
        let mut app = app_with_state_and_row(
            state,
            Row::Train {
                repo: "borsuk".to_string(),
            },
        );
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('g'), &mut sink);
        assert!(sink.0.is_empty(), "g must not send before the confirm");
        assert_eq!(
            app.confirm,
            Some(Confirm::Go {
                repo: "borsuk".to_string(),
                prs: vec![7]
            })
        );

        // An active or saved batch cannot start again.
        let mut app = app_with_row(Row::Train {
            repo: "borsuk".to_string(),
        });
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('g'), &mut sink);
        assert!(app.confirm.is_none());

        // An empty stacked set opens no confirmation.
        let mut app = app_with_row(Row::Train {
            repo: "ryba".to_string(),
        });
        let mut sink = FakeSink::default();
        handle_key(&mut app, pressed('g'), &mut sink);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn s_cycles_the_release_policy() {
        let mut app = app_with_row(Row::Train {
            repo: "borsuk".to_string(),
        });
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
        let mut app = app_with_row(Row::Train {
            repo: "borsuk".to_string(),
        });
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
        ];
        for (selection, character, expected) in cases {
            let mut app = app_with_selection(selection);
            let mut sink = FakeSink::default();
            handle_key(&mut app, pressed(character), &mut sink);
            assert_eq!(sink.0.len(), 1, "key {character}");
            assert_eq!(app.visible_toast(), Some(expected), "key {character}");
        }

        let release_cases = [
            (
                Row::ReleasePr {
                    repo: "borsuk".to_string(),
                    pr: 7,
                },
                ' ',
                "sent stack #7 borsuk",
            ),
            (
                Row::Train {
                    repo: "borsuk".to_string(),
                },
                's',
                "sent policy borsuk every 30m",
            ),
        ];
        for (row, character, expected) in release_cases {
            let mut app = app_with_row(row);
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
