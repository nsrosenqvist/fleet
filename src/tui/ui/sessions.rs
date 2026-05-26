//! Sessions view — sidebar (orchestrator + workers panes), detail
//! pane, and output preview.
//!
//! The view is laid out as a 35/65 horizontal split: the sidebar on
//! the left stacks an always-visible orchestrator pane (3 rows) on top
//! of a workers pane below; the right half splits vertically into a
//! details kv-block on top and an ANSI-passthrough output preview
//! below.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap};

use crate::orchestrator::{OrchestratorSession, OrchestratorState};
use crate::plans::{Plan, PlanState};
use crate::session::Session;
use crate::tui::app::{AppState, SessionsFocus};
use crate::tui::theme::{
    ACCENT, IDENT, MUTED, OK, SELECT_BG, SELECT_FG, WARN, framed_block, framed_block_accent,
    framed_block_titled, kv_line,
};

use super::{detail_kv_pairs, section_heading, state_marker, state_style, state_word};

pub(super) fn render_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Two distinct framed panes, stacked: orchestrator on top
    // (one-row tall — there's only ever one) and workers below
    // (rest of the area). Each owns its own border + title so the
    // user reads the sidebar as two separate sections, not one
    // section with an inline divider.
    let (orchestrator_area, workers_area) = split_sidebar_areas(area);
    render_orchestrator_pane(f, orchestrator_area, state);
    render_workers_pane(f, workers_area, state);
}

/// Layout split: orchestrator pane is 3 rows (1 row of content +
/// the border on top and bottom). Workers pane takes the rest.
/// Returns `(orchestrator_area, workers_area)`.
#[must_use]
fn split_sidebar_areas(area: Rect) -> (Rect, Rect) {
    const ORCHESTRATOR_PANE_HEIGHT: u16 = 3;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(ORCHESTRATOR_PANE_HEIGHT),
            Constraint::Min(0),
        ])
        .split(area);
    (chunks[0], chunks[1])
}

fn render_orchestrator_pane(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // No counter in the title — there's only ever 0 or 1, so a
    // count would be visual noise. Border switches to ACCENT when
    // the orchestrator sub-list owns the focus, so the user can see
    // at a glance which `j/k` target is active.
    let block = if state.sessions_focus == SessionsFocus::Orchestrator {
        framed_block_accent(" orchestrator ")
    } else {
        framed_block(" orchestrator ")
    };
    f.render_widget(block.clone(), area);
    let inner = block.inner(area);
    render_orchestrator_list(f, inner, state);
}

fn render_workers_pane(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = format!(" workers ({}) ", state.sessions.len());
    let block = if state.sessions_focus == SessionsFocus::Workflows {
        framed_block_accent(&title)
    } else {
        framed_block(&title)
    };
    f.render_widget(block.clone(), area);
    let inner = block.inner(area);
    if state.sessions.is_empty() {
        // Empty-state hint inside the pane. The orchestrator pane
        // above this stays as-is (the synthetic "not running" row
        // covers its own empty case), so this hint is workers-only.
        let muted = Style::default().fg(MUTED);
        let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let lines = vec![
            Line::from(Span::styled("(no workers yet)", muted)),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("n", bold_key),
                Span::styled("    open the workflow picker", muted),
            ]),
            Line::from(""),
            Line::from(Span::styled("  Or:  fleet workflow run <name>", muted)),
        ];
        let body = Paragraph::new(lines)
            .block(Block::default().padding(Padding::horizontal(2)))
            .wrap(Wrap { trim: false });
        f.render_widget(body, inner);
    } else {
        render_workflow_list(f, inner, state);
    }
}

fn render_workflow_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let items: Vec<ListItem<'_>> = state
        .sessions
        .iter()
        .map(|s| {
            ListItem::new(Span::styled(
                session_row_label(s, &state.plans, &state.cycle_nodes),
                state_style(s.state),
            ))
        })
        .collect();
    let highlight = if state.sessions_focus == SessionsFocus::Workflows {
        Style::default()
            .fg(SELECT_FG)
            .bg(SELECT_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("▸ ")
        // Reserve the cursor column on every row so the list doesn't
        // shift right when the user Tabs in and the symbol appears.
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, area, &mut state.list_state.clone());
}

fn render_orchestrator_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Always one row, regardless of whether the orchestrator is
    // running: when present we show the live state (marker + agent
    // + state word); when absent we show just the fixed tmux name
    // in muted style — the dimming itself signals "dormant", and
    // keeping the row short avoids overflowing the narrow sidebar
    // pane.
    let item = state.orchestrators.first().map_or_else(
        || {
            ListItem::new(Span::styled(
                format!("  {}", crate::orchestrator::TMUX_SESSION_NAME),
                Style::default().fg(MUTED),
            ))
        },
        |s| {
            ListItem::new(Span::styled(
                orchestrator_row_label(s),
                orchestrator_row_style(s.state),
            ))
        },
    );
    let highlight = if state.sessions_focus == SessionsFocus::Orchestrator {
        Style::default()
            .fg(SELECT_FG)
            .bg(SELECT_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(vec![item])
        .highlight_style(highlight)
        .highlight_symbol("▸ ")
        // Reserve the cursor column on every row so the list doesn't
        // shift right when the user Tabs in and the symbol appears.
        .highlight_spacing(HighlightSpacing::Always);
    // Cursor is always at row 0 (the one row). Copying the state's
    // ListState keeps any pre-existing selection in sync.
    let mut cursor = state.orchestrators_list_state;
    if cursor.selected().is_some() {
        cursor.select(Some(0));
    }
    f.render_stateful_widget(list, area, &mut cursor);
}

#[must_use]
pub(in crate::tui) fn orchestrator_row_label(session: &OrchestratorSession) -> String {
    // Compact label that fits in the narrow sidebar pane: just the
    // state marker + fixed tmux name. The marker (◐/⏸/✗) plus the
    // row's accent/warn/muted style convey active/detached/closed;
    // the agent + state word are visible in the details pane when
    // the row is selected.
    format!(
        "{} {}",
        orchestrator_state_marker(session.state),
        crate::orchestrator::TMUX_SESSION_NAME,
    )
}

#[must_use]
fn orchestrator_row_style(state: OrchestratorState) -> Style {
    match state {
        OrchestratorState::Active => Style::default().fg(ACCENT),
        OrchestratorState::Detached => Style::default().fg(WARN),
        OrchestratorState::Closed => Style::default().fg(MUTED),
    }
}

#[must_use]
fn orchestrator_state_marker(state: OrchestratorState) -> &'static str {
    match state {
        OrchestratorState::Active => "◐",
        OrchestratorState::Detached => "⏸",
        OrchestratorState::Closed => "✗",
    }
}

#[must_use]
pub(in crate::tui) fn session_row_label(
    session: &Session,
    plans: &[Plan],
    cycle_nodes: &std::collections::HashSet<String>,
) -> String {
    use std::fmt::Write as _;

    let cost_suffix = session
        .total_cost_usd()
        .map_or_else(String::new, |v| format!(" ${v:.2}"));
    let warning_prefix = if session_in_cycle(session, cycle_nodes) {
        "⚠ "
    } else {
        ""
    };
    // Lead with the meaningful subject so the row tells the user
    // what this session is *for* — a 20-char hex session id alone
    // can't be cross-referenced at a glance. Order:
    //   <marker> <subject> <workflow> [· title] [$cost] [· plan]
    // where `subject` is `#<issue>` / `pr:<n>` / a trailing
    // session-id stub when neither binding is present.
    let (subject, title) = subject_and_title(session);
    let mut label = format!(
        "{warning_prefix}{} {subject} {}{cost_suffix}",
        state_marker(session.state),
        session.workflow,
    );
    if let Some(title) = title {
        let _ = write!(label, " — {}", truncate_for_row(&title, 60));
    }
    if let Some(annotation) = plan_annotation_for(session, plans) {
        let _ = write!(label, " · {annotation}");
    }
    label
}

/// Return the meaningful identifier for a session and its title (if
/// any). PRs and issues take precedence; sessions with neither
/// fall back to a short session-id stub so consecutive issueless
/// sessions are still distinguishable.
fn subject_and_title(session: &Session) -> (String, Option<String>) {
    if let Some(pr) = session.pr.as_ref() {
        return (format!("pr:{}", pr.number), Some(pr.title.clone()));
    }
    if let Some(issue) = session.issue.as_ref() {
        return (
            format!("#{}", issue.human_id),
            Some(issue.title.clone()),
        );
    }
    // No binding — show a short id stub. Full id is in the details
    // pane; the stub here just disambiguates consecutive rows.
    let raw = session.id.as_str();
    let stub = raw
        .rsplit_once('-')
        .map_or(raw, |(_, tail)| tail);
    (format!("s-…{stub}"), None)
}

/// Trim a string at a char boundary (not a byte index) so we don't
/// slice multi-byte UTF-8 in half. Adds an ellipsis when truncation
/// happened so the user knows there's more to read in the details
/// pane.
fn truncate_for_row(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[must_use]
fn session_in_cycle(session: &Session, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    session
        .issue
        .as_ref()
        .is_some_and(|i| cycle_nodes.contains(&i.human_id))
}

#[must_use]
fn plan_annotation_for(session: &Session, plans: &[Plan]) -> Option<String> {
    let ticket = &session.issue.as_ref()?.human_id;
    let active_plan = plans
        .iter()
        .find(|p| p.state == PlanState::Active && p.position_of(ticket).is_some())?;
    let idx = active_plan.position_of(ticket)?;
    Some(format!(
        "{} ({}/{})",
        active_plan.name,
        idx + 1,
        active_plan.items.len()
    ))
}

/// What the right-hand pane is showing. Drives both the details
/// section (top) and the output preview (bottom) so the two stay in
/// sync: an orchestrator-focused right pane shows orchestrator kv +
/// tmux-pane capture; a worker-focused pane shows session kv + log
/// tail; the help-hint fallback only ever appears when there's nothing
/// to show.
enum DetailTarget<'a> {
    Worker(&'a Session),
    Orchestrator(&'a OrchestratorSession),
    OrchestratorAbsent,
    Empty,
}

fn detail_target(state: &AppState) -> DetailTarget<'_> {
    match state.sessions_focus {
        SessionsFocus::Orchestrator => state
            .orchestrators
            .first()
            .map_or(DetailTarget::OrchestratorAbsent, DetailTarget::Orchestrator),
        SessionsFocus::Workflows => state
            .selected()
            .map_or(DetailTarget::Empty, DetailTarget::Worker),
    }
}

pub(super) fn render_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let target = detail_target(state);
    // Empty case is the only one without an output preview — show the
    // help hint in a single full-height pane.
    if matches!(target, DetailTarget::Empty) {
        render_detail_empty(f, area);
        return;
    }

    // Stack details on top (kv pairs — small, fixed-ish height per
    // case) and the output preview below (grows to fill). Mirrors the
    // pre-AO/Lima layout: two distinct framed blocks rather than one
    // big paragraph, so the eye reads details / output as separate
    // sections.
    let detail_lines = detail_lines_for(&target);
    // +2 for the block's top/bottom border rows; clamp so a wildly
    // long details section can't starve the output preview.
    let details_height = u16::try_from(detail_lines.len() + 2)
        .unwrap_or(u16::MAX)
        .clamp(3, 18);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(details_height), Constraint::Min(0)])
        .split(area);
    render_details_section(f, chunks[0], &target, detail_lines);
    render_output_section(f, chunks[1], &target, state);
}

fn render_detail_empty(f: &mut Frame<'_>, area: Rect) {
    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::from(Span::styled("Select a session, or use one of:", muted)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("j/k", bold_key),
            Span::styled("     navigate", muted),
        ]),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("r", bold_key),
            Span::styled("       reload from disk", muted),
        ]),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("Shift+K", bold_key),
            Span::styled(" mark selected session failed", muted),
        ]),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("q", bold_key),
            Span::styled("       quit", muted),
        ]),
    ];
    let body = Paragraph::new(lines)
        .block(framed_block(" detail ").padding(Padding::horizontal(2)))
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn detail_lines_for(target: &DetailTarget<'_>) -> Vec<Line<'static>> {
    match target {
        DetailTarget::Worker(session) => worker_detail_lines(session),
        DetailTarget::Orchestrator(orch) => orchestrator_detail_lines(orch),
        DetailTarget::OrchestratorAbsent => orchestrator_absent_lines(),
        DetailTarget::Empty => Vec::new(),
    }
}

fn worker_detail_lines(session: &Session) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = detail_kv_pairs(session)
        .into_iter()
        .map(|(k, v)| match k {
            // Ticket / PR ids get the same cyan-bold IDENT treatment
            // the plans view uses for ticket ids, so the eye lands on
            // "what this is for" the same way across views.
            "ticket" | "pr" => kv_line_with_ident_id(k, &v),
            _ => kv_line(k, &v),
        })
        .collect();
    if !session.node_costs.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_heading("node costs:"));
        for (node, usd) in &session.node_costs {
            lines.push(Line::from(format!("  {node:<16} ${usd:.4}")));
        }
    }
    lines
}

/// kv row where `value` is `<id> <title…>`. Splits at the first space
/// so the id gets the IDENT colour + bold (matching the plans view's
/// ticket-id style) and the title reads as plain text. The label
/// stays muted like every other kv row.
fn kv_line_with_ident_id(key: &str, value: &str) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    let ident_style = Style::default().fg(IDENT).add_modifier(Modifier::BOLD);
    let (id, title) = value.split_once(' ').unwrap_or((value, ""));
    let mut spans = vec![
        Span::styled(format!("{key:<14}"), muted),
        Span::styled(id.to_string(), ident_style),
    ];
    if !title.is_empty() {
        spans.push(Span::raw(format!(" {title}")));
    }
    Line::from(spans)
}

fn orchestrator_detail_lines(orch: &OrchestratorSession) -> Vec<Line<'static>> {
    vec![
        kv_line("agent", &orch.agent),
        Line::from(vec![
            Span::styled(format!("{:<14}", "state"), Style::default().fg(MUTED)),
            Span::styled(
                orchestrator_state_word(orch.state).to_string(),
                orchestrator_state_value_style(orch.state),
            ),
        ]),
        kv_line("tmux", crate::orchestrator::TMUX_SESSION_NAME),
    ]
}

fn orchestrator_absent_lines() -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    vec![
        Line::from(Span::styled("(orchestrator not running)", muted)),
        Line::from(""),
        Line::from(vec![
            Span::styled("Press ", muted),
            Span::styled("enter", bold_key),
            Span::styled(" to spawn and attach.", muted),
        ]),
    ]
}

fn render_details_section(
    f: &mut Frame<'_>,
    area: Rect,
    target: &DetailTarget<'_>,
    lines: Vec<Line<'static>>,
) {
    let title = details_title(target);
    let body = Paragraph::new(lines)
        .block(framed_block_titled(title).padding(Padding::horizontal(2)))
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn details_title(target: &DetailTarget<'_>) -> Line<'static> {
    let pad = Span::raw(" ");
    match target {
        DetailTarget::Worker(s) => Line::from(vec![
            pad.clone(),
            Span::styled(
                s.id.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                state_word(s.state),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
            pad,
        ]),
        DetailTarget::Orchestrator(o) => Line::from(vec![
            pad.clone(),
            Span::styled(
                crate::orchestrator::TMUX_SESSION_NAME.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                orchestrator_state_word(o.state),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
            pad,
        ]),
        DetailTarget::OrchestratorAbsent => Line::from(vec![
            pad.clone(),
            Span::styled(
                crate::orchestrator::TMUX_SESSION_NAME.to_string(),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "not running",
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
            pad,
        ]),
        DetailTarget::Empty => Line::from(Span::raw(" detail ")),
    }
}

fn render_output_section(
    f: &mut Frame<'_>,
    area: Rect,
    target: &DetailTarget<'_>,
    state: &AppState,
) {
    let block = framed_block(" output ").padding(Padding::horizontal(2));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Publish the inner dims so the refresh thread can pin the
    // focused tmux window to the panel width. Packed as
    // `(w << 16) | h`. Zero stays zero until we actually have a non-
    // empty area — keeps the first capture from running before we
    // know how wide the panel is.
    let packed = (u32::from(inner.width) << 16) | u32::from(inner.height);
    state
        .refresh_inputs
        .pane_size
        .store(packed, std::sync::atomic::Ordering::Relaxed);

    match target {
        DetailTarget::Worker(_) => render_worker_output(f, inner, state),
        DetailTarget::Orchestrator(_) => render_orchestrator_output(f, inner, state),
        DetailTarget::OrchestratorAbsent => render_output_placeholder(
            f,
            inner,
            "(no output — orchestrator not spawned yet; press enter on the row)",
        ),
        DetailTarget::Empty => {}
    }
}

fn render_worker_output(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    // Three-tier fallback. From freshest to most-stale:
    //   1. `worker_panes` — live `tmux capture-pane` snapshot of a
    //      `--detached` worker's pane (refresh thread, ~1.5 s).
    //   2. `transcript_tail` — `tmux pipe-pane` recording of the same
    //      pane on disk. Survives the tmux session dying between two
    //      capture ticks, and is also what we have *before* the first
    //      tick lands. ANSI-rich; same renderer as tier 1.
    //   3. `log_tail` — per-node `*.log`. For agent nodes this is just
    //      a "ran in tmux pane" placeholder written *after* the agent
    //      exits, so it's only useful for bash / foreground-exec
    //      nodes; kept as the last non-placeholder tier.
    // Then the muted "(no logs yet)" placeholder.
    if let Some(session) = state.selected() {
        if let Some(body) = state
            .worker_panes
            .get(&session.id)
            .filter(|s| !s.is_empty())
        {
            render_ansi_pane(f, area, body);
            return;
        }
        if let Some(body) = state.transcript_tail.as_deref().filter(|s| !s.is_empty()) {
            render_ansi_pane(f, area, body);
            return;
        }
    }
    if state.log_tail.is_empty() {
        render_output_placeholder(f, area, "(no logs yet)");
        return;
    }
    let lines: Vec<Line<'_>> = state
        .log_tail
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_orchestrator_output(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(body) = state.orchestrator_pane.as_deref().filter(|s| !s.is_empty()) else {
        render_output_placeholder(
            f,
            area,
            "(no output captured yet — first refresh after spawn takes ~1.5s)",
        );
        return;
    };
    render_ansi_pane(f, area, body);
}

/// Render `body` (raw bytes from `tmux capture-pane -e`) as styled
/// spans in `area`, tailed to fit. Falls back to plain text on ANSI
/// parse failure so a malformed capture never blanks the pane.
fn render_ansi_pane(f: &mut Frame<'_>, area: Rect, body: &str) {
    let text: ratatui::text::Text<'_> = ansi_to_tui::IntoText::into_text(&body)
        .unwrap_or_else(|_| ratatui::text::Text::raw(body.to_string()));
    let line_count = text.lines.len();
    let take = area.height as usize;
    let tail: Vec<Line<'_>> = text
        .lines
        .into_iter()
        .skip(line_count.saturating_sub(take))
        .collect();
    f.render_widget(Paragraph::new(tail).wrap(Wrap { trim: false }), area);
}

fn render_output_placeholder(f: &mut Frame<'_>, area: Rect, text: &str) {
    let muted = Style::default().fg(MUTED).add_modifier(Modifier::ITALIC);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(text.to_string(), muted)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[must_use]
pub(in crate::tui) fn orchestrator_state_word(state: OrchestratorState) -> &'static str {
    match state {
        OrchestratorState::Active => "active",
        OrchestratorState::Detached => "detached",
        OrchestratorState::Closed => "closed",
    }
}

#[must_use]
fn orchestrator_state_value_style(state: OrchestratorState) -> Style {
    let base = match state {
        OrchestratorState::Active => Style::default().fg(OK),
        OrchestratorState::Detached => Style::default().fg(WARN),
        OrchestratorState::Closed => Style::default().fg(MUTED),
    };
    base.add_modifier(Modifier::BOLD)
}
