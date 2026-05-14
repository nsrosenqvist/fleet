//! Repo-root resolution for the v2 codepaths.
//!
//! The legacy [`crate::repo_root`] in `main.rs` looks for the very specific
//! `Cargo.toml + agent-orchestrator.yaml` combination, which only matches the
//! fleet repo itself. That worked when fleet was a wrapper *around* AO living
//! in this checkout; it cannot work for the distributable binary which has
//! to run in arbitrary user repos.
//!
//! [`fleet_root`] replaces it for v2 callers: walk ancestors of `cwd` for a
//! `.fleet/` directory first (the user already initialised this repo for
//! fleet), and fall back to the nearest `.git/` checkout root (any git repo
//! is a fine default workspace boundary). Anything else returns `cwd` itself
//! so the rest of the binary always gets *some* path — failures surface at
//! the caller's "is this initialised?" check, not here.

use std::path::{Path, PathBuf};

/// Walk ancestors of `cwd` to find the repo's fleet root. Preference order:
/// nearest ancestor containing `.fleet/` > nearest ancestor containing
/// `.git/` > `cwd` itself. Caller decides whether the result is *initialised*
/// (i.e. has `.fleet/`) — this function only locates the boundary.
#[must_use]
pub fn fleet_root(cwd: &Path) -> PathBuf {
    if let Some(root) = find_ancestor_with(cwd, ".fleet") {
        return root;
    }
    if let Some(root) = find_ancestor_with(cwd, ".git") {
        return root;
    }
    cwd.to_path_buf()
}

/// Whether `root` looks initialised for fleet (i.e. has a `.fleet/`
/// directory). Distinct from `fleet_root` so callers can phrase
/// "did the user run `fleet init` here?" without a second filesystem
/// walk.
#[must_use]
pub fn is_initialised(root: &Path) -> bool {
    root.join(".fleet").is_dir()
}

fn find_ancestor_with(cwd: &Path, marker: &str) -> Option<PathBuf> {
    // `Path::is_dir`/`Path::exists` are used over `metadata()` so a broken
    // symlink ancestor returns `false` and the walk continues — never panics.
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join(marker);
        if candidate.exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn fleet_root_returns_cwd_when_no_markers_present() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        assert_eq!(fleet_root(&cwd), cwd);
    }

    #[test]
    fn fleet_root_prefers_dot_fleet_over_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("inner/.fleet")).unwrap();

        let cwd = root.join("inner/deeper");
        fs::create_dir_all(&cwd).unwrap();
        assert_eq!(fleet_root(&cwd), root.join("inner"));
    }

    #[test]
    fn fleet_root_falls_back_to_dot_git_when_no_dot_fleet() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        let cwd = root.join("inner/deep");
        fs::create_dir_all(&cwd).unwrap();
        assert_eq!(fleet_root(&cwd), root);
    }

    #[test]
    fn fleet_root_walks_outwards_to_find_nearest_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".fleet")).unwrap();
        let cwd = root.join("a/b/c/d");
        fs::create_dir_all(&cwd).unwrap();
        assert_eq!(fleet_root(&cwd), root);
    }

    #[test]
    fn fleet_root_finds_marker_at_cwd_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".fleet")).unwrap();
        assert_eq!(fleet_root(root), root);
    }

    #[test]
    fn is_initialised_true_only_when_dot_fleet_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!is_initialised(root));
        fs::create_dir_all(root.join(".fleet")).unwrap();
        assert!(is_initialised(root));
    }

    #[test]
    fn is_initialised_false_when_dot_fleet_is_a_file_not_a_dir() {
        // Edge case: someone created `.fleet` as a regular file. We treat
        // that as "not initialised" so `fleet init` can give a sensible
        // error instead of silently overwriting the file.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".fleet"), "oops").unwrap();
        assert!(!is_initialised(dir.path()));
    }

    #[test]
    fn fleet_root_picks_nearest_dot_fleet_when_nested() {
        // Outer .fleet/ + inner .fleet/. The walk from a deep cwd must
        // return the *closest* ancestor, not the highest one.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path();
        let inner = outer.join("inner");
        fs::create_dir_all(outer.join(".fleet")).unwrap();
        fs::create_dir_all(inner.join(".fleet")).unwrap();
        let cwd = inner.join("deeper");
        fs::create_dir_all(&cwd).unwrap();
        assert_eq!(fleet_root(&cwd), inner);
    }
}
