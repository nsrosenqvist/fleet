//! Orchestrator reaper: if the persisted meta says
//! Active/Detached but the tmux pane is gone, flip it to
//! [`OrchestratorState::Closed`] so the next reuse pass knows it
//! needs to respawn the agent.
//!
//! Why: the orchestrator CLI updates the meta on detach only when
//! fleet itself drove the attach. A pane killed externally
//! (`tmux kill-server`, `Ctrl-B :kill-session`, parent terminal
//! crash) leaves meta stuck in `Active`/`Detached`. The reaper
//! closes that gap so `fleet orchestrator` and the TUI sidebar
//! reflect tmux truth on every refresh.
//!
//! Pure-ish: takes the store + invoker + clock as arguments so
//! tests pin behaviour with `MockProcessInvoker`. The CLI + TUI
//! wrappers build production handles and call this directly.

use anyhow::Result;

use super::store::OrchestratorStore;
use super::tmux;
use super::{OrchestratorState, TMUX_SESSION_NAME};
use crate::process::ProcessInvoker;

/// Report of one sweep — for diagnostic logging, mostly. Tests
/// assert on the field values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Whether any persisted meta was inspected. False when no
    /// orchestrator has ever spawned in this repo.
    pub scanned: bool,
    /// Whether the orchestrator was flipped to Closed during this
    /// sweep.
    pub closed: bool,
    /// Load / save failure description, if any. A corrupt meta
    /// surfaces here so the caller can log it without aborting.
    pub failed: Option<String>,
}

/// If the orchestrator meta says Active/Detached but the tmux pane
/// is gone, mark it Closed. Idempotent: a session that's already
/// Closed (or whose pane is still alive) is left untouched. Returns
/// the report so callers can log it.
///
/// All failure modes (corrupt meta, save errors) are captured in
/// `ReapReport.failed` rather than returning Err — a corrupt meta
/// shouldn't block the rest of fleet from running. Result is kept
/// in the signature as a future-proof slot for fatal failures that
/// might be added later.
#[allow(clippy::unnecessary_wraps)]
pub fn reap(
    store: &OrchestratorStore,
    invoker: &dyn ProcessInvoker,
    now_ms: u64,
) -> Result<ReapReport> {
    let mut report = ReapReport::default();
    if !store.exists() {
        return Ok(report);
    }
    report.scanned = true;
    let mut session = match store.load() {
        Ok(s) => s,
        Err(err) => {
            report.failed = Some(format!("load: {err:#}"));
            return Ok(report);
        }
    };
    // Already-Closed sessions are stable; no need to re-check tmux.
    if session.state == OrchestratorState::Closed {
        return Ok(report);
    }
    if tmux::has_session(invoker, TMUX_SESSION_NAME) {
        return Ok(report);
    }
    session.state = OrchestratorState::Closed;
    session.updated_at_ms = now_ms;
    match store.save(&session) {
        Ok(()) => report.closed = true,
        Err(err) => report.failed = Some(format!("save: {err:#}")),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::OrchestratorSession;
    use crate::process::MockProcessInvoker;
    use anyhow::anyhow;
    use mockall::predicate::eq;
    use std::sync::Arc;

    fn store_at(dir: &tempfile::TempDir) -> OrchestratorStore {
        OrchestratorStore::at(dir.path())
    }

    fn session(state: OrchestratorState) -> OrchestratorSession {
        let mut s = OrchestratorSession::new("claude", 1);
        s.state = state;
        s
    }

    /// Invoker that reports the fixed `fl-orchestrator` tmux session
    /// as alive or dead. Any unmatched query returns an error so the
    /// test fails loudly on accidental probes.
    fn invoker_with_alive(alive: bool) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("tmux"),
                eq(vec![
                    "has-session".to_string(),
                    "-t".to_string(),
                    TMUX_SESSION_NAME.to_string(),
                ]),
            )
            .returning(move |_, _| {
                if alive {
                    Ok(String::new())
                } else {
                    Err(anyhow!("can't find session: {TMUX_SESSION_NAME}"))
                }
            });
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("unexpected tmux call")));
        Arc::new(mock)
    }

    #[test]
    fn reap_marks_dead_pane_closed_and_leaves_live_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store.save(&session(OrchestratorState::Active)).unwrap();
        let invoker = invoker_with_alive(false);
        let report = reap(&store, invoker.as_ref(), 999).unwrap();
        assert!(report.scanned);
        assert!(report.closed);
        assert!(report.failed.is_none());
        let after = store.load().unwrap();
        assert_eq!(after.state, OrchestratorState::Closed);
        assert_eq!(after.updated_at_ms, 999);
    }

    #[test]
    fn reap_leaves_active_alone_when_pane_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let mut s = session(OrchestratorState::Active);
        s.updated_at_ms = 100;
        store.save(&s).unwrap();
        let invoker = invoker_with_alive(true);
        let report = reap(&store, invoker.as_ref(), 999).unwrap();
        assert!(report.scanned);
        assert!(!report.closed);
        let after = store.load().unwrap();
        // updated_at_ms unchanged — we didn't re-save.
        assert_eq!(after.updated_at_ms, 100);
    }

    #[test]
    fn reap_skips_already_closed_meta_without_calling_tmux() {
        // A Closed orchestrator shouldn't trigger a has-session call
        // (tmux state is irrelevant for a terminal session, and the
        // call would be wasted I/O). The mock has no has_session
        // expectation, so any call would hit the catch-all error.
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store.save(&session(OrchestratorState::Closed)).unwrap();
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("unexpected tmux call")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let report = reap(&store, invoker.as_ref(), 1_000).unwrap();
        assert!(report.scanned);
        assert!(!report.closed);
    }

    #[test]
    fn reap_returns_unscanned_for_a_repo_with_no_orchestrator_yet() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path().join("never-created"));
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("unexpected tmux call")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let report = reap(&store, invoker.as_ref(), 0).unwrap();
        assert!(!report.scanned);
        assert!(!report.closed);
    }
}
