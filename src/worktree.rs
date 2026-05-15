//! Per-session git worktree management.
//!
//! Every fleet session that runs in a git repo gets its own
//! `git worktree`: a checkout of a session-private branch into
//! `.fleet/sessions/<id>/worktree/`. The agent container's
//! `/workspace` bind-mount points at that path rather than at the
//! host's shared working tree. Two parallel sessions can't stomp on
//! each other's files; replay can reproduce the exact code state a
//! prior session ran against by basing its new worktree off the
//! prior session's branch tip.
//!
//! ## Why git worktrees rather than clones or stashes
//!
//! - `git clone --local --shared` would work but every session would
//!   need its own `.git` directory; merging back is awkward; we'd
//!   fight the worktree-vs-bare distinction. Worktrees share the
//!   same `.git` and same object database as the main repo.
//! - `git stash create` snapshots the working tree but doesn't give
//!   the agent a place to *make* changes — stash apply mutates the
//!   host's index.
//! - `git worktree add` is purpose-built for this: an additional
//!   working directory with its own HEAD, sharing the object store
//!   with the main repo. Cheap (no object copying), idiomatic, and
//!   commits made there are normal commits on the session branch.
//!
//! ## What's out of scope
//!
//! - **Non-git repos.** Callers MUST check [`is_git_repo`] first and
//!   fall back to a shared workspace; this module assumes the caller
//!   has already confirmed git is usable.
//! - **Uncommitted host changes.** `git worktree add` checks out a
//!   branch — it doesn't carry the host worktree's uncommitted
//!   modifications. That's documented at the workflow-CLI layer
//!   (the user is told to commit before running).
//! - **Untracked files in the host worktree.** Same: not carried.

use anyhow::{Context, Result};
use std::path::Path;

use crate::process::ProcessInvoker;

/// Conventional branch-name prefix for fleet-managed session branches.
/// Users can grep / clean up with `git branch -D 'fleet/session-*'`.
#[allow(dead_code)] // wired in subsequent commits
pub const SESSION_BRANCH_PREFIX: &str = "fleet/session-";

/// Build the branch name fleet uses for a session. Truncates long
/// session ids to keep `git` output readable; sessions are id-unique
/// already, so a short prefix is collision-safe in practice.
#[must_use]
#[allow(dead_code)] // wired in subsequent commits
pub fn session_branch_name(session_id: &str) -> String {
    let short: String = session_id.chars().take(12).collect();
    format!("{SESSION_BRANCH_PREFIX}{short}")
}

/// Check whether `dir` is inside a git working tree. Returns `false`
/// on any error (no git installed, dir doesn't exist, not a repo) —
/// callers treat the result as "can we use worktrees here, yes/no",
/// not as a hard error condition.
#[allow(dead_code)] // wired in subsequent commits
pub fn is_git_repo(invoker: &dyn ProcessInvoker, dir: &Path) -> bool {
    let out = invoker.run(
        "git",
        vec![
            "-C".to_string(),
            dir.to_string_lossy().into_owned(),
            "rev-parse".to_string(),
            "--is-inside-work-tree".to_string(),
        ],
    );
    matches!(out, Ok(s) if s.trim() == "true")
}

/// Return the sha of `HEAD` at `dir`. Caller-friendly error: the
/// context already names the directory.
#[allow(dead_code)] // wired in subsequent commits
pub fn head_sha(invoker: &dyn ProcessInvoker, dir: &Path) -> Result<String> {
    invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                dir.to_string_lossy().into_owned(),
                "rev-parse".to_string(),
                "HEAD".to_string(),
            ],
        )
        .with_context(|| format!("reading HEAD at {}", dir.display()))
}

/// Create a new git worktree at `target_path` checked out on a new
/// branch `branch`, based off `base` (a branch name, tag, or sha).
///
/// `repo_root` is the path *of the main repo* — `git -C <root>
/// worktree add …`. `target_path` is where the new working tree
/// lands. The directory must not exist yet (git refuses otherwise).
#[allow(dead_code)] // wired in subsequent commits
pub fn create_worktree(
    invoker: &dyn ProcessInvoker,
    repo_root: &Path,
    target_path: &Path,
    branch: &str,
    base: &str,
) -> Result<()> {
    invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "worktree".to_string(),
                "add".to_string(),
                "-b".to_string(),
                branch.to_string(),
                target_path.to_string_lossy().into_owned(),
                base.to_string(),
            ],
        )
        .with_context(|| {
            format!(
                "creating worktree at {} on branch `{branch}` from `{base}`",
                target_path.display()
            )
        })?;
    Ok(())
}

/// Remove a git worktree by path. With `force=true`, succeeds even
/// when the worktree has uncommitted changes — used by reap + prune
/// where we're cleaning up after a failed session and don't care
/// about preserving partial work.
#[allow(dead_code)] // wired in subsequent commits
pub fn remove_worktree(
    invoker: &dyn ProcessInvoker,
    repo_root: &Path,
    target_path: &Path,
    force: bool,
) -> Result<()> {
    let mut args = vec![
        "-C".to_string(),
        repo_root.to_string_lossy().into_owned(),
        "worktree".to_string(),
        "remove".to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    args.push(target_path.to_string_lossy().into_owned());
    invoker
        .run("git", args)
        .with_context(|| format!("removing worktree at {}", target_path.display()))?;
    Ok(())
}

/// Delete a local branch by name. `force=true` issues `-D` (allows
/// deleting an unmerged branch); `false` issues `-d` (refuses unless
/// merged). After [`remove_worktree`] the branch is technically still
/// around — call this to actually drop it.
#[allow(dead_code)] // wired in subsequent commits
pub fn delete_branch(
    invoker: &dyn ProcessInvoker,
    repo_root: &Path,
    branch: &str,
    force: bool,
) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "branch".to_string(),
                flag.to_string(),
                branch.to_string(),
            ],
        )
        .with_context(|| format!("deleting branch `{branch}`"))?;
    Ok(())
}

/// Prune stale worktree administrative entries — when a session's
/// `.fleet/sessions/<id>/worktree/` is deleted out-of-band (rm -rf,
/// disk wipe), git's `worktrees/` metadata still references it.
/// Idempotent. Best-effort; never errors fatally.
#[allow(dead_code)] // wired in subsequent commits
pub fn prune_worktrees(invoker: &dyn ProcessInvoker, repo_root: &Path) -> Result<()> {
    invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "worktree".to_string(),
                "prune".to_string(),
            ],
        )
        .with_context(|| format!("pruning stale worktrees at {}", repo_root.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::{always, eq};
    use std::path::PathBuf;

    #[test]
    fn session_branch_name_prefixes_and_truncates() {
        assert_eq!(
            session_branch_name("0123456789ab-cdef"),
            "fleet/session-0123456789ab"
        );
        // Short ids pass through untouched.
        assert_eq!(session_branch_name("abc"), "fleet/session-abc");
    }

    #[test]
    fn is_git_repo_true_when_git_says_true() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .withf(|prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            "/repo".to_string(),
                            "rev-parse".to_string(),
                            "--is-inside-work-tree".to_string(),
                        ]
            })
            .returning(|_, _| Ok("true".to_string()));
        assert!(is_git_repo(&inv, Path::new("/repo")));
    }

    #[test]
    fn is_git_repo_false_on_error() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("not a git repository")));
        assert!(!is_git_repo(&inv, Path::new("/not-a-repo")));
    }

    #[test]
    fn is_git_repo_false_when_git_says_false() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run().returning(|_, _| Ok("false".to_string()));
        assert!(!is_git_repo(&inv, Path::new("/somewhere")));
    }

    #[test]
    fn head_sha_returns_git_output() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "rev-parse".to_string(),
                    "HEAD".to_string(),
                ]),
            )
            .returning(|_, _| Ok("deadbeef".to_string()));
        let sha = head_sha(&inv, Path::new("/repo")).unwrap();
        assert_eq!(sha, "deadbeef");
    }

    #[test]
    fn create_worktree_issues_expected_argv() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "worktree".to_string(),
                    "add".to_string(),
                    "-b".to_string(),
                    "fleet/session-abc".to_string(),
                    "/repo/.fleet/sessions/abc/worktree".to_string(),
                    "HEAD".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        create_worktree(
            &inv,
            Path::new("/repo"),
            &PathBuf::from("/repo/.fleet/sessions/abc/worktree"),
            "fleet/session-abc",
            "HEAD",
        )
        .unwrap();
    }

    #[test]
    fn create_worktree_propagates_git_error() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("fatal: branch already exists")));
        let err = create_worktree(
            &inv,
            Path::new("/repo"),
            &PathBuf::from("/repo/.fleet/sessions/abc/worktree"),
            "fleet/session-abc",
            "HEAD",
        )
        .unwrap_err();
        // The context wrapping includes the target path so callers
        // can correlate the failure with the session.
        assert!(format!("{err:#}").contains("/repo/.fleet/sessions/abc/worktree"));
    }

    #[test]
    fn remove_worktree_force_passes_force_flag() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "worktree".to_string(),
                    "remove".to_string(),
                    "--force".to_string(),
                    "/repo/.fleet/sessions/abc/worktree".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        remove_worktree(
            &inv,
            Path::new("/repo"),
            &PathBuf::from("/repo/.fleet/sessions/abc/worktree"),
            true,
        )
        .unwrap();
    }

    #[test]
    fn remove_worktree_without_force_omits_flag() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "worktree".to_string(),
                    "remove".to_string(),
                    "/repo/.fleet/sessions/abc/worktree".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        remove_worktree(
            &inv,
            Path::new("/repo"),
            &PathBuf::from("/repo/.fleet/sessions/abc/worktree"),
            false,
        )
        .unwrap();
    }

    #[test]
    fn delete_branch_force_uses_big_d() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "branch".to_string(),
                    "-D".to_string(),
                    "fleet/session-abc".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        delete_branch(&inv, Path::new("/repo"), "fleet/session-abc", true).unwrap();
    }

    #[test]
    fn delete_branch_safe_uses_small_d() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "branch".to_string(),
                    "-d".to_string(),
                    "fleet/session-abc".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        delete_branch(&inv, Path::new("/repo"), "fleet/session-abc", false).unwrap();
    }

    #[test]
    fn prune_worktrees_invokes_git_worktree_prune() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "worktree".to_string(),
                    "prune".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::new()));
        prune_worktrees(&inv, Path::new("/repo")).unwrap();
    }

    /// Belt-and-suspenders: a `head_sha` failure must surface the
    /// directory in the error so callers can correlate.
    #[test]
    fn head_sha_error_names_the_directory() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run()
            .with(eq("git"), always())
            .returning(|_, _| Err(anyhow::anyhow!("not a git repository")));
        let err = head_sha(&inv, Path::new("/somewhere")).unwrap_err();
        assert!(format!("{err:#}").contains("/somewhere"));
    }
}
