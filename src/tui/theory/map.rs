//! The MAP panel of the Theory view: one area of the model as a picture.
//!
//! The panel draws the slice of one boundary. The boundary is the outer
//! double-line frame titled with the area id, the states are boxes on
//! rows by transition depth, the transitions are labeled arrows, and the
//! failures that cross the boundary draw under the frame. Text wider
//! than the pane truncates with `…`, and so does content taller than
//! the pane.

use std::collections::{BTreeSet, HashMap, VecDeque};

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::theory::model::{Entry, Model};
use crate::tui::theme::THEME;

/// The widest transition title, in cells.
///
/// A longer title keeps 7 cells and ends with `…`.
const TITLE_CAP: usize = 8;

/// One drawn line of the map.
pub(super) struct MapLine {
    /// The id of the model entry the line draws.
    pub(super) id: String,
    /// The text of the line, before the pane truncates it.
    pub(super) text: String,
    /// True when the line draws under the frame as a failure.
    pub(super) failure: bool,
}

/// The lines of one area map, in draw order.
///
/// One line per transition of the boundary slice, ordered by the depth
/// of its `from` state and then by file order. Then one line per state
/// no drawn transition names, by depth and then file order. Then one
/// line per failure that crosses `boundary`, in file order. A state the
/// depth walk never reaches takes the last depth.
pub(super) fn lines(model: &Model, boundary: &str) -> Vec<MapLine> {
    let slice = model.slice(&[boundary]);
    let mut states: Vec<&str> = Vec::new();
    let mut transitions: Vec<(&str, &str, &str, &str)> = Vec::new();
    let mut failures: Vec<(&str, &str)> = Vec::new();
    for entry in &slice.entries {
        match entry {
            Entry::State { id, .. } => states.push(id.as_str()),
            Entry::Transition {
                id,
                from,
                to,
                title,
                ..
            } => transitions.push((id.as_str(), from.as_str(), to.as_str(), title.as_str())),
            Entry::Failure { id, crosses, .. } if crosses == boundary => {
                failures.push((id.as_str(), crosses.as_str()))
            }
            _ => {}
        }
    }
    let depth = depths(&states, &transitions);
    let last = depth.values().copied().max().unwrap_or(0);
    let level = |name: &str| depth.get(name).copied().unwrap_or(last);

    let mut lines: Vec<MapLine> = Vec::new();
    let mut order: Vec<usize> = (0..transitions.len()).collect();
    order.sort_by_key(|&one| level(transitions[one].1));
    for one in order {
        let (id, from, to, title) = transitions[one];
        lines.push(MapLine {
            id: id.to_string(),
            text: format!(
                "[{from}]\u{2500}\u{2500}{}\u{2500}\u{2500}\u{25b6}[{to}]",
                fit(title, TITLE_CAP)
            ),
            failure: false,
        });
    }
    let named: BTreeSet<&str> = transitions
        .iter()
        .flat_map(|(_, from, to, _)| [*from, *to])
        .collect();
    let mut orphans: Vec<&str> = states
        .iter()
        .copied()
        .filter(|id| !named.contains(id))
        .collect();
    orphans.sort_by_key(|id| level(id));
    for id in orphans {
        lines.push(MapLine {
            id: id.to_string(),
            text: format!("[{id}]"),
            failure: false,
        });
    }
    for (id, crosses) in &failures {
        lines.push(MapLine {
            id: (*id).to_string(),
            text: format!("\u{26a0} {id} CROSSES {crosses}"),
            failure: true,
        });
    }
    lines
}

/// The BFS depth of every state name of the slice.
///
/// The walk follows the transitions, seeded at the states with no
/// incoming transition, else at the first state in file order, else at
/// the first name a transition names. A name the walk never reaches
/// stays out, and the caller gives it the last depth.
fn depths<'a>(
    states: &[&'a str],
    transitions: &[(&'a str, &'a str, &'a str, &'a str)],
) -> HashMap<&'a str, usize> {
    let incoming: BTreeSet<&str> = transitions.iter().map(|(_, _, to, _)| *to).collect();
    let mut seeds: Vec<&str> = states
        .iter()
        .copied()
        .filter(|id| !incoming.contains(id))
        .collect();
    if seeds.is_empty() {
        if let Some(first) = states.first() {
            seeds.push(first);
        } else if let Some((_, from, _, _)) = transitions.first() {
            seeds.push(from);
        }
    }
    let mut depth: HashMap<&str, usize> = HashMap::new();
    let mut queue: VecDeque<&str> = seeds.into_iter().collect();
    for seed in &queue {
        depth.insert(seed, 0);
    }
    while let Some(name) = queue.pop_front() {
        let level = depth[name];
        let next: Vec<&str> = transitions
            .iter()
            .filter(|(_, from, to, _)| *from == name && !depth.contains_key(to))
            .map(|(_, _, to, _)| *to)
            .collect();
        for to in next {
            depth.insert(to, level + 1);
            queue.push_back(to);
        }
    }
    depth
}

/// `text` cut to `width` cells, ending with `…` when it is longer.
pub(super) fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Draw the map pane.
///
/// The frame takes the rows its content needs, the failures draw on one
/// row each under the frame, and the statement strip draws on the last
/// row. The pane caps every part, so a small pane shows the frame, a
/// `…` row inside it, and drops what does not fit.
pub(super) fn draw(
    f: &mut Frame,
    pane: Rect,
    area: &str,
    lines: &[MapLine],
    cursor: Option<&str>,
    strip: Option<&str>,
) {
    if pane.width == 0 || pane.height < 2 {
        return;
    }
    let core: Vec<&MapLine> = lines.iter().filter(|line| !line.failure).collect();
    let failures: Vec<&MapLine> = lines.iter().filter(|line| line.failure).collect();
    let mut budget = pane.height as usize - 2;
    let core_rows = core.len().min(budget);
    budget -= core_rows;
    let fail_rows = failures.len().min(budget);
    budget -= fail_rows;
    let strip_row = usize::from(strip.is_some()).min(budget);

    let frame = Rect {
        height: (core_rows + 2) as u16,
        ..pane
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(THEME.dim())
        .title(Span::styled(
            format!(" {} ", area.to_uppercase()),
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(frame);
    f.render_widget(block, frame);

    let width = pane.width as usize;
    let inner_width = inner.width as usize;
    let cut = core.len() > core_rows;
    let shown = if cut {
        core_rows.saturating_sub(1)
    } else {
        core_rows
    };
    let mut rows: Vec<Line> = core[..shown]
        .iter()
        .map(|line| {
            let style = if cursor == Some(line.id.as_str()) {
                Style::default()
                    .fg(THEME.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(THEME.text)
            };
            Line::from(Span::styled(fit(&line.text, inner_width), style))
        })
        .collect();
    if cut {
        rows.push(Line::from(Span::styled("…", THEME.dim())));
    }
    f.render_widget(Paragraph::new(rows), inner);

    let mut y = pane.y + frame.height;
    for line in failures.iter().take(fail_rows) {
        let row = Rect {
            x: pane.x,
            y,
            width: pane.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&line.text, width),
                Style::default().fg(THEME.error),
            ))),
            row,
        );
        y += 1;
    }
    if strip_row == 1 {
        if let Some(text) = strip {
            let row = Rect {
                x: pane.x,
                y,
                width: pane.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(fit(text, width), THEME.dim()))),
                row,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line texts of one area map, in draw order.
    fn texts(model: &Model, boundary: &str) -> Vec<String> {
        lines(model, boundary)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    /// A boundary entry whose sides name the two states.
    fn boundary(sides: &[&str]) -> Entry {
        Entry::Boundary {
            id: "B-checkout".to_string(),
            title: "checkout".to_string(),
            statement: "the cart pays".to_string(),
            sides: sides.iter().map(|side| side.to_string()).collect(),
            paths: vec!["web/**".to_string()],
        }
    }

    fn state(id: &str) -> Entry {
        Entry::State {
            id: id.to_string(),
            title: id.to_string(),
            statement: "the cart waits".to_string(),
        }
    }

    fn transition(id: &str, title: &str, from: &str, to: &str) -> Entry {
        Entry::Transition {
            id: id.to_string(),
            title: title.to_string(),
            statement: "the poller starts the cart".to_string(),
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    /// Two states and one transition draw one arrow on one line.
    #[test]
    fn one_transition_draws_one_labeled_arrow() {
        let model = Model {
            entries: vec![
                boundary(&["IDLE", "BUSY"]),
                state("IDLE"),
                state("BUSY"),
                transition("T-poll", "poll", "IDLE", "BUSY"),
            ],
        };

        assert_eq!(
            texts(&model, "B-checkout"),
            vec!["[IDLE]──poll──▶[BUSY]".to_string()]
        );
    }

    /// A transition title past 8 cells keeps 7 and ends with `…`.
    #[test]
    fn a_long_transition_title_truncates_at_eight_cells() {
        let model = Model {
            entries: vec![
                boundary(&["IDLE", "BUSY"]),
                state("IDLE"),
                state("BUSY"),
                transition("T-poll", "overnight-refresh", "IDLE", "BUSY"),
            ],
        };

        assert_eq!(
            texts(&model, "B-checkout"),
            vec!["[IDLE]──overnig…──▶[BUSY]".to_string()]
        );
    }

    /// Transitions order by the depth of their `from` state, and a state
    /// no transition names draws its own box line after them.
    #[test]
    fn transitions_order_by_depth_and_orphan_states_draw_last() {
        let model = Model {
            entries: vec![
                boundary(&["IDLE", "BUSY", "DONE", "SIDE"]),
                state("IDLE"),
                state("BUSY"),
                state("DONE"),
                state("SIDE"),
                transition("T-pay", "pay", "BUSY", "DONE"),
                transition("T-poll", "poll", "IDLE", "BUSY"),
            ],
        };

        assert_eq!(
            texts(&model, "B-checkout"),
            vec![
                "[IDLE]──poll──▶[BUSY]".to_string(),
                "[BUSY]──pay──▶[DONE]".to_string(),
                "[SIDE]".to_string(),
            ]
        );
    }

    /// A cycle with no source state seeds the walk at the first state in
    /// file order.
    #[test]
    fn a_cycle_seeds_the_depth_walk_at_the_first_state() {
        let model = Model {
            entries: vec![
                boundary(&["A", "B"]),
                state("A"),
                state("B"),
                transition("T-ab", "loop", "A", "B"),
                transition("T-ba", "back", "B", "A"),
            ],
        };

        assert_eq!(
            texts(&model, "B-checkout"),
            vec!["[A]──loop──▶[B]".to_string(), "[B]──back──▶[A]".to_string(),]
        );
    }

    /// Only a failure that crosses the shown boundary draws, and it
    /// draws after every state and transition line.
    #[test]
    fn only_a_crossing_failure_draws_and_it_draws_last() {
        let mut model = Model {
            entries: vec![
                boundary(&["IDLE", "BUSY"]),
                state("IDLE"),
                state("BUSY"),
                transition("T-poll", "poll", "IDLE", "BUSY"),
                Entry::Failure {
                    id: "FM-2".to_string(),
                    title: "replay".to_string(),
                    statement: "a replay pays twice".to_string(),
                    crosses: "B-checkout".to_string(),
                },
                Entry::Failure {
                    id: "FM-9".to_string(),
                    title: "elsewhere".to_string(),
                    statement: "a miss at another boundary".to_string(),
                    crosses: "B-other".to_string(),
                },
            ],
        };

        assert_eq!(
            texts(&model, "B-checkout"),
            vec![
                "[IDLE]──poll──▶[BUSY]".to_string(),
                "⚠ FM-2 CROSSES B-checkout".to_string(),
            ]
        );
        model.entries[4] = Entry::Failure {
            id: "FM-2".to_string(),
            title: "replay".to_string(),
            statement: "a replay pays twice".to_string(),
            crosses: "B-other".to_string(),
        };
        assert_eq!(texts(&model, "B-checkout"), vec!["[IDLE]──poll──▶[BUSY]"]);
    }

    /// `fit` keeps a text that fits and cuts a longer one to the width
    /// with one `…` cell.
    #[test]
    fn fit_keeps_a_short_text_and_cuts_a_long_one() {
        assert_eq!(fit("[IDLE]", 10), "[IDLE]");
        assert_eq!(fit("[IDLE]", 6), "[IDLE]");
        assert_eq!(fit("[IDLE]", 5), "[IDL…");
        assert_eq!(fit("[IDLE]", 1), "…");
        assert_eq!(fit("[IDLE]", 0), "");
    }
}
