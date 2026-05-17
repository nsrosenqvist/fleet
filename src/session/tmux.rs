//! Naming convention + env-var name for the per-session tmux pane.
//!
//! Workflow sessions launched with `fleet workflow run --detached`
//! live inside a `tmux new-session -d -s fleet-<session-id>`. The
//! relationship between session id and tmux session name is purely
//! conventional — we don't store the tmux name on the [`Session`]
//! value object — so every caller derives it via [`worker_tmux_name`]
//! to keep the spelling identical across the codebase.
//!
//! The inner workflow run (the process inside the tmux session)
//! signals "I'm running detached" to its own executor via the
//! [`SESSION_TMUX_ENV`] env var, which the wrapper exports when it
//! spawns. Agent nodes branch on that to use `attach_pty` (live in
//! the pane) instead of `exec` (silent stdout capture).

use super::SessionId;

/// Env var the wrapper drops into the tmux session so the inner
/// `fleet workflow run` knows it's executing inside a per-session
/// pane. The value is the tmux session name (e.g. `fleet-s-…`) so
/// downstream code that needs to address tmux (`pipe-pane`,
/// `display-message`) doesn't have to re-derive it.
pub const SESSION_TMUX_ENV: &str = "FLEET_TMUX_SESSION";

/// `fleet-<session-id>` — the canonical tmux session name for a
/// detached workflow run. Stable so the TUI's refresh thread,
/// `fleet sessions attach`, and the workflow wrapper all agree on
/// what to pass to `tmux -t`.
#[must_use]
pub fn worker_tmux_name(id: &SessionId) -> String {
    format!("fleet-{}", id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_tmux_name_prefixes_session_id_with_fleet_dash() {
        // Sole purpose of this helper is to keep the spelling pinned;
        // assert the literal so a future "rename" can't silently
        // diverge across the wrapper / TUI / attach CLI.
        let id = SessionId::new("s-abc-0001");
        assert_eq!(worker_tmux_name(&id), "fleet-s-abc-0001");
    }

    #[test]
    fn session_tmux_env_var_name_stays_pinned() {
        // The agent registry and any user-side hook scripts may key
        // off this name; treat it as a public contract.
        assert_eq!(SESSION_TMUX_ENV, "FLEET_TMUX_SESSION");
    }
}
