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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Clear, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap,
};

use crate::ao::SessionInfo;
use crate::lima::VmStatus;

use super::app::{App, IssuesState, RegisterField, RegisterProject, SecretSetup, SpawnPrompt};
use super::bringup::BringUp;
use super::preflight::MissingDep;
use super::theme::{
    ACCENT, ERR, KEY_FG, MUTED, OK, WARN, badge, chip, framed_block, framed_block_titled, key,
    kv_line, modal_key, sep,
};

/// Fixed event-pane height. Title row + four event rows + bottom
/// border = 6 lines. Wide enough to read what just happened
/// without eating the body's vertical space.
const EVENTS_PANE_HEIGHT: u16 = 6;

pub(super) fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // breadcrumb
            Constraint::Min(0),                     // body
            Constraint::Length(EVENTS_PANE_HEIGHT), // events strip
            Constraint::Length(1),                  // status bar
        ])
        .split(area);

    draw_breadcrumb(app, frame, layout[0]);
    draw_body(app, frame, layout[1]);
    draw_events_pane(app, frame, layout[2]);
    draw_status_bar(app, frame, layout[3]);

    // Overlay modals — painted last so they sit on top of the normal
    // layout. Priority matches input dispatch: register-project >
    // secret setup > spawn prompt. None of these are designed to
    // co-exist; only one can be `Some` at a time in practice.
    if let Some(rp) = &app.register_project {
        render_register_project(frame, rp);
    } else if let Some(setup) = &app.secret_setup {
        render_secret_setup(frame, setup);
    } else if let Some(prompt) = &app.spawn_prompt {
        render_spawn_prompt(frame, prompt, app.ao_up);
    }
}

fn draw_breadcrumb(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Left chunk: project chain. Right chunk: AO/VM badges.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(18)])
        .split(area);

    let project_chip = app.current_project_key.as_deref().unwrap_or("(unscoped)");
    let chain: Vec<Span<'_>> = app.selected_session().map_or_else(
        || {
            vec![
                chip("fleet", ACCENT),
                Span::raw(" | "),
                chip(project_chip, ACCENT),
                Span::raw(" | "),
                Span::styled("(no sessions)", Style::default().fg(MUTED)),
            ]
        },
        |s| {
            vec![
                chip("fleet", ACCENT),
                Span::raw(" | "),
                chip(project_chip, ACCENT),
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
        .constraints([Constraint::Length(32), Constraint::Min(0)])
        .split(area);

    draw_sessions_list(app, frame, cols[0]);

    // Size the details panel to its content: `body.len()` rows + 2 borders.
    // Cap at `area.height - 5` so the output panel always gets at least 5
    // rows even when a future session grows extra kv fields.
    let body = build_details_body(app);
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
    // Stack two framed blocks vertically: "orchestrator (n)" on top
    // (omitted entirely when no orchestrator sessions exist) and
    // "workers (n)" below. The workers block hosts the trailing
    // "+ new session" sentinel. Selection is a single flat index
    // across the visible rows in both blocks; whichever block
    // contains the selected row applies the highlight.
    let orch_count = app.orchestrator_count();
    let worker_count = app.worker_count();
    let worker_block_rows = worker_count + 1; // +1 for the sentinel

    let (orch_area, workers_area) = if orch_count == 0 {
        (None, area)
    } else {
        // Frame chrome eats 2 rows (top + bottom border) per block.
        // Cap the orchestrator block's height at its content size +
        // 2 so the workers block gets the rest of the column.
        let orch_h = u16::try_from(orch_count + 2).unwrap_or(3);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(orch_h), Constraint::Min(0)])
            .split(area);
        (Some(split[0]), split[1])
    };

    if let Some(rect) = orch_area {
        draw_sidebar_group(
            app,
            frame,
            rect,
            &format!(" orchestrator ({orch_count}) "),
            0..orch_count,
        );
    }
    draw_sidebar_group(
        app,
        frame,
        workers_area,
        &format!(" workers ({worker_count}) "),
        orch_count..orch_count + worker_block_rows,
    );

    record_sidebar_rects(app, orch_area, workers_area);
}

/// Render one bordered sidebar section (orchestrator or workers).
/// `range` is the slice of `app.sidebar_rows()` this block displays;
/// the row at the end of the worker range is the sentinel and is
/// distinguishable in the row itself, not from a flag here. The
/// block applies the selection highlight only when `app.selected`
/// falls inside `range`.
fn draw_sidebar_group(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    range: std::ops::Range<usize>,
) {
    let rows = app.sidebar_rows();
    let items: Vec<ListItem<'_>> = rows[range.clone()]
        .iter()
        .map(|row| match row {
            crate::tui::app::SidebarRow::Session(s) => {
                let id = s.id.clone().unwrap_or_else(|| "?".into());
                let activity = s.activity.clone().unwrap_or_default();
                let is_orch = s.role.as_deref() == Some("orchestrator");
                let mut spans = vec![Span::raw(format!("{id:<10}"))];
                if is_orch {
                    spans.push(Span::styled(
                        "(read-only) ",
                        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                    ));
                }
                // Sidebar badges from the per-session meta probe. Crash
                // takes priority over "done" — a runtime that died
                // unexpectedly is more urgent than a self-reported
                // completion, and in practice they don't co-occur.
                if let Some(meta) = s.id.as_ref().and_then(|id| app.session_meta.get(id)) {
                    if meta.runtime_crashed() {
                        spans.push(Span::styled(
                            "crashed ",
                            Style::default().fg(ERR).add_modifier(Modifier::BOLD),
                        ));
                    } else if meta.agent_done() {
                        spans.push(Span::styled(
                            "✓ done ",
                            Style::default().fg(OK).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                spans.push(Span::styled(activity, Style::default().fg(MUTED)));
                ListItem::new(Line::from(spans))
            }
            crate::tui::app::SidebarRow::Sentinel => ListItem::new(Line::from(Span::styled(
                "+ new session",
                Style::default().fg(ACCENT).add_modifier(Modifier::ITALIC),
            ))),
        })
        .collect();

    let list = List::new(items)
        .block(framed_block(title))
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(
            Style::default()
                .fg(Color::Indexed(255))
                .bg(Color::Indexed(238))
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    if range.contains(&app.selected) {
        state.select(Some(app.selected - range.start));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// Compute per-row rects for each selectable sidebar entry across
/// the (optional) orchestrator block + the workers block, so the
/// mouse handler can hit-test wheel/click events against rows.
/// Rect indices map 1:1 with the flat ordering used by
/// `App::sidebar_rows()`: orchestrator entries first, then workers,
/// then the trailing sentinel. Rows that overflow the visible area
/// get `Rect::default()` and so can never match a click.
fn record_sidebar_rects(app: &App, orch_area: Option<Rect>, workers_area: Rect) {
    let mut rects = app.sidebar_item_rects.borrow_mut();
    rects.clear();
    rects.resize(app.sidebar_row_count(), Rect::default());

    let orch_count = app.orchestrator_count();
    let mut flat_idx = 0usize;
    if let Some(area) = orch_area {
        record_block_rects(&mut rects, area, flat_idx, orch_count);
        flat_idx += orch_count;
    }
    let worker_rows = app.worker_count() + 1; // workers + sentinel
    record_block_rects(&mut rects, workers_area, flat_idx, worker_rows);
}

/// Fill in `rects[start..start+count]` with one-row rectangles
/// inside `block_area`'s framed interior. Rects that fall outside
/// the visible interior stay at `Rect::default()`.
fn record_block_rects(rects: &mut [Rect], block_area: Rect, start: usize, count: usize) {
    if block_area.height < 2 || block_area.width < 2 || count == 0 {
        return;
    }
    let inner_x = block_area.x.saturating_add(1);
    let inner_y = block_area.y.saturating_add(1);
    let inner_w = block_area.width.saturating_sub(2);
    let inner_h = block_area.height.saturating_sub(2);
    let visible = inner_h.min(u16::try_from(count).unwrap_or(u16::MAX));
    for row in 0..visible {
        let flat = start + row as usize;
        if let Some(slot) = rects.get_mut(flat) {
            *slot = Rect {
                x: inner_x,
                y: inner_y + row,
                width: inner_w,
                height: 1,
            };
        }
    }
}

/// Build the details-panel body. With a selection, shows the kv table
/// for that session; without one, shows a state-aware welcome screen
/// that tells the user what to do next (start AO, spawn a session,
/// add a project entry for cwd, …) instead of a dead `(no session)`
/// placeholder.
fn build_details_body(app: &App) -> Vec<Line<'static>> {
    if let Some(s) = app.selected_session() {
        return build_session_kv_lines(s);
    }
    build_welcome_lines(app)
}

fn build_session_kv_lines(s: &SessionInfo) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut push = |label: &str, value: Option<&str>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            lines.push(kv_line(label, v));
        }
    };
    push("branch", s.branch.as_deref());
    push("ticket", s.issue_id.as_deref());
    push("project", s.project_id.as_deref());
    let worktree = s.workspace_path.as_deref().map(shorten_worktree);
    push("worktree", worktree.as_deref());
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

/// Elide the homedir + `.agent-orchestrator/` prefix on a worktree
/// path so the session-id tail (the part the user actually scans
/// for) survives in a narrow details column. Falls back to a plain
/// char-count truncation if the path doesn't match the expected
/// shape — defensive, so a future AO release that relocates worktrees
/// still renders something legible. Truncating instead of wrapping
/// matters: the details panel sizes its height by logical-line count
/// (see `info_h` in [`draw_body`]), so a wrapped row would clip its
/// continuation against the bottom border.
fn shorten_worktree(path: &str) -> String {
    path.find("/.agent-orchestrator/").map_or_else(
        || truncate(path, 56),
        |idx| {
            let tail = &path[idx + "/.agent-orchestrator".len()..];
            format!("…{tail}")
        },
    )
}

/// State-aware "what do I do next?" panel. Three cases:
///
/// 1. Cwd doesn't match any project in the catalog → tell the user to
///    add one via `c` (or cd into one).
/// 2. Cwd matches a project but AO isn't running → tell them to start
///    it (Shift+S).
/// 3. AO is up but no sessions exist → tell them how to spawn one.
///
/// Each step references the keybind that fixes it, so the user never
/// has to consult the legend to figure out what to press first.
fn build_welcome_lines(app: &App) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    if app.current_project_key.is_none() {
        return vec![
            Line::from(Span::styled(
                "This directory isn't a known fleet project.",
                muted,
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("A", bold_key),
                Span::styled("  to register this directory as a project", muted),
            ]),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("c", bold_key),
                Span::styled("  to edit agent-orchestrator.yaml manually", muted),
            ]),
        ];
    }

    if !app.ao_up {
        return vec![
            Line::from(Span::styled(
                "AO isn't running yet — nothing's listening on :3000.",
                muted,
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("S", bold_key),
                Span::styled("       start the orchestrator inside the VM", muted),
            ]),
            Line::raw(""),
            Line::from(Span::styled("  Once it's up:", muted)),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("n", bold_key),
                Span::styled("       spawn a session on an issue", muted),
            ]),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("t", bold_key),
                Span::styled("       open the tracker (git-bug) to create one", muted),
            ]),
        ];
    }

    vec![
        Line::from(Span::styled(
            "AO is up. No sessions running for this project yet.",
            muted,
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("n", bold_key),
            Span::styled("       spawn a session on an issue", muted),
        ]),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("t", bold_key),
            Span::styled("       open the tracker to browse / create issues", muted),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Or from a shell:  fleet spawn <issue-id>",
            muted,
        )),
    ]
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
        Span::styled(id, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            tag,
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
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
        Paragraph::new(lines)
            .style(style)
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// System-wide event ticker pinned to the bottom of the UI. Polled
/// from `ao events list --since 1h --json` on a slower cadence than
/// per-session refresh (events are sparse). Rows are colored by
/// level: errors red, warnings yellow, info plain. Shows the most
/// recent rows first — AO already returns the log sorted descending.
fn draw_events_pane(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let title = format!(" events ({}) ", app.events.len());
    let block = framed_block(&title).padding(Padding::horizontal(2));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.events.is_empty() {
        let line = Line::from(Span::styled(
            "(no recent activity)",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    let rows: Vec<Line<'_>> = app
        .events
        .iter()
        .take(inner.height as usize)
        .map(event_line)
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);
}

/// One row in the event ticker. Format: `HH:MM:SS  kind  [session]  summary`.
/// The level drives the foreground colour of the summary so an error
/// row reads as red without needing a separate icon column.
fn event_line(e: &crate::ao::EventInfo) -> Line<'static> {
    let time =
        e.ts.as_deref()
            .and_then(|s| s.split('T').nth(1))
            .and_then(|after_t| after_t.split('.').next())
            .unwrap_or("--:--:--");
    let kind = e.kind.as_deref().unwrap_or("?");
    let session = e.session_id.as_deref().unwrap_or("—");
    let summary = e.summary.as_deref().unwrap_or("");
    let level_color = match e.level.as_deref() {
        Some("error") => ERR,
        Some("warn") => WARN,
        _ => MUTED,
    };
    let summary_style = if matches!(e.level.as_deref(), Some("error" | "warn")) {
        Style::default()
            .fg(level_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{time}  "), Style::default().fg(MUTED)),
        Span::styled(format!("{kind:<24}"), Style::default().fg(level_color)),
        Span::styled(format!("  {session:<8}  "), Style::default().fg(MUTED)),
        Span::styled(summary.to_string(), summary_style),
    ])
}

/// Status-bar overlay for an active AO background task. Renders the
/// braille spinner cycling with elapsed time, the task-wide label
/// (`killing sb-42`, `starting AO`, …), an optional sub-phase label
/// for multi-phase tasks like restart-for-orchestrator, and the
/// elapsed seconds in muted grey.
fn draw_in_flight_ao(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let Some(task) = &app.in_flight_ao else {
        return;
    };
    let frame_idx = (task.elapsed().as_millis() / 100) as usize % SPINNER_FRAMES.len();
    let spinner_glyph = SPINNER_FRAMES[frame_idx];
    let badge_text = format!(" {spinner_glyph} ");
    let mut spans: Vec<Span<'_>> = vec![
        badge(&badge_text, ACCENT),
        Span::raw(" "),
        Span::styled(task.label().to_string(), Style::default().fg(ACCENT)),
    ];
    if let Some(phase) = task.phase_label() {
        spans.push(Span::styled(" — ", Style::default().fg(MUTED)));
        spans.push(Span::styled(phase.to_string(), Style::default().fg(MUTED)));
    }
    spans.push(Span::styled(
        format!(" ({}s)", task.elapsed().as_secs()),
        Style::default().fg(MUTED),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status_bar(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Three overlays replace the legend rather than appending to it: a
    // pending confirm, an error flash, an info flash. Match keel's pattern
    // — appending was fine while messages stayed short, but a real shell
    // error ("failed to spawn `limactl`: No such file or directory") runs
    // past the right edge of a typical terminal and chops off the relevant
    // text. Anything load-bearing belongs on the left.
    if let Some(c) = &app.confirm {
        let line = Line::from(vec![
            badge(" ? ", WARN),
            Span::raw(" "),
            Span::styled(c.prompt(), Style::default().fg(WARN)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    // In-flight AO subprocess (kill / stop / start / spawn / restart):
    // spinner + task label, optional sub-phase label for multi-phase
    // tasks, elapsed seconds. Slots in above the action-flash overlays
    // because while a task is running there's no flash to show yet —
    // completion replaces the spinner with a flash via
    // `App::drain_ao_task`.
    if app.in_flight_ao.is_some() {
        draw_in_flight_ao(app, frame, area);
        return;
    }
    // Action-driven flashes (success + failure from user input) stay
    // until the next keystroke / mouse event — see
    // `App::dismiss_flash`. Refresh errors reflect live state and keep
    // showing until the next successful Sessions snapshot clears them.
    if let Some(e) = &app.action_error {
        let line = Line::from(vec![
            badge(" ! ", ERR),
            Span::raw(" "),
            Span::styled(e.clone(), Style::default().fg(ERR)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some(m) = &app.last_info {
        let line = Line::from(vec![
            badge(" ✓ ", OK),
            Span::raw(" "),
            Span::styled(m.clone(), Style::default().fg(OK)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some(e) = &app.refresh_error {
        let line = Line::from(vec![
            badge(" ! ", ERR),
            Span::raw(" "),
            Span::styled(e.clone(), Style::default().fg(ERR)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Capital letters mean shift-modified bindings (S = Shift+S, K =
    // Shift+K, …). No ⇧ glyph anywhere in the legend — the case
    // already carries the meaning and the arrow added visual noise.
    // The orchestrator row is read-only from fleet's perspective:
    // `Enter` is a silent no-op (no attach), `Shift+K` is blocked
    // (would leave the daemon orphaned — see input.rs). Hide both
    // chips so the legend never advertises an action it won't
    // perform; lifecycle for the orchestrator goes through
    // `Shift+S` / `Shift+X` instead.
    let orchestrator_selected = app.is_orchestrator_selected();

    let mut spans: Vec<Span<'_>> = vec![
        chip("[fleet]", ACCENT),
        sep(),
        key("↑/↓"),
        Span::raw(" nav"),
    ];
    if !orchestrator_selected {
        spans.extend([sep(), key("enter"), Span::raw(" attach")]);
    }
    spans.extend([
        sep(),
        key("n"),
        Span::raw(" new"),
        sep(),
        key("t"),
        Span::raw(" tracker"),
    ]);
    if app.current_tracker_has_web() {
        spans.push(sep());
        spans.push(key("T"));
        // "tracker web" rather than just "web" to disambiguate from
        // `W` (AO dashboard) below.
        spans.push(Span::raw(" tracker web"));
    }
    spans.extend([sep(), key("c"), Span::raw(" edit config")]);
    if !orchestrator_selected {
        spans.extend([sep(), key("K"), Span::raw(" kill")]);
    }
    spans.extend([
        sep(),
        key("S"),
        Span::raw("/"),
        key("X"),
        Span::raw(" start/stop"),
        sep(),
        key("W"),
        Span::raw(" web"),
        sep(),
        key("r"),
        Span::raw(" refresh"),
        sep(),
        key("q"),
        Span::raw(" quit"),
    ]);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(MUTED)),
        area,
    );
}

// ─────────────────────── setup modals ───────────────────────
//
// Painted in place of the normal UI when [`crate::tui::preflight::check`]
// found something that needs handling before the main loop can run: a
// missing host binary (dead-end, quit only), or a missing/stopped VM
// (actionable — press y to remediate, q to quit).

/// Missing host binary — fleet can't proceed and there's nothing fleet can
/// do about it. Body lists each dep with its install hint; footer says
/// "press any key to quit".
/// "Spawn session" picker modal. Rendered as an overlay on top of
/// the normal Sessions view while [`crate::tui::app::App::spawn_prompt`]
/// is `Some`. Shows a filter input on top, the tracker issue list
/// below (filtered by the buffer), and footer key hints. Enter
/// submits the highlighted row or, if there's no list / no match,
/// the raw buffer. Esc cancels.
/// "Register this directory as a fleet project" modal. Painted when
/// the user invokes `Shift+A` from the welcome screen. Shows the
/// (non-editable) absolute cwd plus two editable text fields,
/// `name` and `sessionPrefix`. Tab toggles between them; the cursor
/// `█` glyph marks which field receives keystrokes. On save the
/// modal closes and the welcome screen vanishes — see
/// `crate::tui::terminal::save_register_project`.
pub(super) fn render_register_project(frame: &mut Frame<'_>, rp: &RegisterProject) {
    let muted = Style::default().fg(MUTED);
    let bold_accent = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let cursor_style = Style::default().fg(KEY_FG);

    // Build one field row. The cursor glyph rides at the end of the
    // active field so the user has a visual anchor for where their
    // next keystroke lands.
    let field_row = |label: &str, value: &str, focused: bool| -> Line<'static> {
        let value_style = if focused {
            bold_accent
        } else {
            Style::default()
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(format!("  {label:<12}"), muted),
            Span::styled(value.to_string(), value_style),
        ];
        if focused {
            spans.push(Span::styled("█", cursor_style));
        }
        Line::from(spans)
    };

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "Add this directory as a fleet project.",
            muted,
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  path        ", muted),
            // Path isn't editable but rendering it muted/dim makes
            // that legible — the eye reads the two editable rows
            // below as "the live values" by contrast.
            Span::styled(rp.path.display().to_string(), muted),
        ]),
        field_row("name", &rp.name, rp.focus == RegisterField::Name),
        field_row("prefix", &rp.prefix, rp.focus == RegisterField::Prefix),
    ];
    if let Some(err) = &rp.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  error       ", Style::default().fg(ERR)),
            Span::styled(
                err.clone(),
                Style::default().fg(ERR).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    let footer = vec![Line::from(vec![
        modal_key("enter"),
        Span::styled(" save   ", muted),
        modal_key("tab"),
        Span::styled(" switch field   ", muted),
        modal_key("esc"),
        Span::styled(" cancel", muted),
    ])];

    draw_modal_sized(
        frame,
        " register project ",
        ACCENT,
        lines,
        &footer,
        ModalWidth::Fixed(72),
    );
}

/// "Configure Claude OAuth token" modal. Painted when a user
/// action that needs the token finds none configured. Single text
/// field, masked rendering, in-modal error row for save failures
/// (keychain unreachable, config not writable, etc.).
pub(super) fn render_secret_setup(frame: &mut Frame<'_>, setup: &SecretSetup) {
    // OAuth tokens are ~100 chars; one `•` per char would push the
    // modal way past a comfortable reading width. Cap the masked
    // rendering and surface the real length as a separate tally so
    // the user still gets paste-landed feedback without the dots
    // dictating layout.
    const VISIBLE_DOT_CAP: usize = 40;

    let muted = Style::default().fg(MUTED);

    // Name the platform-native credential store so the user knows
    // exactly where their token's going. macOS Keychain / Linux
    // Secret Service (GNOME Keyring / KWallet / etc.) / Windows
    // Credential Manager all flow through the `keyring` crate.
    let keychain_name = match std::env::consts::OS {
        "macos" => "macOS Keychain",
        "windows" => "Windows Credential Manager",
        // Linux + freebsd + others go through the Secret Service
        // DBus protocol; name the well-known providers rather than
        // saying "Linux Secret Service" which is jargon.
        _ => "GNOME Keyring / KWallet (Secret Service)",
    };

    let token_len = setup.buffer.chars().count();
    let visible_dots = token_len.min(VISIBLE_DOT_CAP);
    let mut token_row: Vec<Span<'static>> = vec![
        Span::styled("token       ", muted),
        Span::styled(
            "•".repeat(visible_dots),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(KEY_FG)),
    ];
    if token_len > 0 {
        // Always show the char count — it's the reliable signal
        // that a long paste landed in full, since the dots cap out.
        token_row.push(Span::styled(format!("  ({token_len} chars)"), muted));
    }

    let mut lines = vec![
        Line::from(Span::styled(
            "Fleet needs a Claude Pro / Max OAuth token to spawn",
            muted,
        )),
        Line::from(Span::styled(
            "claude-code sessions inside the VM. Generate one with",
            muted,
        )),
        Line::from(Span::styled(
            "claude-code's official setup-token command:",
            muted,
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled(
                "claude setup-token",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled("Paste the resulting token below.", muted)),
        Line::raw(""),
        Line::from(token_row),
    ];
    if let Some(err) = &setup.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("error  ", Style::default().fg(ERR)),
            Span::styled(
                truncate(err, 70),
                Style::default().fg(ERR).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("Stored in ", muted),
        Span::styled(keychain_name, Style::default().fg(ACCENT)),
        Span::styled(".", muted),
    ]));

    let footer = vec![Line::from(vec![
        modal_key("enter"),
        Span::styled(" save to keychain   ", muted),
        modal_key("esc"),
        Span::styled(" cancel", muted),
    ])];
    // Pin the modal width so a 100-char OAuth paste can't drag the
    // layout sideways. `clamp(area * 0.6, 60, 80)` is wide enough
    // for the explanatory copy and the masked-dots row with its
    // `(N chars)` tally, without overflowing narrow terminals.
    let area = frame.area();
    let pinned = area.width.saturating_mul(3).saturating_div(5).clamp(60, 80);
    draw_modal_sized(
        frame,
        " configure claude oauth token ",
        ACCENT,
        lines,
        &footer,
        ModalWidth::Fixed(pinned),
    );
}

/// Soft warning lines for "AO daemon down" rendered above the issue
/// picker inside the spawn modal. Not a gate — AO creates sessions
/// fine without the daemon, only lifecycle polling (state transitions,
/// PR status, CI signals) is inactive. Surface it so the user
/// notices before spawning and knows the remediation (`Shift+S`).
fn ao_down_warning_lines() -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled("AO daemon not running — ", Style::default().fg(WARN)),
            Span::styled("lifecycle tracking will be off for the new session.", muted),
        ]),
        Line::from(vec![
            Span::styled("  start it with ", muted),
            key("S"),
            Span::styled(" before spawn, or proceed without tracking.", muted),
        ]),
    ]
}

/// Body of the spawn modal: the per-state rendering of the issue list
/// (loading / unsupported tracker / error / loaded). Returns the lines
/// to insert under the filter input; the caller composes them with
/// chrome, the AO-down warning, and the footer.
fn spawn_issue_lines(prompt: &SpawnPrompt) -> Vec<Line<'static>> {
    const MAX_ROWS: usize = 12;
    let muted = Style::default().fg(MUTED);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &prompt.issues {
        IssuesState::Loading => {
            lines.push(Line::from(Span::styled(
                "loading issues from tracker…",
                muted.add_modifier(Modifier::ITALIC),
            )));
        }
        IssuesState::Unsupported { plugin } => {
            lines.push(Line::from(vec![
                Span::styled("tracker `", muted),
                Span::styled(plugin.clone(), Style::default().fg(WARN)),
                Span::styled("` — listing not supported; type the issue id.", muted),
            ]));
        }
        IssuesState::Error(msg) => {
            lines.push(Line::from(vec![
                Span::styled("tracker error: ", Style::default().fg(ERR)),
                Span::styled(
                    truncate(msg, 80),
                    Style::default().fg(ERR).add_modifier(Modifier::ITALIC),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "type the issue id manually below and press Enter.",
                muted,
            )));
        }
        IssuesState::Loaded(all) => {
            let filtered = prompt.filtered();
            let total = all.len();
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    if total == 0 {
                        "(no issues in this repo — type an id manually)".to_string()
                    } else {
                        format!("no issues match `{}` out of {total}", prompt.buffer)
                    },
                    muted.add_modifier(Modifier::ITALIC),
                )));
            } else {
                let selected = prompt.selected_idx.min(filtered.len() - 1);
                let window = scroll_window(filtered.len(), selected, MAX_ROWS);
                for idx in window.clone() {
                    lines.push(issue_line(filtered[idx], idx == selected));
                }
                lines.push(Line::raw(""));
                let shown = window.end - window.start;
                lines.push(Line::from(Span::styled(
                    format!("{shown} of {total} issues"),
                    muted.add_modifier(Modifier::ITALIC),
                )));
            }
        }
    }
    lines
}

pub(super) fn render_spawn_prompt(frame: &mut Frame<'_>, prompt: &SpawnPrompt, ao_up: bool) {
    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Filter input with block caret.
    lines.push(Line::from(vec![
        Span::styled("filter      ", muted),
        Span::styled(prompt.buffer.clone(), bold_key),
        Span::styled("█", Style::default().fg(KEY_FG)),
    ]));
    if !ao_up {
        lines.extend(ao_down_warning_lines());
    }
    lines.push(Line::raw(""));
    lines.extend(spawn_issue_lines(prompt));

    let footer = vec![Line::from(vec![
        modal_key("↑↓"),
        Span::styled(" pick    ", muted),
        modal_key("enter"),
        Span::styled(" submit    ", muted),
        modal_key("esc"),
        Span::styled(" cancel", muted),
    ])];
    // Pin the modal width so it doesn't shrink as the user narrows
    // the filter — a resizing target while you're typing is hard to
    // read. Picks `min(area * 0.75, 100 cols)` so the picker is
    // comfortable on a typical terminal but never overflows a
    // narrow one.
    let area = frame.area();
    let pinned = area
        .width
        .saturating_mul(3)
        .saturating_div(4)
        .clamp(60, 100);
    draw_modal_sized(
        frame,
        " spawn session ",
        ACCENT,
        lines,
        &footer,
        ModalWidth::Fixed(pinned),
    );
}

/// One filtered issue row. Layout: gutter `▶`/space, `human_id`
/// (8-col padded), title (truncated to fit), trailing `[status]`
/// tag for open vs closed at a glance.
fn issue_line(issue: &crate::ao::tracker::Issue, focused: bool) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    let gutter = if focused { "▶ " } else { "  " };
    let id_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(KEY_FG)
    };
    let title_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let status_color = match issue.status.as_str() {
        "open" => OK,
        "closed" => MUTED,
        _ => WARN,
    };
    Line::from(vec![
        Span::styled(gutter.to_string(), id_style),
        Span::styled(format!("{:<10}", issue.human_id), id_style),
        Span::styled(truncate(&issue.title, 60), title_style),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", issue.status),
            Style::default().fg(status_color),
        ),
        Span::raw(" "),
        Span::styled(
            // Surface the first label as a tag — useful for git-bug
            // workflows that use `type:bug` / `type:feature` etc.
            issue
                .labels
                .first()
                .map(|l| format!("({l})"))
                .unwrap_or_default(),
            muted,
        ),
    ])
}

/// Pick a `MAX_ROWS`-wide slice of the filtered list centered on the
/// selected row. Keeps the cursor visible without making the user
/// scroll manually.
fn scroll_window(total: usize, selected: usize, window: usize) -> std::ops::Range<usize> {
    if total <= window {
        return 0..total;
    }
    let half = window / 2;
    let start = selected.saturating_sub(half).min(total - window);
    start..(start + window)
}

/// Cap a string at `max` chars, appending `…` on truncation. Counts
/// chars not bytes so multibyte titles don't slice mid-codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Host-repo isolation gate: `defaults.workspace` in the AO yaml is
/// missing or set to something other than `worktree`. Dead-end modal
/// (no retry/continue) — the user has to edit the yaml to proceed.
/// Reuses the preflight footer pattern: `c` opens `$EDITOR`, any
/// other key quits.
pub(super) fn render_workspace_unsafe(frame: &mut Frame<'_>, current: Option<&str>) {
    let muted = Style::default().fg(MUTED);
    let bold_accent = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let displayed = current.unwrap_or("(unset)");

    let lines = vec![
        Line::from(vec![
            Span::styled("✗ ", Style::default().fg(ERR).add_modifier(Modifier::BOLD)),
            Span::styled("defaults.workspace", bold_accent),
        ]),
        Line::from(Span::styled(
            "  must be `worktree` so AO agents never run inside the host",
            muted,
        )),
        Line::from(Span::styled("  repo's working tree.", muted)),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  current   ", muted),
            Span::styled(
                displayed.to_string(),
                Style::default().fg(ERR).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Edit ~/.config/fleet/agent-orchestrator.yaml and set:",
            muted,
        )),
        Line::raw(""),
        Line::from(Span::styled("    defaults:", bold_accent)),
        Line::from(vec![
            Span::styled("      workspace: ", bold_accent),
            Span::styled(
                "worktree",
                Style::default().fg(OK).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Why: AO's worktree plugin creates a dedicated git worktree",
            muted,
        )),
        Line::from(Span::styled(
            "  per session inside the VM. Any other value (or no value at",
            muted,
        )),
        Line::from(Span::styled(
            "  all) can let an agent run git / rm / edits against your",
            muted,
        )),
        Line::from(Span::styled("  host checkout.", muted)),
    ];
    draw_modal(
        frame,
        " host-repo isolation ",
        ERR,
        lines,
        &[Line::from(vec![
            modal_key("c"),
            Span::styled(" edit agent-orchestrator.yaml   ", muted),
            modal_key("any other key"),
            Span::styled(" quit", muted),
        ])],
    );
}

pub(super) fn render_preflight(frame: &mut Frame<'_>, failures: &[MissingDep]) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Fleet needs the following on this host:",
        Style::default().fg(MUTED),
    )));
    lines.push(Line::raw(""));
    for f in failures {
        lines.push(Line::from(vec![
            Span::styled("✗ ", Style::default().fg(ERR).add_modifier(Modifier::BOLD)),
            Span::styled(
                f.name.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(f.purpose.to_string(), Style::default().fg(MUTED)),
        ]));
        // Two rows so the copy-pasteable command lives on its own line
        // (no whitespace ambiguity, easy to triple-click) and the docs
        // URL sits below it as a clearly secondary aid.
        lines.push(Line::from(vec![
            Span::styled("  install  ", Style::default().fg(MUTED)),
            Span::styled(f.install.to_string(), Style::default().fg(ACCENT)),
        ]));
        if let Some(url) = f.docs {
            lines.push(Line::from(vec![
                Span::styled("  docs     ", Style::default().fg(MUTED)),
                Span::styled(url.to_string(), Style::default().fg(ACCENT)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    draw_modal(
        frame,
        " preflight failed ",
        ERR,
        lines,
        &[Line::from(vec![
            modal_key("c"),
            Span::styled(
                " edit agent-orchestrator.yaml   ",
                Style::default().fg(MUTED),
            ),
            modal_key("any other key"),
            Span::styled(" quit", Style::default().fg(MUTED)),
        ])],
    );
}

/// Live progress modal painted while a tracker-install supervisor
/// is running. Spinner + tool tickmarks + tail of subprocess output.
/// Replaces the earlier "warnings + y/N prompt" flow — installing
/// tracker tools is fleet's job (we provision the VM), so the modal
/// shows progress without asking permission.
pub(super) fn render_tracker_install(
    frame: &mut Frame<'_>,
    install: &crate::tui::tracker_install::TrackerInstall,
) {
    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let n = SPINNER_FRAMES.len() as u64;
    let frame_idx = usize::try_from(install.elapsed_secs() % n).unwrap_or(0);
    let spinner = SPINNER_FRAMES[frame_idx];

    let header = Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Installing tracker tools", bold_key),
        Span::raw("   "),
        Span::styled(format!("{}s", install.elapsed_secs()), muted),
    ]);

    let mut lines = vec![header, Line::raw("")];

    // Step rows: one per tool. Already-finished steps show ✓ / ✗
    // based on exit code; in-flight tool shows a spinner.
    let steps = install.steps();
    for tool in install.tools() {
        let outcome = steps.iter().find(|s| &s.tool == tool);
        match outcome {
            Some(s) if s.code == 0 => {
                lines.push(Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(OK).add_modifier(Modifier::BOLD)),
                    Span::styled(tool.clone(), bold_key),
                ]));
            }
            Some(s) => {
                lines.push(Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(ERR).add_modifier(Modifier::BOLD)),
                    Span::styled(tool.clone(), bold_key),
                    Span::raw("  "),
                    Span::styled(format!("(exit {})", s.code), muted),
                ]));
            }
            None => {
                lines.push(Line::from(vec![
                    Span::styled(format!("{spinner} "), Style::default().fg(ACCENT)),
                    Span::styled(tool.clone(), bold_key),
                ]));
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("output (most recent):", muted)));
    let mut had_any = false;
    for raw in install.tail_lines() {
        had_any = true;
        let truncated = truncate(raw, 80);
        lines.push(Line::from(vec![
            Span::styled("  ", muted),
            Span::styled(truncated, muted),
        ]));
    }
    if !had_any {
        lines.push(Line::from(Span::styled(
            "  (starting…)",
            muted.add_modifier(Modifier::ITALIC),
        )));
    }

    let footer = vec![Line::from(Span::styled(
        "fleet manages tracker tools inside the VM — this completes automatically.",
        muted.add_modifier(Modifier::ITALIC),
    ))];
    draw_modal(frame, " tracker setup ", ACCENT, lines, &footer);
}

// Kept (unused for now) for the future case where we need a
// continue-or-quit preflight warning that *isn't* auto-remediated.
#[allow(dead_code)]
pub(super) fn render_tracker_warnings(
    frame: &mut Frame<'_>,
    warnings: &[crate::tui::preflight::MissingTracker],
) {
    let muted = Style::default().fg(MUTED);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Tracker tools missing — spawn picker will break for these:",
        muted,
    )));
    lines.push(Line::raw(""));
    for w in warnings {
        lines.push(Line::from(vec![
            Span::styled("✗ ", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled(
                w.plugin.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("(used by: {})", w.used_by.join(", ")),
                muted.add_modifier(Modifier::ITALIC),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("needs `{}` inside the fleet-vm guest", w.tool),
                muted,
            ),
        ]));
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled(
        "fleet can install these for you — they live inside the VM,",
        muted.add_modifier(Modifier::ITALIC),
    )));
    lines.push(Line::from(Span::styled(
        "and gh-in-VM reuses your host's `gh auth login` automatically.",
        muted.add_modifier(Modifier::ITALIC),
    )));

    draw_modal(
        frame,
        " tracker warnings ",
        WARN,
        lines,
        &[Line::from(vec![
            modal_key("y"),
            Span::styled(" install in VM   ", muted),
            modal_key("c"),
            Span::styled(" edit config   ", muted),
            modal_key("enter"),
            Span::styled(" continue   ", muted),
            modal_key("q"),
            Span::styled(" quit", muted),
        ])],
    );
}

/// VM missing — `limactl` is installed but no `fleet-vm` instance exists.
/// Offers to run `limactl start fleet-vm` (which creates the instance the
/// first time). 5–10 minute download + cloud-init, so the warning row sets
/// the expectation.
pub(super) fn render_vm_missing(frame: &mut Frame<'_>) {
    let lines = vec![
        Line::from(vec![
            Span::styled("✗ ", Style::default().fg(ERR).add_modifier(Modifier::BOLD)),
            Span::styled(
                "fleet-vm",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Lima instance does not exist", Style::default().fg(MUTED)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Fleet needs a Lima VM named `fleet-vm` to host the ao",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            "orchestrator and tmux sessions.",
            Style::default().fg(MUTED),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "Creating it downloads ~1 GB and runs cloud-init —",
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            "expect 5–10 minutes the first time.",
            Style::default().fg(WARN),
        )),
    ];
    draw_modal(frame, " vm setup ", WARN, lines, &footer_yn("create"));
}

/// VM stopped — instance exists, just isn't running. Offers `limactl
/// start fleet-vm`, which is quick (boot + a small cloud-init replay).
pub(super) fn render_vm_stopped(frame: &mut Frame<'_>) {
    let lines = vec![
        Line::from(vec![
            Span::styled("○ ", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled(
                "fleet-vm",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Lima instance is stopped", Style::default().fg(MUTED)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Fleet can't reach the ao orchestrator until the VM",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled("is running.", Style::default().fg(MUTED))),
    ];
    draw_modal(frame, " vm setup ", WARN, lines, &footer_yn("start"));
}

/// Live progress modal painted while [`crate::tui::bringup::BringUp`] is
/// in flight. Spinner cycles each draw; the body shows the tail of
/// limactl output so the user has something to watch other than an
/// elapsed-time counter.
pub(super) fn render_vm_bringup(frame: &mut Frame<'_>, bringup: &BringUp) {
    // Mod down to the frame index first so `try_from` can never fail.
    let n = SPINNER_FRAMES.len() as u64;
    let frame_idx = usize::try_from(bringup.elapsed_secs() % n).unwrap_or(0);
    let spinner = SPINNER_FRAMES[frame_idx];
    let header = Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Bringing up fleet-vm",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{}s", bringup.elapsed_secs()),
            Style::default().fg(MUTED),
        ),
    ]);

    let mut lines = vec![
        header,
        Line::raw(""),
        Line::from(Span::styled(
            "limactl output (most recent):",
            Style::default().fg(MUTED),
        )),
    ];
    let mut had_any = false;
    for raw in bringup.tail_lines() {
        had_any = true;
        // Indent so the tail block reads as a quoted subprocess log,
        // not as part of the modal's own copy. Truncate at a generous
        // width — the modal sizes itself to fit, but a stray 400-char
        // line shouldn't define the layout.
        let truncated = truncate_for_modal(raw, 80);
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(MUTED)),
            Span::styled(truncated, Style::default().fg(MUTED)),
        ]));
    }
    if !had_any {
        lines.push(Line::from(Span::styled(
            "  (waiting for limactl…)",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
    }

    let footer = vec![Line::from(Span::styled(
        "Cloud-init can take 5–10 minutes on first boot.",
        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
    ))];
    draw_modal(frame, " vm setup ", ACCENT, lines, &footer);
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn truncate_for_modal(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn footer_yn(verb: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        modal_key("y"),
        Span::raw(" "),
        Span::styled(verb.to_string(), Style::default().fg(MUTED)),
        Span::raw("   "),
        modal_key("q"),
        Span::raw(" "),
        Span::styled("quit", Style::default().fg(MUTED)),
    ])]
}

/// Generic centred modal: title chrome + body + footer, separated by a
/// blank row. Body sizes the modal; the footer comes after another blank
/// row so the action keys read as a discrete bar at the bottom.
fn draw_modal(
    frame: &mut Frame<'_>,
    title_text: &str,
    title_color: ratatui::style::Color,
    body: Vec<Line<'static>>,
    footer: &[Line<'static>],
) {
    draw_modal_sized(
        frame,
        title_text,
        title_color,
        body,
        footer,
        ModalWidth::Auto,
    );
}

/// How a modal picks its column width when laid out.
///
/// `Auto` sizes to the longest content line — fine for modals whose
/// content is stable across frames. `Fixed` pins the width to a
/// caller-chosen column count, which the spawn picker needs so the
/// modal doesn't shrink/grow as the user filters the issue list.
#[derive(Clone, Copy)]
enum ModalWidth {
    Auto,
    Fixed(u16),
}

fn draw_modal_sized(
    frame: &mut Frame<'_>,
    title_text: &str,
    title_color: ratatui::style::Color,
    body: Vec<Line<'static>>,
    footer: &[Line<'static>],
    width: ModalWidth,
) {
    let mut lines = body;
    if !footer.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(footer.iter().cloned());
    }
    let width = match width {
        ModalWidth::Auto => lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or(40)
            .max(40)
            .saturating_add(6), // 2 borders + 4 horizontal padding
        ModalWidth::Fixed(w) => usize::from(w),
    };
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2); // borders only; padding is horizontal-only
    let area = center_rect(
        frame.area(),
        u16::try_from(width).unwrap_or(u16::MAX),
        height,
    );

    let title = Line::from(vec![Span::styled(
        title_text.to_string(),
        Style::default()
            .fg(title_color)
            .add_modifier(Modifier::BOLD),
    )]);
    let block = framed_block_titled(title).padding(Padding::horizontal(2));

    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn center_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::shorten_worktree;

    #[test]
    fn shorten_worktree_elides_homedir_prefix() {
        let input = "/home/niklas.guest/.agent-orchestrator/projects/sandbox/worktrees/sb-1";
        assert_eq!(shorten_worktree(input), "…/projects/sandbox/worktrees/sb-1");
    }

    #[test]
    fn shorten_worktree_falls_back_to_truncate_for_unexpected_shape() {
        // Path doesn't contain the expected marker → don't try to
        // be clever, just truncate at the char cap.
        let input = "/some/other/place/where/ao/might/store/work";
        let out = shorten_worktree(input);
        assert!(out.starts_with("/some/other/place"));
        assert!(out.chars().count() <= 56);
    }
}
