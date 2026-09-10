//! The Theory view: what the governor knows about each repository.
//!
//! The view is read only. It draws one block per repository: the header
//! strip with the governor state and the counts, then the AREAS panel with
//! the tier each area reaches.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::sock::{AreaView, StateView, TheoryView};
use crate::theory::verify::Tier;

use super::theme::THEME;

/// The separator of the header strip and the area rows.
const DOT: &str = " · ";

/// Draw the Theory view.
pub(super) fn draw(f: &mut Frame, area: Rect, state: &StateView) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(THEME.dim())
        .title(Span::styled(
            " theory ",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let mut lines: Vec<Line> = Vec::new();
    for (alias, view) in &state.theory {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            alias.clone(),
            Style::default().fg(THEME.repo).add_modifier(Modifier::BOLD),
        )));
        lines.push(strip(view));
        if !view.governor {
            continue;
        }
        lines.push(Line::from(Span::styled("AREAS", THEME.dim())));
        if view.areas.is_empty() {
            lines.push(Line::from(Span::styled("no area", THEME.dim())));
        }
        lines.extend(view.areas.iter().map(area_row));
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
                view.entries.len(),
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
fn area_row(row: &AreaView) -> Line<'static> {
    let mark = mark(row);
    let color = match mark.as_str() {
        "!" => THEME.error,
        "-" => THEME.dim,
        _ => THEME.accent,
    };
    Line::from(vec![
        Span::styled(row.id.clone(), Style::default().fg(THEME.text)),
        Span::styled(DOT, THEME.dim()),
        Span::styled(mark, Style::default().fg(color)),
    ])
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

    fn view(areas: Vec<AreaView>, entries: usize) -> StateView {
        let mut theory = BTreeMap::new();
        theory.insert(
            "borsuk".to_string(),
            TheoryView {
                governor: true,
                error: String::new(),
                entries: vec![crate::sock::EntryView::default(); entries],
                areas,
                skills: BTreeMap::new(),
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
        let backend = TestBackend::new(70, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, f.area(), state)).unwrap();
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
            tier,
            min_tier,
            lint,
        }
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
