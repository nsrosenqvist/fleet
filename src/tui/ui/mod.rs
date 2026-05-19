//! Rendering — pure `(&AppState, &mut Frame) -> ()` plumbing.
//!
//! Per-view DDD-style split: each top-level view (sessions, plans,
//! spawn picker, doctor) has its own submodule for renderers, and
//! cross-cutting chrome (breadcrumb, overlays, status bar) lives in
//! sibling files. No state mutation here; the input layer
//! ([`super::input`]) drives all changes via [`super::app::AppState`].
//!
//! Display-side helpers that are intrinsically tied to a single view
//! (state-marker glyphs, kv-line builders) live with that view.
//! Helpers shared across views — `centered_rect`, the session-state
//! word/marker/style trio, `format_cost`/`format_epoch_ms`,
//! `detail_kv_pairs` — sit here in `mod.rs` so every view can reach
//! them without going through a sibling.

mod chrome;
mod doctor;
mod plans;
mod sessions;
mod spawn;
mod status_bar;

// Re-exports — anything the input/event-loop side, tests, or external
// callers reach for through `super::ui::*` lives here. Inside the `ui`
// tree, submodules pull from each other via `super::` directly.
//
// `plan_state_word` is surfaced for the non-test path; the rest are
// test-only re-exports that mirror the pre-split flat `ui::*`
// surface tests assert against. `render_status_line` survives as a
// test-only helper now — the live renderer reads from
// `state.status` directly and no longer needs the steady-state line
// pre-built into `status_line`.
pub(super) use plans::plan_state_word;
#[cfg(test)]
pub(super) use status_bar::render_status_line;

#[cfg(test)]
pub(super) use plans::{
    append_deps_lines, issue_detail_lines, plan_detail_lines, plan_failure_word, plan_item_word,
    plan_row_label, ticket_detail_lines,
};
#[cfg(test)]
pub(super) use sessions::{orchestrator_row_label, session_row_label};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Clear;

use crate::session::{Session, SessionState};

use super::app::{AppState, Overlay, View};
use super::theme::{ACCENT, ERR, OK, WARN};

/// Full-frame render entry point. Called once per event-loop tick from
/// [`super::terminal::run`].
pub(super) fn render(f: &mut Frame<'_>, state: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // breadcrumb
            Constraint::Min(0),    // body
            Constraint::Length(1), // status bar
        ])
        .split(f.area());
    chrome::render_breadcrumb(f, outer[0], state);
    let body_area = outer[1];
    match state.view {
        View::Sessions | View::Spawn => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(body_area);
            sessions::render_sidebar(f, body[0], state);
            sessions::render_detail(f, body[1], state);
        }
        View::Doctor => {
            doctor::render_doctor(f, body_area, state);
        }
        View::Plans => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(40),
                    Constraint::Percentage(35),
                ])
                .split(body_area);
            plans::render_plans_sidebar(f, body[0], state);
            plans::render_plans_detail(f, body[1], state);
            plans::render_plans_ticket(f, body[2], state);
        }
    }
    if state.view == View::Spawn {
        // Taller + wider than the old workflow-only picker — needs
        // room for the filter line, tab strip, scrollable list, and
        // help footer without the issue title getting truncated to a
        // useless prefix.
        let modal = centered_rect(body_area, 78, 75);
        f.render_widget(Clear, modal);
        spawn::render_spawn(f, modal, state);
    }
    match &state.overlay {
        Overlay::Confirm { prompt, .. } => {
            let modal = centered_rect(body_area, 50, 30);
            f.render_widget(Clear, modal);
            chrome::render_confirm(f, modal, prompt);
        }
        Overlay::Error { message } => {
            let modal = centered_rect(body_area, 50, 25);
            f.render_widget(Clear, modal);
            chrome::render_error(f, modal, message);
        }
        Overlay::None => {}
    }
    status_bar::render_status(f, outer[2], state);
}

/// Centred sub-rectangle of `parent`, sized to `pct_x`% wide and
/// `pct_y`% tall (each clamped to `[10, 95]`).
#[must_use]
pub fn centered_rect(parent: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let pct_x = pct_x.clamp(10, 95);
    let pct_y = pct_y.clamp(10, 95);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(parent);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Render a Unix-epoch-ms timestamp as `YYYY-MM-DD HH:MM:SS UTC` —
/// the readable side of ISO 8601, with a space separator and an
/// explicit zone tag so the value is unambiguous at a glance. UTC
/// because turning a local TZ into a string in this crate would drag
/// in a system-timezone lookup that's flaky on minimal devcontainer
/// images. Falls back to the raw ms on the rare overflow / formatter
/// failure so the value is still surfaced.
#[must_use]
pub fn format_epoch_ms(ms: u64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let nanos = i128::from(ms).saturating_mul(1_000_000);
    let Ok(dt) = OffsetDateTime::from_unix_timestamp_nanos(nanos) else {
        return format!("{ms} ms (epoch)");
    };
    // Compile-time format descriptor — no allocation for the format
    // string, single allocation for the output buffer.
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC");
    dt.format(&fmt)
        .unwrap_or_else(|_| format!("{ms} ms (epoch)"))
}

/// Short word for a session state.
pub const fn state_word(s: SessionState) -> &'static str {
    match s {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::AwaitingGate => "awaiting_gate",
        SessionState::Completed => "completed",
        SessionState::Failed => "failed",
        SessionState::Crashed => "crashed",
    }
}

/// Single-glyph state marker for the sidebar list.
pub const fn state_marker(s: SessionState) -> &'static str {
    match s {
        SessionState::Created => "·",
        SessionState::Running => "▶",
        SessionState::AwaitingGate => "⏸",
        SessionState::Completed => "✓",
        SessionState::Failed => "✗",
        SessionState::Crashed => "‼",
    }
}

pub(super) fn state_style(s: SessionState) -> Style {
    match s {
        SessionState::Completed => Style::default().fg(OK),
        SessionState::Failed | SessionState::Crashed => Style::default().fg(ERR),
        SessionState::Running => Style::default().fg(WARN),
        SessionState::AwaitingGate => Style::default().fg(ACCENT),
        SessionState::Created => Style::default(),
    }
}

/// Render a USD cost as `$0.42` (two decimals) or `-` when absent.
#[must_use]
pub fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "-".to_string(), |v| format!("${v:.2}"))
}

/// Walk every session and sum the ones with cost data. Returns
/// `(total, samples)`. Only `render_status_line` consumes this now,
/// which itself is `cfg(test)` — gated together.
#[cfg(test)]
#[must_use]
pub fn lifetime_cost(sessions: &[Session]) -> (f64, usize) {
    let mut total = 0.0;
    let mut samples = 0usize;
    for s in sessions {
        if let Some(v) = s.total_cost_usd() {
            total += v;
            samples += 1;
        }
    }
    (total, samples)
}

/// Pure helper: build the (label, value) pairs the detail view shows
/// at the top, in display order.
#[must_use]
pub fn detail_kv_pairs(session: &Session) -> Vec<(&'static str, String)> {
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("workflow", session.workflow.clone()),
        ("state", state_word(session.state).to_string()),
        (
            "node",
            session
                .current_node
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ),
    ];
    if let Some(path) = session.worktree_path.as_deref() {
        pairs.push(("worktree", path.display().to_string()));
    }
    if let Some(branch) = session.branch.as_deref() {
        pairs.push(("branch", branch.to_string()));
    }
    pairs.push(("updated", format_epoch_ms(session.updated_at_ms)));
    pairs.push(("cost", format_cost(session.total_cost_usd())));
    pairs
}

/// Underline-then-bold heading used by detail-pane sections (e.g.
/// "node costs:", "Body:", "Items (N)", "Blocks (N):"). Centralised
/// so every section heading reads the same — DRY for the three views
/// that build kv-style detail panes.
pub(super) fn section_heading(text: impl Into<String>) -> Line<'static> {
    use ratatui::text::Span;
    Line::from(Span::styled(
        text.into(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))
}

/// Borrowed/owned-friendly truncation: returns the original string by
/// reference when it already fits, owned otherwise. Saves an
/// allocation on the common case (most issue titles are short).
pub(super) fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    std::borrow::Cow::Owned(format!("{truncated}…"))
}
