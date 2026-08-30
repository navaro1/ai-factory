//! Draws the pipeline view and handles its keys.
//!
//! The view lists the four stages in pipeline order. Inside a stage it
//! groups the tickets by repository. A stage header row shows the running
//! and queued counts against the limit and marks a limit that differs from
//! the config file with a star. The release stage shows one train row per
//! repository with the queue, the stacked set, the policy, and the
//! countdown.
//!
//! Chunk 19 adds the interaction keys to this file. The `Row` model and
//! the selection movement exist so those keys can resolve their target.

use super::theme::THEME;
use crate::config::ReleasePolicy;
use crate::model::Stage;
use crate::sock::{StateView, TaskView};
use crate::tasks::TaskState;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{App, Selection};

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

/// The current time in milliseconds since the Unix epoch.
///
/// The value is 0 before the epoch, which no real clock reports.
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
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

/// Draw the pipeline view into `area`.
pub(super) fn draw(f: &mut Frame, app: &App, area: Rect) {
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
        spans.extend(row_spans(state, row));
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
fn row_spans(state: &StateView, row: &Row) -> Vec<Span<'static>> {
    match row {
        Row::Stage { stage } => stage_spans(state, *stage),
        Row::Repo { repo, .. } => repo_spans(repo),
        Row::Ticket { index } => match state.tasks.get(*index) {
            Some(task) => ticket_spans(task),
            None => vec![Span::raw("    (missing task)")],
        },
        Row::Train { repo } => train_spans(state, repo),
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
        spans.push(Span::styled(
            "  paused",
            Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// The spans of one repository group header row.
fn repo_spans(repo: &str) -> Vec<Span<'static>> {
    vec![
        Span::raw("  "),
        Span::styled(repo.to_string(), THEME.dim().add_modifier(Modifier::BOLD)),
    ]
}

/// The spans of one ticket row: item, state, and attempt.
fn ticket_spans(task: &TaskView) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(
            format!("{}{}", task.kind.as_str(), task.number),
            Style::default().fg(THEME.text),
        ),
        Span::raw(" "),
    ];
    spans.push(state_span(&task.state));
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
fn train_spans(state: &StateView, repo: &str) -> Vec<Span<'static>> {
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
        let remaining = fire.saturating_sub(epoch_ms());
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
                next_fire_ms: Some(epoch_ms() + 90_000),
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
pub(super) fn render_to_string(app: &App) -> String {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| super::render(frame, app)).unwrap();
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
        let app = App {
            state: Some(empty_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
        for stage in ["refine", "implement", "review", "release"] {
            assert!(text.contains(stage), "missing stage header {stage}");
        }
        assert!(text.contains("no tasks"));
        assert!(text.contains("0/0 of 3"));
        assert!(!text.contains("limit differs"));
    }

    #[test]
    fn full_state_shows_stage_counts_and_tickets() {
        let app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
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
        let app = App {
            state: Some(sample_view()),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
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
        let app = App {
            state: Some(sample_view()),
            connected: true,
            selection: Selection::Row(2),
            ..App::default()
        };
        let text = render_to_string(&app);
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
        let app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
        // A global pause marks the header and all four stage headers.
        let marked = text.lines().filter(|line| line.contains("paused")).count();
        assert_eq!(marked, 5);

        // A stage pause marks only that stage header.
        let state = StateView {
            paused: PausedView {
                global: false,
                stages: vec![Stage::Refine],
                repos: Vec::new(),
            },
            ..sample_view()
        };
        let app = App {
            state: Some(state),
            connected: true,
            ..App::default()
        };
        let text = render_to_string(&app);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("paused"))
            .collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("refine"));
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
}
