//! Brainstorm reaper: sweep `.fleet/planning/<id>/` entries and
//! mark any session whose tmux pane is gone as
//! [`BrainstormState::Closed`].
//!
//! Why: the brainstorm CLI updates the meta on detach (see
//! `run_default` / `run_attach`) only when fleet itself drove the
//! attach. A pane killed externally (`tmux kill-server`,
//! `Ctrl-B :kill-session`, parent terminal crash) leaves meta
//! stuck in `Active`/`Detached`. The reaper closes that gap so
//! `fleet brainstorm list` and the TUI sidebar reflect tmux truth
//! on every refresh.
//!
//! Pure-ish: takes the store + invoker + clock as arguments so
//! tests pin behaviour with `MockProcessInvoker`. The CLI + TUI
//! wrappers build production handles and call this directly.

use anyhow::Result;

use super::store::BrainstormStore;
use super::tmux;
use super::{BrainstormId, BrainstormState};
use crate::process::ProcessInvoker;

/// Report of one sweep — for diagnostic logging, mostly. Tests
/// assert on the field values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub scanned: usize,
    pub closed: Vec<BrainstormId>,
    /// Per-session load / save failures — surfaced so a corrupt
    /// `meta.json` doesn't block the rest of the sweep.
    pub failed: Vec<(BrainstormId, String)>,
}

/// Mark every Active/Detached brainstorm whose tmux pane is gone
/// as Closed. Idempotent: a session that's already Closed (or
/// whose pane is still alive) is left untouched. Returns the
/// report so callers can log it.
pub fn reap(
    store: &BrainstormStore,
    invoker: &dyn ProcessInvoker,
    now_ms: u64,
) -> Result<ReapReport> {
    let mut report = ReapReport::default();
    let ids = store.list()?;
    for id in ids {
        report.scanned += 1;
        let mut session = match store.load(&id) {
            Ok(s) => s,
            Err(err) => {
                report.failed.push((id, format!("load: {err:#}")));
                continue;
            }
        };
        // Already-Closed sessions are stable; no need to re-check
        // tmux.
        if session.state == BrainstormState::Closed {
            continue;
        }
        if tmux::has_session(invoker, &session.tmux_session) {
            continue;
        }
        session.state = BrainstormState::Closed;
        session.updated_at_ms = now_ms;
        match store.save(&session) {
            Ok(()) => report.closed.push(session.id),
            Err(err) => report.failed.push((session.id, format!("save: {err:#}"))),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brainstorm::BrainstormSession;
    use crate::process::MockProcessInvoker;
    use anyhow::anyhow;
    use mockall::predicate::eq;
    use std::sync::Arc;

    fn store_at(dir: &tempfile::TempDir) -> BrainstormStore {
        BrainstormStore::at(dir.path())
    }

    fn session(id: &str, state: BrainstormState) -> BrainstormSession {
        let mut s = BrainstormSession::new(BrainstormId::new(id), "claude", 1);
        s.state = state;
        s
    }

    /// Invoker that reports each named tmux session as alive or
    /// dead per the `(name, alive)` tuples. Any unmatched query
    /// returns "session not found" so the reaper treats it as
    /// dead.
    fn invoker_with_alive(alive: &[(&'static str, bool)]) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        for (name, is_alive) in alive {
            let copy_alive = *is_alive;
            let copy_name = name.to_string();
            mock.expect_run()
                .with(
                    eq("tmux"),
                    eq(vec![
                        "has-session".to_string(),
                        "-t".to_string(),
                        copy_name.clone(),
                    ]),
                )
                .returning(move |_, _| {
                    if copy_alive {
                        Ok(String::new())
                    } else {
                        Err(anyhow!("can't find session: {copy_name}"))
                    }
                });
        }
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("unexpected tmux call")));
        Arc::new(mock)
    }

    #[test]
    fn reap_marks_dead_panes_closed_and_leaves_live_ones_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store
            .save(&session("b-live", BrainstormState::Active))
            .unwrap();
        store
            .save(&session("b-dead", BrainstormState::Detached))
            .unwrap();
        let invoker = invoker_with_alive(&[
            ("fleet-brainstorm-b-live", true),
            ("fleet-brainstorm-b-dead", false),
        ]);
        let report = reap(&store, invoker.as_ref(), 999).unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.closed, vec![BrainstormId::new("b-dead")]);
        assert!(report.failed.is_empty());
        // Disk reflects the truth.
        let live = store.load(&BrainstormId::new("b-live")).unwrap();
        assert_eq!(live.state, BrainstormState::Active);
        let dead = store.load(&BrainstormId::new("b-dead")).unwrap();
        assert_eq!(dead.state, BrainstormState::Closed);
        assert_eq!(dead.updated_at_ms, 999);
    }

    #[test]
    fn reap_skips_already_closed_sessions_without_calling_tmux() {
        // A session already marked Closed shouldn't trigger a
        // has-session call (tmux state is irrelevant for a
        // terminal session, and the call would be wasted I/O).
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store
            .save(&session("b-done", BrainstormState::Closed))
            .unwrap();
        // Mock has no `has_session` expectation, so any call would
        // fall through to the catch-all "unexpected tmux call"
        // error. If reap erroneously calls tmux, the test fails.
        let invoker = invoker_with_alive(&[]);
        let report = reap(&store, invoker.as_ref(), 1_000).unwrap();
        assert_eq!(report.scanned, 1);
        assert!(report.closed.is_empty());
    }

    #[test]
    fn reap_returns_empty_for_a_repo_with_no_brainstorms() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path().join("never-created"));
        let invoker = invoker_with_alive(&[]);
        let report = reap(&store, invoker.as_ref(), 0).unwrap();
        assert_eq!(report.scanned, 0);
        assert!(report.closed.is_empty());
    }

    #[test]
    fn reap_does_not_touch_an_already_closed_meta_on_dead_pane() {
        // Same shape as the "skip Closed" test but with the pane
        // explicitly absent. Either way the session is stable.
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let mut s = session("b-closed", BrainstormState::Closed);
        s.updated_at_ms = 100;
        store.save(&s).unwrap();
        let invoker = invoker_with_alive(&[]);
        let _ = reap(&store, invoker.as_ref(), 999).unwrap();
        let loaded = store.load(&BrainstormId::new("b-closed")).unwrap();
        // updated_at_ms unchanged — we didn't re-save.
        assert_eq!(loaded.updated_at_ms, 100);
    }
}
