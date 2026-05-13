//! Per-mode keyboard handlers.
//!
//! Each handler is `(&mut App, KeyEvent) -> ()`. Side effects that need the
//! terminal handle (interactive children, suspending the alternate screen)
//! are deferred by pushing a [`Command`] onto the app's command queue —
//! [`crate::tui::terminal`] drains the queue after each draw.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::app::{App, ClickKind, ClickTarget, Command, Confirm};

/// Default mode: navigation + action keys. Falls through to no-op on
/// unknown keys so e.g. `Shift+F1` doesn't accidentally fire an action.
pub(super) fn handle_key_normal(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        // Quit (`q`, `Ctrl+C`). The Ctrl+C arm is listed first so it
        // wins over the plain `c` arm below.
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        (KeyCode::Down | KeyCode::Char('j'), _) => app.nav_down(),
        (KeyCode::Up, _) => app.nav_up(),
        // `k` alone navigates up (vim). Holding shift uses `K` for kill.
        (KeyCode::Char('k'), m) if !m.contains(KeyModifiers::SHIFT) => app.nav_up(),

        (KeyCode::Char('r'), _) => app.push_command(Command::RequestRefresh),
        (KeyCode::Enter, _) => {
            // Sentinel row → open the spawn modal instead of attaching.
            // Orchestrator entry → read-only pane view (no attach);
            // fleet covers the orchestrator's coordination role via
            // the spawn modal, so dropping into its tmux session
            // would just expose a redundant claude REPL. The sidebar
            // tags the row `(read-only)` for discoverability.
            if app.is_sentinel_selected() {
                app.open_spawn_prompt();
            } else if app.is_orchestrator_selected() {
                // Silent no-op. Selection already shows the
                // orchestrator's pane in the output panel.
            } else {
                app.push_command(Command::AttachSelected);
            }
        }

        (KeyCode::Char('K') | KeyCode::Delete, _) => {
            if let Some(id) = app.selected_session_id() {
                app.confirm = Some(Confirm::KillSession(id));
            }
        }
        (KeyCode::Char('c'), _) => app.push_command(Command::EditConfig),
        (KeyCode::Char('t'), _) => app.push_command(Command::TrackerPreview),
        (KeyCode::Char('T'), _) => app.push_command(Command::TrackerWeb),
        (KeyCode::Char('n'), _) => app.open_spawn_prompt(),
        (KeyCode::Char('S'), _) => app.push_command(Command::StartAo),
        (KeyCode::Char('X'), _) => {
            app.confirm = Some(Confirm::StopAo);
        }
        (KeyCode::Char('W'), _) => app.push_command(Command::OpenWeb),
        _ => {}
    }
}

/// Spawn-prompt mode: filter the issue list with the buffer, navigate
/// the filtered rows with arrow keys, submit the selected row (or the
/// raw buffer when no list / no match) on Enter, cancel on Esc.
pub(super) fn handle_key_spawn_prompt(app: &mut App, key: KeyEvent) {
    let Some(prompt) = app.spawn_prompt.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Enter => {
            // Prefer the picker row when one is highlighted — the
            // user just spent effort filtering down to it. Fall back
            // to the raw buffer for the "tracker unsupported" case
            // or when the list has no matches.
            let filtered = prompt.filtered();
            let chosen: Option<String> = filtered
                .get(prompt.selected_idx)
                .map(|i| i.human_id.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let trimmed = prompt.buffer.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });
            let Some(issue) = chosen else {
                return;
            };
            app.spawn_prompt = None;
            app.push_command(Command::Spawn(issue));
        }
        KeyCode::Esc => {
            // Just dismiss; no "cancelled" flash — the user pressed
            // Esc themselves, they know the prompt closed, and a
            // flash would just hide the legend until they pressed
            // another key.
            app.spawn_prompt = None;
        }
        KeyCode::Down | KeyCode::Tab => {
            let n = prompt.filtered().len();
            if n > 0 {
                prompt.selected_idx = (prompt.selected_idx + 1) % n;
            }
        }
        KeyCode::Up | KeyCode::BackTab => {
            let n = prompt.filtered().len();
            if n > 0 {
                prompt.selected_idx = (prompt.selected_idx + n - 1) % n;
            }
        }
        KeyCode::Backspace => {
            prompt.buffer.pop();
            app.spawn_prompt_reset_selection();
        }
        KeyCode::Char(c) if !c.is_control() => {
            prompt.buffer.push(c);
            app.spawn_prompt_reset_selection();
        }
        _ => {}
    }
}

/// Secret-setup mode: collect the user's Claude OAuth token in a
/// masked buffer. Enter submits to the keychain + config writer
/// (via `Command::SaveOauthToken`); Esc cancels. Other keys editing
/// the buffer also clear any in-modal error so the user sees they
/// recovered.
pub(super) fn handle_key_secret_setup(app: &mut App, key: KeyEvent) {
    let Some(setup) = app.secret_setup.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Enter => {
            let token = setup.buffer.trim().to_string();
            if token.is_empty() {
                setup.error = Some("token is empty".to_string());
                return;
            }
            app.push_command(Command::SaveOauthToken(token));
        }
        KeyCode::Esc => {
            app.secret_setup = None;
        }
        KeyCode::Backspace => {
            setup.buffer.pop();
            setup.error = None;
        }
        KeyCode::Char(c) if !c.is_control() => {
            setup.buffer.push(c);
            setup.error = None;
        }
        _ => {}
    }
}

/// Confirm mode: `y`/`Y` resolves to the pending action, anything else
/// cancels. Always returns to Normal mode (by clearing `app.confirm`).
pub(super) fn handle_key_confirm(app: &mut App, key: KeyEvent) {
    let Some(pending) = app.confirm.take() else {
        return;
    };
    if let KeyCode::Char('y' | 'Y') = key.code {
        match pending {
            Confirm::KillSession(id) => app.push_command(Command::KillSession(id)),
            Confirm::StopAo => app.push_command(Command::StopAo),
            Confirm::RestartAoForOrchestrator => {
                app.push_command(Command::RestartAoForOrchestrator);
            }
        }
    }
    // Cancel path is silent — the user explicitly pressed a non-y
    // key to back out, so a confirmation flash would just be noise.
}

/// Normal-mode mouse handler. Left-click on a sidebar row selects it;
/// double-click within `DOUBLE_CLICK_WINDOW` attaches (same effect as
/// Enter). Scroll wheel events are ignored — moving the selection on
/// scroll surprised users who expected the wheel to be inert.
/// Clicks outside any tracked rect are no-ops.
pub(super) fn handle_mouse_normal(app: &mut App, me: MouseEvent) {
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Borrow the rect buffer in a tight scope so it's released
            // before we mutate `app` further.
            let hit = {
                let rects = app.sidebar_item_rects.borrow();
                hit_test(&rects, me.column, me.row)
            };
            let Some(idx) = hit else { return };
            let target = ClickTarget::SidebarItem(idx);
            match app.resolve_click(target) {
                ClickKind::Select => app.select_at(idx),
                ClickKind::Activate => {
                    // Make sure the activation runs against the row we
                    // just clicked, even if a stale selection still
                    // points elsewhere.
                    app.select_at(idx);
                    if app.is_sentinel_selected() {
                        app.open_spawn_prompt();
                    } else if app.is_orchestrator_selected() {
                        // Read-only — see handle_key_normal.
                    } else {
                        app.push_command(Command::AttachSelected);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Find the index of the first rect that contains `(col, row)`. Linear
/// scan — these vecs are tiny (one entry per visible session) so a
/// y-sorted binary search wouldn't pay back the added complexity.
fn hit_test(rects: &[Rect], col: u16, row: u16) -> Option<usize> {
    rects.iter().position(|r| rect_contains(*r, col, row))
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    #[test]
    fn hit_test_returns_first_matching_rect() {
        let rects = vec![r(1, 1, 10, 1), r(1, 2, 10, 1), r(1, 3, 10, 1)];
        assert_eq!(hit_test(&rects, 5, 2), Some(1));
    }

    #[test]
    fn hit_test_misses_outside_all_rects() {
        let rects = vec![r(1, 1, 10, 1)];
        assert_eq!(hit_test(&rects, 50, 50), None);
    }

    #[test]
    fn hit_test_skips_zero_area_rects_for_offscreen_rows() {
        // Off-screen rows are stored as Rect::default() — width=0,
        // height=0. A click that happens to land at (0,0) must not match
        // those slots.
        let rects = vec![Rect::default(), r(1, 1, 10, 1)];
        assert_eq!(hit_test(&rects, 0, 0), None);
        assert_eq!(hit_test(&rects, 5, 1), Some(1));
    }
}
