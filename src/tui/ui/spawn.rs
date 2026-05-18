//! Spawn picker modal — filter input, tab strip, scrollable Issue /
//! Workflow list, count + help footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap};

use crate::tui::app::{AppState, IssuesState, SpawnTab};
use crate::tui::theme::{
    ACCENT, ERR, IDENT, MUTED, OK, SELECT_BG, SELECT_FG, WARN, framed_block_accent, label_chip,
    modal_key,
};

use super::truncate;

pub(super) fn render_spawn(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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
