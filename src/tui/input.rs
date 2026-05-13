//! Per-mode keyboard handlers.
//!
//! Each handler is `(&mut App, KeyEvent) -> ()`. Side effects that need the
//! terminal handle (interactive children, suspending the alternate screen)
//! are deferred by pushing a [`Command`] onto the app's command queue —
//! [`crate::tui::terminal`] drains the queue after each draw.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::app::{App, ClickKind, ClickTarget, Command, Confirm, RegisterField};

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
            // Killing the orchestrator standalone leaves the daemon
            // up with no orchestrator — a degraded state with no
            // recovery path inside AO. Force users through Shift+X
            // (stops the whole daemon cleanly) or Shift+S (rebuilds
            // it) instead. The status-bar legend hides the `K kill`
            // chip when the orchestrator row is selected, so this
            // branch is mostly defence-in-depth for the keystroke.
            if app.is_orchestrator_selected() {
                return;
            }
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

        // Shift+A — open the "register this directory as a fleet
        // project" modal. Gated on the welcome state so it can't
        // accidentally fire when the user is already scoped to a
        // project (and the keymap legend doesn't advertise it then
        // either — see `build_welcome_lines` for the welcome-only
        // hint).
        (KeyCode::Char('A'), _) if app.current_project_key.is_none() => {
            app.open_register_project();
        }
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

/// Register-project mode: two-field form (name + sessionPrefix).
/// Tab toggles which field receives keystrokes. Enter validates
/// non-empty inputs and emits `Command::SaveRegisterProject`; Esc
/// dismisses with no flash (the user cancelled deliberately).
///
/// While the user types in the name field, the prefix re-derives on
/// every keystroke unless the user has already started editing the
/// prefix manually — `prefix_touched` latches that intent so the
/// auto-suggestion doesn't trample a deliberate choice. The prefix
/// field itself accepts only `[a-z0-9]` (anything else is silently
/// ignored) because AO uses the prefix as a path segment.
pub(super) fn handle_key_register_project(app: &mut App, key: KeyEvent) {
    if app.register_project.is_none() {
        return;
    }
    match key.code {
        KeyCode::Enter => {
            // Snapshot the buffers, run validation, then either set
            // an inline error or push the save command. The save
            // handler closes the modal on success / writes the error
            // back on failure.
            let rp = app.register_project.as_mut().expect("checked above");
            let name = rp.name.trim().to_string();
            let prefix = rp.prefix.trim().to_string();
            if name.is_empty() {
                rp.error = Some("name is empty".to_string());
                return;
            }
            if prefix.is_empty() {
                rp.error = Some("prefix is empty".to_string());
                return;
            }
            let path = rp.path.clone();
            // The yaml key matches the name verbatim — AO is
            // case-sensitive on project keys and the user already
            // sees this value in the modal, so no surprise lower-casing.
            app.push_command(Command::SaveRegisterProject {
                key: name.clone(),
                name,
                prefix,
                path,
            });
        }
        KeyCode::Esc => {
            app.register_project = None;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            let rp = app.register_project.as_mut().expect("checked above");
            rp.focus = match rp.focus {
                RegisterField::Name => RegisterField::Prefix,
                RegisterField::Prefix => RegisterField::Name,
            };
            rp.error = None;
        }
        KeyCode::Backspace => {
            let (need_refresh, focus) = {
                let rp = app.register_project.as_mut().expect("checked above");
                rp.error = None;
                match rp.focus {
                    RegisterField::Name => {
                        rp.name.pop();
                        (!rp.prefix_touched, RegisterField::Name)
                    }
                    RegisterField::Prefix => {
                        rp.prefix.pop();
                        (false, RegisterField::Prefix)
                    }
                }
            };
            let _ = focus;
            if need_refresh {
                app.refresh_register_prefix();
            }
        }
        KeyCode::Char(c) if !c.is_control() => {
            let need_refresh = {
                let rp = app.register_project.as_mut().expect("checked above");
                rp.error = None;
                match rp.focus {
                    RegisterField::Name => {
                        rp.name.push(c);
                        !rp.prefix_touched
                    }
                    RegisterField::Prefix => {
                        // Prefix is part of git branch / path
                        // segments downstream; constrain to safe
                        // chars rather than letting `agent/<id>`
                        // pick up arbitrary unicode.
                        if c.is_ascii_alphanumeric() {
                            rp.prefix.push(c.to_ascii_lowercase());
                            rp.prefix_touched = true;
                        }
                        false
                    }
                }
            };
            if need_refresh {
                app.refresh_register_prefix();
            }
        }
        _ => {}
    }
}

/// Confirm mode: `Enter` or `y`/`Y` resolves to the pending action;
/// `Esc` / `n` / `N` (or any other key) cancels. Enter is the default
/// so users can power through the dialog without reaching for `y`.
/// Always returns to Normal mode by clearing `app.confirm`.
pub(super) fn handle_key_confirm(app: &mut App, key: KeyEvent) {
    let Some(pending) = app.confirm.take() else {
        return;
    };
    let confirmed = matches!(key.code, KeyCode::Enter | KeyCode::Char('y' | 'Y'));
    if confirmed {
        match pending {
            Confirm::KillSession(id) => app.push_command(Command::KillSession(id)),
            Confirm::StopAo => app.push_command(Command::StopAo),
            Confirm::RestartAoForOrchestrator => {
                app.push_command(Command::RestartAoForOrchestrator);
            }
        }
    }
    // Cancel path is silent — the user explicitly pressed a non-confirm
    // key to back out, so a confirmation flash would just be noise.
}

/// Normal-mode mouse handler. Left-click on a sidebar row selects it;
/// double-click within `DOUBLE_CLICK_WINDOW` attaches (same effect as
/// Enter). Scroll wheel events are ignored — moving the selection on
/// scroll surprised users who expected the wheel to be inert.
/// Clicks outside any tracked rect are no-ops.
pub(super) fn handle_mouse_normal(app: &mut App, me: MouseEvent) {
    // Only react to left-button down. Scroll-wheel + right-click +
    // motion events are intentional no-ops; an early-return keeps
    // the body flat instead of wrapping it in an outer match.
    if me.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
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
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
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
