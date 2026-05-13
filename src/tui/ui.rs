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
use ratatui::widgets::{Clear, List, ListItem, ListState, Padding, Paragraph, Wrap};

use crate::ao::SessionInfo;
use crate::lima::VmStatus;

use super::app::{App, IssuesState, SpawnPrompt};
use super::bringup::BringUp;
use super::preflight::MissingDep;
use super::theme::{
    ACCENT, ERR, KEY_FG, MUTED, OK, WARN, badge, chip, framed_block, framed_block_titled, key,
    kv_line, sep,
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

    // Overlay modals — painted last so they sit on top of the normal
    // layout. The spawn prompt punches a `Clear` through the frame so
    // background content can't leak through.
    if let Some(prompt) = &app.spawn_prompt {
        render_spawn_prompt(frame, prompt);
    }
}

fn draw_breadcrumb(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Left chunk: project chain. Right chunk: AO/VM badges.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(18)])
        .split(area);

    let project_chip = app
        .current_project_key
        .as_deref()
        .unwrap_or("(unscoped)");
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
        .constraints([Constraint::Length(24), Constraint::Min(0)])
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
    // Real session rows + trailing "+ new session" sentinel. The
    // sentinel renders italic-accent and is always selectable, so an
    // empty list is never a dead-end — pressing Enter on the only
    // visible row spawns.
    let mut items: Vec<ListItem<'_>> = app
        .sessions
        .iter()
        .map(|s| {
            let id = s.id.clone().unwrap_or_else(|| "?".into());
            let activity = s.activity.clone().unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{id:<8}")),
                Span::styled(activity, Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(Span::styled(
        "+ new session",
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::ITALIC),
    ))));

    let title = format!(" sessions ({}) ", app.sessions.len());
    let list = List::new(items)
        .block(framed_block(&title))
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut state = ListState::default();
    state.select(Some(app.selected));
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
    if area.height < 2 || area.width < 2 {
        return;
    }
    let inner_x = area.x.saturating_add(1);
    let inner_y = area.y.saturating_add(1);
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    // Include the sentinel row so it's clickable.
    let total_rows = app.sidebar_row_count();
    rects.resize(total_rows, Rect::default());
    let visible = inner_h.min(u16::try_from(total_rows).unwrap_or(u16::MAX));
    for row in 0..visible {
        rects[row as usize] = Rect {
            x: inner_x,
            y: inner_y + row,
            width: inner_w,
            height: 1,
        };
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
    let bold_key = Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD);

    if app.current_project_key.is_none() {
        return vec![
            Line::from(Span::styled(
                "This directory isn't a known fleet project.",
                muted,
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  Press ", muted),
                Span::styled("c", bold_key),
                Span::styled(
                    "  to edit agent-orchestrator.yaml and add a `path:`",
                    muted,
                ),
            ]),
            Line::from(Span::styled(
                "  entry that points at this directory, then press ",
                muted,
            )),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("r", bold_key),
                Span::styled("  to reload.", muted),
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
                Span::styled("⇧S", bold_key),
                Span::styled(
                    "      start the orchestrator inside the VM",
                    muted,
                ),
            ]),
            Line::raw(""),
            Line::from(Span::styled("  Once it's up:", muted)),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("n", bold_key),
                Span::styled(
                    "       spawn a session on an issue",
                    muted,
                ),
            ]),
            Line::from(vec![
                Span::styled("  ", muted),
                Span::styled("t", bold_key),
                Span::styled(
                    "       open the tracker (git-bug) to create one",
                    muted,
                ),
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
            Span::styled(
                "       spawn a session on an issue",
                muted,
            ),
        ]),
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled("t", bold_key),
            Span::styled(
                "       open the tracker to browse / create issues",
                muted,
            ),
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

    let spans: Vec<Span<'_>> = vec![
        chip("[fleet]", ACCENT),
        sep(),
        key("↑/↓"),
        Span::raw(" nav"),
        sep(),
        key("Enter"),
        Span::raw(" attach"),
        sep(),
        key("n"),
        Span::raw(" new"),
        sep(),
        key("t"),
        Span::raw(" tracker"),
        sep(),
        key("c"),
        Span::raw(" edit config"),
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
pub(super) fn render_spawn_prompt(frame: &mut Frame<'_>, prompt: &SpawnPrompt) {
    const MAX_ROWS: usize = 12;

    let muted = Style::default().fg(MUTED);
    let bold_key = Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Filter input with block caret.
    lines.push(Line::from(vec![
        Span::styled("filter      ", muted),
        Span::styled(prompt.buffer.clone(), bold_key),
        Span::styled("█", Style::default().fg(KEY_FG)),
    ]));
    lines.push(Line::raw(""));

    // Body: depends on the issue-fetch state.
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
                Span::styled(
                    "` — listing not supported; type the issue id.",
                    muted,
                ),
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
        IssuesState::Loaded(_) => {
            // Build the filtered window with selection chrome.
            let filtered = prompt.filtered();
            let total = match &prompt.issues {
                IssuesState::Loaded(all) => all.len(),
                _ => 0,
            };
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
                for (offset, idx) in window.clone().enumerate() {
                    let issue = filtered[idx];
                    let focused = idx == selected;
                    lines.push(issue_line(issue, focused));
                    let _ = offset;
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

    let footer = vec![Line::from(vec![
        Span::styled("[↑↓]", bold_key),
        Span::styled(" pick    ", muted),
        Span::styled("[Enter]", bold_key),
        Span::styled(" submit    ", muted),
        Span::styled("[Esc]", bold_key),
        Span::styled(" cancel", muted),
    ])];
    draw_modal(frame, " spawn session ", ACCENT, lines, &footer);
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
        Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD)
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

pub(super) fn render_preflight(frame: &mut Frame<'_>, failures: &[MissingDep]) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Fleet needs the following on this host:",
        Style::default().fg(MUTED),
    )));
    lines.push(Line::raw(""));
    for f in failures {
        lines.push(Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default().fg(ERR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                f.name.to_string(),
                Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
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
        &[Line::from(Span::styled(
            "Press any key to quit.",
            Style::default().fg(MUTED),
        ))],
    );
}

/// VM missing — `limactl` is installed but no `fleet-vm` instance exists.
/// Offers to run `limactl start fleet-vm` (which creates the instance the
/// first time). 5–10 minute download + cloud-init, so the warning row sets
/// the expectation.
pub(super) fn render_vm_missing(frame: &mut Frame<'_>) {
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default().fg(ERR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "fleet-vm",
                Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "Lima instance does not exist",
                Style::default().fg(MUTED),
            ),
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
            Span::styled(
                "○ ",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "fleet-vm",
                Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Lima instance is stopped", Style::default().fg(MUTED)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Fleet can't reach the ao orchestrator until the VM",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            "is running.",
            Style::default().fg(MUTED),
        )),
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
            Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
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
        Span::styled(
            "[y]",
            Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(verb.to_string(), Style::default().fg(MUTED)),
        Span::raw("   "),
        Span::styled(
            "[q]",
            Style::default().fg(KEY_FG).add_modifier(Modifier::BOLD),
        ),
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
    let mut lines = body;
    if !footer.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(footer.iter().cloned());
    }
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(40)
        .max(40)
        .saturating_add(6); // 2 borders + 4 horizontal padding
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
