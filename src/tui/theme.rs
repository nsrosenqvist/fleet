//! Colour tokens + small inline-span helpers shared by every panel.
//!
//! Pulling these constants out of the individual `draw_*` functions means
//! that swapping the palette (or wiring a future light/dark switch) is one
//! file to touch, not seven. Keep the surface small — a handful of named
//! colours and the three or four span builders that appear in more than one
//! place.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

/// Primary accent — section titles, the leading chip in the breadcrumb /
/// status bar, the highlight colour for the selected list row.
pub const ACCENT: Color = Color::Cyan;

/// Muted foreground for secondary text and panel borders. Reads as "less
/// important than the surrounding content" without disappearing entirely.
pub const MUTED: Color = Color::DarkGray;

/// Status palette. Used by the AO/VM badge dots and by status-bar flashes
/// (info / error).
pub const OK: Color = Color::Green;
pub const ERR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;

/// Foreground for hotkey glyphs in the status bar — bright white against the
/// muted-grey body of the bar so the keys read at a glance.
pub const KEY_FG: Color = Color::White;

/// A bordered panel with a coloured title. Used by every framed pane
/// (sessions list, details, output).
pub fn framed_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(ACCENT),
        ))
}

/// Bold coloured label — used for breadcrumb chips and the leading `[fleet]`
/// chip in the status bar.
pub fn chip(text: &str, color: Color) -> Span<'_> {
    Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// Dot separator between hotkey groups in the status bar.
pub fn sep() -> Span<'static> {
    Span::styled("  ·  ", Style::default().fg(MUTED))
}

/// Hotkey glyph (`↑`, `Enter`, `K`, …) rendered bold-white.
pub fn key(k: &str) -> Span<'_> {
    Span::styled(
        k.to_string(),
        Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
    )
}
