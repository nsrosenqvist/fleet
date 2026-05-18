//! Bottom status bar — action-driven flashes, autonomous mode badge,
//! and the per-view keybinding legend.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::plans::{Plan, PlanState};
use crate::session::Session;
use crate::tui::app::{AppState, View};
use crate::tui::theme::{ACCENT, ERR, MUTED, OK, badge, chip, key as theme_key, sep};

use super::lifetime_cost;

pub(super) fn render_status(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Action-driven flashes replace the legend rather than appending to
    // it — long error messages from a real shell-out can run past the
    // right edge of a typical terminal and chop off the relevant text.
    // Anything load-bearing belongs on the left.
    if state.autonomous.enabled() || state.autonomous.status() != "autonomous: OFF" {
        let line = Line::from(vec![
            badge(" auto ", ACCENT),
            Span::raw(" "),
            Span::styled(
                state.autonomous.status().to_string(),
                Style::default().fg(ACCENT),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let trimmed = state.status_line.trim();
    if !trimmed.is_empty() && trimmed != "ready" && !is_steady_state_line(trimmed) {
        // Heuristic: status messages naming a failure (kill rejected,
        // spawn failed, reload failed) get the error badge; everything
        // else is treated as info. The wording is deliberate — every
        // failure path in `app.rs` includes "failed" or "rejected" in
        // its status line.
        let is_error =
            trimmed.contains("failed") || trimmed.contains("rejected") || trimmed.contains("error");
        let (glyph, color) = if is_error {
            (" ! ", ERR)
        } else {
            (" ✓ ", OK)
        };
        let line = Line::from(vec![
            badge(glyph, color),
            Span::raw(" "),
            Span::styled(trimmed.to_string(), Style::default().fg(color)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let spans = status_legend_spans(state);
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(MUTED)),
        area,
    );
}

/// `true` when `s` is the steady-state status line produced by
/// [`render_status_line`] (`N sessions`, possibly with cost/plan
/// suffixes). Those aren't action results — they're the default the
/// app resets to on every `reload`, so we should show the legend
/// instead of badging them as a flash.
fn is_steady_state_line(s: &str) -> bool {
    // The render_status_line shape always begins with a digit count
    // followed by " sessions". Anything matching that is the
    // information line, not a transient flash.
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    // Skip the rest of the leading number, then look for ` sessions`.
    while let Some(c) = chars.clone().next() {
        if c.is_ascii_digit() {
            chars.next();
        } else {
            break;
        }
    }
    chars.as_str().starts_with(" sessions")
}

/// Build the bottom-bar legend for the active view. Capital letters
/// stand for Shift-modified bindings (`K` = Shift+K, etc.) — no `⇧`
/// glyph because the case already carries the meaning.
fn status_legend_spans(state: &AppState) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = vec![chip("[fleet]", ACCENT), sep()];
    match state.view {
        View::Sessions => {
            spans.extend([
                theme_key("↑/↓"),
                Span::raw(" nav"),
                sep(),
                theme_key("enter"),
                Span::raw(" attach"),
                sep(),
                theme_key("n"),
                Span::raw(" new"),
                sep(),
                theme_key("d"),
                Span::raw(" doctor"),
                sep(),
                theme_key("p"),
                Span::raw(" plans"),
                sep(),
                theme_key("K"),
                Span::raw(" kill"),
                sep(),
                theme_key("A"),
                Span::raw(" auto"),
                sep(),
                theme_key("T"),
                Span::raw(" tracker"),
                sep(),
                theme_key("r"),
                Span::raw(" reload"),
                sep(),
                theme_key("q"),
                Span::raw(" quit"),
            ]);
        }
        View::Doctor => {
            spans.extend([
                theme_key("esc"),
                Span::raw("/"),
                theme_key("d"),
                Span::raw(" back"),
                sep(),
                theme_key("r"),
                Span::raw(" re-probe"),
                sep(),
                theme_key("q"),
                Span::raw(" quit"),
            ]);
        }
        View::Spawn => {
            spans.extend([
                theme_key("↑/↓"),
                Span::raw(" pick"),
                sep(),
                theme_key("tab"),
                Span::raw(" switch"),
                sep(),
                theme_key("enter"),
                Span::raw(" spawn"),
                sep(),
                theme_key("esc"),
                Span::raw(" cancel"),
            ]);
        }
        View::Plans => {
            spans.extend([
                theme_key("↑/↓"),
                Span::raw(" nav"),
                sep(),
                theme_key("tab"),
                Span::raw(" focus"),
                sep(),
                theme_key("P"),
                Span::raw(" pause/resume"),
                sep(),
                theme_key("C"),
                Span::raw(" complete"),
                sep(),
                theme_key("u"),
                Span::raw(" unblock"),
                sep(),
                theme_key("r"),
                Span::raw(" reload"),
                sep(),
                theme_key("esc"),
                Span::raw("/"),
                theme_key("p"),
                Span::raw(" back"),
            ]);
        }
    }
    spans
}

/// Render the status-bar tail used by the sidebar when autonomous
/// mode isn't taking over the line.
#[must_use]
pub(in crate::tui) fn render_status_line(sessions: &[Session], plans: &[Plan]) -> String {
    let (total, samples) = lifetime_cost(sessions);
    let active_plans = plans
        .iter()
        .filter(|p| p.state == PlanState::Active)
        .count();
    let mut out = format!(" {} sessions", sessions.len());
    if samples > 0 {
        use std::fmt::Write as _;
        let _ = write!(out, " · ${total:.2} total ({samples} with cost)");
    }
    if active_plans > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            " · {active_plans} active plan{}",
            if active_plans == 1 { "" } else { "s" }
        );
    }
    out.push(' ');
    out
}
