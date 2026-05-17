//! Rendering — pure `(&AppState, &mut Frame) -> ()` plumbing.
//!
//! No state mutation here; the input layer ([`super::input`]) drives
//! all changes via [`super::app::AppState`]. Display-side helpers
//! (state markers/words, label formatting, deps rendering) also live
//! here because they only exist to produce frames.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::brainstorm::{BrainstormSession, BrainstormState};
use crate::plans::{Plan, PlanItemState, PlanState};
use crate::session::{Session, SessionState};

use super::app::{AppState, Overlay, PlansFocus, SessionsFocus, View};

/// Full-frame render entry point. Called once per event-loop tick from
/// [`super::terminal::run`].
pub(super) fn render(f: &mut Frame<'_>, state: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());
    match state.view {
        View::Sessions | View::Spawn => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(outer[0]);
            render_sidebar(f, body[0], state);
            render_detail(f, body[1], state);
        }
        View::Doctor => {
            render_doctor(f, outer[0], state);
        }
        View::Plans => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(40),
                    Constraint::Percentage(35),
                ])
                .split(outer[0]);
            render_plans_sidebar(f, body[0], state);
            render_plans_detail(f, body[1], state);
            render_plans_ticket(f, body[2], state);
        }
    }
    if state.view == View::Spawn {
        let modal = centered_rect(outer[0], 60, 60);
        f.render_widget(Clear, modal);
        render_spawn(f, modal, state);
    }
    match &state.overlay {
        Overlay::Confirm { prompt, .. } => {
            let modal = centered_rect(outer[0], 50, 30);
            f.render_widget(Clear, modal);
            render_confirm(f, modal, prompt);
        }
        Overlay::Error { message } => {
            let modal = centered_rect(outer[0], 50, 25);
            f.render_widget(Clear, modal);
            render_error(f, modal, message);
        }
        Overlay::None => {}
    }
    render_status(f, outer[1], state);
}

fn render_confirm(f: &mut Frame<'_>, area: Rect, prompt: &str) {
    let mut lines: Vec<Line<'static>> = prompt.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y] yes    [n / Esc] cancel",
        Style::default().fg(Color::Yellow),
    )));
    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_error(f: &mut Frame<'_>, area: Rect, message: &str) {
    let mut lines: Vec<Line<'static>> =
        message.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key to dismiss",
        Style::default().fg(Color::DarkGray),
    )));
    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Error ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
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

fn render_doctor(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().title(" Doctor ").borders(Borders::ALL);
    let Some(snapshot) = state.doctor.as_ref() else {
        let body = Paragraph::new("(probing host...)").block(block);
        f.render_widget(body, area);
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let repo_value = format!(
        "{}  ({})",
        snapshot.root.display(),
        if snapshot.initialised {
            "initialised"
        } else {
            "not initialised — run `fleet init`"
        },
    );
    lines.push(kv_line("repo", &repo_value));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "runtime adapter:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(kv_line(
        "configured",
        &format!(
            "{}  (hardening: {})",
            snapshot.configured_adapter, snapshot.configured_hardening,
        ),
    ));
    match &snapshot.adapter {
        Ok((name, caps)) => {
            lines.push(kv_line("resolved", &format!("{name}  {}", caps.describe())));
        }
        Err(msg) => {
            lines.push(Line::from(vec![
                Span::styled("resolved  ", Style::default().fg(Color::DarkGray)),
                Span::styled(msg.clone(), Style::default().fg(Color::Red)),
            ]));
        }
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "agents:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if snapshot.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, env) in &snapshot.agents {
            let env_str = if env.is_empty() {
                String::new()
            } else {
                format!("  (env: {})", env.join(", "))
            };
            lines.push(Line::from(format!("  - {name}{env_str}")));
        }
    }
    lines.push(Line::from(""));

    lines.push(kv_line("tracker", &snapshot.tracker));

    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_spawn(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default()
        .title(" Spawn workflow ")
        .borders(Borders::ALL);
    if state.spawn_workflows.is_empty() {
        let body = Paragraph::new(
            "(no workflows found under .fleet/workflows/)\n\n\
             Run `fleet init` to scaffold the default standard / hotfix / review-only\n\
             workflows, or drop a `<name>.yaml` into `.fleet/workflows/` by hand.",
        )
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem<'_>> = state
        .spawn_workflows
        .iter()
        .map(|name| ListItem::new(Line::from(name.clone())))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = state.spawn_list_state.clone();
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_plans_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = format!(" Plans ({}) ", state.plans.len());
    let block = Block::default().title(title).borders(Borders::ALL);
    if state.plans.is_empty() {
        let body = Paragraph::new(
            "(no plans yet)\n\nUse `fleet plan new \"<name>\" --tickets a,b,c` to create one.",
        )
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem<'_>> = state
        .plans
        .iter()
        .map(|p| {
            ListItem::new(Span::styled(
                plan_row_label(p, &state.cycle_nodes),
                plan_row_style(p),
            ))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = state.plans_list_state.clone();
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_plans_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(plan) = state.selected_plan() else {
        let body = Paragraph::new("Select a plan — j/k or arrows. r refresh, Esc/p back, q quit.")
            .block(
                Block::default()
                    .title(" Plan detail ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    };
    let selected_item = if state.plans_focus == PlansFocus::Items {
        state.plans_items_state.selected()
    } else {
        None
    };
    let title = if state.plans_focus == PlansFocus::Items {
        format!(" {} · items ", plan.id)
    } else {
        format!(" {} ", plan.id)
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let body = Paragraph::new(plan_detail_lines(plan, selected_item, &state.tickets_by_id))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_plans_ticket(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().title(" Ticket ").borders(Borders::ALL);
    let lines = ticket_detail_lines(state);
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

/// Pure: build the lines for the ticket-detail pane against the current
/// `state`. Five cases covered: sidebar focus, empty plan, no cache
/// entry, cached error, cached `IssueDetail`. Tests assert on the
/// rendered surface without a ratatui Frame.
#[must_use]
pub(super) fn ticket_detail_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.plans_focus == PlansFocus::Sidebar {
        return vec![Line::from(Span::styled(
            "Tab to focus items, then j/k to navigate. Selected item's tracker issue will appear here.",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let Some(plan) = state.selected_plan() else {
        return vec![Line::from(Span::styled(
            "(no plan selected)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let Some(item_idx) = state.plans_items_state.selected() else {
        return vec![Line::from(Span::styled(
            "(plan has no items)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let Some(item) = plan.items.get(item_idx) else {
        return vec![Line::from(Span::styled(
            "(item index out of range)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let ticket_id = &item.ticket_id;
    let mut lines = match state.focused_issue_cache.get(ticket_id) {
        None => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled(
                "(fetching…)",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        Some(Err(msg)) => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled(
                format!("tracker read failed: {msg}"),
                Style::default().fg(Color::Red),
            )),
        ],
        Some(Ok(detail)) => issue_detail_lines(detail),
    };
    append_deps_lines(
        &mut lines,
        ticket_id,
        &state.deps_doc,
        &state.cycle_nodes,
        &state.tickets_by_id,
    );
    lines
}

/// Append "Blocked on:" / "Blocks:" / cycle-warning rows for `ticket_id`.
pub(super) fn append_deps_lines(
    lines: &mut Vec<Line<'static>>,
    ticket_id: &str,
    deps: &crate::deps::DepsDoc,
    cycle_nodes: &std::collections::HashSet<String>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) {
    let blocked_on: Vec<&crate::deps::DepEdge> = deps
        .edges
        .iter()
        .filter(|e| e.blocked == ticket_id)
        .collect();
    let blocks: Vec<&crate::deps::DepEdge> = deps
        .edges
        .iter()
        .filter(|e| e.blocked_on == ticket_id)
        .collect();
    let in_cycle = cycle_nodes.contains(ticket_id);

    if blocked_on.is_empty() && blocks.is_empty() && !in_cycle {
        return;
    }

    lines.push(Line::from(""));
    if !blocked_on.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Blocked on ({}):", blocked_on.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for edge in &blocked_on {
            let kind = match edge.reason {
                crate::deps::BlockedReason::Ticket => "ticket",
                crate::deps::BlockedReason::Freeform => "freeform",
            };
            let title_suffix = format_title_suffix(&edge.blocked_on, edge.reason, titles);
            lines.push(Line::from(format!(
                "  • {}{title_suffix}  [{kind}]",
                edge.blocked_on
            )));
        }
    }
    if !blocks.is_empty() {
        if !blocked_on.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Blocks ({}):", blocks.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for edge in &blocks {
            let title_suffix = format_title_suffix(
                &edge.blocked,
                crate::deps::BlockedReason::Ticket,
                titles,
            );
            lines.push(Line::from(format!("  • {}{title_suffix}", edge.blocked)));
        }
    }
    if in_cycle {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "⚠  Part of a deps cycle — supervisor can't auto-resolve.",
            Style::default().fg(Color::Yellow),
        )));
    }
}

#[must_use]
fn format_title_suffix(
    id: &str,
    reason: crate::deps::BlockedReason,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> String {
    if matches!(reason, crate::deps::BlockedReason::Freeform) {
        return String::new();
    }
    titles
        .get(id)
        .map(|i| format!(" — {}", i.title))
        .unwrap_or_default()
}

#[must_use]
pub(super) fn issue_detail_lines(detail: &crate::tracker::IssueDetail) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv_line("id", &detail.issue.human_id),
        kv_line("title", &detail.issue.title),
        kv_line("status", &detail.issue.status),
    ];
    if !detail.issue.labels.is_empty() {
        lines.push(kv_line(
            "labels",
            &format!("[{}]", detail.issue.labels.join(", ")),
        ));
    }
    let comment_count = detail.comments.len();
    lines.push(kv_line("comments", &comment_count.to_string()));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Body:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if detail.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no body)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for line in detail.body.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

#[must_use]
pub(super) fn plan_row_label(plan: &Plan, cycle_nodes: &std::collections::HashSet<String>) -> String {
    let (done, total) = plan.progress();
    let warning_prefix = if plan_in_cycle(plan, cycle_nodes) {
        "⚠ "
    } else {
        ""
    };
    format!(
        "{warning_prefix}{} {} {done}/{total} {}",
        plan_state_marker(plan.state),
        plan.id,
        plan.name,
    )
}

#[must_use]
fn plan_row_style(plan: &Plan) -> Style {
    match plan.state {
        PlanState::Active => Style::default().fg(Color::Green),
        PlanState::Paused => Style::default().fg(Color::Yellow),
        PlanState::Completed | PlanState::Abandoned => Style::default().fg(Color::DarkGray),
    }
}

#[must_use]
fn plan_state_marker(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "●",
        PlanState::Paused => "⏸",
        PlanState::Completed => "✓",
        PlanState::Abandoned => "✗",
    }
}

#[must_use]
pub(super) fn plan_detail_lines(
    plan: &Plan,
    selected_item: Option<usize>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let (done, total) = plan.progress();
    let mut out = vec![
        kv_line("name", &plan.name),
        kv_line("state", plan_state_word(plan.state)),
        kv_line("progress", &format!("{done}/{total}")),
        kv_line("policy", plan_failure_word(plan.on_item_failure)),
    ];
    if let Some(epic) = &plan.epic_ref {
        out.push(kv_line("epic", &format!("{}:{}", epic.tracker, epic.id)));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        format!("Items ({total})"),
        Style::default().fg(Color::DarkGray),
    )));
    use std::fmt::Write as _;
    for (idx, item) in plan.items.iter().enumerate() {
        let cursor = if selected_item == Some(idx) {
            "▸ "
        } else {
            "  "
        };
        let mut row = format!(
            "{cursor}{idx:>3}. {marker} {state:<11} {ticket}",
            marker = plan_item_marker(item.state),
            state = plan_item_word(item.state),
            ticket = item.ticket_id,
        );
        if let Some(title) = titles.get(&item.ticket_id).map(|i| i.title.as_str()) {
            let _ = write!(row, " — {title}");
        }
        if item.injected {
            row.push_str("  (injected)");
        }
        if let Some(session) = &item.session_id {
            let _ = write!(row, "  · session {session}");
        }
        out.push(Line::from(Span::styled(row, plan_item_style(item.state))));
    }
    out
}

#[must_use]
pub(super) fn plan_state_word(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "active",
        PlanState::Paused => "paused",
        PlanState::Completed => "completed",
        PlanState::Abandoned => "abandoned",
    }
}

#[must_use]
pub(super) fn plan_item_word(state: PlanItemState) -> &'static str {
    match state {
        PlanItemState::Pending => "pending",
        PlanItemState::InProgress => "in-progress",
        PlanItemState::Completed => "completed",
        PlanItemState::Skipped => "skipped",
        PlanItemState::Failed => "failed",
    }
}

#[must_use]
fn plan_item_marker(state: PlanItemState) -> &'static str {
    match state {
        PlanItemState::Pending => "○",
        PlanItemState::InProgress => "◐",
        PlanItemState::Completed => "✓",
        PlanItemState::Skipped => "⊝",
        PlanItemState::Failed => "✗",
    }
}

#[must_use]
fn plan_item_style(state: PlanItemState) -> Style {
    match state {
        PlanItemState::Pending => Style::default(),
        PlanItemState::InProgress => Style::default().fg(Color::Cyan),
        PlanItemState::Completed | PlanItemState::Skipped => Style::default().fg(Color::DarkGray),
        PlanItemState::Failed => Style::default().fg(Color::Red),
    }
}

#[must_use]
pub(super) fn plan_failure_word(policy: crate::plans::ItemFailurePolicy) -> &'static str {
    match policy {
        crate::plans::ItemFailurePolicy::Stop => "stop",
        crate::plans::ItemFailurePolicy::Continue => "continue",
        crate::plans::ItemFailurePolicy::RetryOnce => "retry-once",
    }
}

fn render_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = if state.brainstorms.is_empty() {
        format!(" Sessions ({}) ", state.sessions.len())
    } else {
        format!(
            " Sessions ({}) · Brainstorms ({}) ",
            state.sessions.len(),
            state.brainstorms.len(),
        )
    };
    let outer = Block::default().title(title).borders(Borders::ALL);
    if state.sessions.is_empty() && state.brainstorms.is_empty() {
        let body = Paragraph::new(
            "(no sessions)\n\n\
             Use `fleet workflow run <name>` from the shell to create a workflow session, \
             or `fleet brainstorm` for an interactive planning session.",
        )
        .block(outer)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    f.render_widget(outer.clone(), area);
    let inner = outer.inner(area);
    let (workflow_area, brainstorm_area) =
        split_sidebar_areas(inner, state.sessions.len(), state.brainstorms.len());
    if let Some(area) = workflow_area {
        render_workflow_list(f, area, state);
    }
    if let Some(area) = brainstorm_area {
        render_brainstorm_list(f, area, state);
    }
}

#[must_use]
fn split_sidebar_areas(
    inner: Rect,
    n_workflows: usize,
    n_brainstorms: usize,
) -> (Option<Rect>, Option<Rect>) {
    if n_workflows == 0 && n_brainstorms == 0 {
        return (None, None);
    }
    if n_brainstorms == 0 {
        return (Some(inner), None);
    }
    if n_workflows == 0 {
        return (None, Some(inner));
    }
    let workflow_share = u32::try_from(n_workflows + 1).unwrap_or(u32::MAX);
    let brainstorm_share = u32::try_from(n_brainstorms + 1).unwrap_or(u32::MAX);
    let total = workflow_share.saturating_add(brainstorm_share);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Ratio(workflow_share, total),
            Constraint::Ratio(brainstorm_share, total),
        ])
        .split(inner);
    (Some(chunks[0]), Some(chunks[1]))
}

fn render_workflow_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut items: Vec<ListItem<'_>> = Vec::with_capacity(state.sessions.len() + 1);
    items.push(ListItem::new(Span::styled(
        "── Workflows ──".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    for s in &state.sessions {
        items.push(ListItem::new(Span::styled(
            session_row_label(s, &state.plans, &state.cycle_nodes),
            state_style(s.state),
        )));
    }
    let highlight = if state.sessions_focus == SessionsFocus::Workflows {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("> ");
    let mut shifted = state.list_state.clone();
    if let Some(i) = shifted.selected() {
        shifted.select(Some(i + 1));
    }
    f.render_stateful_widget(list, area, &mut shifted);
}

fn render_brainstorm_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut items: Vec<ListItem<'_>> = Vec::with_capacity(state.brainstorms.len() + 1);
    items.push(ListItem::new(Span::styled(
        "── Brainstorms ──".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    for b in &state.brainstorms {
        items.push(ListItem::new(Span::styled(
            brainstorm_row_label(b),
            brainstorm_row_style(b.state),
        )));
    }
    let highlight = if state.sessions_focus == SessionsFocus::Brainstorms {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("> ");
    let mut shifted = state.brainstorms_list_state.clone();
    if let Some(i) = shifted.selected() {
        shifted.select(Some(i + 1));
    }
    f.render_stateful_widget(list, area, &mut shifted);
}

#[must_use]
pub fn brainstorm_row_label(session: &BrainstormSession) -> String {
    format!(
        "{} {}  agent={}  ({})",
        brainstorm_state_marker(session.state),
        session.id,
        session.agent,
        brainstorm_state_word(session.state),
    )
}

#[must_use]
fn brainstorm_row_style(state: BrainstormState) -> Style {
    match state {
        BrainstormState::Active => Style::default().fg(Color::Cyan),
        BrainstormState::Detached => Style::default().fg(Color::Yellow),
        BrainstormState::Closed => Style::default().fg(Color::DarkGray),
    }
}

#[must_use]
fn brainstorm_state_marker(state: BrainstormState) -> &'static str {
    match state {
        BrainstormState::Active => "◐",
        BrainstormState::Detached => "⏸",
        BrainstormState::Closed => "✗",
    }
}

#[must_use]
fn brainstorm_state_word(state: BrainstormState) -> &'static str {
    match state {
        BrainstormState::Active => "active",
        BrainstormState::Detached => "detached",
        BrainstormState::Closed => "closed",
    }
}

#[must_use]
pub fn session_row_label(
    session: &Session,
    plans: &[Plan],
    cycle_nodes: &std::collections::HashSet<String>,
) -> String {
    let cost_suffix = session
        .total_cost_usd()
        .map_or_else(String::new, |v| format!(" ${v:.2}"));
    let warning_prefix = if session_in_cycle(session, cycle_nodes) {
        "⚠ "
    } else {
        ""
    };
    let mut label = format!(
        "{warning_prefix}{} {} {}{cost_suffix}",
        state_marker(session.state),
        session.id,
        session.workflow,
    );
    if let Some(annotation) = plan_annotation_for(session, plans) {
        use std::fmt::Write as _;
        let _ = write!(label, " · {annotation}");
    }
    label
}

#[must_use]
fn session_in_cycle(session: &Session, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    session
        .issue
        .as_ref()
        .is_some_and(|i| cycle_nodes.contains(&i.human_id))
}

#[must_use]
fn plan_in_cycle(plan: &Plan, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    plan.items
        .iter()
        .any(|item| cycle_nodes.contains(&item.ticket_id))
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

fn render_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(session) = state.selected() else {
        let body = Paragraph::new(
            "Select a session — j/k or arrows. r refresh, Shift+K mark failed, q quit.",
        )
        .block(Block::default().title(" Detail ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    };

    let mut lines: Vec<Line<'static>> = detail_kv_pairs(session)
        .into_iter()
        .map(|(k, v)| kv_line(k, &v))
        .collect();
    if !session.node_costs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "node costs:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (node, usd) in &session.node_costs {
            lines.push(Line::from(format!("  {node:<16} ${usd:.4}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "log tail:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if state.log_tail.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no logs yet)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for log_line in &state.log_tail {
            lines.push(Line::from(format!("  {log_line}")));
        }
    }
    let title = format!(" {} ", session.id);
    let body = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_status(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let help = match state.view {
        View::Sessions => {
            "[q] quit  [Tab] focus  [j/k] nav  [Enter] attach  [r] reload  [d] doctor  [p] plans  [Shift+K] kill  [n] spawn  [Shift+A] auto  [Shift+B] brainstorm"
        }
        View::Doctor => "[q] quit  [Esc/d] back  [r] re-probe",
        View::Spawn => "[Esc/q] cancel  [j/k] nav  [Enter] spawn",
        View::Plans => {
            "[q] quit  [Esc/p] back  [Tab] focus  [j/k] nav  [Shift+P] pause/resume  [Shift+C] complete  [u] unblock  [r] reload"
        }
    };
    let tail = if state.autonomous.enabled() || state.autonomous.status() != "autonomous: OFF" {
        state.autonomous.status().to_string()
    } else {
        state.status_line.clone()
    };
    let bar = format!("{help} — {tail}");
    let p = Paragraph::new(bar).style(Style::default().bg(Color::Black).fg(Color::Gray));
    f.render_widget(p, area);
}

fn kv_line(key: &str, value: &str) -> Line<'static> {
    let key_owned = format!("{key:<9} ");
    Line::from(vec![
        Span::styled(key_owned, Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
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
    pairs.push(("updated", format!("{} ms (epoch)", session.updated_at_ms)));
    pairs.push(("cost", format_cost(session.total_cost_usd())));
    pairs
}

/// Short word for a state.
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

fn state_style(s: SessionState) -> Style {
    match s {
        SessionState::Completed => Style::default().fg(Color::Green),
        SessionState::Failed | SessionState::Crashed => Style::default().fg(Color::Red),
        SessionState::Running => Style::default().fg(Color::Yellow),
        SessionState::AwaitingGate => Style::default().fg(Color::Cyan),
        SessionState::Created => Style::default(),
    }
}

/// Render a USD cost as `$0.42` (two decimals) or `-` when absent.
#[must_use]
pub fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "-".to_string(), |v| format!("${v:.2}"))
}

/// Walk every session and sum the ones with cost data. Returns
/// `(total, samples)`.
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

/// Render the status-bar tail used by the sidebar when autonomous
/// mode isn't taking over the line.
#[must_use]
pub fn render_status_line(sessions: &[Session], plans: &[Plan]) -> String {
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
