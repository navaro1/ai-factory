//! The Theory view: what the governor knows about each repository.
//!
//! The view draws one block per repository: the header strip with the
//! governor state and the counts, then the AREAS panel with the tier each
//! area reaches. The operator marks one repository with `j` and `k`, and
//! `v` asks for the run skill of one surface.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::sock::{Action, AreaView, StateView, TheoryAction, TheoryView};
use crate::theory::verify::Tier;

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

/// The Theory view state.
///
/// It holds no repository data. The marked alias survives a state push, and
/// the shell falls back to the first repository when the marked one is gone.
#[derive(Debug, Default)]
pub(super) struct Theory {
    /// The alias the operator marked.
    marked: Option<String>,
    /// The surface name typed so far, while the input is open.
    input: Option<String>,
}

impl Theory {
    /// True while the surface input holds the keyboard.
    pub(super) fn typing(&self) -> bool {
        self.input.is_some()
    }

    /// The alias the keys act on: the marked repository, else the first one.
    fn current(&self, state: &StateView) -> Option<String> {
        let marked = self
            .marked
            .as_ref()
            .filter(|alias| state.theory.contains_key(*alias));
        match marked {
            Some(alias) => Some(alias.clone()),
            None => state.theory.keys().next().cloned(),
        }
    }

    /// The key hints of the footer.
    pub(super) fn footer_hints(&self) -> String {
        match &self.input {
            Some(buffer) => format!("surface: {buffer}_{DOT}enter send{DOT}esc cancel"),
            None => format!("1-6 view{DOT}j/k row{DOT}v run skill{DOT}esc home"),
        }
    }

    /// Handle one key while the Theory view is open.
    pub(super) fn handle_key(&mut self, state: &StateView, key: KeyEvent) -> Outcome {
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

    /// Move the mark by `delta` rows, without wrapping.
    fn move_mark(&mut self, state: &StateView, delta: isize) {
        let aliases: Vec<&String> = state.theory.keys().collect();
        if aliases.is_empty() {
            return;
        }
        let current = self.current(state);
        let at = current
            .as_ref()
            .and_then(|alias| aliases.iter().position(|one| *one == alias))
            .unwrap_or(0);
        let next = (at as isize + delta).clamp(0, aliases.len() as isize - 1) as usize;
        self.marked = Some(aliases[next].clone());
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
pub(super) fn draw(f: &mut Frame, area: Rect, state: &StateView, view: &Theory) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(THEME.dim())
        .title(Span::styled(
            " theory ",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let marked = view.current(state);
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
        lines.extend(row.areas.iter().map(area_row));
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
        render_with(state, &Theory::default())
    }

    fn render_with(state: &StateView, view: &Theory) -> String {
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

    /// One key press with no modifier.
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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

        assert!(render_with(&state, &pane).contains("> borsuk"));

        pane.handle_key(&state, press(KeyCode::Char('j')));
        assert!(render_with(&state, &pane).contains("> qubitsok"));
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
