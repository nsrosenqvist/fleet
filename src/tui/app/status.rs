//! Encapsulated state for the TUI's bottom status bar.
//!
//! Owns the transient flash message that action handlers emit
//! ("plan → paused", "kill cancelled", error reports). Handles two
//! kinds of auto-clear:
//!
//! - **TTL-based**: every flash carries a wall-clock timestamp; after
//!   [`FLASH_TTL`] elapses the message is considered expired and
//!   [`Self::current`] returns `None` so the renderer falls back to
//!   the per-view legend without the caller having to remember.
//! - **Key-driven**: `terminal::drive` calls [`Self::dismiss`] on
//!   every Key event (mouse / paste events deliberately don't fire,
//!   so a Shift-mouse text-selection for copying doesn't wipe the
//!   error the user is trying to read). Keyboard interaction means
//!   the user has acknowledged the bar and is moving on.
//!
//! The state lives behind a struct so callers never reach for a
//! `String` field directly — every mutation flows through methods
//! that stamp the timestamp, and the renderer asks the bar for its
//! current text rather than inspecting raw fields. That moves the
//! "should this still be visible?" decision into one place.

use std::time::{Duration, Instant};

/// How long a flash message stays on screen absent any keystroke.
/// Short enough that a stale error doesn't haunt the bar after the
/// user moves on; long enough to read a sentence.
pub const FLASH_TTL: Duration = Duration::from_secs(4);

/// Transient state for the bottom status bar.
///
/// Wraps the previous `(status_line: String, flash_set_at:
/// Option<Instant>)` pair into a typed surface so write sites can't
/// forget to stamp the timestamp and the renderer can't accidentally
/// surface an expired message.
#[derive(Debug, Default, Clone)]
pub struct StatusBar {
    /// `(message, when-set)`. `None` is the steady state and renders
    /// as the per-view keybinding legend.
    flash: Option<(String, Instant)>,
}

impl StatusBar {
    /// Replace any current flash with `msg`, stamping `now`. Use for
    /// action results: button-press outcomes, error reports,
    /// confirmations. The message stays visible for up to
    /// [`FLASH_TTL`] or until the next keystroke clears it.
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    /// Drop the current flash immediately. Called on every key event
    /// by the input dispatcher so the user gets back to the legend
    /// as soon as they interact with the TUI.
    pub fn dismiss(&mut self) {
        self.flash = None;
    }

    /// Current flash text, iff still within the TTL. `None` after
    /// expiry or after a dismiss — the renderer falls through to the
    /// legend in that case.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        match &self.flash {
            Some((msg, at)) if at.elapsed() < FLASH_TTL => Some(msg.as_str()),
            _ => None,
        }
    }

    /// `true` when there's a non-expired flash waiting to render.
    /// Convenience for cases where the caller only needs a yes/no.
    #[must_use]
    #[allow(dead_code)] // surface kept symmetric with `current`; first caller lands later.
    pub fn has_active_flash(&self) -> bool {
        self.current().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_status_bar_has_no_flash() {
        let bar = StatusBar::default();
        assert!(bar.current().is_none());
        assert!(!bar.has_active_flash());
    }

    #[test]
    fn flash_then_current_returns_message() {
        let mut bar = StatusBar::default();
        bar.flash(" plan paused ");
        assert_eq!(bar.current(), Some(" plan paused "));
        assert!(bar.has_active_flash());
    }

    #[test]
    fn dismiss_clears_flash() {
        let mut bar = StatusBar::default();
        bar.flash("x");
        bar.dismiss();
        assert!(bar.current().is_none());
    }

    #[test]
    fn flash_overrides_previous_message_and_resets_timer() {
        let mut bar = StatusBar::default();
        bar.flash("first");
        std::thread::sleep(Duration::from_millis(20));
        bar.flash("second");
        // Without the timer reset, "first" would dominate elapsed
        // calculation; checking the *text* is enough to lock the
        // override semantics.
        assert_eq!(bar.current(), Some("second"));
    }

    #[test]
    fn current_returns_none_after_ttl() {
        // Hand-stamp the flash in the past so we don't have to sleep
        // for the full TTL in tests. checked_sub guards against
        // monotonic-clock skew on platforms where the test runner
        // hasn't been up long enough.
        let stamp = Instant::now()
            .checked_sub(FLASH_TTL + Duration::from_millis(10))
            .expect("monotonic clock is at least TTL old");
        let bar = StatusBar {
            flash: Some(("stale".into(), stamp)),
        };
        assert!(bar.current().is_none());
        assert!(!bar.has_active_flash());
    }
}
