//! Rendering — pure `(&AppState, &mut Frame) -> ()` plumbing.
//!
//! No state mutation here; the input layer ([`super::input`]) drives
//! all changes via [`super::app::AppState`]. Display-side helpers
//! (state markers/words, label formatting, deps rendering) also live
//! here because they only exist to produce frames.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};

use crate::orchestrator::{OrchestratorSession, OrchestratorState};
use crate::plans::{Plan, PlanItemState, PlanState};
use crate::session::{Session, SessionState};

use super::app::{AppState, IssuesState, Overlay, PlansFocus, SessionsFocus, SpawnTab, View};
use super::theme::{
    ACCENT, ERR, IDENT, MUTED, OK, SELECT_BG, SELECT_FG, WARN, badge, chip, framed_block,
    framed_block_accent, framed_block_accent_titled, framed_block_titled, key as theme_key,
    kv_line, label_chip, modal_key, sep,
};

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
    render_breadcrumb(f, outer[0], state);
    let body_area = outer[1];
    match state.view {
        View::Sessions | View::Spawn => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(body_area);
            render_sidebar(f, body[0], state);
            render_detail(f, body[1], state);
        }
        View::Doctor => {
            render_doctor(f, body_area, state);
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
            render_plans_sidebar(f, body[0], state);
            render_plans_detail(f, body[1], state);
            render_plans_ticket(f, body[2], state);
        }
    }
    if state.view == View::Spawn {
        // Taller + wider than the old workflow-only picker — needs
        // room for the filter line, tab strip, scrollable list, and
        // help footer without the issue title getting truncated to a
        // useless prefix.
        let modal = centered_rect(body_area, 78, 75);
        f.render_widget(Clear, modal);
        render_spawn(f, modal, state);
    }
    match &state.overlay {
        Overlay::Confirm { prompt, .. } => {
            let modal = centered_rect(body_area, 50, 30);
            f.render_widget(Clear, modal);
            render_confirm(f, modal, prompt);
        }
        Overlay::Error { message } => {
            let modal = centered_rect(body_area, 50, 25);
            f.render_widget(Clear, modal);
            render_error(f, modal, message);
        }
        Overlay::None => {}
    }
    render_status(f, outer[2], state);
}

/// Top breadcrumb row. Left side: `fleet | <repo> | <session-id> <state>`
/// (or `(no sessions)` when nothing's selected). Right side: a pair of
/// dot badges for the runtime adapter (probed at startup) and the
/// autonomous engine (off until `Shift+A`).
fn render_breadcrumb(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(22)])
        .split(area);

    let repo_chip = state
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("fleet");
    let muted = Style::default().fg(MUTED);
    let mut left: Vec<Span<'_>> = vec![
        chip("fleet", ACCENT),
        Span::styled(" | ", muted),
        chip(repo_chip, ACCENT),
        Span::styled(" | ", muted),
    ];
    match state.selected() {
        Some(s) => {
            left.push(Span::raw(s.id.to_string()));
            left.push(Span::raw(" "));
            left.push(Span::styled(state_word(s.state).to_string(), muted));
        }
        None => {
            left.push(Span::styled("(no sessions)", muted));
        }
    }
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);

    // Right: runtime + autonomous dots. `runtime:●` derives from the
    // doctor snapshot probed in `terminal::run`; muted when missing
    // (e.g. tests that bypass `run`). `auto:●` is green when the
    // engine is on, muted when off.
    let runtime_color = match state.doctor.as_ref() {
        Some(d) if d.adapter.is_ok() => OK,
        Some(_) => ERR,
        None => MUTED,
    };
    let auto_color = if state.autonomous.enabled() {
        OK
    } else {
        MUTED
    };
    let right = Line::from(vec![
        Span::styled("runtime", muted),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(runtime_color)),
        Span::raw("  "),
        Span::styled("auto", muted),
        Span::raw(":"),
        Span::styled("●", Style::default().fg(auto_color)),
        Span::raw(" "),
    ]);
    f.render_widget(
        Paragraph::new(right).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

fn render_confirm(f: &mut Frame<'_>, area: Rect, prompt: &str) {
    let muted = Style::default().fg(MUTED);
    let mut lines: Vec<Line<'static>> = prompt
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default())))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        modal_key("y"),
        Span::styled(" yes    ", muted),
        modal_key("n"),
        Span::styled(" / ", muted),
        modal_key("esc"),
        Span::styled(" cancel", muted),
    ]));
    let title = Line::from(Span::styled(
        " Confirm ",
        Style::default().fg(WARN).add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(WARN))
        .title(title)
        .padding(Padding::horizontal(2));
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_error(f: &mut Frame<'_>, area: Rect, message: &str) {
    let muted = Style::default().fg(MUTED);
    let mut lines: Vec<Line<'static>> =
        message.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("press any key to dismiss", muted)));
    let title = Line::from(Span::styled(
        " Error ",
        Style::default().fg(ERR).add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ERR))
        .title(title)
        .padding(Padding::horizontal(2));
    let body = Paragraph::new(lines)
        .block(block)
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
    let block = framed_block(" doctor ").padding(Padding::horizontal(2));
    let Some(snapshot) = state.doctor.as_ref() else {
        let body = Paragraph::new(Span::styled(
            "(probing host...)",
            Style::default().fg(MUTED),
        ))
        .block(block);
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
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
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
                Span::styled(format!("{:<14}", "resolved"), Style::default().fg(MUTED)),
                Span::styled(msg.clone(), Style::default().fg(ERR)),
            ]));
        }
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "agents:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    if snapshot.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(MUTED),
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
    // The spawn picker is the active navigation target while open —
    // accent border to match the Plans-view focus convention.
    let outer = framed_block_accent(" spawn session ").padding(Padding::horizontal(2));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    // Three horizontal bands inside the modal:
    //   - top:    filter + tab strip (3 rows)
    //   - middle: scrollable list (rest)
    //   - footer: count + help line (2 rows)
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);

    render_spawn_header(f, rows[0], state);
    render_spawn_body(f, rows[1], state);
    render_spawn_footer(f, rows[2], state);
}

/// Filter input on row 0, tab strip on row 2 (blank row between for
/// breathing room). The block-style caret on the filter mirrors the
/// old AO-era spawn modal so the cursor location is unambiguous.
fn render_spawn_header(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let muted = Style::default().fg(MUTED);
    let bold_accent = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    let filter_line = Line::from(vec![
        Span::styled("filter   ", muted),
        Span::styled(state.spawn.filter.clone(), bold_accent),
        Span::styled("█", Style::default().fg(ACCENT)),
    ]);

    let active = state.spawn.tab;
    let tab = |label: &str, this_tab: SpawnTab| -> Span<'static> {
        if active == this_tab {
            Span::styled(
                format!("[ {label} ]"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("  {label}  "), muted)
        }
    };
    let tab_line = Line::from(vec![
        tab("Issue", SpawnTab::Issue),
        Span::raw("  "),
        tab("Workflow", SpawnTab::Workflow),
        Span::raw("        "),
        Span::styled("(Tab to switch)", muted.add_modifier(Modifier::ITALIC)),
    ]);

    let lines = vec![filter_line, Line::from(""), tab_line];
    f.render_widget(Paragraph::new(lines), area);
}

fn render_spawn_body(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    match state.spawn.tab {
        SpawnTab::Issue => render_spawn_issues(f, area, state),
        SpawnTab::Workflow => render_spawn_workflows(f, area, state),
    }
}

fn render_spawn_issues(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let muted = Style::default().fg(MUTED);
    // Placeholders for the non-Loaded states. `Loaded` falls through
    // to the list-widget render below.
    match &state.spawn.issues {
        IssuesState::Loading => {
            let line = Line::from(Span::styled(
                "loading issues from tracker…",
                muted.add_modifier(Modifier::ITALIC),
            ));
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        IssuesState::Unsupported { plugin } => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("tracker `", muted),
                    Span::styled(plugin.clone(), Style::default().fg(WARN)),
                    Span::styled("` — listing not implemented.", muted),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Type the issue id manually and press Enter, or Tab to the",
                    muted,
                )),
                Line::from(Span::styled(
                    "Workflow tab to fire a workflow without binding an issue.",
                    muted,
                )),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        IssuesState::Error(msg) => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("tracker error: ", Style::default().fg(ERR)),
                    Span::styled(
                        truncate(msg, 80).into_owned(),
                        Style::default().fg(ERR).add_modifier(Modifier::ITALIC),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Type the issue id manually below and press Enter,",
                    muted,
                )),
                Line::from(Span::styled("or Tab to the Workflow tab.", muted)),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        IssuesState::Loaded(_) => {}
    }
    let filtered = state.spawn.filtered_issues();
    if filtered.is_empty() {
        let total_loaded = match &state.spawn.issues {
            IssuesState::Loaded(all) => all.len(),
            _ => 0,
        };
        let placeholder = if total_loaded == 0 {
            "(no issues in this repo — type an id manually and press Enter)".to_string()
        } else if state.spawn.filter.is_empty() {
            "(no open issues — press `o` to show closed)".to_string()
        } else {
            format!(
                "no issues match `{}` — press Enter to submit it as a manual id",
                state.spawn.filter
            )
        };
        let line = Line::from(Span::styled(
            placeholder,
            muted.add_modifier(Modifier::ITALIC),
        ));
        f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), area);
        return;
    }
    let items: Vec<ListItem<'_>> = filtered
        .iter()
        .map(|issue| {
            let resolved = state.resolved_workflow_for(issue);
            let overridden = state.spawn.workflow_override.contains_key(&issue.human_id);
            ListItem::new(spawn_issue_rows(issue, &resolved, overridden))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(SELECT_FG)
                .bg(SELECT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(Some(state.spawn.issue_idx.min(filtered.len() - 1)));
    f.render_stateful_widget(list, area, &mut list_state);
}

/// Two-row issue card: id+title+status on row 0, dim resolved-workflow
/// hint + labels on row 1. Two rows is busier than the old one-row UX
/// but the routing hint is the new affordance that earns its keep.
fn spawn_issue_rows(
    issue: &crate::tracker::Issue,
    resolved_workflow: &str,
    overridden: bool,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let id_style = Style::default().fg(IDENT).add_modifier(Modifier::BOLD);
    let status_color = match issue.status.as_str() {
        "open" => OK,
        "closed" => MUTED,
        _ => WARN,
    };
    let primary = Line::from(vec![
        Span::styled(format!("{:<10}", issue.human_id), id_style),
        Span::raw(truncate(&issue.title, 60).into_owned()),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", issue.status),
            Style::default().fg(status_color),
        ),
    ]);
    let mut secondary_spans: Vec<Span<'static>> = Vec::with_capacity(4);
    let arrow_style = if overridden {
        // Highlight the override so the user knows they've moved off
        // the auto-routed default.
        Style::default().fg(WARN)
    } else {
        muted
    };
    secondary_spans.push(Span::styled("            → ", arrow_style));
    secondary_spans.push(Span::styled(resolved_workflow.to_string(), arrow_style));
    if overridden {
        secondary_spans.push(Span::styled(" (override)", arrow_style));
    }
    if !issue.labels.is_empty() {
        secondary_spans.push(Span::styled("   ", muted));
        for (i, l) in issue.labels.iter().enumerate() {
            if i > 0 {
                secondary_spans.push(Span::raw(" "));
            }
            secondary_spans.push(label_chip(l));
        }
    }
    vec![primary, Line::from(secondary_spans)]
}

fn render_spawn_workflows(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let muted = Style::default().fg(MUTED);
    // Two empty-state shapes:
    //   - no workflows on disk → tell the user to `fleet init`.
    //   - workflows exist but none opt in via `trigger.issueless:
    //     true` → explain the opt-in so the user knows where the
    //     filtering is coming from.
    if state.spawn.workflows.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "(no workflows under .fleet/workflows/)",
                muted,
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Run `fleet init` to scaffold the defaults, or drop a",
                muted,
            )),
            Line::from(Span::styled(
                "`<name>.yaml` into `.fleet/workflows/` by hand.",
                muted,
            )),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }
    let filtered = state.spawn.filtered_workflows();
    if filtered.is_empty() {
        let any_issueless = state.spawn.workflows.iter().any(|w| w.issueless);
        let lines = if any_issueless {
            vec![Line::from(Span::styled(
                format!("no workflows match `{}`", state.spawn.filter),
                muted.add_modifier(Modifier::ITALIC),
            ))]
        } else {
            vec![
                Line::from(Span::styled("(no issueless workflows defined)", muted)),
                Line::from(""),
                Line::from(Span::styled(
                    "The Workflow tab only shows workflows that opt in via",
                    muted,
                )),
                Line::from(Span::styled(
                    "`trigger.issueless: true` in their YAML — e.g. a",
                    muted,
                )),
                Line::from(Span::styled(
                    "refactor-candidate scan or a codebase audit.",
                    muted,
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Workflows that need a ticket live on the Issue tab.",
                    muted,
                )),
            ]
        };
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }
    let items: Vec<ListItem<'_>> = filtered
        .iter()
        .map(|entry| ListItem::new(Line::from(entry.name.clone())))
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(SELECT_FG)
                .bg(SELECT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(Some(state.spawn.workflow_idx.min(filtered.len() - 1)));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_spawn_footer(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let muted = Style::default().fg(MUTED);
    let count_line = match state.spawn.tab {
        SpawnTab::Issue => match &state.spawn.issues {
            IssuesState::Loaded(all) => {
                let shown = state.spawn.filtered_issues().len();
                let mut chunks: Vec<Span<'static>> = vec![Span::styled(
                    format!("{shown} of {} issues", all.len()),
                    muted,
                )];
                if state.spawn.show_closed {
                    chunks.push(Span::styled(
                        "  ·  showing closed".to_string(),
                        muted.add_modifier(Modifier::ITALIC),
                    ));
                }
                Line::from(chunks)
            }
            _ => Line::from(""),
        },
        SpawnTab::Workflow => {
            let shown = state.spawn.filtered_workflows().len();
            let total_issueless = state.spawn.workflows.iter().filter(|w| w.issueless).count();
            Line::from(Span::styled(
                format!("{shown} of {total_issueless} issueless workflows"),
                muted,
            ))
        }
    };
    let mut help: Vec<Span<'static>> = vec![
        modal_key("↑↓"),
        Span::styled(" pick  ", muted),
        modal_key("Tab"),
        Span::styled(" switch  ", muted),
    ];
    if state.spawn.tab == SpawnTab::Issue {
        help.push(modal_key("o"));
        help.push(Span::styled(" closed  ", muted));
        help.push(modal_key("w"));
        help.push(Span::styled(" workflow  ", muted));
    }
    help.push(modal_key("Enter"));
    help.push(Span::styled(" spawn  ", muted));
    help.push(modal_key("Esc"));
    help.push(Span::styled(" cancel", muted));
    f.render_widget(Paragraph::new(vec![count_line, Line::from(help)]), area);
}

/// Borrowed/owned-friendly truncation: returns the original string by
/// reference when it already fits, owned otherwise. Saves an
/// allocation on the common case (most issue titles are short).
fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    std::borrow::Cow::Owned(format!("{truncated}…"))
}

fn render_plans_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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

fn render_plans_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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

fn render_plans_ticket(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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
pub(super) fn ticket_detail_lines(state: &AppState) -> Vec<Line<'static>> {
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
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for edge in &blocked_on {
            lines.push(dep_row(&edge.blocked_on, edge.reason, titles));
        }
    }
    if !blocks.is_empty() {
        if !blocked_on.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Blocks ({}):", blocks.len()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
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
pub(super) fn issue_detail_lines(detail: &crate::tracker::IssueDetail) -> Vec<Line<'static>> {
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
    lines.push(Line::from(Span::styled(
        "Body:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
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
pub(super) fn plan_row_label(
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
    out.push(Line::from(Span::styled(
        format!("Items ({total})"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    out
}

/// Combined header + items as a flat `Vec<Line>` — the shape the
/// existing tests assert on. Production rendering takes a different
/// path (see `render_plans_detail`) so the items can use a List widget
/// with a full-row selection highlight, but the assertion surface
/// here stays stable.
#[cfg(test)]
#[must_use]
pub(super) fn plan_detail_lines(
    plan: &Plan,
    selected_item: Option<usize>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let mut out = plan_header_lines(plan, titles);
    for (idx, item) in plan.items.iter().enumerate() {
        out.push(plan_item_line(
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
) -> Line<'static> {
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
    if let Some(session) = &item.session_id {
        spans.push(Span::styled("  · session ", muted));
        spans.push(Span::styled(
            session.to_string(),
            Style::default().fg(IDENT),
        ));
    }
    Line::from(spans)
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
        PlanItemState::InProgress => Style::default().fg(ACCENT),
        PlanItemState::Completed | PlanItemState::Skipped => Style::default().fg(MUTED),
        PlanItemState::Failed => Style::default().fg(ERR),
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
pub fn orchestrator_row_label(session: &OrchestratorSession) -> String {
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

fn render_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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
        .map(|(k, v)| kv_line(k, &v))
        .collect();
    if !session.node_costs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "node costs:",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for (node, usd) in &session.node_costs {
            lines.push(Line::from(format!("  {node:<16} ${usd:.4}")));
        }
    }
    lines
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
    // orchestrator's tmux window to the panel width. Packed as
    // `(w << 16) | h`. Zero stays zero until we actually have a non-
    // empty area — keeps the first capture from running before we
    // know how wide the panel is.
    let packed = (u32::from(inner.width) << 16) | u32::from(inner.height);
    state
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
    // Parse `tmux capture-pane -e` ANSI escapes into styled spans so
    // Claude Code's colors come through. On parse failure, fall back
    // to plain text — a malformed capture should never blank the
    // pane.
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
fn orchestrator_state_word(state: OrchestratorState) -> &'static str {
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

fn render_status(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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
