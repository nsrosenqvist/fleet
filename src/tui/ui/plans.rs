//! Plans view — three vertical columns: sidebar list, plan-detail
//! pane (header + items), and ticket-detail pane (issue body + deps).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap};

use crate::plans::{Plan, PlanItemState, PlanState};
use crate::tui::app::{AppState, PlansFocus};
use crate::tui::theme::{
    ACCENT, ERR, IDENT, MUTED, OK, SELECT_BG, SELECT_FG, WARN, framed_block, framed_block_accent,
    framed_block_accent_titled, framed_block_titled, kv_line, label_chip,
};

use super::section_heading;

pub(super) fn render_plans_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = format!(" plans ({}) ", state.plans.len());
    let block = if state.plans_focus == PlansFocus::Sidebar {
        framed_block_accent(&title)
    } else {
        framed_block(&title)
    };
    if state.plans.is_empty() {
        let muted = Style::default().fg(MUTED);
        let lines = vec![
            Line::from(Span::styled("(no plans yet)", muted)),
            Line::from(""),
            Line::from(Span::styled(
                "Use `fleet plan new \"<name>\" --tickets a,b,c`",
                muted,
            )),
            Line::from(Span::styled("to create one.", muted)),
        ];
        let body = Paragraph::new(lines)
            .block(block.padding(Padding::horizontal(2)))
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
                .fg(SELECT_FG)
                .bg(SELECT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ")
        // Reserve the cursor column on every row so the list doesn't
        // shift right when the user Tabs in and the symbol appears.
        .highlight_spacing(HighlightSpacing::Always);
    let mut list_state = state.plans_list_state;
    f.render_stateful_widget(list, area, &mut list_state);
}

pub(super) fn render_plans_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(plan) = state.selected_plan() else {
        let body = Paragraph::new(Line::from(Span::styled(
            "Select a plan — j/k or arrows. r refresh, Esc/p back, q quit.",
            Style::default().fg(MUTED),
        )))
        .block(framed_block(" plan detail ").padding(Padding::horizontal(2)))
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    };
    let selected_item = if state.plans_focus == PlansFocus::Items {
        state.plans_items_state.selected()
    } else {
        None
    };
    // Mirror keel's info-panel header: bold-accent name + italic-dim
    // kind tag.
    let kind = if state.plans_focus == PlansFocus::Items {
        "items"
    } else {
        "plan"
    };
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            plan.id.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            kind,
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ),
        Span::raw(" "),
    ]);
    let block = if state.plans_focus == PlansFocus::Items {
        framed_block_accent_titled(title)
    } else {
        framed_block_titled(title)
    }
    .padding(Padding::horizontal(2));

    // The pane is laid out as: framed_block chrome on the outside,
    // header rows (kv + "Items (N)") rendered as a Paragraph on top,
    // items rendered as a List on the bottom. Using a List for the
    // items section lets the highlight_style paint the *full* row
    // width when an item is selected — the look the user expects from
    // the sessions sidebar. A pure Paragraph render only colours the
    // characters that exist on the row, which would make the
    // highlight look like a half-filled smear.
    let header = plan_header_lines(plan, &state.tickets_by_id);
    let header_height = u16::try_from(header.len()).unwrap_or(u16::MAX);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(0)])
        .split(inner);

    f.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), rows[0]);

    // Each item renders as 1 or 2 lines (title row + optional session
    // row when a session is bound). `ListItem::new` takes a
    // `Vec<Line>` which becomes a multi-line item; the highlight bar
    // spans the whole item which is what we want — selection covers
    // both lines.
    let item_lines: Vec<ListItem<'_>> = plan
        .items
        .iter()
        .enumerate()
        .map(|(idx, item)| ListItem::new(plan_item_line(idx, item, false, &state.tickets_by_id)))
        .collect();
    let highlight = Style::default()
        .fg(SELECT_FG)
        .bg(SELECT_BG)
        .add_modifier(Modifier::BOLD);
    let list = List::new(item_lines)
        .highlight_style(highlight)
        .highlight_symbol("▸ ")
        // Reserve the cursor column on every row so the list doesn't
        // shift right when the user Tabs in and the symbol appears.
        .highlight_spacing(HighlightSpacing::Always);
    let mut item_cursor = state.plans_items_state;
    if state.plans_focus != PlansFocus::Items {
        // Show no highlight when the user has Tab'd back to the
        // sidebar — the visual "this row is selected" cue belongs to
        // whichever pane has focus.
        item_cursor.select(None);
    }
    f.render_stateful_widget(list, rows[1], &mut item_cursor);
    let _ = selected_item; // kept for the test-facing plan_detail_lines combiner
}

pub(super) fn render_plans_ticket(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = framed_block(" ticket ").padding(Padding::horizontal(2));
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
pub(in crate::tui) fn ticket_detail_lines(state: &AppState) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    if state.plans_focus == PlansFocus::Sidebar {
        return vec![Line::from(Span::styled(
            "Tab to focus items, then j/k to navigate. Selected item's tracker issue will appear here.",
            muted,
        ))];
    }
    let Some(plan) = state.selected_plan() else {
        return vec![Line::from(Span::styled("(no plan selected)", muted))];
    };
    let Some(item_idx) = state.plans_items_state.selected() else {
        return vec![Line::from(Span::styled("(plan has no items)", muted))];
    };
    let Some(item) = plan.items.get(item_idx) else {
        return vec![Line::from(Span::styled("(item index out of range)", muted))];
    };
    let ticket_id = &item.ticket_id;
    let mut lines = match state.focused_issue_cache.get(ticket_id) {
        None => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled("(fetching…)", muted)),
        ],
        Some(Err(msg)) => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled(
                format!("tracker read failed: {msg}"),
                Style::default().fg(ERR),
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
pub(in crate::tui) fn append_deps_lines(
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
        lines.push(section_heading(format!(
            "Blocked on ({}):",
            blocked_on.len()
        )));
        for edge in &blocked_on {
            lines.push(dep_row(&edge.blocked_on, edge.reason, titles));
        }
    }
    if !blocks.is_empty() {
        if !blocked_on.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(section_heading(format!("Blocks ({}):", blocks.len())));
        for edge in &blocks {
            // Left-hand side is always a ticket id (only the right
            // side carries `free:` prefixes for freeform edges), so
            // pass Ticket reason unconditionally.
            lines.push(dep_row(
                &edge.blocked,
                crate::deps::BlockedReason::Ticket,
                titles,
            ));
        }
    }
    if in_cycle {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "⚠  Part of a deps cycle — supervisor can't auto-resolve.",
            Style::default().fg(WARN),
        )));
    }
}

/// One row of the "Blocked on" / "Blocks" list. Composed of styled
/// spans so the ticket id picks up the IDENT colour used in the items
/// list and the issue-detail header — the user reads the same id the
/// same way wherever it appears. Freeform edges use the muted palette
/// since their `free:<slug>` payload isn't a real ticket reference.
fn dep_row(
    id: &str,
    reason: crate::deps::BlockedReason,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    let is_freeform = matches!(reason, crate::deps::BlockedReason::Freeform);
    let id_style = if is_freeform {
        muted.add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(IDENT).add_modifier(Modifier::BOLD)
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("  • ", muted),
        Span::styled(id.to_string(), id_style),
    ];
    if !is_freeform {
        if let Some(title) = titles.get(id).map(|i| i.title.clone()) {
            spans.push(Span::styled(" — ", muted));
            spans.push(Span::raw(title));
        }
    }
    // Trailing `[ticket]` / `[freeform]` tag in muted so it reads as
    // an annotation, not part of the row's primary content.
    let kind = if is_freeform { "freeform" } else { "ticket" };
    spans.push(Span::styled(format!("  [{kind}]"), muted));
    Line::from(spans)
}

#[must_use]
pub(in crate::tui) fn issue_detail_lines(
    detail: &crate::tracker::IssueDetail,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let label = |s: &str| Span::styled(format!("{s:<14}"), muted);
    let mut lines: Vec<Line<'static>> = vec![
        // id: cyan + bold so it matches the plan-items column where
        // the user just navigated from.
        Line::from(vec![
            label("id"),
            Span::styled(
                detail.issue.human_id.clone(),
                Style::default().fg(IDENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        kv_line("title", &detail.issue.title),
        // status word coloured by tracker convention: open = OK,
        // closed = MUTED, anything else = WARN. Bold so it lifts off
        // the row.
        Line::from(vec![
            label("status"),
            Span::styled(
                detail.issue.status.clone(),
                issue_status_value_style(&detail.issue.status),
            ),
        ]),
    ];
    if !detail.issue.labels.is_empty() {
        let mut row: Vec<Span<'static>> = vec![label("labels")];
        for (i, l) in detail.issue.labels.iter().enumerate() {
            if i > 0 {
                row.push(Span::raw(" "));
            }
            row.push(label_chip(l));
        }
        lines.push(Line::from(row));
    }
    let comment_count = detail.comments.len();
    let comment_style = if comment_count > 0 {
        Style::default().fg(OK).add_modifier(Modifier::BOLD)
    } else {
        muted
    };
    lines.push(Line::from(vec![
        label("comments"),
        Span::styled(comment_count.to_string(), comment_style),
    ]));
    lines.push(Line::from(""));
    lines.push(section_heading("Body:"));
    if detail.body.trim().is_empty() {
        lines.push(Line::from(Span::styled("  (no body)", muted)));
    } else {
        for line in detail.body.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

#[must_use]
pub(in crate::tui) fn plan_row_label(
    plan: &Plan,
    cycle_nodes: &std::collections::HashSet<String>,
) -> String {
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
        PlanState::Active => Style::default().fg(OK),
        PlanState::Paused => Style::default().fg(WARN),
        PlanState::Completed | PlanState::Abandoned => Style::default().fg(MUTED),
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

/// Header-only portion of the plan-detail pane: kv rows + the
/// "Items (N)" section heading + a trailing blank row that the
/// renderer leaves *out* of the items list. Split from
/// [`plan_detail_lines`] so the production renderer can lay this
/// section out as a Paragraph and the items below it as a List
/// (which can paint a full-row selection highlight).
#[must_use]
fn plan_header_lines(
    plan: &Plan,
    _titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let (done, total) = plan.progress();
    let mut out: Vec<Line<'static>> = vec![
        kv_line("name", &plan.name),
        Line::from(vec![
            Span::styled(format!("{:<14}", "state"), muted),
            Span::styled(
                plan_state_word(plan.state).to_string(),
                plan_state_value_style(plan.state),
            ),
        ]),
        progress_kv_line(done, total),
        Line::from(vec![
            Span::styled(format!("{:<14}", "policy"), muted),
            Span::styled(
                plan_failure_word(plan.on_item_failure).to_string(),
                plan_policy_value_style(plan.on_item_failure),
            ),
        ]),
    ];
    if let Some(epic) = &plan.epic_ref {
        out.push(kv_line("epic", &format!("{}:{}", epic.tracker, epic.id)));
    }
    out.push(Line::from(""));
    out.push(section_heading(format!("Items ({total})")));
    out
}

/// Combined header + items as a flat `Vec<Line>` — the shape the
/// existing tests assert on. Production rendering takes a different
/// path (see `render_plans_detail`) so the items can use a List widget
/// with a full-row selection highlight, but the assertion surface
/// here stays stable.
#[cfg(test)]
#[must_use]
pub(in crate::tui) fn plan_detail_lines(
    plan: &Plan,
    selected_item: Option<usize>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let mut out = plan_header_lines(plan, titles);
    for (idx, item) in plan.items.iter().enumerate() {
        out.extend(plan_item_line(
            idx,
            item,
            selected_item == Some(idx),
            titles,
        ));
    }
    out
}

/// One row in the plan-items list. Composed of multiple spans so the
/// ticket id and status word can stand out against the muted index and
/// optional title. For completed/skipped items we tone the title +
/// ticket id down to MUTED so the line of sight stays on what's still
/// in flight.
fn plan_item_line(
    idx: usize,
    item: &crate::plans::PlanItem,
    is_selected: bool,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let item_state = item.state;
    let recede = matches!(
        item_state,
        PlanItemState::Completed | PlanItemState::Skipped
    );
    let state_style = plan_item_style(item_state).add_modifier(Modifier::BOLD);
    let ident_style = if recede {
        muted.add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(IDENT).add_modifier(Modifier::BOLD)
    };

    let cursor = if is_selected { "▸ " } else { "  " };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{cursor}{idx:>3}. "), muted),
        // Marker glyph + state word both get the state colour, so the
        // pair reads as one tightly-coupled "status indicator" segment.
        Span::styled(format!("{} ", plan_item_marker(item_state)), state_style),
        Span::styled(format!("{:<11} ", plan_item_word(item_state)), state_style),
        // Ticket id: cyan + bold. Distinct from the violet accent so an
        // id never reads as a "selected" or "active" cue.
        Span::styled(item.ticket_id.clone(), ident_style),
    ];

    if let Some(title) = titles.get(&item.ticket_id).map(|i| i.title.as_str()) {
        spans.push(Span::styled(" — ", muted));
        let title_style = if recede { muted } else { Style::default() };
        spans.push(Span::styled(title.to_string(), title_style));
    }
    if item.injected {
        spans.push(Span::styled(
            "  (injected)",
            Style::default().fg(WARN).add_modifier(Modifier::ITALIC),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if let Some(session) = &item.session_id {
        // Wrap the session id onto a second line so the full id is
        // always readable regardless of terminal width. Indent under
        // the state column (cursor + idx + marker + state-word
        // padding ≈ 22 cols) so the session lines up with the title
        // text above it.
        lines.push(Line::from(vec![
            Span::styled("                      · session ", muted),
            Span::styled(session.to_string(), Style::default().fg(IDENT)),
        ]));
    }
    lines
}

/// Build the "progress" row of the plan header. The done count adopts
/// OK once it's > 0; the slash and total stay muted so the eye lands
/// on the active number. When done equals total, the whole fraction
/// goes OK — a "fully done" tell at a glance.
fn progress_kv_line(done: usize, total: usize) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    let ok = Style::default().fg(OK).add_modifier(Modifier::BOLD);
    let label = Span::styled(format!("{:<14}", "progress"), muted);
    if total > 0 && done == total {
        return Line::from(vec![label, Span::styled(format!("{done}/{total}"), ok)]);
    }
    let done_style = if done == 0 { muted } else { ok };
    Line::from(vec![
        label,
        Span::styled(done.to_string(), done_style),
        Span::styled(format!("/{total}"), muted),
    ])
}

/// Tracker-issue status word style. `open` reads OK, `closed` recedes
/// to MUTED, anything else (state machines vary — Linear/Jira may add
/// custom states) gets WARN so the user notices a non-standard value.
#[must_use]
fn issue_status_value_style(status: &str) -> Style {
    let base = match status {
        "open" => Style::default().fg(OK),
        "closed" => Style::default().fg(MUTED),
        _ => Style::default().fg(WARN),
    };
    base.add_modifier(Modifier::BOLD)
}

#[must_use]
fn plan_state_value_style(state: PlanState) -> Style {
    let base = match state {
        PlanState::Active => Style::default().fg(OK),
        PlanState::Paused => Style::default().fg(WARN),
        PlanState::Completed => Style::default().fg(MUTED),
        PlanState::Abandoned => Style::default().fg(ERR),
    };
    base.add_modifier(Modifier::BOLD)
}

#[must_use]
fn plan_policy_value_style(policy: crate::plans::ItemFailurePolicy) -> Style {
    let base = match policy {
        crate::plans::ItemFailurePolicy::Stop => Style::default().fg(ERR),
        crate::plans::ItemFailurePolicy::RetryOnce => Style::default().fg(WARN),
        crate::plans::ItemFailurePolicy::Continue => Style::default().fg(OK),
    };
    base.add_modifier(Modifier::BOLD)
}

#[must_use]
pub(in crate::tui) fn plan_state_word(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "active",
        PlanState::Paused => "paused",
        PlanState::Completed => "completed",
        PlanState::Abandoned => "abandoned",
    }
}

#[must_use]
pub(in crate::tui) fn plan_item_word(state: PlanItemState) -> &'static str {
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
        PlanItemState::InProgress => Style::default().fg(ACCENT),
        PlanItemState::Completed | PlanItemState::Skipped => Style::default().fg(MUTED),
        PlanItemState::Failed => Style::default().fg(ERR),
    }
}

#[must_use]
pub(in crate::tui) fn plan_failure_word(policy: crate::plans::ItemFailurePolicy) -> &'static str {
    match policy {
        crate::plans::ItemFailurePolicy::Stop => "stop",
        crate::plans::ItemFailurePolicy::Continue => "continue",
        crate::plans::ItemFailurePolicy::RetryOnce => "retry-once",
    }
}

#[must_use]
fn plan_in_cycle(plan: &Plan, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    plan.items
        .iter()
        .any(|item| cycle_nodes.contains(&item.ticket_id))
}
