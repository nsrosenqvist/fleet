//! Serde types modeling `ao status --json` and `ao session ls --json` outputs.
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
}
