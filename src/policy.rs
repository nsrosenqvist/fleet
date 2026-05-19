//! Branch-protection policy primitives.
//!
//! Single source of truth for "which refs are protected against direct
//! push." Consumed by:
//! - the `fleet-git` shim (via env vars injected at agent spawn)
//! - the worktree-creation assertion
//! - (future) the `CreatePr` `NodeKind` handler once it lands
//!
//! Detection is `git symbolic-ref refs/remotes/origin/HEAD`; the
//! fallback when detection fails is `["main", "master"]` plus a
//! tracing warn. The shim fails safe — an over-broad protected set
//! costs a denied push, an under-broad set costs an unintended push.

use crate::process::ProcessInvoker;
use crate::repo_config::GitGuardConfig;
use std::path::Path;

/// Resolve the repo's default branch by reading the remote's symbolic
/// ref. `None` if git refuses for any reason — callers fall back to
/// the documented defaults. Stubbed via `MockProcessInvoker` in tests.
#[must_use]
pub fn detect_default_branch(invoker: &dyn ProcessInvoker, repo_root: &Path) -> Option<String> {
    let out = invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "symbolic-ref".to_string(),
                "--short".to_string(),
                "refs/remotes/origin/HEAD".to_string(),
            ],
        )
        .ok()?;
    let s = out.trim();
    s.strip_prefix("origin/").map(str::to_string).or_else(|| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}

/// Effective protected-branch set: user-configured names ∪ the
/// auto-detected default branch (or `["main", "master"]` when
/// detection fails).
///
/// Result is order-stable (user entries first, then detected), de-
/// duplicated, never empty unless the user empties it *and* the
/// detection somehow returns an empty result (which git can't produce).
#[must_use]
pub fn protected_branches(
    invoker: &dyn ProcessInvoker,
    repo_root: &Path,
    cfg: &GitGuardConfig,
) -> Vec<String> {
    let mut out: Vec<String> = cfg.protected_branches.clone();
    let push_unique = |name: String, out: &mut Vec<String>| {
        if !out.contains(&name) {
            out.push(name);
        }
    };
    if let Some(b) = detect_default_branch(invoker, repo_root) {
        push_unique(b, &mut out);
    } else {
        tracing::warn!(
            "could not resolve `refs/remotes/origin/HEAD`; falling back to [main, master] for push protection"
        );
        push_unique("main".to_string(), &mut out);
        push_unique("master".to_string(), &mut out);
    }
    out
}

/// Exact case-sensitive match. Globs are intentionally not supported —
/// the protected set is small and explicit. Pattern matching belongs
/// in the allowlist surface, not the protection check.
///
/// Allow-listed dead code: consumed by the parallel `CreatePr` handler
/// work in `src/workflow/executor.rs` once that lands.
#[must_use]
#[allow(dead_code)]
pub fn is_protected(name: &str, protected: &[String]) -> bool {
    protected.iter().any(|p| p == name)
}

/// Newline-joined encoding for env-var transport to the shim. Empty
/// lines decode to nothing, so trailing newlines are harmless.
#[must_use]
pub fn encode_for_env(items: &[String]) -> String {
    items.join("\n")
}

/// Counterpart to [`encode_for_env`]. The shim duplicates this
/// six-line decoder inline (it does not depend on the fleet library
/// crate); the function here is the policy module's canonical
/// reference and the half tested round-trip with `encode_for_env`.
#[must_use]
#[allow(dead_code)]
pub fn decode_from_env(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use anyhow::anyhow;

    fn cfg_with(protected: &[&str]) -> GitGuardConfig {
        GitGuardConfig {
            enabled: true,
            protected_branches: protected.iter().map(|s| (*s).to_string()).collect(),
            allow_push_to: vec![],
        }
    }

    #[test]
    fn detect_default_branch_strips_origin_prefix() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Ok("origin/main".to_string()));
        assert_eq!(
            detect_default_branch(&inv, Path::new("/repo")).as_deref(),
            Some("main")
        );
    }

    #[test]
    fn detect_default_branch_returns_none_on_error() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Err(anyhow!("no symbolic ref")));
        assert!(detect_default_branch(&inv, Path::new("/repo")).is_none());
    }

    #[test]
    fn protected_branches_unions_detected_with_configured() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Ok("origin/develop".to_string()));
        let cfg = cfg_with(&["release"]);
        let v = protected_branches(&inv, Path::new("/repo"), &cfg);
        assert!(v.contains(&"release".to_string()));
        assert!(v.contains(&"develop".to_string()));
    }

    #[test]
    fn protected_branches_falls_back_to_main_and_master_when_detect_fails() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run().returning(|_, _| Err(anyhow!("no remote")));
        let v = protected_branches(&inv, Path::new("/repo"), &GitGuardConfig::default());
        assert!(v.contains(&"main".to_string()));
        assert!(v.contains(&"master".to_string()));
    }

    #[test]
    fn protected_branches_dedupes_when_user_lists_detected_branch() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Ok("origin/main".to_string()));
        let cfg = cfg_with(&["main"]);
        let v = protected_branches(&inv, Path::new("/repo"), &cfg);
        assert_eq!(v.iter().filter(|s| s.as_str() == "main").count(), 1);
    }

    #[test]
    fn is_protected_exact_match_no_globs() {
        let protected = vec!["main".to_string(), "release/*".to_string()];
        assert!(is_protected("main", &protected));
        // Case-sensitive: git refs are case-sensitive in practice.
        assert!(!is_protected("MAIN", &protected));
        // No glob expansion — `release/*` only matches literal `release/*`.
        assert!(!is_protected("release/1.0", &protected));
        assert!(is_protected("release/*", &protected));
    }

    #[test]
    fn env_round_trip_preserves_items_and_drops_blanks() {
        let items = vec![
            "main".to_string(),
            "master".to_string(),
            "release".to_string(),
        ];
        let encoded = encode_for_env(&items);
        assert_eq!(decode_from_env(&encoded), items);

        // Trailing newline / blank lines drop out.
        assert_eq!(
            decode_from_env("main\nmaster\n\n"),
            vec!["main".to_string(), "master".to_string()]
        );
        assert_eq!(decode_from_env(""), Vec::<String>::new());
    }
}
