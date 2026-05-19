//! Bottom status bar — action-driven flashes, autonomous mode badge,
//! and the per-view keybinding legend.
//!
//! Flash state is owned by [`crate::tui::app::StatusBar`]. This
//! module is now a pure renderer: it asks the bar for `current()`
//! (which encapsulates the TTL) and either paints that flash or
//! falls back to the legend.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[cfg(test)]
use crate::plans::{Plan, PlanState};
#[cfg(test)]
use crate::session::Session;
use crate::tui::app::{AppState, View};
use crate::tui::theme::{ACCENT, ERR, MUTED, OK, badge, chip, key as theme_key, sep};

#[cfg(test)]
use super::lifetime_cost;

pub(super) fn render_status(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Flashes ride the single StatusBar channel — autonomous engine
    // status changes ("spawned X for #Y") are routed through
    // `surface_autonomous_status_change` so they decay with the
    // same TTL and dismiss-on-key semantics as plan toggles, kill
    // confirmations, etc. The breadcrumb's `auto:●` dot remains the
    // source of truth for "is autonomous enabled?" — the bottom bar
    // doesn't need to repeat it.
    if let Some(msg) = state.status.current() {
        let trimmed = msg.trim();
        // Heuristic: status messages naming a failure (kill
        // rejected, spawn failed, reload failed) get the error
        // badge; everything else reads as info. The wording is
        // deliberate — every failure path in the app modules
        // includes "failed", "rejected", or "error" in its flash
        // string.
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

/// Build the bottom-bar legend for the active view. Capital letters
/// stand for Shift-modified bindings (`K` = Shift+K, etc.) — no `⇧`
/// glyph because the case already carries the meaning.
#[allow(clippy::too_many_lines)] // one match arm per view; each is a flat list of spans.
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
                theme_key("L"),
                Span::raw(" loop"),
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
                theme_key("A"),
                Span::raw(" auto"),
                sep(),
                theme_key("L"),
                Span::raw(" loop"),
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
                theme_key("R"),
                Span::raw(" retry-failed"),
                sep(),
                theme_key("u"),
                Span::raw(" unblock"),
                sep(),
                theme_key("A"),
                Span::raw(" auto"),
                sep(),
                theme_key("L"),
                Span::raw(" loop"),
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

/// Render the steady-state "N sessions · …" tail. The live status
/// bar no longer pre-builds this — `state.status` owns the flash
/// channel directly — but tests still assert against the wording,
/// so the helper survives gated on `cfg(test)`.
#[cfg(test)]
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
