//! Per-mode keyboard handlers.
//!
//! Each handler is `(&mut App, KeyEvent) -> ()`. Side effects that need the
//! terminal handle (interactive children, suspending the alternate screen)
//! are deferred by pushing a [`Command`] onto the app's command queue —
//! [`crate::tui::terminal`] drains the queue after each draw.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::app::{App, ClickKind, ClickTarget, Command, Confirm, View};

/// Lines moved per scroll-wheel notch. Three matches the j/k cadence
/// closely enough that mixing keyboard and wheel doesn't feel jumpy.
const WHEEL_LINES: usize = 3;

/// Default mode: navigation + action keys. Falls through to no-op on
/// unknown keys so e.g. `Shift+F1` doesn't accidentally fire an action.
pub(super) fn handle_key_normal(app: &mut App, key: KeyEvent) {
    // Global keys (work from any view).
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        // Top-level view switches. Uppercase = "deliberate, modified
        // key"; lowercase letters stay free for per-view actions.
        (KeyCode::Char('C'), _) => {
            app.view = View::Config;
            // Re-read in case `c` + $EDITOR or an external edit changed
            // the file since we last looked.
            app.reload_ao_config();
            return;
        }
        (KeyCode::Char('H'), _) => {
            app.view = View::Sessions;
            return;
        }
        _ => {}
    }

    match app.view {
        View::Sessions => handle_key_sessions(app, key),
        View::Config => handle_key_config(app, key),
    }
}

fn handle_key_sessions(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Down | KeyCode::Char('j'), _) => app.nav_down(),
        (KeyCode::Up, _) => app.nav_up(),
        // `k` alone navigates up (vim). Holding shift uses `K` for kill.
        (KeyCode::Char('k'), m) if !m.contains(KeyModifiers::SHIFT) => app.nav_up(),

        (KeyCode::Char('r'), _) => app.push_command(Command::RequestRefresh),
        (KeyCode::Enter, _) => app.push_command(Command::AttachSelected),

        (KeyCode::Char('K') | KeyCode::Delete, _) => {
            if let Some(id) = app.selected_session_id() {
                app.confirm = Some(Confirm::KillSession(id));
            }
        }
        (KeyCode::Char('c'), _) => app.push_command(Command::EditConfig),
        (KeyCode::Char('t'), _) => app.push_command(Command::TrackerPreview),
        (KeyCode::Char('S'), _) => app.push_command(Command::StartAo),
        (KeyCode::Char('X'), _) => {
            app.confirm = Some(Confirm::StopAo);
        }
        (KeyCode::Char('W'), _) => app.push_command(Command::OpenWeb),
        _ => {}
    }
}

fn handle_key_config(app: &mut App, key: KeyEvent) {
    // Read-only first pass: `r` to re-read, `c` still shells out to
    // $EDITOR until the form-editor lands. No selection / navigation
    // yet — that arrives with the editing pass.
    match (key.code, key.modifiers) {
        (KeyCode::Char('r'), _) => app.reload_ao_config(),
        (KeyCode::Char('c'), _) => app.push_command(Command::EditConfig),
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
        }
    } else {
        app.flash_ok("cancelled");
    }
}

/// Normal-mode mouse handler. Left-click on a sidebar row selects it;
/// double-click within `DOUBLE_CLICK_WINDOW` attaches (same effect as
/// Enter). Scroll wheel moves the selection at the keyboard cadence.
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
                    // Make sure the attach runs against the row we just
                    // clicked, even if a stale selection still points
                    // elsewhere.
                    app.select_at(idx);
                    app.push_command(Command::AttachSelected);
                }
            }
        }
        MouseEventKind::ScrollDown => {
            for _ in 0..WHEEL_LINES {
                app.nav_down();
            }
        }
        MouseEventKind::ScrollUp => {
            for _ in 0..WHEEL_LINES {
                app.nav_up();
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
