//! Materialize fleet's worker-rules AGENTS.md into the XDG config dir so
//! `agent-orchestrator.yaml` entries can reference it via a single
//! absolute path that works regardless of which repo fleet was launched
//! from.
//!
//! AO resolves `agentRulesFile` relative to each project's `path`, which
//! means a relative `templates/AGENTS.md` only works for the fleet repo
//! itself. Once fleet manages more than one project, the file has to
//! live somewhere project-agnostic. `~/.config/fleet/templates/AGENTS.md`
//! is that place — same directory as the AO yaml, already bind-mounted
//! into the Lima VM, already part of fleet's XDG footprint.
//!
//! The canonical content lives in `templates/AGENTS.md` and is baked
//! into the binary via `include_str!`. On startup we write it to the
//! XDG location only when the on-disk bytes differ, so user edits to a
//! stale copy are overwritten on the next fleet build (intentional —
//! the binary is the source of truth) and the no-op case touches no
//! disk state.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Worker-rules markdown baked into the binary at compile time.
/// Source of truth is `templates/AGENTS.md` at the repo root.
const WORKER_AGENTS_MD: &str = include_str!("../templates/AGENTS.md");

/// Filename under the XDG `templates/` subdir.
const AGENTS_MD_FILENAME: &str = "AGENTS.md";

/// Resolve the canonical path where the worker AGENTS.md should live.
/// Returns `None` when the XDG dir is unresolvable (`$HOME` and
/// `$XDG_CONFIG_HOME` both unset) — callers treat that as "skip
/// materialisation" rather than an error, matching the loose
/// preflight behaviour around the AO yaml.
pub fn worker_agents_md_path() -> Option<PathBuf> {
    crate::ao::config::AoConfig::workdir().map(|d| d.join("templates").join(AGENTS_MD_FILENAME))
}

/// Idempotently write the embedded worker AGENTS.md to the XDG path.
/// Returns the path when the file is in sync (whether we wrote it or
/// not). A divergent on-disk copy is overwritten without prompting:
/// the binary is the source of truth, and fleet rebuilds are the
/// supported way to update the worker rules.
pub fn ensure_worker_agents_md() -> Result<Option<PathBuf>> {
    let Some(path) = worker_agents_md_path() else {
        return Ok(None);
    };
    write_if_changed(&path, WORKER_AGENTS_MD)?;
    Ok(Some(path))
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == content
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_content_is_non_empty() {
        // Guards against the file being accidentally truncated to zero
        // bytes — `include_str!` would happily embed an empty file.
        assert!(WORKER_AGENTS_MD.len() > 100);
        assert!(WORKER_AGENTS_MD.contains("ao report"));
    }

    #[test]
    fn write_if_changed_creates_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("nested").join("deeper").join("AGENTS.md");
        write_if_changed(&target, "hello").expect("write");
        let read_back = std::fs::read_to_string(&target).expect("read");
        assert_eq!(read_back, "hello");
    }

    #[test]
    fn write_if_changed_is_noop_when_content_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("AGENTS.md");
        std::fs::write(&target, "hello").expect("seed");
        let mtime_before = std::fs::metadata(&target)
            .expect("stat")
            .modified()
            .unwrap();
        // Sleep a hair so an mtime bump would be observable, then
        // re-run with the same content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_if_changed(&target, "hello").expect("write");
        let mtime_after = std::fs::metadata(&target)
            .expect("stat")
            .modified()
            .unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "no-op write must not touch mtime"
        );
    }

    #[test]
    fn write_if_changed_overwrites_divergent_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("AGENTS.md");
        std::fs::write(&target, "stale").expect("seed");
        write_if_changed(&target, "fresh").expect("write");
        let read_back = std::fs::read_to_string(&target).expect("read");
        assert_eq!(read_back, "fresh");
    }
}
