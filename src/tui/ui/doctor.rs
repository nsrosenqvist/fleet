//! Doctor view — host + adapter + agents probe summary.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph, Wrap};

use crate::tui::app::AppState;
use crate::tui::theme::{ERR, MUTED, framed_block, kv_line};

pub(super) fn render_doctor(f: &mut Frame<'_>, area: Rect, state: &AppState) {
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

    lines.push(super::section_heading("runtime adapter:"));
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

    lines.push(super::section_heading("agents:"));
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
