//! Persistent UI shell: top breadcrumb + confirm/error overlays.
//!
//! The breadcrumb runs along the very top of the frame on every view.
//! The two overlay renderers are called from [`super::render`] when
//! [`super::super::app::Overlay`] is non-`None`. Both overlays draw
//! inside a centred sub-rect that the entry-point lays out — these
//! functions only paint the modal body.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};

use crate::tui::app::AppState;
use crate::tui::theme::{ACCENT, ERR, MUTED, OK, chip, modal_key};

use super::state_word;

/// Top breadcrumb row. Left side: `fleet | <repo> | <session-id> <state>`
/// (or `(no sessions)` when nothing's selected). Right side: a pair of
/// dot badges for the runtime adapter (probed at startup) and the
/// autonomous engine (off until `Shift+A`).
pub(super) fn render_breadcrumb(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(22)])
        .split(area);

    let repo_chip = state
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("fleet");
    let muted = Style::default().fg(MUTED);
    let mut left: Vec<Span<'_>> = vec![
        chip("fleet", ACCENT),
        Span::styled(" | ", muted),
        chip(repo_chip, ACCENT),
        Span::styled(" | ", muted),
    ];
    match state.selected() {
        Some(s) => {
            left.push(Span::raw(s.id.to_string()));
            left.push(Span::raw(" "));
            left.push(Span::styled(state_word(s.state).to_string(), muted));
        }
        None => {
            left.push(Span::styled("(no sessions)", muted));
        }
    }
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);

    // Right: runtime + autonomous dots. `runtime:●` derives from the
    // doctor snapshot probed in `terminal::run`; muted when missing
    // (e.g. tests that bypass `run`). `auto:●` is green when the
    // engine is on, muted when off.
    let runtime_color = match state.doctor.as_ref() {
        Some(d) if d.adapter.is_ok() => OK,
        Some(_) => ERR,
        None => MUTED,
    };
    let auto_color = if state.autonomous.enabled() {
        OK
    } else {
        MUTED
    };
    let right = Line::from(vec![
        Span::styled("runtime", muted),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(runtime_color)),
        Span::raw("  "),
        Span::styled("auto", muted),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(auto_color)),
        Span::raw(" "),
    ]);
    f.render_widget(
        Paragraph::new(right).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

pub(super) fn render_confirm(f: &mut Frame<'_>, area: Rect, prompt: &str) {
    let muted = Style::default().fg(MUTED);
    let mut lines: Vec<Line<'static>> = prompt
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default())))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        modal_key("y"),
        Span::styled(" yes    ", muted),
        modal_key("n"),
        Span::styled(" / ", muted),
        modal_key("esc"),
        Span::styled(" cancel", muted),
    ]));
    let title = Line::from(Span::styled(
        " Confirm ",
        Style::default()
            .fg(crate::tui::theme::WARN)
            .add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui::theme::WARN))
        .title(title)
        .padding(Padding::horizontal(2));
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

pub(super) fn render_error(f: &mut Frame<'_>, area: Rect, message: &str) {
    let muted = Style::default().fg(MUTED);
    let mut lines: Vec<Line<'static>> =
        message.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("press any key to dismiss", muted)));
    let title = Line::from(Span::styled(
        " Error ",
        Style::default().fg(ERR).add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ERR))
        .title(title)
        .padding(Padding::horizontal(2));
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}
