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
use std::path::PathBuf;

use super::{IssueContext, SessionId, SessionState};

/// One session row. Owned by the store; mutated only via [`Session::transition_to`]
/// so the state-machine rules are the single gate for state changes.
///
/// Note: `Eq` is intentionally *not* derived — [`Self::node_costs`]'s
/// `f64` values exclude `Eq`. Callers compare with `PartialEq`, which
/// is enough for `assert_eq!` in tests; nothing in the binary stores
/// `Session` in a hash-set or treats `==` as a total equivalence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// OS process id of the fleet process currently driving this
    /// session. Set when the executor enters `Running`, cleared when
    /// it leaves (clean exit or transition to `AwaitingGate`). The
    /// session reaper consults this to distinguish a session that is
    /// genuinely making progress from one whose driver crashed and
    /// left the state stuck in `Running`.
    #[serde(default)]
    pub driver_pid: Option<u32>,
    /// Per-node LLM cost in USD, populated after each agent node
    /// completes. A node is *in* this map iff fleet successfully
    /// parsed a cost figure from the agent's captured output —
    /// absent ≠ "$0.00", absent = "no parse / didn't run". UIs
    /// preserve the distinction.
    #[serde(default)]
    pub node_costs: BTreeMap<String, f64>,
    /// Resolved `outputs:` from upstream workflow nodes, keyed by
    /// `node_id → local_name → value`. Persisted so a `fleet workflow
    /// resume` after a gate, or a `fleet workflow replay --rerun-from`
    /// downstream of an output-producing node, sees the same
    /// upstream context the original run did. Without it, a `when:`
    /// predicate gated on an upstream `outputs:` would always read
    /// the default after a resume/replay boundary.
    #[serde(default)]
    pub outputs: BTreeMap<String, BTreeMap<String, String>>,
    /// Absolute path to the per-session git worktree, when one was
    /// created (i.e. when the workspace is a git repo and fleet
    /// minted `fleet/session-<short-id>` for this run). `None` for
    /// non-git workspaces — in which case the agent runs against
    /// the shared repo root and replay loses its code-state
    /// guarantees. `replay` reads this off the src session to base
    /// the new session's worktree at the same code state.
    #[serde(default)]
    pub worktree_path: Option<PathBuf>,
    /// Name of the git branch checked out in [`Self::worktree_path`].
    /// `replay` uses this as the git ref to base the new session's
    /// worktree on. `None` when [`Self::worktree_path`] is `None`.
    #[serde(default)]
    pub branch: Option<String>,
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
            driver_pid: None,
            node_costs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            worktree_path: None,
            branch: None,
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

    /// Stamp the OS pid of the fleet process currently driving this
    /// session. The reaper uses this to detect "Running but no driver
    /// alive". Bumps `updated_at_ms` so a freshly-claimed session is
    /// visibly recent.
    pub fn set_driver_pid(&mut self, pid: u32, now_ms: u64) {
        self.driver_pid = Some(pid);
        self.updated_at_ms = now_ms;
    }

    /// Clear the driver pid. Called on clean exit (the driver is
    /// relinquishing the session — either it transitioned to a terminal
    /// state, or it parked the session in `AwaitingGate` for a user to
    /// resume later).
    pub fn clear_driver_pid(&mut self, now_ms: u64) {
        self.driver_pid = None;
        self.updated_at_ms = now_ms;
    }

    /// Record an agent's reported USD cost for one workflow node.
    /// Overwrites the previous value if a node fires more than once
    /// (e.g. via `loop_back_to`) — the latest run is the one the user
    /// cares about for "what did this session cost me?" framing.
    pub fn record_node_cost(&mut self, node_id: impl Into<String>, usd: f64, now_ms: u64) {
        self.node_costs.insert(node_id.into(), usd);
        self.updated_at_ms = now_ms;
    }

    /// Record the per-session worktree path + branch fleet minted for
    /// this session. Atomic write so a reader (TUI / reaper) never
    /// sees one half-populated. Bumps `updated_at_ms` so the sidebar
    /// reflects the activity.
    #[allow(dead_code)] // wired by the workflow CLI in a subsequent commit
    pub fn set_worktree(&mut self, path: PathBuf, branch: impl Into<String>, now_ms: u64) {
        self.worktree_path = Some(path);
        self.branch = Some(branch.into());
        self.updated_at_ms = now_ms;
    }

    /// Drop the worktree pointer once the worktree has been pruned
    /// off-disk. Branch name is *kept* — the branch may persist after
    /// the worktree is gone (the user can still `git checkout`/`merge`
    /// it). Bumps `updated_at_ms`.
    #[allow(dead_code)] // wired by `fleet sessions prune` in a subsequent commit
    pub fn clear_worktree_path(&mut self, now_ms: u64) {
        self.worktree_path = None;
        self.updated_at_ms = now_ms;
    }

    /// Sum across [`Self::node_costs`]. Returns `None` only when the
    /// map is empty (no agent ran or no parse succeeded); a session
    /// that genuinely cost zero returns `Some(0.0)`. Consumed by the
    /// display layer (TUI sidebar, `fleet sessions show`); landed
    /// ahead of those callers so the data contract is one commit.
    #[must_use]
    #[allow(dead_code)]
    pub fn total_cost_usd(&self) -> Option<f64> {
        if self.node_costs.is_empty() {
            None
        } else {
            Some(self.node_costs.values().sum())
        }
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
        let err = s.transition_to(SessionState::Completed, 2_000).unwrap_err();
        assert!(format!("{err}").contains("cannot transition"));
        assert_eq!(s.state, SessionState::Created);
        assert_eq!(
            s.updated_at_ms, 1_000,
            "timestamp must not advance on reject"
        );
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
            labels: Vec::new(),
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
    fn new_session_has_no_driver_pid() {
        assert_eq!(fresh().driver_pid, None);
    }

    #[test]
    fn set_driver_pid_stamps_and_bumps_timestamp() {
        let mut s = fresh();
        s.set_driver_pid(4242, 2_000);
        assert_eq!(s.driver_pid, Some(4242));
        assert_eq!(s.updated_at_ms, 2_000);
    }

    #[test]
    fn clear_driver_pid_resets_to_none_and_bumps_timestamp() {
        let mut s = fresh();
        s.set_driver_pid(4242, 2_000);
        s.clear_driver_pid(3_000);
        assert_eq!(s.driver_pid, None);
        assert_eq!(s.updated_at_ms, 3_000);
    }

    #[test]
    fn driver_pid_round_trips_through_json() {
        let mut s = fresh();
        s.set_driver_pid(4242, 2_000);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.driver_pid, Some(4242));
    }

    #[test]
    fn missing_driver_pid_defaults_to_none_on_load() {
        // Backward compatibility with meta.json from before driver_pid
        // landed.
        let json = r#"{
            "id": "s-1",
            "workflow": "standard",
            "state": "running",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert_eq!(s.driver_pid, None);
    }

    #[test]
    fn new_session_has_empty_node_costs() {
        assert!(fresh().node_costs.is_empty());
    }

    #[test]
    fn record_node_cost_inserts_and_bumps_timestamp() {
        let mut s = fresh();
        s.record_node_cost("plan", 0.42, 2_000);
        assert_eq!(s.node_costs.get("plan"), Some(&0.42));
        assert_eq!(s.updated_at_ms, 2_000);
    }

    #[test]
    fn record_node_cost_overwrites_on_repeat() {
        // A `loop_back_to` cycle re-runs the same node. The latest
        // cost is what we keep — earlier figures already showed up in
        // the user's run.
        let mut s = fresh();
        s.record_node_cost("review", 0.10, 2_000);
        s.record_node_cost("review", 0.25, 3_000);
        assert_eq!(s.node_costs.get("review"), Some(&0.25));
    }

    #[test]
    fn total_cost_usd_returns_none_when_no_nodes_recorded() {
        assert_eq!(fresh().total_cost_usd(), None);
    }

    #[test]
    fn total_cost_usd_returns_some_zero_when_agent_reported_zero() {
        // The "agent ran and cost zero" case must not collapse into
        // the same Option::None as "agent never ran". Lock the
        // distinction.
        let mut s = fresh();
        s.record_node_cost("plan", 0.0, 2_000);
        assert_eq!(s.total_cost_usd(), Some(0.0));
    }

    #[test]
    fn total_cost_usd_sums_every_node() {
        let mut s = fresh();
        s.record_node_cost("plan", 0.10, 2_000);
        s.record_node_cost("implement", 0.30, 3_000);
        s.record_node_cost("review", 0.05, 4_000);
        let total = s.total_cost_usd().unwrap();
        assert!((total - 0.45).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn node_costs_round_trip_through_json() {
        let mut s = fresh();
        s.record_node_cost("plan", 0.42, 2_000);
        s.record_node_cost("review", 0.13, 3_000);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.node_costs.get("plan"), Some(&0.42));
    }

    #[test]
    fn missing_node_costs_defaults_to_empty_on_load() {
        let json = r#"{
            "id": "s-1",
            "workflow": "standard",
            "state": "running",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(s.node_costs.is_empty());
    }

    #[test]
    fn new_session_has_no_worktree_or_branch() {
        let s = fresh();
        assert!(s.worktree_path.is_none());
        assert!(s.branch.is_none());
    }

    #[test]
    fn set_worktree_stamps_both_fields_and_bumps_timestamp() {
        let mut s = fresh();
        s.set_worktree(
            PathBuf::from("/repo/.fleet/sessions/s-test/worktree"),
            "fleet/session-s-test",
            2_000,
        );
        assert_eq!(
            s.worktree_path.as_deref(),
            Some(std::path::Path::new(
                "/repo/.fleet/sessions/s-test/worktree"
            ))
        );
        assert_eq!(s.branch.as_deref(), Some("fleet/session-s-test"));
        assert_eq!(s.updated_at_ms, 2_000);
    }

    #[test]
    fn clear_worktree_path_keeps_branch_and_bumps_timestamp() {
        // Branch survives the worktree directory — the user can still
        // `git checkout fleet/session-<id>` after `fleet sessions prune`
        // removed the on-disk worktree.
        let mut s = fresh();
        s.set_worktree(
            PathBuf::from("/repo/.fleet/sessions/s-test/worktree"),
            "fleet/session-s-test",
            2_000,
        );
        s.clear_worktree_path(3_000);
        assert!(s.worktree_path.is_none());
        assert_eq!(
            s.branch.as_deref(),
            Some("fleet/session-s-test"),
            "branch must persist after worktree pruning"
        );
        assert_eq!(s.updated_at_ms, 3_000);
    }

    #[test]
    fn worktree_round_trips_through_json() {
        let mut s = fresh();
        s.set_worktree(
            PathBuf::from("/repo/.fleet/sessions/s-test/worktree"),
            "fleet/session-s-test",
            2_000,
        );
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn missing_worktree_fields_default_to_none_on_load() {
        // Backward compatibility with meta.json written before the
        // worktree fields landed: serde(default) makes both keys
        // optional on the wire.
        let json = r#"{
            "id": "s-1",
            "workflow": "standard",
            "state": "created",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(s.worktree_path.is_none());
        assert!(s.branch.is_none());
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
