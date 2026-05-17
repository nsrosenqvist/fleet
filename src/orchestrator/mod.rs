//! The orchestrator: one fixed per-repo interactive agent session
//! (Claude Code by default) that the user attaches to for
//! planning + worker coordination conversations.
//!
//! Shape, distinct from workflow sessions:
//!
//! - Runs on the *host*, not in a container. No worktree, no
//!   per-ticket scoping.
//! - Wrapped in a fixed-name tmux session (`fl-orchestrator`) so
//!   the user can attach / detach freely across terminal restarts.
//! - Wider tool surface than the bridge: tracker reads + creates,
//!   plan management, sessions inspection, deps surgery — reached
//!   via the host `fleet` CLI, no HTTP plumbing.
//! - Transcript captured to `.fleet/orchestrator/transcript.log`
//!   so a re-attach (or a post-hoc audit) has the prior context.
//! - **Single session by design.** There is no id minting, no
//!   listing, no concurrent orchestrators. State lives flat at
//!   `.fleet/orchestrator/` with `meta.json`, `prompt.md`,
//!   `transcript.log`, and (for aider) `aider-chat-history.md`
//!   in that one directory.
//!
//! tmux invocation, the prompt template, the launcher strategies
//! (per-agent argv shaping), and the CLI surface all live in
//! sibling modules that build on this one.

use serde::{Deserialize, Serialize};

pub mod launcher;
pub mod prompt;
pub mod reaper;
pub mod store;
pub mod tmux;

/// The fixed tmux session name. Matches the legacy `fl-orchestrator`
/// label the TUI sidebar pane shows.
pub const TMUX_SESSION_NAME: &str = "fl-orchestrator";

/// Lifecycle of the orchestrator. Distinct from the workflow
/// session state machine because the orchestrator doesn't have
/// `Running`/`Failed`/`Crashed` — it's user-driven, with the tmux
/// pane either attached, detached, or closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorState {
    /// User is currently attached, agent is running.
    Active,
    /// Tmux pane alive but no user attached. Re-attach via
    /// `fleet orchestrator` resumes.
    Detached,
    /// Session ended (user explicitly killed or tmux pane gone).
    /// Not terminal: `fleet orchestrator` will respawn the agent
    /// next time it's invoked.
    Closed,
}

/// The orchestrator session record. Persisted at
/// `.fleet/orchestrator/meta.json`. There is exactly one of these
/// per repo (or zero, before first spawn).
///
/// `pid` is the tmux *server* pid captured at start, used as a
/// liveness probe — if the pid is gone, the session is treated as
/// Closed on next list/inspect (similar to how `Session::pid` +
/// the reaper interact for workflow sessions).
///
/// `agent_session_id` is the agent's own conversation handle, when
/// the agent supports pre-assigning one. For claude it's the UUID
/// passed to `--session-id` at spawn; on respawn we use it as
/// `claude --resume <uuid>` so the conversation continues. Aider
/// and codex don't take a pre-assigned id, so this stays None for
/// them; they recover continuity through `--restore-chat-history`
/// and `codex resume --last` respectively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorSession {
    pub agent: String,
    pub state: OrchestratorState,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl OrchestratorSession {
    /// Build a fresh Active orchestrator record for `agent`. The
    /// tmux session name is fixed ([`TMUX_SESSION_NAME`]) so it
    /// doesn't need to be carried in the value object.
    #[must_use]
    pub fn new(agent: impl Into<String>, created_at_ms: u64) -> Self {
        Self {
            agent: agent.into(),
            state: OrchestratorState::Active,
            pid: None,
            agent_session_id: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_session_starts_active_with_no_pid_or_agent_session_id() {
        let s = OrchestratorSession::new("claude", 100);
        assert_eq!(s.state, OrchestratorState::Active);
        assert_eq!(s.agent, "claude");
        assert_eq!(s.created_at_ms, 100);
        assert_eq!(s.updated_at_ms, 100);
        assert!(s.pid.is_none());
        assert!(s.agent_session_id.is_none());
    }

    #[test]
    fn orchestrator_session_round_trips_through_json_with_full_fields() {
        let mut s = OrchestratorSession::new("claude-code", 100);
        s.state = OrchestratorState::Detached;
        s.pid = Some(12345);
        s.agent_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string());
        s.updated_at_ms = 200;
        let json = serde_json::to_string(&s).unwrap();
        let round: OrchestratorSession = serde_json::from_str(&json).unwrap();
        assert_eq!(round, s);
        assert!(json.contains("\"state\":\"detached\""), "json: {json}");
        assert!(
            json.contains("\"agent_session_id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\""),
            "json: {json}",
        );
    }

    #[test]
    fn orchestrator_session_tolerates_missing_pid_and_agent_session_id_on_load() {
        // Brand-new sessions may serialise without `pid` set yet
        // (the tmux probe runs after meta is first written).
        // `#[serde(default)]` should yield None for both optional
        // fields.
        let json = r#"{
            "agent": "claude-code",
            "state": "active",
            "created_at_ms": 1,
            "updated_at_ms": 1
        }"#;
        let s: OrchestratorSession = serde_json::from_str(json).unwrap();
        assert!(s.pid.is_none());
        assert!(s.agent_session_id.is_none());
    }

    #[test]
    fn tmux_session_name_constant_is_fl_orchestrator() {
        // Both the CLI spawn path and the TUI sidebar display logic
        // read this constant. Pin it so a rename here can't
        // silently desync from the user-facing label.
        assert_eq!(TMUX_SESSION_NAME, "fl-orchestrator");
    }
}
