//! Host-side pull-request / code-forge integration.
//!
//! `CodeHost` is the PR-side counterpart of [`crate::tracker::Tracker`].
//! The two abstractions are deliberately independent: a repo using
//! `git-bug` for issues can still push to a GitHub remote and have
//! pull requests there. The tracker handles tickets; the code host
//! handles PRs, CI checks, and code reviews.
//!
//! The split matters for two reasons:
//! - **Backend independence**: `tracker: git-bug` with `code_host:
//!   github` is a supported configuration. Without this split, PR
//!   methods would have to live on `Tracker` and either bail on
//!   git-bug or pretend not to exist.
//! - **Read vs. write split mirrors the tracker contract**: the trait
//!   surface is full-featured, but at the bridge layer only the read
//!   methods are exposed to agents inside the sandbox — write ops
//!   (`create_pr`, `pr_comment`) stay in workflow-engine territory.
//!   See `src/bin/fleet-pr.rs` and the `/pr/*` routes in the bridge.
//!
//! `GitHubCodeHost` shells `gh` through a [`ProcessInvoker`] so the
//! GitHub implementation is testable without the real binary
//! installed.
//!
//! This module is wired into the executor and bridge in later stages
//! of the loop-scheduler / PR-aware feature; the `dead_code` allow
//! below scopes the temporary unusedness to this stage and disappears
//! as callers light up.

#![allow(dead_code)]

pub mod github;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

pub use github::GitHubCodeHost;

use crate::process::ProcessInvoker;
use crate::repo_config::CodeHostChoice;
// PR filter types come from the workflow spec because YAML parsing
// owns their definition; we re-use them here so node-side and
// trait-side filters can't drift apart.
pub use crate::workflow::spec::{CiStatus, PrFilter, PrState};

/// One pull request as enumerated by [`CodeHost::list_prs`]. Fields
/// match the minimum set the scheduler / executor / bridge need.
/// `head_ref`/`head_sha`/`base_ref` are sufficient for the worktree
/// layer to fetch the PR's head and check it out; `labels` enables
/// the in-Rust label filter on top of the host's native list call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSummary {
    pub number: u32,
    pub title: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub state: PrState,
    pub draft: bool,
    pub url: String,
    pub labels: Vec<String>,
}

/// A pull request plus its body and author. Returned by
/// [`CodeHost::read_pr`]; surfaced verbatim by the bridge's
/// `GET /pr/read` route for agent context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDetail {
    pub summary: PrSummary,
    pub body: String,
    pub author: String,
}

/// Conclusion of a single CI check on a PR. `Pending` covers
/// in-progress, queued, and waiting; the underlying CI distinctions
/// don't survive the trait surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Pending,
    Skipped,
    Neutral,
    Cancelled,
}

/// One check on a PR. `run_id` is the GitHub Actions / equivalent
/// run identifier — when present, [`CodeHost::pr_check_logs`] can
/// fetch the failed step's logs by it. `None` when the host's list
/// API doesn't expose it (e.g. pending status checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub conclusion: CheckConclusion,
    pub details_url: Option<String>,
    pub run_id: Option<u64>,
}

/// Summary of all checks on a PR. `has_failures` / `failed_count` /
/// `pending_count` are pre-aggregated so the `pr-checks` workflow
/// node's `when:` expressions can branch on a scalar without parsing
/// the array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksSummary {
    pub checks: Vec<Check>,
    pub has_failures: bool,
    pub failed_count: u32,
    pub pending_count: u32,
}

/// Pluggable PR / code-host backend.
///
/// Every method has a default bail-by-name impl so a test stub or
/// future-backend with partial coverage doesn't have to repeat
/// boilerplate. The two production paths today are
/// [`GitHubCodeHost`] (full coverage) and the absence of a code host
/// (auto-detect returned `None`, the executor's PR node kinds error
/// out at run time with a clear "no code host configured" message).
pub trait CodeHost: Send + Sync {
    fn name(&self) -> &'static str;

    /// Enumerate PRs matching `filter`. Implementations may evaluate
    /// `filter.ci_status` in-process by consulting the host's check
    /// rollup; `filter.labels` is an OR-match.
    fn list_prs(&self, _repo_root: &Path, _filter: &PrFilter) -> Result<Vec<PrSummary>> {
        bail!("code host `{}` does not implement list_prs", self.name())
    }

    /// Read a single PR with body + author. Used by the bridge's
    /// `GET /pr/read` route.
    fn read_pr(&self, _repo_root: &Path, _number: u32) -> Result<PrDetail> {
        bail!("code host `{}` does not implement read_pr", self.name())
    }

    /// Read the CI checks rollup for a PR.
    fn pr_checks(&self, _repo_root: &Path, _number: u32) -> Result<ChecksSummary> {
        bail!("code host `{}` does not implement pr_checks", self.name())
    }

    /// Fetch the failed-step logs for a check. Implementations may
    /// return a graceful "logs unavailable" body when the host's API
    /// doesn't expose a run id for the check (e.g. pending checks).
    fn pr_check_logs(&self, _repo_root: &Path, _number: u32, _check_name: &str) -> Result<String> {
        bail!(
            "code host `{}` does not implement pr_check_logs",
            self.name()
        )
    }

    /// Post a comment on `number`. Write op — not exposed via the
    /// bridge; reached through the `pr-comment` workflow node.
    fn pr_comment(&self, _repo_root: &Path, _number: u32, _body: &str) -> Result<()> {
        bail!("code host `{}` does not implement pr_comment", self.name())
    }

    /// Open a new PR. `base` / `head` callers may pass any branch
    /// names the host recognises; the executor's `create-pr` node
    /// supplies repo defaults when the YAML omits them. Write op —
    /// invoked from workflow nodes, not the bridge.
    fn create_pr(
        &self,
        _repo_root: &Path,
        _title: &str,
        _body: &str,
        _base: &str,
        _head: &str,
        _draft: bool,
    ) -> Result<PrSummary> {
        bail!("code host `{}` does not implement create_pr", self.name())
    }
}

/// Build the configured code host. Returns `None` when
/// [`CodeHostChoice::Auto`] sees no recognisable remote — callers
/// treat that the same as "no code host available" and surface a
/// clear error if any PR-aware node tries to run.
#[must_use]
pub fn build(
    choice: CodeHostChoice,
    invoker: Arc<dyn ProcessInvoker>,
    repo_root: &Path,
) -> Option<Box<dyn CodeHost>> {
    match choice {
        CodeHostChoice::Auto => match detect_from_remote(&*invoker, repo_root) {
            Some(CodeHostChoice::Github) => Some(Box::new(GitHubCodeHost::new(invoker))),
            // Auto without a recognised remote => no code host.
            Some(CodeHostChoice::Auto) | None => None,
        },
        CodeHostChoice::Github => Some(Box::new(GitHubCodeHost::new(invoker))),
    }
}

/// Inspect `git remote get-url origin` and pick a backend. Currently
/// only recognises `github.com` (HTTPS or SSH); unknown hosts return
/// `None` so the caller can fall back to the config default rather
/// than guess wrong. Pure modulo the subprocess call.
#[must_use]
pub fn detect_from_remote(invoker: &dyn ProcessInvoker, repo_root: &Path) -> Option<CodeHostChoice> {
    let out = invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "remote".to_string(),
                "get-url".to_string(),
                "origin".to_string(),
            ],
        )
        .ok()?;
    let url = out.trim();
    if url.is_empty() {
        return None;
    }
    if remote_url_host_is(url, "github.com") {
        return Some(CodeHostChoice::Github);
    }
    None
}

/// Return `true` if `url` (an HTTPS / SSH git remote URL) points at
/// `host`. Recognises:
/// - `https://github.com/owner/repo.git`
/// - `git@github.com:owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git`
///
/// Pure; exposed so the remote-detection tests don't have to spin up
/// a `MockProcessInvoker`.
#[must_use]
pub fn remote_url_host_is(url: &str, host: &str) -> bool {
    // HTTPS or SSH-protocol form: `<scheme>://[user@]host/<path>`.
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        let after_user = rest.split('@').next_back().unwrap_or(rest);
        let host_part = after_user.split('/').next().unwrap_or("");
        // Strip a trailing port if present.
        let host_only = host_part.split(':').next().unwrap_or("");
        return host_only.eq_ignore_ascii_case(host);
    }
    // SCP-like form: `user@host:owner/repo.git`.
    if let Some(rest) = url.split_once('@').map(|(_, r)| r) {
        let host_part = rest.split(':').next().unwrap_or("");
        return host_part.eq_ignore_ascii_case(host);
    }
    false
}

/// Parse a check's "details" URL into a run id when possible. GitHub's
/// `gh pr checks` doesn't print the run id directly — only a URL like
/// `https://github.com/owner/repo/actions/runs/123456/job/789012`.
/// The run id is the path component after `/runs/`. Returns `None`
/// when no `/runs/<digits>` segment is present (the link points at a
/// non-Actions check, e.g. a third-party CI service).
#[must_use]
pub fn run_id_from_details_url(url: &str) -> Option<u64> {
    let after = url.split("/runs/").nth(1)?;
    let id_part = after.split('/').next()?;
    id_part.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    struct StubCodeHost;
    impl CodeHost for StubCodeHost {
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn default_methods_bail_with_code_host_name_in_message() {
        let h = StubCodeHost;
        let repo = Path::new("/repo");
        for (op, err) in [
            ("list_prs", h.list_prs(repo, &PrFilter::default()).err()),
            ("read_pr", h.read_pr(repo, 1).err()),
            ("pr_checks", h.pr_checks(repo, 1).err()),
            ("pr_check_logs", h.pr_check_logs(repo, 1, "x").err()),
            ("pr_comment", h.pr_comment(repo, 1, "hi").err()),
            (
                "create_pr",
                h.create_pr(repo, "t", "b", "main", "feat", false).err(),
            ),
        ] {
            let msg = format!(
                "{:#}",
                err.unwrap_or_else(|| panic!("expected {op} to bail"))
            );
            assert!(msg.contains("stub"), "{op}: missing host name — {msg}");
            assert!(msg.contains(op), "{op}: missing op name — {msg}");
        }
    }

    #[test]
    fn detect_recognises_github_https_url() {
        assert!(remote_url_host_is(
            "https://github.com/owner/repo.git",
            "github.com"
        ));
        assert!(remote_url_host_is(
            "https://GitHub.com/owner/repo",
            "github.com"
        ));
    }

    #[test]
    fn detect_recognises_github_ssh_scp_url() {
        assert!(remote_url_host_is(
            "git@github.com:owner/repo.git",
            "github.com"
        ));
    }

    #[test]
    fn detect_recognises_github_ssh_protocol_url() {
        assert!(remote_url_host_is(
            "ssh://git@github.com/owner/repo.git",
            "github.com"
        ));
    }

    #[test]
    fn detect_rejects_unknown_host() {
        assert!(!remote_url_host_is(
            "https://gitlab.com/owner/repo.git",
            "github.com"
        ));
        assert!(!remote_url_host_is(
            "git@bitbucket.org:owner/repo.git",
            "github.com"
        ));
    }

    #[test]
    fn detect_from_remote_picks_github_for_recognised_https_url() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("git"),
                eq(vec![
                    "-C".to_string(),
                    "/repo".to_string(),
                    "remote".to_string(),
                    "get-url".to_string(),
                    "origin".to_string(),
                ]),
            )
            .returning(|_, _| Ok("https://github.com/owner/repo.git".to_string()));
        let detected = detect_from_remote(&mock, Path::new("/repo"));
        assert_eq!(detected, Some(CodeHostChoice::Github));
    }

    #[test]
    fn detect_from_remote_returns_none_for_unknown_remote() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok("https://example.com/owner/repo.git".to_string()));
        assert_eq!(detect_from_remote(&mock, Path::new("/repo")), None);
    }

    #[test]
    fn detect_from_remote_returns_none_when_git_fails() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("no remote")));
        assert_eq!(detect_from_remote(&mock, Path::new("/repo")), None);
    }

    #[test]
    fn build_with_explicit_github_returns_github_host() {
        let mock = MockProcessInvoker::new();
        let invoker = Arc::new(mock) as Arc<dyn ProcessInvoker>;
        let host = build(CodeHostChoice::Github, invoker, Path::new("/repo")).unwrap();
        assert_eq!(host.name(), "github");
    }

    #[test]
    fn build_with_auto_and_github_remote_returns_github_host() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok("git@github.com:owner/repo.git".to_string()));
        let invoker = Arc::new(mock) as Arc<dyn ProcessInvoker>;
        let host = build(CodeHostChoice::Auto, invoker, Path::new("/repo")).unwrap();
        assert_eq!(host.name(), "github");
    }

    #[test]
    fn build_with_auto_and_unknown_remote_returns_none() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok("https://example.com/x.git".to_string()));
        let invoker = Arc::new(mock) as Arc<dyn ProcessInvoker>;
        assert!(build(CodeHostChoice::Auto, invoker, Path::new("/repo")).is_none());
    }

    #[test]
    fn run_id_extracted_from_actions_details_url() {
        let url = "https://github.com/owner/repo/actions/runs/123456/job/789012";
        assert_eq!(run_id_from_details_url(url), Some(123_456));
    }

    #[test]
    fn run_id_none_for_url_without_runs_segment() {
        assert_eq!(
            run_id_from_details_url("https://example.com/some/other/check/42"),
            None
        );
    }

    #[test]
    fn checks_summary_serdes_round_trip() {
        let summary = ChecksSummary {
            checks: vec![Check {
                name: "build".into(),
                conclusion: CheckConclusion::Failure,
                details_url: Some("https://github.com/x/y/actions/runs/9/job/1".into()),
                run_id: Some(9),
            }],
            has_failures: true,
            failed_count: 1,
            pending_count: 0,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: ChecksSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }
}
