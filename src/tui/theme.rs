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
    Span::styled("  ·  ", Style::default().fg(MUTED))
}

/// Hotkey glyph (`↑`, `Enter`, `K`, …) rendered bold-white.
pub fn key(k: &str) -> Span<'_> {
    Span::styled(
        k.to_string(),
        Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
    )
}

/// OSC 8 hyperlink: makes `text` cmd/ctrl-clickable in terminals that
/// support the escape (`Ghostty`, `iTerm2`, `WezTerm`, `Kitty`, `Windows
/// Terminal`, modern xterm). Terminals that ignore the escape render
/// `text` plainly, so the URL is still readable and copy-pasteable.
/// Styled underlined so users get a visual cue that the text behaves
/// like a link.
pub fn hyperlink(url: &str, text: &str) -> Span<'static> {
    // OSC 8 wire format: ESC ] 8 ;; <url> ESC \ <text> ESC ] 8 ;; ESC \
    // The escape characters have width 0 in unicode-width, but the URL
    // is embedded literally, which inflates Line::width() by ~url-len
    // chars. Acceptable here since the docs line is never the widest
    // line in the modal.
    let raw = format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\");
    Span::styled(
        raw,
        Style::default().fg(ACCENT).add_modifier(Modifier::UNDERLINED),
    )
}
