//! Per-mode keyboard handlers.
//!
//! Each handler is `(&mut App, KeyEvent) -> ()`. Side effects that need the
//! terminal handle (interactive children, suspending the alternate screen)
//! are deferred by pushing a [`Command`] onto the app's command queue —
//! [`crate::tui::terminal`] drains the queue after each draw.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, Command, Confirm};

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
