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
pub(super) struct Theme {
    /// The normal foreground of text.
    pub(super) text: Color,
    /// The foreground of secondary text.
    pub(super) dim: Color,
    /// The foreground of highlights, stage headers, and the active tab.
    pub(super) accent: Color,
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
