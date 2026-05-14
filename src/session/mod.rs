//! Fleet's session model.
//!
//! A *session* is a workflow run — not a single agent process. It owns:
//! - an identity ([`SessionId`]) the rest of the binary can reference;
//! - a lifecycle state ([`state::SessionState`]) constrained by an explicit
//!   transition table, so unmodelled transitions become compile-time errors
//!   when the enum grows new variants;
//! - the workflow it was launched against;
//! - the current node within that workflow (set by the engine, displayed by
//!   the TUI);
//! - timestamps for created/updated, useful for the sidebar and for crash
//!   forensics.
//!
//! The [`store::SessionStore`] persists sessions to
//! `.fleet/sessions/<id>/meta.json`. The store is intentionally minimal: it
//! reads and writes a single JSON document per session plus two empty
//! sibling directories (`artifacts/`, `logs/`) the workflow engine will
//! populate. Higher-level concerns (TUI rendering, autonomous-mode slot
//! accounting, crash sweeping) compose on top of this — they do not live
//! here.
//!
//! Module split follows SRP: state-machine rules in `state`, the value
//! object in `aggregate`, the persistence repository in `store`. Tests
//! sit at the bottom of each module against the unit under test.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod aggregate;
pub mod state;
pub mod store;

pub use aggregate::Session;
pub use state::SessionState;
// `store::SessionStore` is intentionally not re-exported yet: nothing in
// the binary references it from outside the `session` module, so a
// top-level `pub use` would trip the `unused_imports` lint that the
// workspace builds with `-D warnings`. The workflow engine (next chunk)
// will pull it in via the full path or reinstate this re-export.

/// Stable identifier for a session. Opaque to callers — they only need to
/// pass it around and key off it. Serialised transparently as a JSON string
/// so the on-disk meta file stays human-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Source for newly-minted session IDs. Behind a trait so tests don't
/// depend on wallclock time and aren't subject to ID collisions when many
/// sessions are minted in the same millisecond.
pub trait IdSource: Send + Sync {
    fn mint(&self) -> SessionId;
}

/// Production ID source: millisecond Unix timestamp + an in-process
/// monotonic counter, packed as `s-<13 hex>-<4 hex>`. The counter
/// guarantees in-process uniqueness even when multiple sessions are
/// created in the same millisecond; cross-process collisions are not
/// defended against, because fleet is the only writer to a given
/// `.fleet/sessions/` directory by design (a second fleet process in
/// the same repo would step on its own session files long before
/// hitting an ID collision).
#[derive(Debug, Default)]
pub struct ClockIdSource;

impl IdSource for ClockIdSource {
    fn mint(&self) -> SessionId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        SessionId::new(format!("s-{ms:013x}-{n:04x}"))
    }
}

/// Convenience: current wallclock as Unix-epoch milliseconds, used to stamp
/// `created_at_ms` / `updated_at_ms` on the value object. Tests pass an
/// explicit value to [`Session::new`] / [`Session::transition_to`] instead
/// of calling this.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_through_string() {
        let id = SessionId::new("s-abc");
        assert_eq!(id.as_str(), "s-abc");
        assert_eq!(format!("{id}"), "s-abc");
    }

    #[test]
    fn session_ids_compare_by_value() {
        assert_eq!(SessionId::new("a"), SessionId::new("a"));
        assert_ne!(SessionId::new("a"), SessionId::new("b"));
    }

    #[test]
    fn session_id_serialises_as_a_bare_string() {
        let id = SessionId::new("s-42");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""s-42""#);
    }

    #[test]
    fn session_id_deserialises_from_a_bare_string() {
        let id: SessionId = serde_json::from_str(r#""s-42""#).unwrap();
        assert_eq!(id, SessionId::new("s-42"));
    }

    #[test]
    fn clock_id_source_mints_distinct_ids() {
        let src = ClockIdSource;
        let a = src.mint();
        let b = src.mint();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("s-"));
    }

    #[test]
    fn clock_id_source_format_is_stable() {
        // `s-<13 hex>-<4 hex>` — used to grep crash forensics and the
        // session sidebar; lock the shape.
        let id = ClockIdSource.mint();
        let parts: Vec<_> = id.as_str().split('-').collect();
        assert_eq!(parts.len(), 3, "id = {id}");
        assert_eq!(parts[0], "s");
        assert_eq!(parts[1].len(), 13);
        assert_eq!(parts[2].len(), 4);
    }
}
