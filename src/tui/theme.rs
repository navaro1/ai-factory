//! Holds the constant color palette for the terminal UI.
//!
//! The palette is a plain constant struct. The UI never reads the v0.4
//! token file, because that pipeline goes away with the v0.4 tree.

use ratatui::style::{Color, Modifier, Style};

/// The color palette of the terminal UI.
///
/// The palette defines both foreground and background colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Theme {
    /// The base background of the terminal application.
    pub(super) background: Color,
    /// The normal foreground of text.
    pub(super) text: Color,
    /// The foreground of secondary text.
    pub(super) dim: Color,
    /// The foreground of highlights, stage headers, and the active tab.
    pub(super) accent: Color,
    /// The foreground of repository aliases.
    pub(super) repo: Color,
    /// The foreground of healthy state, such as a running count.
    pub(super) ok: Color,
    /// The foreground of attention state, such as a pause or a toast.
    pub(super) warn: Color,
    /// The foreground of failure state.
    pub(super) error: Color,
    /// The background of the selected row.
    pub(super) selected_bg: Color,
}

/// The one palette the whole UI uses.
pub(super) const THEME: Theme = Theme {
    background: Color::Rgb(0x09, 0x0C, 0x14),
    text: Color::Rgb(0xE7, 0xEC, 0xFF),
    dim: Color::Rgb(0x87, 0x90, 0xAD),
    accent: Color::Rgb(0x55, 0xE6, 0xFF),
    repo: Color::Rgb(0xFF, 0x5F, 0xD7),
    ok: Color::Rgb(0x7D, 0xFF, 0xB2),
    warn: Color::Rgb(0xFF, 0xCC, 0x66),
    error: Color::Rgb(0xFF, 0x6B, 0x7A),
    selected_bg: Color::Rgb(0x18, 0x20, 0x33),
};

impl Theme {
    /// The style of secondary text.
    pub(super) fn dim(self) -> Style {
        Style::default().fg(self.dim)
    }

    /// The style of the selected row. The row keeps its own colors.
    pub(super) fn selected(self) -> Style {
        Style::default().bg(self.selected_bg)
    }

    /// The style of the banner that reports a lost daemon connection.
    pub(super) fn banner(self) -> Style {
        Style::default()
            .fg(Color::Black)
            .bg(self.error)
            .add_modifier(Modifier::BOLD)
    }

    /// The style of the toast that confirms a sent action.
    pub(super) fn toast(self) -> Style {
        Style::default().fg(Color::Black).bg(self.warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_palette_uses_the_required_true_colors() {
        assert_eq!(THEME.background, Color::Rgb(0x09, 0x0C, 0x14));
        assert_eq!(THEME.text, Color::Rgb(0xE7, 0xEC, 0xFF));
        assert_eq!(THEME.accent, Color::Rgb(0x55, 0xE6, 0xFF));
        assert_eq!(THEME.repo, Color::Rgb(0xFF, 0x5F, 0xD7));
        assert_eq!(THEME.warn, Color::Rgb(0xFF, 0xCC, 0x66));
        assert_eq!(THEME.ok, Color::Rgb(0x7D, 0xFF, 0xB2));
        assert_eq!(THEME.error, Color::Rgb(0xFF, 0x6B, 0x7A));
    }
}
