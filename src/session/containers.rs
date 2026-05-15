//! On-disk active-container tracking for crash forensics.
//!
//! The workflow executor starts ephemeral containers per agent node
//! and stops them when the node finishes. If fleet dies mid-node, the
//! container is leaked — the engine doesn't garbage-collect rootless
//! containers automatically, so it'll keep occupying memory and disk
//! until the user kills it. Without persistence, the reaper that runs
//! at fleet startup has no way to know which container ids belonged
//! to which crashed session.
//!
//! This module solves that with one zero-byte marker file per active
//! container under `<session_dir>/.containers/<container_id>`. The
//! executor:
//!   - calls [`mark_active`] immediately after `start_container`
//!   - calls [`mark_stopped`] immediately after `stop`
//!
//! The reaper later calls [`list_active`] to discover any markers
//! that survived a fleet crash. Today the leaked ids are surfaced
//! through `crash.json`; a future enhancement will stop them
//! automatically (needs adapter plumbing through the CLI reap path).
//!
//! Atomicity comes for free: every marker is a separate file, and
//! `create_dir_all` + `File::create` are safe even from parallel
//! fanout siblings that each touch their own filename. No locking
//! needed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Subdirectory holding the marker files. Begins with `.` so
/// `SessionStore::list` (which already skips dot-prefixed entries)
/// continues to behave correctly when we add this sibling.
pub const CONTAINERS_DIR: &str = ".containers";

/// Path to the active-container marker for `container_id` under
/// `session_dir`. Pure — does not check existence.
#[must_use]
pub fn marker_path(session_dir: &Path, container_id: &str) -> PathBuf {
    session_dir.join(CONTAINERS_DIR).join(container_id)
}

/// Mark a container as active for this session. Idempotent — if the
/// marker already exists (e.g. an executor retried `start_container`
/// with the same id), the second call is a no-op. Failures are
/// surfaced to the caller because losing this signal silently means
/// the reaper later misses a leaked container.
pub fn mark_active(session_dir: &Path, container_id: &str) -> Result<()> {
    let dir = session_dir.join(CONTAINERS_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = marker_path(session_dir, container_id);
    // Empty file; the *name* is the payload.
    std::fs::write(&path, []).with_context(|| format!("writing marker {}", path.display()))?;
    Ok(())
}

/// Mark a container as no-longer-active. Tolerates missing marker —
/// stop is called from cleanup paths that may execute even when
/// start failed, so "marker not there" is success.
pub fn mark_stopped(session_dir: &Path, container_id: &str) -> Result<()> {
    let path = marker_path(session_dir, container_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(anyhow::Error::from(err).context(format!("removing marker {}", path.display())))
        }
    }
}

/// Enumerate active-container ids for this session. Returns an empty
/// vec when the dir is missing — a session that never ran an agent
/// node has nothing to report. The result is sorted so crash.json
/// reads deterministically.
pub fn list_active(session_dir: &Path) -> Result<Vec<String>> {
    let dir = session_dir.join(CONTAINERS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(anyhow::Error::from(err).context(format!("reading {}", dir.display())));
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("scanning {}", dir.display()))?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Drop every marker. Called by the reaper after the leaked ids have
/// been folded into `crash.json` — without this, a second sweep
/// would re-report the same orphans forever. Best-effort: a remove
/// failure for one file does not stop the others.
pub fn clear_all(session_dir: &Path) -> Result<()> {
    let dir = session_dir.join(CONTAINERS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(anyhow::Error::from(err).context(format!("reading {}", dir.display())));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("scanning {}", dir.display()))?;
        if entry.file_type()?.is_file() {
            if let Err(err) = std::fs::remove_file(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %err,
                    "clearing container marker failed"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn marker_path_lands_in_containers_subdir() {
        let p = marker_path(Path::new("/a/b"), "c-1");
        assert_eq!(p, PathBuf::from("/a/b/.containers/c-1"));
    }

    #[test]
    fn mark_active_creates_dir_and_file() {
        let dir = fixture();
        mark_active(dir.path(), "c-42").unwrap();
        assert!(dir.path().join(".containers/c-42").is_file());
    }

    #[test]
    fn mark_active_is_idempotent() {
        let dir = fixture();
        mark_active(dir.path(), "c-42").unwrap();
        mark_active(dir.path(), "c-42").unwrap();
        assert!(dir.path().join(".containers/c-42").is_file());
    }

    #[test]
    fn mark_stopped_removes_marker() {
        let dir = fixture();
        mark_active(dir.path(), "c-42").unwrap();
        mark_stopped(dir.path(), "c-42").unwrap();
        assert!(!dir.path().join(".containers/c-42").exists());
    }

    #[test]
    fn mark_stopped_tolerates_missing_marker() {
        // Cleanup paths may call mark_stopped even when start_container
        // failed. "Not there" must be success.
        let dir = fixture();
        mark_stopped(dir.path(), "never-existed").unwrap();
    }

    #[test]
    fn list_active_returns_empty_when_dir_absent() {
        let dir = fixture();
        assert!(list_active(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn list_active_returns_sorted_marker_names() {
        let dir = fixture();
        mark_active(dir.path(), "c-3").unwrap();
        mark_active(dir.path(), "c-1").unwrap();
        mark_active(dir.path(), "c-2").unwrap();
        assert_eq!(list_active(dir.path()).unwrap(), vec!["c-1", "c-2", "c-3"]);
    }

    #[test]
    fn list_active_omits_directories() {
        // Future-proof against sub-dirs landing in `.containers/`
        // (e.g. per-engine subdirs). Only file markers count as
        // active.
        let dir = fixture();
        mark_active(dir.path(), "c-real").unwrap();
        std::fs::create_dir_all(dir.path().join(".containers/some-subdir")).unwrap();
        assert_eq!(list_active(dir.path()).unwrap(), vec!["c-real"]);
    }

    #[test]
    fn clear_all_drops_every_marker() {
        let dir = fixture();
        mark_active(dir.path(), "c-1").unwrap();
        mark_active(dir.path(), "c-2").unwrap();
        clear_all(dir.path()).unwrap();
        assert!(list_active(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn clear_all_tolerates_missing_dir() {
        let dir = fixture();
        clear_all(dir.path()).unwrap();
    }
}
