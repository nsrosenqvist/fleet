//! Persistence for the orchestrator session at
//! `<fleet_root>/.fleet/orchestrator/`. Flat layout (no per-id
//! subdirectory) — there is exactly one orchestrator per repo.
//!
//! Shape mirrors [`crate::session::store::SessionStore`] and
//! [`crate::plans::store::PlanStore`]: store knows the disk
//! layout, value-object owns its own validation, atomic-rename
//! writes for crash safety.
//!
//! `meta.json` rather than `meta.yaml` here because tmux PIDs +
//! transient state mutate often and JSON's faster to parse for
//! the TUI's per-refresh loads. (Plans rarely mutate; YAML's
//! human-friendliness pays off there.)

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::OrchestratorSession;

/// Persistence adapter for `.fleet/orchestrator/`. The single
/// orchestrator session lives directly in this dir — no nested
/// per-id subdirectory, since there is only one.
#[derive(Debug, Clone)]
pub struct OrchestratorStore {
    root: PathBuf,
}

impl OrchestratorStore {
    /// Open a store rooted at a specific directory. Tests use this
    /// with a tempdir; production goes through [`Self::for_repo`].
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Open a store anchored at `<fleet_root>/.fleet/orchestrator/`.
    #[must_use]
    pub fn for_repo(fleet_root: impl AsRef<Path>) -> Self {
        Self::at(fleet_root.as_ref().join(".fleet").join("orchestrator"))
    }

    /// Root directory. Exposed for diagnostics and tests.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the meta.json file.
    #[must_use]
    pub fn meta_path(&self) -> PathBuf {
        self.root.join("meta.json")
    }

    /// Path of the captured transcript file. Populated by tmux
    /// `pipe-pane` after `fleet orchestrator` spawns the session —
    /// see [`super::tmux::pipe_pane_to`].
    #[must_use]
    pub fn transcript_path(&self) -> PathBuf {
        self.root.join("transcript.log")
    }

    /// Path of the system-prompt file. Written at session start
    /// from the rendered template in `super::prompt`.
    #[must_use]
    pub fn prompt_path(&self) -> PathBuf {
        self.root.join("prompt.md")
    }

    /// Whether the orchestrator has any persisted state. False on a
    /// brand-new repo; True after the first `fleet orchestrator`
    /// spawn.
    pub fn exists(&self) -> bool {
        self.meta_path().exists()
    }

    /// Atomically write the orchestrator meta. Materialises the
    /// orchestrator dir on first save.
    pub fn save(&self, session: &OrchestratorSession) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating orchestrator dir at {}", self.root.display()))?;
        let path = self.meta_path();
        let body = serde_json::to_string_pretty(session)
            .with_context(|| "serialising orchestrator session to JSON")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing tmp meta at {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Load the orchestrator meta. Errors with path context if the
    /// file is missing — callers should check [`Self::exists`]
    /// first when "no orchestrator yet" is a normal branch.
    pub fn load(&self) -> Result<OrchestratorSession> {
        let path = self.meta_path();
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading orchestrator meta at {}", path.display()))?;
        let session: OrchestratorSession = serde_json::from_str(&body)
            .with_context(|| format!("parsing orchestrator meta at {}", path.display()))?;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{OrchestratorSession, OrchestratorState};

    fn sample() -> OrchestratorSession {
        OrchestratorSession::new("claude-code", 1_700_000_000_000)
    }

    #[test]
    fn for_repo_anchors_at_fleet_orchestrator() {
        let store = OrchestratorStore::for_repo("/repo");
        assert_eq!(store.root(), Path::new("/repo/.fleet/orchestrator"));
    }

    #[test]
    fn paths_live_flat_under_the_root() {
        // No per-id subdir — the single orchestrator gets its files
        // directly in `.fleet/orchestrator/`.
        let store = OrchestratorStore::for_repo("/repo");
        assert_eq!(store.meta_path(), Path::new("/repo/.fleet/orchestrator/meta.json"));
        assert_eq!(
            store.transcript_path(),
            Path::new("/repo/.fleet/orchestrator/transcript.log")
        );
        assert_eq!(
            store.prompt_path(),
            Path::new("/repo/.fleet/orchestrator/prompt.md")
        );
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path());
        let mut s = sample();
        s.state = OrchestratorState::Detached;
        s.pid = Some(12345);
        store.save(&s).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn save_overwrites_existing_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path());
        let mut s = sample();
        store.save(&s).unwrap();
        s.state = OrchestratorState::Closed;
        s.updated_at_ms = 1_700_000_001_000;
        store.save(&s).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.state, OrchestratorState::Closed);
        assert_eq!(loaded.updated_at_ms, 1_700_000_001_000);
    }

    #[test]
    fn exists_returns_false_before_first_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path().join("never-created"));
        assert!(!store.exists());
    }

    #[test]
    fn exists_returns_true_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path());
        store.save(&sample()).unwrap();
        assert!(store.exists());
    }

    #[test]
    fn load_errors_with_path_context_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::at(dir.path());
        let err = store.load().unwrap_err();
        assert!(format!("{err:#}").contains("meta.json"));
    }
}
