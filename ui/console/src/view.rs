use std::sync::OnceLock;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::status::{Classification, PaneStatus};
use crate::theme::{Rgb, Tokens};

pub struct Overlay {
    pub title: String,
    pub text: String,
    pub scroll: u16,
}

struct Palette {
    bg: Color,
    fg: Color,
    surface: Color,
    accent: Color,
    warn: Color,
    error: Color,
    dim: Color,
    cyan: Color,
}

fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(build_palette)
}

fn build_palette() -> Palette {
    let neutral = Palette {
        bg: Color::Reset,
        fg: Color::Reset,
        surface: Color::Rgb(27, 22, 51),
        accent: Color::Rgb(0, 255, 163),
        warn: Color::Rgb(255, 211, 25),
        error: Color::Rgb(255, 41, 117),
        dim: Color::Rgb(138, 134, 184),
        cyan: Color::Rgb(0, 229, 255),
    };
    let Ok(tokens) = Tokens::embedded() else {
        return neutral;
    };
    let ui = |hex: &str, fallback: Color| -> Color {
        Rgb::parse(hex).map(|c| c.color()).unwrap_or(fallback)
    };
    Palette {
        bg: tokens.rgb("bg").map(|c| c.color()).unwrap_or(neutral.bg),
        fg: tokens.rgb("fg").map(|c| c.color()).unwrap_or(neutral.fg),
        surface: ui(&tokens.ui.surface, neutral.surface),
        accent: ui(&tokens.ui.accent, neutral.accent),
        warn: ui(&tokens.ui.warn, neutral.warn),
        error: ui(&tokens.ui.error, neutral.error),
        dim: ui(&tokens.ui.dim, neutral.dim),
        cyan: tokens
            .rgb("cyan")
            .map(|c| c.color())
            .unwrap_or(neutral.cyan),
    }
}

fn state_color(state: &str, p: &Palette) -> Color {
    match state {
        "draft waiting" => p.accent,
        "working" => p.cyan,
        "needs trust" | "exited" => p.error,
        "empty" => p.dim,
        _ => p.fg,
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let p = palette();
    let bg_style = Style::default().bg(p.bg).fg(p.fg);
    f.render_widget(Block::default().style(bg_style), f.area());

    let Some(report) = &app.report else {
        let text =
            Paragraph::new("waiting for factory session...").style(Style::default().fg(p.dim));
        f.render_widget(text, center(f.area(), 40, 3));
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", report.session),
            Style::default().fg(p.bg).bg(p.accent),
        ),
        Span::raw(format!(
            "  {} panes   refreshed every 2s",
            report.panes.len()
        )),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    if report.panes.is_empty() {
        let text = Paragraph::new("no agent panes found").style(Style::default().fg(p.warn));
        f.render_widget(text, chunks[2]);
    } else {
        if let Some(pane) = report.panes.first() {
            draw_card(f, chunks[1], pane, app.selected == 0, 1, p);
        }
        let grid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        for (col, column) in grid.iter().enumerate() {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(*column);
            for (row, area) in rows.iter().enumerate() {
                let idx = 1 + col * 2 + row;
                if let Some(pane) = report.panes.get(idx) {
                    draw_card(f, *area, pane, app.selected == idx, idx + 1, p);
                }
            }
        }
    }

    let footer = Line::from(vec![
        Span::styled(" 1-5", Style::default().fg(p.accent)),
        Span::raw(" select  "),
        Span::styled("Enter", Style::default().fg(p.accent)),
        Span::raw(" submit draft  "),
        Span::styled("r", Style::default().fg(p.accent)),
        Span::raw(" press enter  "),
        Span::styled("s", Style::default().fg(p.accent)),
        Span::raw(" next  "),
        Span::styled("l", Style::default().fg(p.accent)),
        Span::raw(" scrollback  "),
        Span::styled("q", Style::default().fg(p.accent)),
        Span::raw(" quit  "),
        Span::styled(app.message.clone(), Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[3]);

    if let Some(overlay) = &app.overlay {
        let width = f.area().width.saturating_sub(4);
        let height = f.area().height.saturating_sub(4);
        let area = center(f.area(), width, height);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.cyan))
            .title(format!(" {} ", overlay.title));
        let text = Paragraph::new(overlay.text.clone())
            .block(block)
            .scroll((overlay.scroll, 0));
        f.render_widget(Clear, area);
        f.render_widget(text, area);
    }
}

fn draw_card(
    f: &mut Frame,
    area: Rect,
    pane: &PaneStatus,
    selected: bool,
    number: usize,
    p: &Palette,
) {
    let class: &Classification = &pane.class;
    let border_style = if selected {
        Style::default().fg(p.accent)
    } else {
        Style::default().fg(p.dim)
    };
    let title_style = if selected {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.fg)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(vec![
            Span::styled(format!(" {number} "), title_style),
            Span::styled(class.role.clone(), title_style),
        ]));
    let model = if class.model.is_empty() {
        class.agent.clone()
    } else {
        class.model.clone()
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(" pane   ", Style::default().fg(p.dim)),
            Span::raw(pane.pane.clone()),
        ]),
        Line::from(vec![
            Span::styled(" model  ", Style::default().fg(p.dim)),
            Span::raw(model),
        ]),
        Line::from(vec![
            Span::styled(" state  ", Style::default().fg(p.dim)),
            Span::styled(
                class.state.clone(),
                Style::default().fg(state_color(&class.state, p)),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

pub fn draw_cockpit(f: &mut Frame, state: &crate::cockpit::CockpitState) {
    let p = palette();
    let bg_style = Style::default().bg(p.bg).fg(p.fg);
    f.render_widget(Block::default().style(bg_style), f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(6),
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    let header = Line::from(vec![
        Span::styled(" aif v4 ", Style::default().fg(p.bg).bg(p.accent)),
        Span::raw(format!("  {}  ", state.message)),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    let status = state.status.clone().unwrap_or(serde_json::json!({}));
    let paused = status["paused"].as_bool().unwrap_or(false);
    let stale = status["stale_source"].as_bool().unwrap_or(false);
    let trusted = status["trusted"].as_bool().unwrap_or(false);
    let revision = status["revision"].as_u64().unwrap_or(0);
    let factory = status["factory_id"].as_str().unwrap_or("?");
    let summary = vec![
        Line::from(vec![
            Span::styled(" factory ", Style::default().fg(p.dim)),
            Span::raw(factory.to_owned()),
        ]),
        Line::from(vec![
            Span::styled(" revision ", Style::default().fg(p.dim)),
            Span::raw(format!("{revision}")),
        ]),
        Line::from(vec![
            Span::styled(" state    ", Style::default().fg(p.dim)),
            Span::styled(
                if paused { "paused" } else { "running" },
                Style::default().fg(if paused { p.warn } else { p.accent }),
            ),
            Span::raw(format!("  stale {stale}  trusted {trusted}")),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.dim))
        .title(" daemon ");
    f.render_widget(Paragraph::new(summary).block(block), chunks[1]);

    if let Some(tasks) = status["tasks"].as_array() {
        let rows: Vec<Line> = tasks
            .iter()
            .enumerate()
            .map(|(idx, task)| {
                let id = task["id"].as_str().unwrap_or("?");
                let task_state = task["state"].as_str().unwrap_or("?");
                let title = task["title"].as_str().unwrap_or("");
                let selected = idx == state.selected;
                Line::from(vec![
                    Span::styled(
                        format!(" {:<2} ", idx + 1),
                        Style::default().fg(if selected { p.accent } else { p.dim }),
                    ),
                    Span::styled(
                        format!("{id:<38}"),
                        Style::default().fg(if selected { p.accent } else { p.fg }),
                    ),
                    Span::styled(
                        format!(" {task_state:<16}"),
                        Style::default().fg(task_state_color(task_state, p)),
                    ),
                    Span::raw(title.to_owned()),
                ])
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(" tasks ");
        f.render_widget(Paragraph::new(rows).block(block), chunks[2]);
    }

    let recent: Vec<Line> = state
        .records
        .iter()
        .rev()
        .take(1)
        .map(|r| Line::from(Span::styled(truncate_record(r), Style::default().fg(p.dim))))
        .collect();
    f.render_widget(Paragraph::new(recent), chunks[3]);

    let footer = Line::from(vec![
        Span::styled("1-9", Style::default().fg(p.accent)),
        Span::raw(" select  "),
        Span::styled("Enter", Style::default().fg(p.accent)),
        Span::raw(" submit  "),
        Span::styled("c", Style::default().fg(p.accent)),
        Span::raw(" cancel  "),
        Span::styled("r", Style::default().fg(p.accent)),
        Span::raw(" retry  "),
        Span::styled("C/f", Style::default().fg(p.accent)),
        Span::raw(" complete/fail  "),
        Span::styled("p/P", Style::default().fg(p.accent)),
        Span::raw(" pause/resume  "),
        Span::styled("q", Style::default().fg(p.accent)),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[4]);
}

fn task_state_color(state: &str, p: &Palette) -> Color {
    match state {
        "queued" | "presenting" => p.dim,
        "awaiting_user" | "reserved" => p.accent,
        "accepted" | "running" => p.cyan,
        "cancel_requested" | "uncertain" => p.warn,
        "failed" => p.error,
        _ => p.fg,
    }
}

fn truncate_record(record: &str) -> String {
    let chars: Vec<char> = record.chars().collect();
    if chars.len() > 120 {
        chars[..120].iter().collect()
    } else {
        record.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::StatusReport;

    fn sample_report() -> StatusReport {
        let class = |role: &str, agent: &str, model: &str, state: &str| Classification {
            role: role.into(),
            agent: agent.into(),
            model: model.into(),
            state: state.into(),
        };
        StatusReport {
            session: "demo-factory".into(),
            running: true,
            panes: vec![
                PaneStatus {
                    pane: "terminal_0".into(),
                    class: class("Planner", "claude", "claude-fable-5", "idle"),
                },
                PaneStatus {
                    pane: "terminal_1".into(),
                    class: class("Refiner", "opencode", "openai/gpt-5.6-sol", "draft waiting"),
                },
                PaneStatus {
                    pane: "terminal_2".into(),
                    class: class(
                        "Implementer",
                        "opencode",
                        "zai-coding-plan/glm-5.3-flash",
                        "draft waiting",
                    ),
                },
                PaneStatus {
                    pane: "terminal_3".into(),
                    class: class("Reviewer", "opencode", "openai/gpt-5.6-sol", "working"),
                },
                PaneStatus {
                    pane: "terminal_4".into(),
                    class: class("Releaser", "claude", "claude-opus-5", "needs trust"),
                },
            ],
        }
    }

    #[test]
    fn cards_render_roles_and_states() {
        let app = App {
            report: Some(sample_report()),
            selected: 1,
            overlay: None,
            message: String::new(),
            session: "demo-factory".into(),
        };
        let backend = ratatui::backend::TestBackend::new(110, 34);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        for expected in [
            "Planner",
            "Refiner",
            "Implementer",
            "Reviewer",
            "Releaser",
            "draft waiting",
        ] {
            assert!(content.contains(expected), "missing {expected}");
        }
    }
}
