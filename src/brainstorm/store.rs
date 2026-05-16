//! Persistence for brainstorm sessions: one
//! `<fleet_root>/.fleet/planning/<id>/` directory per session,
//! holding `meta.json` plus `transcript.log` and `prompt.md` files
//! that the tmux integration / tool server populate later.
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

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use super::BrainstormId;
use super::BrainstormSession;

/// Persistence adapter for `.fleet/planning/<id>/`. Each session
/// owns its own directory; `meta.json` carries the value object,
/// `transcript.log` and `prompt.md` are populated by the tmux
/// integration + tool server in later commits.
#[derive(Debug, Clone)]
pub struct BrainstormStore {
    root: PathBuf,
}

impl BrainstormStore {
    /// Open a store rooted at a specific directory. Tests use this
    /// with a tempdir; production goes through [`Self::for_repo`].
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Open a store anchored at `<fleet_root>/.fleet/planning/`.
    #[must_use]
    pub fn for_repo(fleet_root: impl AsRef<Path>) -> Self {
        Self::at(fleet_root.as_ref().join(".fleet").join("planning"))
    }

    /// Root directory. Exposed for diagnostics and tests.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding one session's meta + transcript + prompt
    /// files. Does not check existence — callers use this to
    /// derive paths for both reads and writes.
    #[must_use]
    pub fn session_dir(&self, id: &BrainstormId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// Path of the meta.json file for a given session id.
    #[must_use]
    pub fn meta_path(&self, id: &BrainstormId) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    /// Path of the captured transcript file for a given session id.
    /// Populated by tmux `pipe-pane` after `fleet brainstorm`
    /// spawns the session — see [`crate::brainstorm::tmux::pipe_pane_to`].
    #[must_use]
    pub fn transcript_path(&self, id: &BrainstormId) -> PathBuf {
        self.session_dir(id).join("transcript.log")
    }

    /// Path of the system-prompt file. Written at session start by
    /// the prompt template in P5-C5.
    #[must_use]
    pub fn prompt_path(&self, id: &BrainstormId) -> PathBuf {
        self.session_dir(id).join("prompt.md")
    }

    /// Materialise a brand-new session on disk: create the
    /// per-session dir + meta.json. Errors if meta.json already
    /// exists (collision on id minting; caller should mint a fresh
    /// one).
    pub fn create(&self, session: &BrainstormSession) -> Result<()> {
        let dir = self.session_dir(&session.id);
        let meta = self.meta_path(&session.id);
        if meta.exists() {
            bail!(
                "brainstorm session {} already exists on disk at {}",
                session.id,
                meta.display(),
            );
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating brainstorm dir at {}", dir.display()))?;
        self.save(session)
    }

    /// Atomically replace the on-disk meta. Materialises the
    /// session dir on first save (callers that bypassed `create`
    /// get the dir for free).
    pub fn save(&self, session: &BrainstormSession) -> Result<()> {
        let dir = self.session_dir(&session.id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating brainstorm dir at {}", dir.display()))?;
        let path = self.meta_path(&session.id);
        let body = serde_json::to_string_pretty(session)
            .with_context(|| format!("serialising brainstorm `{}` to JSON", session.id))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing tmp meta at {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Load one session by id.
    pub fn load(&self, id: &BrainstormId) -> Result<BrainstormSession> {
        let path = self.meta_path(id);
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading brainstorm meta at {}", path.display()))?;
        let session: BrainstormSession = serde_json::from_str(&body)
            .with_context(|| format!("parsing brainstorm meta at {}", path.display()))?;
        Ok(session)
    }

    /// Every brainstorm session id on disk. Empty when the
    /// `.fleet/planning/` directory hasn't been created yet
    /// (which is normal — fleet only mints it on first
    /// `fleet brainstorm`).
    pub fn list(&self) -> Result<Vec<BrainstormId>> {
        let read = match std::fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("reading {}", self.root.display()));
            }
        };
        let mut ids = Vec::new();
        for entry in read {
            let entry =
                entry.with_context(|| format!("reading entry in {}", self.root.display()))?;
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            // Skip atomic-write tmp artefacts and hidden dirs.
            if name.starts_with('.') {
                continue;
            }
            ids.push(BrainstormId::new(name));
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brainstorm::{BrainstormSession, BrainstormState};

    fn sample(id: &str) -> BrainstormSession {
        BrainstormSession::new(BrainstormId::new(id), "claude-code", 1_700_000_000_000)
    }

    #[test]
    fn for_repo_anchors_at_fleet_planning() {
        let store = BrainstormStore::for_repo("/repo");
        assert_eq!(store.root(), Path::new("/repo/.fleet/planning"));
    }

    #[test]
    fn session_dir_paths_carry_the_id() {
        let store = BrainstormStore::for_repo("/repo");
        let dir = store.session_dir(&BrainstormId::new("b-abc"));
        assert_eq!(dir, Path::new("/repo/.fleet/planning/b-abc"));
        assert_eq!(
            store.meta_path(&BrainstormId::new("b-abc")),
            Path::new("/repo/.fleet/planning/b-abc/meta.json")
        );
        assert_eq!(
            store.transcript_path(&BrainstormId::new("b-abc")),
            Path::new("/repo/.fleet/planning/b-abc/transcript.log")
        );
        assert_eq!(
            store.prompt_path(&BrainstormId::new("b-abc")),
            Path::new("/repo/.fleet/planning/b-abc/prompt.md")
        );
    }

    #[test]
    fn create_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let mut s = sample("b-1");
        s.state = BrainstormState::Detached;
        s.pid = Some(12345);
        store.create(&s).unwrap();
        let loaded = store.load(&BrainstormId::new("b-1")).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn create_errors_when_session_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let s = sample("b-existing");
        store.create(&s).unwrap();
        let err = store.create(&s).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn save_overwrites_existing_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let mut s = sample("b-1");
        store.save(&s).unwrap();
        s.state = BrainstormState::Closed;
        s.updated_at_ms = 1_700_000_001_000;
        store.save(&s).unwrap();
        let loaded = store.load(&BrainstormId::new("b-1")).unwrap();
        assert_eq!(loaded.state, BrainstormState::Closed);
        assert_eq!(loaded.updated_at_ms, 1_700_000_001_000);
    }

    #[test]
    fn list_returns_empty_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path().join("not-yet"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_returns_session_dirs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        store.save(&sample("b-2")).unwrap();
        store.save(&sample("b-1")).unwrap();
        store.save(&sample("b-3")).unwrap();
        let ids = store.list().unwrap();
        assert_eq!(
            ids.iter().map(BrainstormId::as_str).collect::<Vec<_>>(),
            vec!["b-1", "b-2", "b-3"]
        );
    }

    #[test]
    fn list_skips_hidden_and_non_dir_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("loose.txt"), "x").unwrap();
        store.save(&sample("b-1")).unwrap();
        let ids = store.list().unwrap();
        assert_eq!(ids, vec![BrainstormId::new("b-1")]);
    }

    #[test]
    fn load_errors_with_path_context_for_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let err = store.load(&BrainstormId::new("b-nope")).unwrap_err();
        assert!(format!("{err:#}").contains("b-nope/meta.json"));
    }
}
