//! Encapsulated state for the TUI's bottom status bar.
//!
//! Owns the transient flash message that action handlers emit
//! ("plan → paused", "kill cancelled", error reports). Two
//! decay paths:
//!
//! - **TTL-based** (info flashes only): the flash carries a
//!   wall-clock timestamp; after [`FLASH_TTL`] elapses the message
//!   is considered expired and [`Self::current`] returns `None` so
//!   the renderer falls back to the per-view legend.
//! - **Key-driven**: `terminal::drive` calls [`Self::dismiss`] on
//!   every Key event (mouse / paste events deliberately don't fire,
//!   so a Shift-mouse text-selection for copying doesn't wipe the
//!   error the user is trying to read). Keyboard interaction means
//!   the user has acknowledged the bar and is moving on.
//!
//! **Errors are sticky** — `flash_error` sets a [`FlashKind::Error`]
//! flash that ignores the TTL and only clears on dismiss. The
//! rationale: a 4-second timer on "orchestrator failed: exit code 1"
//! means the operator can't copy the message or even read it twice;
//! info flashes ("plan → paused") are throwaway and decay fine.
//!
//! The state lives behind a struct so callers never reach for a
//! `String` field directly — every mutation flows through methods
//! that stamp the timestamp + kind, and the renderer asks the bar
//! for both `current()` text and `current_kind()` styling hint.

use std::time::{Duration, Instant};

/// How long an *info* flash stays on screen absent any keystroke.
/// Errors ignore this — see [`FlashKind`].
pub const FLASH_TTL: Duration = Duration::from_secs(4);

/// What kind of flash is currently up. Drives both the decay
/// behaviour (errors sticky, info TTL'd) and the renderer's badge
/// colour. Determined at write time so the renderer doesn't have
/// to keep re-classifying via substring matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashKind {
    /// Confirmations and progress messages. TTL-decay applies.
    Info,
    /// Failure reports. Sticky until the next keystroke so the
    /// operator can read / copy / screenshot the diagnostic. The
    /// renderer styles these with the error badge.
    Error,
}

/// Transient state for the bottom status bar.
#[derive(Debug, Default, Clone)]
pub struct StatusBar {
    /// `(message, when-set, kind)`. `None` is the steady state
    /// (renders as the per-view keybinding legend).
    flash: Option<(String, Instant, FlashKind)>,
}

impl StatusBar {
    /// Set an *info* flash — TTL applies, dismiss on keystroke.
    /// Use for action confirmations and benign progress reports.
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now(), FlashKind::Info));
    }

    /// Set an *error* flash — sticky until dismissed, ignoring the
    /// TTL. Use for any failure the operator might want to read
    /// twice or copy: spawn failures, daemon-unreachable, save
    /// failures, etc. The renderer's "is this an error?" heuristic
    /// no longer has to guess from the message text.
    pub fn flash_error(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now(), FlashKind::Error));
    }

    /// Drop the current flash immediately. Called on every key event
    /// by the input dispatcher so the user gets back to the legend
    /// as soon as they interact with the TUI — and so the sticky
    /// error message goes away once they've acknowledged it.
    pub fn dismiss(&mut self) {
        self.flash = None;
    }

    /// Current flash text. Info flashes return `None` after
    /// [`FLASH_TTL`]; errors return `Some` until [`Self::dismiss`]
    /// clears them.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        match &self.flash {
            Some((msg, _, FlashKind::Error)) => Some(msg.as_str()),
            Some((msg, at, FlashKind::Info)) if at.elapsed() < FLASH_TTL => Some(msg.as_str()),
            _ => None,
        }
    }

    /// Kind of the currently-visible flash (mirrors what
    /// [`Self::current`] would return). `None` when the bar is in
    /// its steady state.
    #[must_use]
    pub fn current_kind(&self) -> Option<FlashKind> {
        match &self.flash {
            Some((_, _, FlashKind::Error)) => Some(FlashKind::Error),
            Some((_, at, FlashKind::Info)) if at.elapsed() < FLASH_TTL => Some(FlashKind::Info),
            _ => None,
        }
    }

    /// `true` when there's a visible flash waiting to render.
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
        assert!(bar.current_kind().is_none());
        assert!(!bar.has_active_flash());
    }

    #[test]
    fn info_flash_then_current_returns_message_with_info_kind() {
        let mut bar = StatusBar::default();
        bar.flash(" plan paused ");
        assert_eq!(bar.current(), Some(" plan paused "));
        assert_eq!(bar.current_kind(), Some(FlashKind::Info));
    }

    #[test]
    fn error_flash_returns_message_with_error_kind() {
        let mut bar = StatusBar::default();
        bar.flash_error("orchestrator failed: exit 1");
        assert_eq!(bar.current(), Some("orchestrator failed: exit 1"));
        assert_eq!(bar.current_kind(), Some(FlashKind::Error));
    }

    #[test]
    fn dismiss_clears_any_flash() {
        let mut bar = StatusBar::default();
        bar.flash("x");
        bar.dismiss();
        assert!(bar.current().is_none());

        bar.flash_error("y");
        bar.dismiss();
        assert!(bar.current().is_none());
    }

    #[test]
    fn second_flash_overrides_previous_message() {
        let mut bar = StatusBar::default();
        bar.flash("first");
        bar.flash("second");
        assert_eq!(bar.current(), Some("second"));
        // Error → Info override demotes the kind, that's intended:
        // the new flash represents the latest user-visible event.
        bar.flash_error("oops");
        assert_eq!(bar.current_kind(), Some(FlashKind::Error));
        bar.flash("ok");
        assert_eq!(bar.current_kind(), Some(FlashKind::Info));
    }

    #[test]
    fn info_flash_expires_after_ttl() {
        let stamp = Instant::now()
            .checked_sub(FLASH_TTL + Duration::from_millis(10))
            .expect("monotonic clock is at least TTL old");
        let bar = StatusBar {
            flash: Some(("stale".into(), stamp, FlashKind::Info)),
        };
        assert!(bar.current().is_none());
        assert!(bar.current_kind().is_none());
    }

    #[test]
    fn error_flash_does_not_expire_with_ttl() {
        // Errors are sticky-until-dismiss. A flash stamped well past
        // the TTL still surfaces; only `dismiss` removes it.
        let stamp = Instant::now()
            .checked_sub(FLASH_TTL * 100)
            .expect("monotonic clock is at least 100×TTL old");
        let bar = StatusBar {
            flash: Some(("orchestrator failed: exit 1".into(), stamp, FlashKind::Error)),
        };
        assert_eq!(bar.current(), Some("orchestrator failed: exit 1"));
        assert_eq!(bar.current_kind(), Some(FlashKind::Error));
    }
}
