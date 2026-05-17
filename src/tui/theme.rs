//! Colour tokens + small inline-span helpers shared by every panel.
//!
//! Pulling these constants out of the individual `draw_*` functions means
//! that swapping the palette (or wiring a future light/dark switch) is one
//! file to touch, not seven. Keep the surface small — a handful of named
//! colours and the three or four span builders that appear in more than one
//! place.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

/// Primary accent — section titles, the leading chip in the breadcrumb /
/// status bar, the highlight colour for the selected list row.
///
/// 256-colour index 141 is a soft violet/lavender. Picked because the
/// status palette already claims red/green/yellow and cyan made fleet
/// read as a keel clone — violet leaves every status colour visually
/// distinct from the accent and gives the UI its own identity.
pub const ACCENT: Color = Color::Indexed(141);

/// Muted foreground for secondary text and panel borders. Reads as "less
/// important than the surrounding content" without disappearing entirely.
pub const MUTED: Color = Color::DarkGray;

/// Status palette. Used by the runtime/auto badge dots and by status-bar
/// flashes (info / error).
pub const OK: Color = Color::Green;
pub const ERR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;

/// Indexed background for the selected sidebar row — a dark grey one step
/// brighter than the terminal default so the selection reads against
/// black/near-black backgrounds without competing with the accent border.
pub const SELECT_BG: Color = Color::Indexed(238);

/// Indexed foreground for the selected sidebar row — near-white. Pairs
/// with [`SELECT_BG`] for the highlight pill.
pub const SELECT_FG: Color = Color::Indexed(255);

/// A bordered panel with a single-string title rendered bold-accent.
/// Used by panels whose title is plain text (the sessions list, the
/// output pane).
pub fn framed_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

/// Variant of [`framed_block`] that takes a pre-styled [`Line`] so the
/// caller can compose multiple spans (e.g. bold name + dim italic kind
/// tag, like keel's info-panel header).
pub fn framed_block_titled(title: Line<'_>) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(title)
}

/// One key/value row for the details panel. 14-col left-aligned muted
/// key + plain value matches keel's info panel; the panel block adds
/// horizontal padding so there's breathing room from the border.
pub fn kv_line(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), Style::default().fg(MUTED)),
        Span::raw(value.to_string()),
    ])
}

/// Bold coloured label — used for breadcrumb chips and the leading `[fleet]`
/// chip in the status bar.
pub fn chip(text: &str, color: Color) -> Span<'_> {
    Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// Block-style badge — black foreground on a coloured background, used by
/// status-bar flashes (`! `, `? `, `✓ `) to read as a discrete pill that
/// won't disappear into a long red error line.
pub fn badge(text: &str, color: Color) -> Span<'_> {
    Span::styled(
        text.to_string(),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

/// Dot separator between hotkey groups in the status bar.
pub fn sep() -> Span<'static> {
    Span::styled(" · ", Style::default().fg(MUTED))
}

/// Hotkey glyph (`↑`, `enter`, `K`, …) rendered bold-accent.
///
/// Convention across fleet:
/// - Shift-modified keys render as the capitalised letter alone (`S`
///   for Shift-S, `T` for Shift-T) — no `⇧` glyph, since the capital
///   letter is unambiguous and the arrow added noise.
/// - Named keys are lowercase (`enter`, `esc`, `tab`) so they read
///   as words rather than buttons, and don't compete visually with
///   the single-letter chords.
/// - Color is the ACCENT colour so hotkeys pop against the muted
///   status-bar body without using a separate "key colour".
pub fn key(k: &str) -> Span<'_> {
    Span::styled(
        k.to_string(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

/// Hotkey glyph as rendered inside a modal footer — same shape as
/// [`key`] but muted instead of accented. Modals already have the
/// user's full attention (they block the rest of the UI), so the
/// footer keys don't need to compete with the body. Bold-on-muted
/// keeps the key glyph distinguishable from the surrounding labels
/// without shouting.
pub fn modal_key(k: &str) -> Span<'_> {
    Span::styled(
        k.to_string(),
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )
}
