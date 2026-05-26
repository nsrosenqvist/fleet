//! Persistence for sessions: one `meta.json` per session, plus the empty
//! `artifacts/` and `logs/` siblings the workflow engine will fill in.
//!
//! The store is intentionally dumb. It does not validate the state
//! machine (that's the value object's job), does not know about
//! workflows (that's the engine's job), and does not implement file
//! locking (single-writer-per-repo is a fleet invariant). What it *does*
//! own is the on-disk layout — if `meta.json` ever moves or grows
//! sidecar files, this is the only module that needs to change.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

use super::{Session, SessionId};

/// Persistence adapter for sessions, rooted at `<fleet_root>/.fleet/sessions/`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Open a store rooted at `<fleet_root>/.fleet/sessions/`. The directory
    /// itself does not need to exist yet; [`Self::create`] will materialise
    /// it on first use. Pass the directory containing the per-session
    /// subdirectories, *not* the fleet repo root — making the boundary
    /// explicit keeps tests trivial and avoids hard-coding `.fleet/sessions`
    /// at every call site.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Convenience: open a store anchored at `<fleet_root>/.fleet/sessions/`.
    /// Same shape as [`Self::at`], but takes the fleet root and applies the
    /// canonical relative path so callers don't repeat the literal.
    #[must_use]
    pub fn for_repo(fleet_root: impl AsRef<Path>) -> Self {
        Self::at(fleet_root.as_ref().join(".fleet").join("sessions"))
    }

    /// Directory where this store keeps its per-session subdirectories.
    /// Exposed for the TUI's "sessions root" diagnostic display.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the per-session directory for `id`. Does not check that the
    /// directory exists — callers use this for diagnostic messages too.
    #[must_use]
    pub fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// Path to a session's `meta.json` (also useful for diagnostics).
    #[must_use]
    pub fn meta_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    /// Materialise a brand-new session on disk: create the per-session
    /// directory, the empty `artifacts/` and `logs/` subdirs, and write
    /// `meta.json`. Errors if a `meta.json` already exists for the id
    /// — the caller decides whether to mint a new id or surface the
    /// collision.
    ///
    /// The per-session *directory* may pre-exist (e.g. the workflow CLI
    /// created it to host a `git worktree` before calling into the
    /// executor). The collision check is the meta.json specifically;
    /// the subdirs are created idempotently.
    pub fn create(&self, session: &Session) -> Result<()> {
        let dir = self.session_dir(&session.id);
        let meta = self.meta_path(&session.id);
        if meta.exists() {
            bail!(
                "session {} already exists on disk at {}",
                session.id,
                meta.display()
            );
        }
        std::fs::create_dir_all(dir.join("artifacts"))
            .with_context(|| format!("creating artifacts dir at {}", dir.display()))?;
        std::fs::create_dir_all(dir.join("logs"))
            .with_context(|| format!("creating logs dir at {}", dir.display()))?;
        self.write_meta(session)
    }

    /// Overwrite the on-disk `meta.json` for a session that already exists.
    /// Errors if the session has not been [`Self::create`]d yet — callers
    /// shouldn't be saving over a directory they didn't materialise.
    pub fn save(&self, session: &Session) -> Result<()> {
        let dir = self.session_dir(&session.id);
        if !dir.is_dir() {
            bail!(
                "session {} has no on-disk directory at {} (call create first)",
                session.id,
                dir.display()
            );
        }
        self.write_meta(session)
    }

    /// Load a session by id. Returns an error if the directory or
    /// `meta.json` is missing, or if the JSON is malformed.
    pub fn load(&self, id: &SessionId) -> Result<Session> {
        let path = self.meta_path(id);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let session: Session =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if session.id != *id {
            bail!(
                "session id mismatch: directory {} contains meta.json with id {}",
                id,
                session.id
            );
        }
        Ok(session)
    }

    /// Remove a session's directory entirely (meta.json, logs/, artifacts/,
    /// transcript.log, `.containers/`, …). Idempotent: a missing directory
    /// is treated as already-deleted, not an error. Callers are expected
    /// to have already detached any git worktree / container that used to
    /// live under this id — `delete` does not touch git or the runtime.
    pub fn delete(&self, id: &SessionId) -> Result<()> {
        let dir = self.session_dir(id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(anyhow!(err).context(format!("removing {}", dir.display()))),
        }
    }

    /// List session ids in directory order. Directory order is unsorted —
    /// callers that want chronological order should sort by id (the
    /// `ClockIdSource` mint format is millisecond-prefixed and lexicographic
    /// ≈ chronological) or load each session and sort by `created_at_ms`.
    pub fn list(&self) -> Result<Vec<SessionId>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            // No sessions root yet (fresh repo) = empty list, not an error.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(anyhow!(err).context(format!("reading {}", self.root.display())));
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("scanning {}", self.root.display()))?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                // Non-UTF-8 directory names exist on Unix but fleet's id
                // format is ASCII; skip such entries rather than failing
                // the whole `list`.
                continue;
            };
            // Skip dotfiles/hidden entries — keeps the layout future-proof
            // (e.g. `.lock` if we later add one) without churning callers.
            if name.starts_with('.') {
                continue;
            }
            ids.push(SessionId::new(name));
        }
        Ok(ids)
    }

    fn write_meta(&self, session: &Session) -> Result<()> {
        let path = self.meta_path(&session.id);
        let json = serde_json::to_string_pretty(session)
            .with_context(|| format!("serialising session {}", session.id))?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Sum every session's per-node USD costs into a single lifetime
    /// figure. Unreadable sessions (bad meta.json) are skipped silently
    /// — surfaces in `fleet sessions list` separately and shouldn't
    /// trip up budget arithmetic. Returns `0.0` for a fresh repo with
    /// no sessions.
    pub fn sum_costs(&self) -> Result<f64> {
        let mut total = 0.0;
        for id in self.list()? {
            if let Ok(s) = self.load(&id) {
                if let Some(c) = s.total_cost_usd() {
                    total += c;
                }
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionState, now_ms};

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().to_path_buf());
        (dir, store)
    }

    fn session(id: &str) -> Session {
        Session::new(SessionId::new(id), "standard", 1_000)
    }

    #[test]
    fn for_repo_anchors_at_dot_fleet_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::for_repo(dir.path());
        assert_eq!(s.root(), dir.path().join(".fleet").join("sessions"));
    }

    #[test]
    fn create_materialises_directory_and_meta() {
        let (_dir, s) = store();
        let session = session("s-1");
        s.create(&session).unwrap();
        let sdir = s.session_dir(&session.id);
        assert!(sdir.is_dir());
        assert!(sdir.join("artifacts").is_dir());
        assert!(sdir.join("logs").is_dir());
        assert!(sdir.join("meta.json").is_file());
    }

    #[test]
    fn create_refuses_to_clobber_existing_meta() {
        let (_dir, s) = store();
        let session = session("s-1");
        s.create(&session).unwrap();
        let err = s.create(&session).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn create_tolerates_pre_existing_session_dir() {
        // Workflow CLI may have created the per-session dir already
        // (e.g. to host a `git worktree` before the executor mints
        // meta.json). create() must accept the existing dir and
        // proceed to write artifacts/, logs/, and meta.json.
        let (_dir, s) = store();
        let session = session("s-1");
        let sdir = s.session_dir(&session.id);
        std::fs::create_dir_all(&sdir).unwrap();
        // Stash something into the dir to prove create() doesn't
        // clobber pre-existing contents (e.g. a worktree).
        std::fs::write(sdir.join("placeholder"), b"hi").unwrap();
        s.create(&session).unwrap();
        assert!(sdir.join("artifacts").is_dir());
        assert!(sdir.join("logs").is_dir());
        assert!(sdir.join("meta.json").is_file());
        assert!(sdir.join("placeholder").is_file());
    }

    #[test]
    fn delete_removes_session_dir_and_is_idempotent() {
        // The TUI's Shift+X and `fleet sessions forget` both end up
        // here. Two-call test pins idempotency so a retry after a
        // partial failure won't surface a "file not found" error.
        let (_dir, s) = store();
        let session = session("s-gone");
        s.create(&session).unwrap();
        assert!(s.session_dir(&session.id).is_dir());
        s.delete(&session.id).unwrap();
        assert!(!s.session_dir(&session.id).exists());
        s.delete(&session.id).unwrap();
    }

    #[test]
    fn load_round_trips_session() {
        let (_dir, s) = store();
        let mut session = session("s-1");
        session.transition_to(SessionState::Running, 2_000).unwrap();
        session.set_current_node(Some("plan".to_string()), 2_500);
        s.create(&session).unwrap();
        let back = s.load(&session.id).unwrap();
        assert_eq!(back, session);
    }

    #[test]
    fn load_errors_for_unknown_session() {
        let (_dir, s) = store();
        let err = s.load(&SessionId::new("nope")).unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
    }

    #[test]
    fn load_detects_id_mismatch_in_meta() {
        let (_dir, s) = store();
        let session = session("s-real");
        s.create(&session).unwrap();
        // Move the session directory to a different name; the meta.json
        // inside still says "s-real" so load(under-new-name) should
        // detect the mismatch.
        let renamed = SessionId::new("s-renamed");
        std::fs::rename(s.session_dir(&session.id), s.session_dir(&renamed)).unwrap();
        let err = s.load(&renamed).unwrap_err();
        assert!(format!("{err}").contains("session id mismatch"));
    }

    #[test]
    fn save_overwrites_existing_meta() {
        let (_dir, s) = store();
        let mut session = session("s-1");
        s.create(&session).unwrap();
        session.transition_to(SessionState::Running, 2_000).unwrap();
        s.save(&session).unwrap();
        assert_eq!(s.load(&session.id).unwrap().state, SessionState::Running);
    }

    #[test]
    fn save_refuses_when_no_directory_was_created() {
        let (_dir, s) = store();
        let session = session("s-orphan");
        let err = s.save(&session).unwrap_err();
        assert!(format!("{err}").contains("no on-disk directory"));
    }

    #[test]
    fn list_is_empty_when_root_missing() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::at(dir.path().join("does-not-exist"));
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn list_returns_session_ids_for_existing_subdirs() {
        let (_dir, s) = store();
        s.create(&session("s-a")).unwrap();
        s.create(&session("s-b")).unwrap();
        let mut ids: Vec<_> = s.list().unwrap().into_iter().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(ids, vec![SessionId::new("s-a"), SessionId::new("s-b")]);
    }

    #[test]
    fn list_skips_dotfiles_and_regular_files() {
        let (_dir, s) = store();
        std::fs::create_dir_all(s.root()).unwrap();
        std::fs::create_dir_all(s.root().join(".hidden-dir")).unwrap();
        std::fs::write(s.root().join("not-a-session.txt"), b"x").unwrap();
        s.create(&session("s-real")).unwrap();
        let ids = s.list().unwrap();
        assert_eq!(ids, vec![SessionId::new("s-real")]);
    }

    #[test]
    fn meta_path_is_under_session_dir() {
        let (_dir, s) = store();
        let id = SessionId::new("s-x");
        assert_eq!(s.meta_path(&id), s.session_dir(&id).join("meta.json"));
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionStore>();
    }

    #[test]
    fn now_ms_returns_nonzero_in_production() {
        // Sanity check the wallclock helper used by the production path.
        // Not deterministic; just check it isn't always 0.
        assert!(now_ms() > 0);
    }

    #[test]
    fn sum_costs_returns_zero_for_empty_store() {
        let (_dir, s) = store();
        assert!((s.sum_costs().unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn sum_costs_aggregates_across_sessions_skipping_cost_less_ones() {
        let (_dir, s) = store();
        // Session with cost.
        let mut a = session("s-a");
        a.record_node_cost("plan", 0.10, 2);
        a.record_node_cost("review", 0.32, 3);
        s.create(&a).unwrap();
        // Session with cost.
        let mut b = session("s-b");
        b.record_node_cost("only", 0.05, 4);
        s.create(&b).unwrap();
        // Session with no costs recorded — should contribute 0 to
        // the lifetime sum (total_cost_usd() returns None, sum_costs
        // skips it).
        let c = session("s-c");
        s.create(&c).unwrap();
        let total = s.sum_costs().unwrap();
        // 0.10 + 0.32 + 0.05 = 0.47
        assert!((total - 0.47).abs() < 1e-9, "got {total}");
    }
}
