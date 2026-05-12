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
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::lima::VmStatus;

use super::app::App;
use super::theme::{ACCENT, ERR, MUTED, OK, WARN, chip, framed_block, key, sep};

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

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(0)])
        .split(cols[1]);
    draw_details(app, frame, rows[0]);
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
}

fn draw_details(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.selected_session();
    let body = selected.map_or_else(
        || Text::from("(no session selected)").style(Style::default().fg(MUTED)),
        |s| {
            let kv = |label: &str, value: &str| {
                Line::from(vec![
                    Span::styled(format!("  {label:<10}"), Style::default().fg(MUTED)),
                    Span::raw(value.to_string()),
                ])
            };
            Text::from(vec![
                kv("Branch", s.branch.as_deref().unwrap_or("(none)")),
                kv("Ticket", s.issue_id.as_deref().unwrap_or("(none)")),
                kv("Project", s.project_id.as_deref().unwrap_or("(none)")),
                kv("Status", s.status.as_deref().unwrap_or("(none)")),
                kv("Activity", s.activity.as_deref().unwrap_or("(none)")),
                kv("Last", s.last_activity.as_deref().unwrap_or("(none)")),
                kv(
                    "Summary",
                    s.claude_summary
                        .as_deref()
                        .or(s.summary.as_deref())
                        .unwrap_or("(none)"),
                ),
            ])
        },
    );
    let title = selected
        .and_then(|s| s.id.as_deref())
        .map_or_else(|| " details ".to_string(), |id| format!(" details — {id} "));
    frame.render_widget(
        Paragraph::new(body)
            .block(framed_block(&title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_output(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = framed_block(" output ");
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
