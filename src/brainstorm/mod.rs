//! Brainstorm sessions: attachable host-side agent sessions for
//! planning, ticket triage, and supervisor coordination. Different
//! shape from workflow sessions:
//!
//! - Run on the *host*, not in a container. No worktree, no
//!   per-ticket scoping.
//! - Wrapped in a tmux session (named `fleet-brainstorm-<id>`) so
//!   the user can attach / detach freely across terminal restarts.
//! - Wider tool surface than the bridge: tracker reads + creates,
//!   plan management, sessions inspection, deps surgery. Bound to
//!   the brainstorm *session's* bearer token, not a ticket.
//! - Transcript captured to `.fleet/planning/<id>/transcript.log`
//!   so a re-attach (or a post-hoc audit) has the prior context.
//!
//! Module shape mirrors `plans` and `deps`:
//! - value-object types + serde here,
//! - [`store::BrainstormStore`] handles `.fleet/planning/<id>/`
//!   persistence,
//! - id minting via [`BrainstormIdSource`] /
//!   [`ClockBrainstormIdSource`] so tests pin ids without the
//!   wallclock.
//!
//! tmux invocation, the tool server, the prompt template, and the
//! CLI surface all live in sibling modules that build on this
//! one.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod server;
pub mod store;
pub mod tmux;

/// Stable identifier for a brainstorm session. Mirrors the
/// [`crate::session::SessionId`] /
/// [`crate::plans::PlanId`] opaque-newtype contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrainstormId(String);

impl BrainstormId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BrainstormId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle of a brainstorm session. Distinct from the workflow
/// session state machine because brainstorm doesn't have
/// `Running`/`Failed`/`Crashed` etc. — it's user-driven, with the
/// tmux pane either attached, detached, or closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrainstormState {
    /// User is currently attached, agent is running.
    Active,
    /// Tmux pane alive but no user attached. Re-attach via
    /// `fleet brainstorm attach <id>` resumes.
    Detached,
    /// Session ended (user explicitly killed or tmux session gone).
    /// Terminal; cannot be re-attached.
    Closed,
}

/// One brainstorm session: id, the agent name fleet exec'd, the
/// current state, the tmux session name (so `fleet brainstorm
/// attach` knows what to talk to), and timestamps.
///
/// `pid` is the tmux *server* pid captured at start, used as a
/// liveness probe — if the pid is gone, the session is treated as
/// Closed on next list/inspect (similar to how `Session::pid` +
/// the reaper interact for workflow sessions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainstormSession {
    pub id: BrainstormId,
    pub agent: String,
    pub state: BrainstormState,
    pub tmux_session: String,
    #[serde(default)]
    pub pid: Option<u32>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl BrainstormSession {
    /// Build a fresh Active session for `agent`. Mints the tmux
    /// session name deterministically from `id` so a re-attach
    /// always finds the same tmux pane.
    #[must_use]
    pub fn new(id: BrainstormId, agent: impl Into<String>, created_at_ms: u64) -> Self {
        let tmux_session = format!("fleet-brainstorm-{}", id.as_str());
        Self {
            id,
            agent: agent.into(),
            state: BrainstormState::Active,
            tmux_session,
            pid: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }
}

/// Source for newly-minted brainstorm ids. Mirrors
/// [`crate::session::IdSource`] and [`crate::plans::PlanIdSource`].
pub trait BrainstormIdSource: Send + Sync {
    fn mint(&self) -> BrainstormId;
}

/// Production id source: `b-<13 hex ms>-<4 hex counter>`. Same
/// pattern as `s-<…>-<…>` (sessions) and `plan-<…>-<…>` (plans)
/// so lexicographic ≈ chronological order across all three
/// surfaces.
#[derive(Debug, Default)]
pub struct ClockBrainstormIdSource;

impl BrainstormIdSource for ClockBrainstormIdSource {
    fn mint(&self) -> BrainstormId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        BrainstormId::new(format!("b-{ms:013x}-{n:04x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brainstorm_id_round_trips_through_string() {
        let id = BrainstormId::new("b-abc");
        assert_eq!(id.as_str(), "b-abc");
        assert_eq!(format!("{id}"), "b-abc");
    }

    #[test]
    fn brainstorm_id_serialises_transparently_as_a_string() {
        let id = BrainstormId::new("b-1");
        let yaml = serde_yml::to_string(&id).unwrap();
        assert!(yaml.contains("b-1"), "yaml: {yaml}");
        let round: BrainstormId = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(round, id);
    }

    #[test]
    fn clock_brainstorm_id_source_uses_b_prefix_and_hex_segments() {
        let src = ClockBrainstormIdSource;
        let id = src.mint();
        let s = id.as_str();
        assert!(s.starts_with("b-"), "got: {s}");
        let rest = &s["b-".len()..];
        let mut parts = rest.split('-');
        let ms = parts.next().expect("ms segment");
        let counter = parts.next().expect("counter segment");
        assert!(parts.next().is_none(), "exactly two hex segments");
        assert_eq!(ms.len(), 13);
        assert_eq!(counter.len(), 4);
        assert!(ms.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(counter.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn clock_brainstorm_id_source_mints_unique_ids_in_a_burst() {
        let src = ClockBrainstormIdSource;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            assert!(seen.insert(src.mint()));
        }
    }

    #[test]
    fn brainstorm_session_new_seeds_tmux_session_name_from_id() {
        let s = BrainstormSession::new(BrainstormId::new("b-1"), "claude-code", 100);
        assert_eq!(s.tmux_session, "fleet-brainstorm-b-1");
        assert_eq!(s.state, BrainstormState::Active);
        assert_eq!(s.agent, "claude-code");
        assert_eq!(s.created_at_ms, 100);
        assert_eq!(s.updated_at_ms, 100);
        assert!(s.pid.is_none());
    }

    #[test]
    fn brainstorm_session_round_trips_through_json_with_full_fields() {
        let mut s = BrainstormSession::new(BrainstormId::new("b-1"), "claude-code", 100);
        s.state = BrainstormState::Detached;
        s.pid = Some(12345);
        s.updated_at_ms = 200;
        let json = serde_json::to_string(&s).unwrap();
        let round: BrainstormSession = serde_json::from_str(&json).unwrap();
        assert_eq!(round, s);
        assert!(json.contains("\"state\":\"detached\""), "json: {json}");
    }

    #[test]
    fn brainstorm_session_tolerates_missing_pid_on_load() {
        // Brand-new sessions may serialise without `pid` set yet
        // (the tmux probe runs after meta is first written).
        // `#[serde(default)]` should yield None.
        let json = r#"{
            "id": "b-1",
            "agent": "claude-code",
            "state": "active",
            "tmux_session": "fleet-brainstorm-b-1",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: BrainstormSession = serde_json::from_str(json).unwrap();
        assert!(s.pid.is_none());
    }
}
