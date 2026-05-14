//! Lifecycle states a session moves through, and the transition rules
//! between them.
//!
//! Modelled as a closed enum with an explicit transition predicate. The
//! benefit over scattered string comparisons: when we add a state (e.g.
//! `Paused` for autonomous-mode quota waits) the match in [`SessionState::can_transition_to`]
//! becomes a compile error until every existing transition decides whether
//! the new variant is a legal successor — exactly the property we want
//! from a lifecycle model.
//!
//! The states themselves come straight from the plan:
//!
//!   `Created` → `Running` → `AwaitingGate` → `Running` → terminal
//!                         ↘ terminal
//!
//! where *terminal* ∈ { `Completed`, `Failed`, `Crashed` }.
//!
//! - `Created` is the just-minted state: session exists, no workflow node
//!   has fired yet.
//! - `Running` is "a workflow node is executing." The TUI shows current node.
//! - `AwaitingGate` is "a human gate node is blocking progress."
//! - `Completed` / `Failed` / `Crashed` are terminal. `Failed` is a graceful
//!   exit (the workflow ran out of steps to take); `Crashed` is the
//!   recovery state for a session whose process died unexpectedly.

use serde::{Deserialize, Serialize};

/// Where a session is in its lifecycle. Serialised in `snake_case` so the
/// on-disk `meta.json` reads naturally (`"awaiting_gate"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Running,
    AwaitingGate,
    Completed,
    Failed,
    Crashed,
}

impl SessionState {
    /// Whether this is a terminal state (no further transitions allowed).
    /// The workflow engine treats terminal sessions as inert; the TUI
    /// renders them in the "completed/crashed" sidebar buckets.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Crashed)
    }

    /// Whether a transition from `self` to `next` is allowed.
    ///
    /// Same-state "transitions" return `false`: callers should not
    /// re-save the session just because they re-confirmed its state.
    /// That keeps `updated_at_ms` meaningful as a "something changed"
    /// signal.
    ///
    /// Nested matches (outer on `from`, inner on `to`) are exhaustive
    /// in both directions: when a new variant lands on [`Self`], the
    /// outer match goes non-exhaustive *and* each "true" arm's inner
    /// match goes non-exhaustive — both compile errors, forcing the
    /// person adding the state to audit every edge of the table. The
    /// flat `match (self, next)` form would be more compact but would
    /// trip `clippy::unnested_or_patterns` *and* lose the inner
    /// exhaustiveness check.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Created => match next {
                Self::Running | Self::Failed | Self::Crashed => true,
                Self::Created | Self::AwaitingGate | Self::Completed => false,
            },
            Self::Running => match next {
                Self::AwaitingGate | Self::Completed | Self::Failed | Self::Crashed => true,
                Self::Created | Self::Running => false,
            },
            Self::AwaitingGate => match next {
                Self::Running | Self::Completed | Self::Failed | Self::Crashed => true,
                Self::Created | Self::AwaitingGate => false,
            },
            // Terminal states reject every successor — no inner match.
            Self::Completed | Self::Failed | Self::Crashed => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cartesian product of all (from, to) pairs — used to confirm the
    /// transition table matches expectations exhaustively.
    fn all_states() -> &'static [SessionState] {
        &[
            SessionState::Created,
            SessionState::Running,
            SessionState::AwaitingGate,
            SessionState::Completed,
            SessionState::Failed,
            SessionState::Crashed,
        ]
    }

    #[test]
    fn terminal_states_are_the_three_documented_ones() {
        assert!(SessionState::Completed.is_terminal());
        assert!(SessionState::Failed.is_terminal());
        assert!(SessionState::Crashed.is_terminal());
        assert!(!SessionState::Created.is_terminal());
        assert!(!SessionState::Running.is_terminal());
        assert!(!SessionState::AwaitingGate.is_terminal());
    }

    #[test]
    fn created_can_become_running() {
        assert!(SessionState::Created.can_transition_to(SessionState::Running));
    }

    #[test]
    fn created_can_short_circuit_to_failed_or_crashed() {
        // A session that fails to provision its container never reaches
        // Running. The lifecycle must allow Created → Failed/Crashed.
        assert!(SessionState::Created.can_transition_to(SessionState::Failed));
        assert!(SessionState::Created.can_transition_to(SessionState::Crashed));
    }

    #[test]
    fn created_cannot_jump_straight_to_completed_or_awaiting_gate() {
        // Skipping Running is not a legal path — the workflow engine
        // must have at least had a chance to run.
        assert!(!SessionState::Created.can_transition_to(SessionState::Completed));
        assert!(!SessionState::Created.can_transition_to(SessionState::AwaitingGate));
    }

    #[test]
    fn running_can_reach_every_other_non_created_state() {
        assert!(SessionState::Running.can_transition_to(SessionState::AwaitingGate));
        assert!(SessionState::Running.can_transition_to(SessionState::Completed));
        assert!(SessionState::Running.can_transition_to(SessionState::Failed));
        assert!(SessionState::Running.can_transition_to(SessionState::Crashed));
    }

    #[test]
    fn awaiting_gate_can_resume_to_running() {
        assert!(SessionState::AwaitingGate.can_transition_to(SessionState::Running));
    }

    #[test]
    fn awaiting_gate_can_jump_to_terminal_states() {
        // Useful for "user cancelled at the gate" → Failed, or the
        // crash sweeper picking up a stuck gate session.
        assert!(SessionState::AwaitingGate.can_transition_to(SessionState::Completed));
        assert!(SessionState::AwaitingGate.can_transition_to(SessionState::Failed));
        assert!(SessionState::AwaitingGate.can_transition_to(SessionState::Crashed));
    }

    #[test]
    fn terminal_states_cannot_transition_to_anything() {
        for &terminal in &[
            SessionState::Completed,
            SessionState::Failed,
            SessionState::Crashed,
        ] {
            for &next in all_states() {
                assert!(
                    !terminal.can_transition_to(next),
                    "terminal {terminal:?} unexpectedly allows → {next:?}"
                );
            }
        }
    }

    #[test]
    fn same_state_transitions_are_rejected() {
        for &s in all_states() {
            assert!(!s.can_transition_to(s), "{s:?} → {s:?} should be rejected");
        }
    }

    #[test]
    fn serialises_in_snake_case() {
        // The on-disk meta.json reads more naturally as `"awaiting_gate"`
        // than `"AwaitingGate"`; lock the casing.
        let s = serde_json::to_string(&SessionState::AwaitingGate).unwrap();
        assert_eq!(s, r#""awaiting_gate""#);
    }

    #[test]
    fn deserialises_snake_case() {
        let s: SessionState = serde_json::from_str(r#""running""#).unwrap();
        assert_eq!(s, SessionState::Running);
    }
}
