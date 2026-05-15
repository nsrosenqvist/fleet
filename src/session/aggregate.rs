//! The [`Session`] value object: identity + lifecycle state + workflow
//! pointer + timestamps.
//!
//! Keeps the value object pure: no I/O, no clock reads, no logging. Time
//! flows in as a parameter, transitions consult the explicit table in
//! [`crate::session::state::SessionState`], and the persistence boundary
//! lives in [`crate::session::store::SessionStore`].

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{IssueContext, SessionId, SessionState};

/// One session row. Owned by the store; mutated only via [`Session::transition_to`]
/// so the state-machine rules are the single gate for state changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    /// Workflow this run is bound to. Today: the basename (without
    /// `.yaml`) of the file under `.fleet/workflows/`. The store does not
    /// validate that the file exists — that's the workflow engine's job.
    pub workflow: String,
    pub state: SessionState,
    /// `id` of the workflow node currently executing, or `None` when no
    /// node has fired yet (state == `Created`) or when the session is
    /// terminal (`Completed`/`Failed`/`Crashed` — informational, the node
    /// that last ran).
    #[serde(default)]
    pub current_node: Option<String>,
    /// Tracker issue the workflow is acting on, if any. Persisted so
    /// `fleet workflow resume` after a gate retains the issue context
    /// the caller specified on the original `--issue <id>` — without
    /// this the resumed run loses `FLEET_ISSUE_*` env in agent / bash
    /// nodes downstream of the gate.
    #[serde(default)]
    pub issue: Option<IssueContext>,
    /// Per-node `loop_back_to` counters. The executor mutates this
    /// in place as cycles fire; persistence keeps the counters honest
    /// across a `resume` boundary. Without it a gate placed
    /// downstream of a loop would reset `max_loops` on every resume
    /// and a workflow could spin forever.
    #[serde(default)]
    pub loop_counts: BTreeMap<String, u32>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Session {
    /// Create a fresh session in [`SessionState::Created`]. Caller is
    /// responsible for minting the id (typically via [`super::IdSource`])
    /// and providing wallclock time (via [`super::now_ms`] in production,
    /// a fixed value in tests).
    #[must_use]
    pub fn new(id: SessionId, workflow: impl Into<String>, now_ms: u64) -> Self {
        Self {
            id,
            workflow: workflow.into(),
            state: SessionState::Created,
            current_node: None,
            issue: None,
            loop_counts: BTreeMap::new(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }

    /// Move the session to `next`. Returns an error if the transition is
    /// not allowed by the state machine; the session is left untouched in
    /// that case so the caller can retry or surface the error verbatim.
    pub fn transition_to(&mut self, next: SessionState, now_ms: u64) -> Result<()> {
        if !self.state.can_transition_to(next) {
            bail!(
                "session {} cannot transition {:?} -> {:?}",
                self.id,
                self.state,
                next
            );
        }
        self.state = next;
        self.updated_at_ms = now_ms;
        Ok(())
    }

    /// Update which workflow node is current. Independent of state
    /// transitions: nodes can advance while the session stays `Running`.
    /// Also bumps `updated_at_ms` so the sidebar's "last activity" stamp
    /// reflects node progress, not just state changes.
    pub fn set_current_node(&mut self, node: Option<String>, now_ms: u64) {
        self.current_node = node;
        self.updated_at_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Session {
        Session::new(SessionId::new("s-test"), "standard", 1_000)
    }

    #[test]
    fn new_session_starts_in_created() {
        let s = fresh();
        assert_eq!(s.state, SessionState::Created);
        assert_eq!(s.workflow, "standard");
        assert_eq!(s.current_node, None);
        assert_eq!(s.created_at_ms, 1_000);
        assert_eq!(s.updated_at_ms, 1_000);
    }

    #[test]
    fn transition_to_running_updates_state_and_timestamp() {
        let mut s = fresh();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(s.created_at_ms, 1_000);
        assert_eq!(s.updated_at_ms, 2_000);
    }

    #[test]
    fn illegal_transition_leaves_session_untouched() {
        let mut s = fresh();
        // Created -> Completed is illegal.
        let err = s
            .transition_to(SessionState::Completed, 2_000)
            .unwrap_err();
        assert!(format!("{err}").contains("cannot transition"));
        assert_eq!(s.state, SessionState::Created);
        assert_eq!(s.updated_at_ms, 1_000, "timestamp must not advance on reject");
    }

    #[test]
    fn terminal_state_cannot_be_left() {
        let mut s = fresh();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        s.transition_to(SessionState::Completed, 3_000).unwrap();
        let err = s.transition_to(SessionState::Running, 4_000).unwrap_err();
        assert!(format!("{err}").contains("cannot transition"));
        assert_eq!(s.state, SessionState::Completed);
    }

    #[test]
    fn awaiting_gate_resumes_to_running() {
        let mut s = fresh();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        s.transition_to(SessionState::AwaitingGate, 3_000).unwrap();
        s.transition_to(SessionState::Running, 4_000).unwrap();
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(s.updated_at_ms, 4_000);
    }

    #[test]
    fn set_current_node_bumps_timestamp() {
        let mut s = fresh();
        s.set_current_node(Some("plan".to_string()), 2_000);
        assert_eq!(s.current_node.as_deref(), Some("plan"));
        assert_eq!(s.updated_at_ms, 2_000);
    }

    #[test]
    fn set_current_node_can_clear() {
        let mut s = fresh();
        s.set_current_node(Some("plan".to_string()), 2_000);
        s.set_current_node(None, 3_000);
        assert!(s.current_node.is_none());
        assert_eq!(s.updated_at_ms, 3_000);
    }

    #[test]
    fn round_trips_through_json() {
        let mut s = fresh();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        s.set_current_node(Some("plan".to_string()), 2_500);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn round_trips_with_issue_and_loop_counts() {
        let mut s = fresh();
        s.issue = Some(IssueContext {
            id: "gh:42".to_string(),
            human_id: "42".to_string(),
            title: "Fix the parser".to_string(),
        });
        s.loop_counts.insert("revise".to_string(), 2);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn missing_issue_and_loop_counts_default_to_empty_on_load() {
        // Backward compatibility with meta.json from before these
        // fields landed: serde(default) makes the absent keys parse.
        let json = r#"{
            "id": "s-1",
            "workflow": "standard",
            "state": "created",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(s.issue.is_none());
        assert!(s.loop_counts.is_empty());
    }

    #[test]
    fn missing_current_node_defaults_to_none_on_load() {
        // Older meta.json files might not have current_node yet. Keep
        // backward compatibility: deserialise without the key.
        let json = r#"{
            "id": "s-1",
            "workflow": "standard",
            "state": "created",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert_eq!(s.current_node, None);
    }
}
