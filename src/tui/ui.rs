//! Pure renderer — `(&App, &mut Frame) -> ()`.
//!
//! Layout is three vertical bands:
//!
//! ```text
//! ┌─ breadcrumb (project chain) ··················· AO:●  VM:●─┐
//! ├─ sessions ─────┬─ details ──────────────────────────────────┤
//! │ ▸ sb-1         │  Branch    feature/...                     │
//! │   sb-2         │  Ticket    AO-42                           │
//! │   sb-3         ├─ output ───────────────────────────────────┤
//! │                │  (tail of tmux pane capture)               │
//! ├─ [fleet] · ↑↓ nav · Enter attach · K kill · q quit ─────────┤
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! No state mutation here. Anything that wants to react to a click or key
//! mutates state via the input layer, which the next draw reflects.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Padding, Paragraph, Wrap};

use crate::ao::SessionInfo;
use crate::lima::VmStatus;

use super::app::App;
use super::theme::{
    ACCENT, ERR, MUTED, OK, WARN, chip, framed_block, framed_block_titled, key, kv_line, sep,
};

pub(super) fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // breadcrumb
            Constraint::Min(0),    // body
            Constraint::Length(1), // status bar
        ])
        .split(area);

    draw_breadcrumb(app, frame, layout[0]);
    draw_body(app, frame, layout[1]);
    draw_status_bar(app, frame, layout[2]);
}

fn draw_breadcrumb(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Left chunk: project chain. Right chunk: AO/VM badges.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(18)])
        .split(area);

    let chain = app.selected_session().map_or_else(
        || {
            vec![
                chip("fleet", ACCENT),
                Span::raw(" | "),
                Span::raw("(no sessions)"),
            ]
        },
        |s| {
            vec![
                chip("fleet", ACCENT),
                Span::raw(" | "),
                Span::raw(s.project_id.clone().unwrap_or_else(|| "?".into())),
                Span::raw(" | "),
                Span::raw(s.id.clone().unwrap_or_else(|| "?".into())),
                Span::raw(" "),
                Span::styled(
                    s.status.clone().unwrap_or_else(|| "?".into()),
                    Style::default().fg(MUTED),
                ),
            ]
        },
    );
    frame.render_widget(Paragraph::new(Line::from(chain)), cols[0]);

    let ao_color = if app.ao_up { OK } else { ERR };
    let vm_color = match app.vm_status {
        VmStatus::Running => OK,
        _ => ERR,
    };
    let badges = Line::from(vec![
        Span::raw(" "),
        Span::styled("AO", Style::default().fg(MUTED)),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(ao_color)),
        Span::raw("  "),
        Span::styled("VM", Style::default().fg(MUTED)),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(vm_color)),
    ]);
    frame.render_widget(Paragraph::new(badges).alignment(Alignment::Right), cols[1]);
}

fn draw_body(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(0)])
        .split(area);

    draw_sessions_list(app, frame, cols[0]);

    // Size the details panel to its content: `body.len()` rows + 2 borders.
    // Cap at `area.height - 5` so the output panel always gets at least 5
    // rows even when a future session grows extra kv fields.
    let body = build_details_body(app.selected_session());
    let max_info_h = cols[1].height.saturating_sub(5).max(3);
    let info_h = (u16::try_from(body.len()).unwrap_or(u16::MAX) + 2).min(max_info_h);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(info_h), Constraint::Min(0)])
        .split(cols[1]);
    draw_details(app, body, frame, rows[0]);
    draw_output(app, frame, rows[1]);
}

fn draw_sessions_list(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let items: Vec<ListItem<'_>> = if app.sessions.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no sessions)",
            Style::default().fg(MUTED),
        )))]
    } else {
        app.sessions
            .iter()
            .map(|s| {
                let id = s.id.clone().unwrap_or_else(|| "?".into());
                let activity = s.activity.clone().unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{id:<8}")),
                    Span::styled(activity, Style::default().fg(MUTED)),
                ]))
            })
            .collect()
    };
    let title = format!(" sessions ({}) ", app.sessions.len());
    let list = List::new(items)
        .block(framed_block(&title))
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut state = ListState::default();
    if !app.sessions.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);

    record_sidebar_rects(app, area);
}

/// Compute the per-row rect for each session in the sidebar list so the
/// mouse handler can hit-test wheel/click events against rows. Mirrors the
/// list's internal geometry (1-row border on each side, height 1 per row);
/// rows that overflow the visible area get `Rect::default()` and so can
/// never match a click.
fn record_sidebar_rects(app: &App, area: Rect) {
    let mut rects = app.sidebar_item_rects.borrow_mut();
    rects.clear();
    if app.sessions.is_empty() || area.height < 2 || area.width < 2 {
        return;
    }
    let inner_x = area.x.saturating_add(1);
    let inner_y = area.y.saturating_add(1);
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    rects.resize(app.sessions.len(), Rect::default());
    // Cap the visible rows at the smaller of (sessions, inner_h, u16::MAX).
    // Anything past the cap stays as `Rect::default()` so a click can never
    // hit an off-screen row.
    let visible = inner_h.min(u16::try_from(rects.len()).unwrap_or(u16::MAX));
    for row in 0..visible {
        rects[row as usize] = Rect {
            x: inner_x,
            y: inner_y + row,
            width: inner_w,
            height: 1,
        };
    }
}

/// Build the details-panel body for the currently selected session.
/// Empty-value rows are skipped — keel's pattern is "what's relevant
/// shows up; what isn't, doesn't" rather than padding with `(none)`.
/// The first row is always present when a session is selected so the
/// panel never collapses to zero rows mid-frame.
fn build_details_body(session: Option<&SessionInfo>) -> Vec<Line<'static>> {
    let Some(s) = session else {
        return vec![Line::from(Span::styled(
            "(no session selected)",
            Style::default().fg(MUTED),
        ))];
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut push = |label: &str, value: Option<&str>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            lines.push(kv_line(label, v));
        }
    };
    push("branch", s.branch.as_deref());
    push("ticket", s.issue_id.as_deref());
    push("project", s.project_id.as_deref());
    push("activity", s.activity.as_deref());
    push("last", s.last_activity.as_deref());
    push(
        "summary",
        s.claude_summary.as_deref().or(s.summary.as_deref()),
    );
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no metadata)",
            Style::default().fg(MUTED),
        )));
    }
    lines
}

fn details_title(session: Option<&SessionInfo>) -> Line<'static> {
    // Mirror keel's info-panel header: leading space, bold-accent name,
    // double space, italic-dim "kind" tag, trailing space. For fleet the
    // kind tag is the live session status (`agentic`, `idle`, …) so the
    // title doubles as a live state indicator.
    let Some(s) = session else {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "details",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
    };
    let id = s.id.clone().unwrap_or_else(|| "?".into());
    let tag = s
        .status
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "session".into());
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            id,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            tag,
            Style::default()
                .fg(MUTED)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::raw(" "),
    ])
}

fn draw_details(app: &App, body: Vec<Line<'static>>, frame: &mut Frame<'_>, area: Rect) {
    let title = details_title(app.selected_session());
    let block = framed_block_titled(title).padding(Padding::horizontal(2));
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_output(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = framed_block(" output ").padding(Padding::horizontal(2));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let selected_id = app.selected_session_id();
    let body = selected_id.as_deref().map_or_else(
        || "(no session selected)".to_string(),
        |id| match app.pane_outputs.get(id) {
            Some(pane) if !pane.is_empty() => pane.clone(),
            _ => "(no output captured yet — first refresh after spawn takes ~1.5s)".to_string(),
        },
    );

    // Tail the captured pane so the most recent activity is visible. tmux
    // capture-pane gives plain text; the last `area.height` lines fit. If
    // the body is shorter, just render as is.
    let line_count = body.lines().count();
    let take = inner.height as usize;
    let lines: Vec<Line<'_>> = body
        .lines()
        .skip(line_count.saturating_sub(take))
        .map(Line::raw)
        .collect();
    let style = if selected_id
        .as_deref()
        .and_then(|id| app.pane_outputs.get(id))
        .is_some_and(|s| !s.is_empty())
    {
        Style::default()
    } else {
        Style::default().fg(MUTED)
    };
    frame.render_widget(
        Paragraph::new(lines).style(style).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_status_bar(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Confirmation prompt takes over the status bar when active.
    if let Some(c) = &app.confirm {
        let prompt = Line::from(vec![
            chip("[confirm]", WARN),
            Span::raw("  "),
            Span::raw(c.prompt()),
        ]);
        frame.render_widget(
            Paragraph::new(prompt).style(Style::default().fg(WARN)),
            area,
        );
        return;
    }

    let mut spans: Vec<Span<'_>> = vec![
        chip("[fleet]", ACCENT),
        sep(),
        key("↑/↓"),
        Span::raw(" nav"),
        sep(),
        key("Enter"),
        Span::raw(" attach"),
        sep(),
        key("t"),
        Span::raw(" tracker"),
        sep(),
        key("c"),
        Span::raw(" config"),
        sep(),
        key("K"),
        Span::raw(" kill"),
        sep(),
        key("⇧S"),
        Span::raw("/⇧X start/stop"),
        sep(),
        key("⇧W"),
        Span::raw(" web"),
        sep(),
        key("r"),
        Span::raw(" refresh"),
        sep(),
        key("q"),
        Span::raw(" quit"),
    ];
    if let Some(e) = &app.last_error {
        spans.push(sep());
        spans.push(Span::styled(
            format!("[error] {e}"),
            Style::default().fg(ERR),
        ));
    } else if let Some(m) = &app.last_info {
        spans.push(sep());
        spans.push(Span::styled(m.clone(), Style::default().fg(OK)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(MUTED)),
        area,
    );
}
