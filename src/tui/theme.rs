//! Holds the constant color palette for the terminal UI.
//!
//! The palette is a plain constant struct. The UI never reads the v0.4
//! token file, because that pipeline goes away with the v0.4 tree.

use ratatui::style::{Color, Modifier, Style};

/// The color palette of the terminal UI.
///
/// Every color works on a dark and on a light terminal, because the base
/// text keeps the terminal default foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The normal foreground of text.
    pub text: Color,
    /// The foreground of secondary text.
    pub dim: Color,
    /// The foreground of highlights, stage headers, and the active tab.
    pub accent: Color,
    /// The foreground of healthy state, such as a running count.
    pub ok: Color,
    /// The foreground of attention state, such as a pause or a toast.
    pub warn: Color,
    /// The foreground of failure state.
    pub error: Color,
    /// The background of the selected row.
    pub selected_bg: Color,
}

/// The one palette the whole UI uses.
pub const THEME: Theme = Theme {
    text: Color::Reset,
    dim: Color::DarkGray,
    accent: Color::Cyan,
    ok: Color::Green,
    warn: Color::Yellow,
    error: Color::Red,
    selected_bg: Color::DarkGray,
};

impl Theme {
    /// The style of secondary text.
    pub fn dimmed(self) -> Style {
        Style::default().fg(self.dim)
    }

    /// The style of the selected row. The row keeps its own colors.
    pub fn selected(self) -> Style {
        Style::default().bg(self.selected_bg)
    }

    /// The style of the banner that reports a lost daemon connection.
    pub fn banner(self) -> Style {
        Style::default()
            .fg(Color::Black)
            .bg(self.error)
            .add_modifier(Modifier::BOLD)
    }

    /// The style of the toast that confirms a sent action.
    pub fn toast(self) -> Style {
        Style::default().fg(Color::Black).bg(self.warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_carry_the_palette_colors() {
        assert_eq!(THEME.dimmed().fg, Some(THEME.dim));
        assert_eq!(THEME.selected().bg, Some(THEME.selected_bg));
        assert_eq!(THEME.banner().bg, Some(THEME.error));
        assert_eq!(THEME.toast().bg, Some(THEME.warn));
    }
}
