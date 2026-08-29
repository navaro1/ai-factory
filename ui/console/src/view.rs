use std::sync::OnceLock;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::status::{Classification, PaneStatus, StatusReport};
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
}

fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(|| {
        let fallback = Palette {
            bg: Color::Reset,
            fg: Color::Reset,
            surface: Color::Rgb(27, 22, 51),
            accent: Color::Rgb(0, 255, 163),
            warn: Color::Rgb(255, 211, 25),
            error: Color::Rgb(255, 41, 117),
            dim: Color::Rgb(138, 134, 184),
        };
        match Tokens::embedded() {
            Ok(tokens) => Palette {
                bg: color(&tokens, "bg", fallback.bg),
                fg: color(&tokens, "fg", fallback.fg),
                surface: Rgb::parse(&tokens.ui.surface)
                    .map(|c| c.color())
                    .unwrap_or(fallback.surface),
                accent: Rgb::parse(&tokens.ui.accent)
                    .map(|c| c.color())
                    .unwrap_or(fallback.accent),
                warn: Rgb::parse(&tokens.ui.warn)
                    .map(|c| c.color())
                    .unwrap_or(fallback.warn),
                error: Rgb::parse(&tokens.ui.error)
                    .map(|c| c.color())
                    .unwrap_or(fallback.error),
                dim: Rgb::parse(&tokens.ui.dim)
                    .map(|c| c.color())
                    .unwrap_or(fallback.dim),
            },
            Err(_) => fallback,
        }
    })
}

fn color(tokens: &Tokens, key: &str, fallback: Color) -> Color {
    tokens.rgb(key).map(|c| c.color()).unwrap_or(fallback)
}

fn state_color(state: &str, p: &Palette) -> Color {
    match state {
        "draft waiting" => p.accent,
        "working" => color(tokens_none(), "cyan", p.fg),
        "needs trust" | "exited" => p.error,
        "empty" => p.dim,
        _ => p.fg,
    }
}

fn tokens_none() -> &'static Tokens {
    static T: OnceLock<Tokens> = OnceLock::new();
    T.get_or_init(|| Tokens::embedded().unwrap_or(fallback_tokens()))
}

fn fallback_tokens() -> Tokens {
    serde_json::from_str("{}").unwrap_or_else(|_| panic!("static empty tokens must parse"))
}

pub fn draw(f: &mut Frame, app: &App) {
    let p = palette();
    let bg_style = Style::default().bg(p.bg).fg(p.fg);
    f.render_widget(Block::default().style(bg_style), f.area());

    let Some(report) = &app.report else {
        let text = Paragraph::new("waiting for factory session...")
            .style(Style::default().fg(p.dim));
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
        Span::styled(format!(" {} ", report.session), Style::default().fg(p.bg).bg(p.accent)),
        Span::raw(format!(
            "  {} panes   refreshed via zellij every 2s",
            report.panes.len()
        )),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    if report.panes.is_empty() {
        let text = Paragraph::new("no agent panes found").style(Style::default().fg(p.warn));
        f.render_widget(text, chunks[2]);
    } else {
        let planner_area = chunks[1];
        if let Some(pane) = report.panes.first() {
            draw_card(f, planner_area, pane, app.selected == 0, 1, p);
        }
        let grid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        for (col, areas) in grid.iter().enumerate() {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(*areas);
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
        Span::raw(" press enter in pane  "),
        Span::styled("s", Style::default().fg(p.accent)),
        Span::raw(" next  "),
        Span::styled("l", Style::default().fg(p.accent)),
        Span::raw(" scrollback  "),
        Span::styled("q", Style::default().fg(p.accent)),
        Span::raw(" quit  "),
        Span::styled(&app.message, Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[3]);

    if let Some(overlay) = &app.overlay {
        let area = center(f.area(), f.area().width.saturating_sub(4), f.area().height.saturating_sub(4));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.cyan_border()))
            .title(format!(" {} ", overlay.title));
        let text = Paragraph::new(overlay.text.clone())
            .block(block)
            .scroll((overlay.scroll, 0));
        f.render_widget(Clear, area);
        f.render_widget(text, area);
    }
}

impl Palette {
    fn cyan_border(&self) -> Color {
        self.accent
    }
}

fn draw_card(f: &mut Frame, area: Rect, pane: &PaneStatus, selected: bool, number: usize, p: &Palette) {
    let class: &Classification = &pane.class;
    let border = if selected {
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
        .border_style(border)
        .title(Line::from(vec![
            Span::styled(format!(" {number} "), title_style),
            Span::styled(class.role.clone(), title_style),
        ]));
    let lines = vec![
        Line::from(vec![
            Span::styled(" pane   ", Style::default().fg(p.dim)),
            Span::raw(pane.pane.clone()),
        ]),
        Line::from(vec![
            Span::styled(" model  ", Style::default().fg(p.dim)),
            Span::raw(if class.model.is_empty() {
                class.agent.clone()
            } else {
                class.model.clone()
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                PaneStatus { pane: "terminal_0".into(), class: class("Planner", "claude", "claude-fable-5", "idle") },
                PaneStatus { pane: "terminal_1".into(), class: class("Refiner", "opencode", "openai/gpt-5.6-sol", "draft waiting") },
                PaneStatus { pane: "terminal_2".into(), class: class("Implementer", "opencode", "zai-coding-plan/glm-5.3-flash", "draft waiting") },
                PaneStatus { pane: "terminal_3".into(), class: class("Reviewer", "opencode", "openai/gpt-5.6-sol", "working") },
                PaneStatus { pane: "terminal_4".into(), class: class("Releaser", "claude", "claude-opus-5", "needs trust") },
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
            "Planner", "Refiner", "Implementer", "Reviewer", "Releaser", "draft waiting",
        ] {
            assert!(content.contains(expected), "missing {expected}");
        }
    }
}
