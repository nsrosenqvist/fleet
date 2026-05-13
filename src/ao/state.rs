//! Serde types modeling `ao status --json`, `ao session ls --json`, and
//! `ao events list --json` outputs.
//!
//! Both endpoints return `{ "data": [..], "meta": { ... } }`. The per-session
//! shapes overlap but are not identical. `SessionInfo` is the union of both,
//! with every field optional + `#[serde(alias = ...)]` so it deserializes
//! either output cleanly. Fields are documented with which command sources them.

#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq, Eq)]
pub struct AoResponse<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    #[serde(default)]
    pub meta: AoMeta,
}

#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AoMeta {
    #[serde(default)]
    pub hidden_terminated_count: u32,
}

/// Union of fields from `ao status --json` and `ao session ls --json`.
/// Field-permissive on purpose: AO is pre-1.0, schemas drift.
///
/// Note: `Eq` is implementable here because `serde_json::Value` itself is `Eq`
/// (its `Number` variant excludes `NaN` deliberately).
#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Session id. `ao status` calls this `name`; `ao session ls` calls it `id`.
    #[serde(default, alias = "name")]
    pub id: Option<String>,

    #[serde(default)]
    pub role: Option<String>,

    #[serde(default)]
    pub branch: Option<String>,

    /// Lifecycle status: "spawning", "working", "stuck", "done", etc.
    #[serde(default)]
    pub status: Option<String>,

    /// Project id. `ao status` calls this `project`; `ao session ls` calls it `projectId`.
    #[serde(default, alias = "project")]
    pub project_id: Option<String>,

    /// Project display name (only on `ao session ls`).
    #[serde(default)]
    pub project_name: Option<String>,

    /// Tracker issue id. `ao status` calls this `issue`; `ao session ls` calls it `issueId`.
    #[serde(default, alias = "issue")]
    pub issue_id: Option<String>,

    /// PR URL or object. AO's shape varies; keep raw for now.
    #[serde(default)]
    pub pr: Option<serde_json::Value>,

    #[serde(default)]
    pub pr_number: Option<i64>,

    /// Worktree path inside the VM (only on `ao session ls`).
    #[serde(default)]
    pub workspace_path: Option<String>,

    /// Live activity: `idle`, `working`, `waiting_input`, `blocked`, etc.
    #[serde(default)]
    pub activity: Option<String>,

    /// Human-readable last activity (e.g. "50m ago") — `ao status` only.
    #[serde(default)]
    pub last_activity: Option<String>,

    /// ISO-8601 last activity timestamp — `ao session ls` only.
    #[serde(default)]
    pub last_activity_at: Option<String>,

    /// Agent's auto-generated summary.
    #[serde(default)]
    pub summary: Option<String>,

    /// Claude-specific summary, if the agent is claude-code.
    #[serde(default)]
    pub claude_summary: Option<String>,

    #[serde(default)]
    pub ci_status: Option<String>,

    #[serde(default)]
    pub review_decision: Option<String>,

    #[serde(default)]
    pub pending_threads: Option<u32>,

    /// Per-session agent reports. Raw for now; can be modeled when needed.
    #[serde(default)]
    pub reports: Vec<serde_json::Value>,
}

/// Per-session lifecycle/agent state read from
/// `~/.agent-orchestrator/projects/<project>/sessions/<id>.json`.
/// Populated by [`super::invoke::Ao::session_meta_bulk`] from inside
/// the VM (the JSON files live on the guest filesystem, the host can't
/// read them directly).
///
/// Surfaces fields that `ao session ls --json` doesn't expose but the
/// sidebar wants to badge on:
///
/// - `agent_reported_state` is what the agent last self-declared via
///   `ao report …` (most often `"completed"` for the "done" badge).
/// - `runtime_state` / `runtime_reason` reveal an unexpectedly-dead
///   tmux pane (`runtime.state == "missing"` with a reason other
///   than `"manual_kill_requested"` — the same criterion the crash
///   sweep uses, minus the age threshold).
#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    #[serde(default)]
    pub agent_reported_state: Option<String>,
    #[serde(default)]
    pub session_state: Option<String>,
    #[serde(default)]
    pub session_reason: Option<String>,
    #[serde(default)]
    pub runtime_state: Option<String>,
    #[serde(default)]
    pub runtime_reason: Option<String>,
}

impl SessionMeta {
    /// True when the agent has self-reported `"completed"`. Drives the
    /// green `✓ done` chip in the sidebar.
    #[must_use]
    pub fn agent_done(&self) -> bool {
        self.agent_reported_state.as_deref() == Some("completed")
    }

    /// True when the session's runtime has died unexpectedly — i.e.
    /// the tmux pane is gone but the user didn't `ao session kill` it.
    /// Drives the red `crashed` chip. Mirrors the eligibility filter
    /// in [`crate::tui::cleanup`] minus the age threshold (the badge
    /// should appear immediately, before the sweep would consider
    /// reaping).
    #[must_use]
    pub fn runtime_crashed(&self) -> bool {
        self.runtime_state.as_deref() == Some("missing")
            && self.runtime_reason.as_deref() != Some("manual_kill_requested")
            && self.session_reason.as_deref() != Some("manually_killed")
    }
}

/// Wrapper for `ao events list --json`. The top-level shape uses
/// `events` (not `data`), so we can't reuse [`AoResponse`].
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct EventsResponse {
    #[serde(default = "Vec::new")]
    pub events: Vec<EventInfo>,
}

/// One row from the AO event log. Captures session spawns/kills,
/// lifecycle transitions, CI failures, review events, etc. Used by
/// fleet's bottom-pane ticker for system-wide situational awareness.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventInfo {
    /// Sequential id, monotonic per-project. Useful as a `selected`
    /// anchor and for de-duping when the log is re-fetched.
    #[serde(default)]
    pub id: u64,

    /// ISO-8601 timestamp; we display the local time-of-day portion.
    #[serde(default)]
    pub ts: Option<String>,

    #[serde(default)]
    pub project_id: Option<String>,

    /// Empty for project-wide events (spawn-failed before a session
    /// was created, etc.).
    #[serde(default)]
    pub session_id: Option<String>,

    /// `session.spawned`, `session.killed`, `lifecycle.transition`,
    /// `ci.failed`, `review.requested`, etc. Free-form per AO's roadmap.
    #[serde(default)]
    pub kind: Option<String>,

    /// `info`, `warn`, `error`, `debug`. Drives the row's foreground colour.
    #[serde(default)]
    pub level: Option<String>,

    #[serde(default)]
    pub summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured live from `limactl shell fleet-vm -- ao status --json` against
    // a sandbox with one spawning worker session.
    const STATUS_JSON: &str = r#"
    {
      "data": [
        {
          "name": "sb-1",
          "role": "worker",
          "branch": "agent/fa8098a",
          "status": "spawning",
          "summary": null,
          "claudeSummary": null,
          "pr": null,
          "prNumber": null,
          "issue": "fa8098a",
          "lastActivity": "50m ago",
          "project": "sandbox",
          "ciStatus": null,
          "reviewDecision": null,
          "pendingThreads": null,
          "activity": "idle",
          "reports": []
        }
      ],
      "meta": { "hiddenTerminatedCount": 0 }
    }
    "#;

    // Captured live from `ao session ls --json` against the same session.
    const SESSION_LS_JSON: &str = r#"
    {
      "data": [
        {
          "id": "sb-1",
          "projectId": "sandbox",
          "projectName": "sandbox",
          "role": "worker",
          "branch": "agent/fa8098a",
          "status": "spawning",
          "issueId": "fa8098a",
          "pr": null,
          "workspacePath": "/home/niklas.guest/.agent-orchestrator/projects/sandbox/worktrees/sb-1",
          "lastActivityAt": "2026-05-12T13:39:14.000Z"
        }
      ],
      "meta": { "hiddenTerminatedCount": 0 }
    }
    "#;

    #[test]
    fn deserializes_status_json() {
        let r: AoResponse<SessionInfo> = serde_json::from_str(STATUS_JSON).expect("parse ok");
        assert_eq!(r.data.len(), 1);
        assert_eq!(r.meta.hidden_terminated_count, 0);
        let s = &r.data[0];
        assert_eq!(s.id.as_deref(), Some("sb-1"));
        assert_eq!(s.role.as_deref(), Some("worker"));
        assert_eq!(s.branch.as_deref(), Some("agent/fa8098a"));
        assert_eq!(s.status.as_deref(), Some("spawning"));
        assert_eq!(s.project_id.as_deref(), Some("sandbox"));
        assert_eq!(s.issue_id.as_deref(), Some("fa8098a"));
        assert_eq!(s.activity.as_deref(), Some("idle"));
        assert_eq!(s.last_activity.as_deref(), Some("50m ago"));
    }

    #[test]
    fn deserializes_session_ls_json() {
        let r: AoResponse<SessionInfo> = serde_json::from_str(SESSION_LS_JSON).expect("parse ok");
        assert_eq!(r.data.len(), 1);
        let s = &r.data[0];
        assert_eq!(s.id.as_deref(), Some("sb-1"));
        assert_eq!(s.project_id.as_deref(), Some("sandbox"));
        assert_eq!(s.project_name.as_deref(), Some("sandbox"));
        assert_eq!(s.issue_id.as_deref(), Some("fa8098a"));
        assert_eq!(
            s.workspace_path.as_deref(),
            Some("/home/niklas.guest/.agent-orchestrator/projects/sandbox/worktrees/sb-1")
        );
        assert_eq!(
            s.last_activity_at.as_deref(),
            Some("2026-05-12T13:39:14.000Z")
        );
    }

    #[test]
    fn tolerates_unknown_fields() {
        // AO might add fields later; we must not crash.
        let json = r#"
        {
          "data": [
            {
              "id": "x",
              "futureField": "something new",
              "deepStuff": { "nested": true }
            }
          ],
          "meta": { "hiddenTerminatedCount": 0, "extra": 42 }
        }
        "#;
        let r: AoResponse<SessionInfo> = serde_json::from_str(json).expect("parse ok");
        assert_eq!(r.data[0].id.as_deref(), Some("x"));
    }

    /// Captured live from `ao events list --json` against the fleet
    /// project after a spawn+kill cycle. Holds the real shape of the
    /// top-level wrapper (`events`, not `data`) and the per-event
    /// rows fleet's bottom-pane ticker renders.
    const EVENTS_JSON: &str = r#"{
      "version": 1,
      "meta": { "resultCount": 2 },
      "events": [
        {
          "id": 10,
          "tsEpoch": 1778674371946,
          "ts": "2026-05-13T12:12:51.946Z",
          "projectId": "fleet",
          "sessionId": "fl-2",
          "source": "session-manager",
          "kind": "session.killed",
          "level": "info",
          "summary": "killed: fl-2",
          "data": { "reason": "manually_killed" }
        },
        {
          "id": 9,
          "ts": "2026-05-13T12:12:41.412Z",
          "projectId": "fleet",
          "sessionId": "fl-2",
          "kind": "session.spawned",
          "level": "info",
          "summary": "spawned: fl-2"
        }
      ]
    }"#;

    #[test]
    fn deserializes_events_json() {
        let r: EventsResponse = serde_json::from_str(EVENTS_JSON).expect("parse ok");
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].id, 10);
        assert_eq!(r.events[0].kind.as_deref(), Some("session.killed"));
        assert_eq!(r.events[0].level.as_deref(), Some("info"));
        assert_eq!(r.events[0].session_id.as_deref(), Some("fl-2"));
        assert_eq!(r.events[1].summary.as_deref(), Some("spawned: fl-2"));
    }

    #[test]
    fn session_meta_agent_done_only_on_completed() {
        let mut m = SessionMeta::default();
        assert!(!m.agent_done(), "default empty meta isn't done");
        m.agent_reported_state = Some("working".into());
        assert!(!m.agent_done());
        m.agent_reported_state = Some("completed".into());
        assert!(m.agent_done());
    }

    #[test]
    fn session_meta_runtime_crashed_excludes_manual_kills() {
        // Definition of a crash: runtime is missing AND the user
        // didn't ask for it (the kill path sets reason
        // "manual_kill_requested" / sessionReason "manually_killed").
        let mut m = SessionMeta {
            runtime_state: Some("missing".into()),
            runtime_reason: Some("tmux_pane_died".into()),
            session_reason: Some("runtime_crashed".into()),
            ..Default::default()
        };
        assert!(m.runtime_crashed(), "tmux pane death is a crash");

        m.runtime_reason = Some("manual_kill_requested".into());
        assert!(
            !m.runtime_crashed(),
            "manually-killed runtime must not register as a crash",
        );

        m.runtime_reason = Some("tmux_pane_died".into());
        m.session_reason = Some("manually_killed".into());
        assert!(
            !m.runtime_crashed(),
            "manually-killed session must not register as a crash",
        );

        m.session_reason = Some("runtime_crashed".into());
        m.runtime_state = Some("alive".into());
        assert!(!m.runtime_crashed(), "alive runtime can't be crashed");
    }

    #[test]
    fn session_meta_round_trips_through_bulk_shape() {
        // The shape emitted by the META_NODE_SCRIPT — verify our serde
        // attributes line up with the camelCase keys it produces.
        let raw = r#"{
            "agentReportedState": "completed",
            "sessionState": "idle",
            "sessionReason": "research_complete",
            "runtimeState": "missing",
            "runtimeReason": "tmux_pane_died"
        }"#;
        let m: SessionMeta = serde_json::from_str(raw).expect("parse");
        assert_eq!(m.agent_reported_state.as_deref(), Some("completed"));
        assert_eq!(m.session_state.as_deref(), Some("idle"));
        assert_eq!(m.session_reason.as_deref(), Some("research_complete"));
        assert_eq!(m.runtime_state.as_deref(), Some("missing"));
        assert_eq!(m.runtime_reason.as_deref(), Some("tmux_pane_died"));
        assert!(m.agent_done());
        assert!(m.runtime_crashed());
    }

    #[test]
    fn deserializes_events_with_missing_optional_fields() {
        // Defensive: a future AO build might drop or rename
        // optional fields. As long as the top-level `events` array
        // is present, the response should parse and the unknown
        // bits land as `None`.
        let json = r#"{ "events": [ { "id": 1 } ] }"#;
        let r: EventsResponse = serde_json::from_str(json).expect("parse ok");
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].kind.is_none());
        assert!(r.events[0].summary.is_none());
    }
}
