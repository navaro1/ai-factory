//! The Amber CRT look of the Theory view.
//!
//! Amber on near-black, double-line frames, uppercase block titles. The
//! six-color scale carries no red, so the miss rows, the hold rows, and
//! the strip error keep the semantic `THEME` colors. White marks the
//! selected row alone.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType};

/// The six colors of the Amber CRT scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Crt {
    /// The normal foreground of text.
    pub(super) amber: Color,
    /// The foreground of secondary text.
    pub(super) dim: Color,
    /// The foreground of block titles, the governor word, and highlights.
    pub(super) bright: Color,
    /// The foreground of the double-line frame.
    pub(super) frame: Color,
    /// The accent of the selected row.
    pub(super) white: Color,
    /// The near-black background of the view.
    pub(super) background: Color,
}

/// The one palette the Theory view uses.
pub(super) const CRT: Crt = Crt {
    amber: Color::Rgb(0xFF, 0xB0, 0x00),
    dim: Color::Rgb(0x9A, 0x6A, 0x00),
    bright: Color::Rgb(0xFF, 0xD8, 0x66),
    frame: Color::Rgb(0xC9, 0x8A, 0x00),
    white: Color::Rgb(0xFF, 0xF7, 0xE0),
    background: Color::Rgb(0x0B, 0x0A, 0x06),
};

/// The double-line frame of one Theory view block.
///
/// The border draws in the frame color, the title draws uppercase and
/// bold in the bright color, and the block paints the near-black
/// background of the view.
pub(super) fn frame(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(CRT.frame))
        .style(Style::default().bg(CRT.background))
        .title(Span::styled(
            format!(" {} ", title.to_uppercase()),
            Style::default().fg(CRT.bright).add_modifier(Modifier::BOLD),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn the_palette_locks_the_six_crt_colors() {
        assert_eq!(CRT.amber, Color::Rgb(0xFF, 0xB0, 0x00));
        assert_eq!(CRT.dim, Color::Rgb(0x9A, 0x6A, 0x00));
        assert_eq!(CRT.bright, Color::Rgb(0xFF, 0xD8, 0x66));
        assert_eq!(CRT.frame, Color::Rgb(0xC9, 0x8A, 0x00));
        assert_eq!(CRT.white, Color::Rgb(0xFF, 0xF7, 0xE0));
        assert_eq!(CRT.background, Color::Rgb(0x0B, 0x0A, 0x06));
    }

    /// The screen text of one render, one buffer row per line.
    fn render(widget: Block<'static>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal
            .draw(|f| f.render_widget(widget, f.area()))
            .unwrap();
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

    #[test]
    fn the_frame_draws_double_borders_and_an_uppercase_title() {
        let text = render(frame("pay"));

        assert!(text.contains('╔'), "{text}");
        assert!(text.contains('╚'), "{text}");
        assert!(text.contains('╝'), "{text}");
        assert!(text.contains(" PAY "), "{text}");
        assert!(!text.contains("pay"), "{text}");
    }
}
