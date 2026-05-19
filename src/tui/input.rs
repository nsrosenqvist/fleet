//! Keyboard input dispatch — the seam between [`super::terminal`]'s
//! event loop and [`super::app::AppState`]'s mutation methods.
//!
//! This module is a thin facade: per-view dispatch lives as
//! `handle_key_*` methods on [`AppState`] (close to the state they
//! mutate), and [`handle_key`] here is the single entry point the event
//! loop calls each tick. Splitting input from state at this layer
//! keeps the call shape consistent (`input::handle_key(&mut state,
//! key, store)`) without forcing every internal method to become
//! `pub(super)` for cross-module access.

use crossterm::event::KeyEvent;

use crate::session::store::SessionStore;

use super::app::{Action, AppState};

/// Per-tick keyboard dispatch. Returns the [`Action`] the event loop
/// should perform (quit, suspend to open the orchestrator, etc.) or
/// [`Action::None`] when the keystroke was fully consumed in-place.
///
/// Dismissing the status flash here means *any* keystroke clears
/// stale messages — the user moving on is the signal that they've
/// acknowledged whatever was shown. If the handler itself emits a
/// new flash (e.g. `Shift+P` flashes "plan → paused"), that flash
/// supersedes the clear because writes happen after the dismiss.
/// Mouse and paste events deliberately don't reach this path, so a
/// Shift-mouse text-selection for copying doesn't wipe the error
/// the user is trying to read.
pub(super) fn handle_key(state: &mut AppState, key: KeyEvent, store: &SessionStore) -> Action {
    state.status.dismiss();
    state.handle_key(key, store)
}
