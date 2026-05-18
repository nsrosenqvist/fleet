#![allow(dead_code)]
//! Host-side GitHub code host. Shells `gh` for every operation.
//!
//! Auth is the host's `gh auth login` — fleet doesn't manage it. The
//! shape mirrors [`crate::tracker::github::GitHubTracker`] one-for-one
//! so an operator who already trusts the issue path inherits the same
//! contract for PRs.
//!
//! `gh` argv shapes used here (released CLI):
//! - `gh pr list --json number,title,headRefName,headRefOid,baseRefName,state,isDraft,url,labels,statusCheckRollup --limit 200 --state <state>`
//! - `gh pr view <n> --json number,title,headRefName,headRefOid,baseRefName,state,isDraft,url,labels,body,author`
//! - `gh pr checks <n> --json name,bucket,state,link,workflowName`
//! - `gh run view <run_id> --log-failed`
//! - `gh pr comment <n> --body <body>`
//! - `gh pr create --title <t> --body <b> --base <base> --head <head> [--draft]`

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use super::{
    Check, CheckConclusion, ChecksSummary, CiStatus, CodeHost, PrDetail, PrFilter, PrState,
    PrSummary, run_id_from_details_url,
};
use crate::process::ProcessInvoker;
use crate::tracker::shell_quote_path;

pub struct GitHubCodeHost {
    invoker: Arc<dyn ProcessInvoker>,
}

impl GitHubCodeHost {
    pub const fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { invoker }
    }

    /// `cd <repo> && <cmd>` wrapped for `sh -c`. Identical pattern to
    /// the tracker's `GitHubTracker::shell_cmd` (kept private to each
    /// module so the abstraction stays narrow).
    fn shell_cmd(repo_root: &Path, cmd: &str) -> Vec<String> {
        let wrapped = format!("cd {} && {cmd}", shell_quote_path(repo_root));
        vec!["-c".to_string(), wrapped]
    }
}

impl CodeHost for GitHubCodeHost {
    fn name(&self) -> &'static str {
        "github"
    }

    fn list_prs(&self, repo_root: &Path, filter: &PrFilter) -> Result<Vec<PrSummary>> {
        let state_flag = pr_state_to_gh_state(filter.state);
        let cmd = format!(
            "gh pr list --json number,title,headRefName,headRefOid,baseRefName,state,\
             isDraft,url,labels,statusCheckRollup --limit 200 --state {state_flag}"
        );
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .context("invoking `gh pr list`")?;
        let paired = parse_gh_pr_list_with_ci_status(&stdout);
        Ok(paired
            .into_iter()
            .filter(|(pr, ci)| pr_matches_filter(pr, *ci, filter))
            .map(|(pr, _)| pr)
            .collect())
    }

    fn read_pr(&self, repo_root: &Path, number: u32) -> Result<PrDetail> {
        let cmd = format!(
            "gh pr view {number} --json number,title,headRefName,headRefOid,baseRefName,\
             state,isDraft,url,labels,body,author"
        );
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh pr view {number}`"))?;
        parse_gh_pr_view(&stdout)
    }

    fn pr_checks(&self, repo_root: &Path, number: u32) -> Result<ChecksSummary> {
        let cmd = format!("gh pr checks {number} --json name,bucket,state,link,workflowName");
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh pr checks {number}`"))?;
        Ok(parse_gh_pr_checks(&stdout))
    }

    fn pr_check_logs(&self, repo_root: &Path, number: u32, check_name: &str) -> Result<String> {
        let summary = self.pr_checks(repo_root, number)?;
        let Some(check) = summary.checks.iter().find(|c| c.name == check_name) else {
            return Ok(format!(
                "no check named `{check_name}` on PR #{number} \
                 (available: {})",
                summary
                    .checks
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        };
        let Some(run_id) = check.run_id else {
            return Ok(format!(
                "logs unavailable for `{check_name}` on PR #{number}: \
                 no GitHub Actions run id is associated with this check \
                 (status={:?}, details_url={:?})",
                check.conclusion, check.details_url,
            ));
        };
        let cmd = format!("gh run view {run_id} --log-failed");
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh run view {run_id} --log-failed`"))
    }

    fn pr_comment(&self, repo_root: &Path, number: u32, body: &str) -> Result<()> {
        let cmd = format!(
            "gh pr comment {number} --body {}",
            shell_quote_str(body),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh pr comment {number}`"))?;
        Ok(())
    }

    fn create_pr(
        &self,
        repo_root: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        draft: bool,
    ) -> Result<PrSummary> {
        let mut cmd = format!(
            "gh pr create --title {} --body {} --base {} --head {}",
            shell_quote_str(title),
            shell_quote_str(body),
            shell_quote_str(base),
            shell_quote_str(head),
        );
        if draft {
            cmd.push_str(" --draft");
        }
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh pr create --title {title}`"))?;
        let number = parse_gh_pr_create_url(&stdout).ok_or_else(|| {
            anyhow!("`gh pr create` did not print a recognisable URL; stdout: {stdout:?}")
        })?;
        // Round-trip through `read_pr` so callers get full summary
        // including head_sha — `gh pr create` only emits the URL.
        self.read_pr(repo_root, number).map(|d| d.summary)
    }
}

fn pr_state_to_gh_state(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

/// Match `pr` against `filter`. `ci_status_hint` is the aggregated CI
/// status pulled from the host's check rollup at parse time —
/// alongside the summary rather than on it because the rollup is a
/// host-specific concern that doesn't belong on the public
/// `PrSummary` surface.
///
/// `filter.labels` is an OR-match: at least one of the configured
/// labels must appear on the PR.
fn pr_matches_filter(pr: &PrSummary, ci_status_hint: Option<CiStatus>, filter: &PrFilter) -> bool {
    if let Some(want) = filter.ci_status {
        if ci_status_hint != Some(want) {
            return false;
        }
    }
    if !filter.labels.is_empty()
        && !filter
            .labels
            .iter()
            .any(|want| pr.labels.iter().any(|have| have == want))
    {
        return false;
    }
    true
}

/// Parse `gh pr list --json …` output. Pairs each summary with the
/// aggregated CI status from its `statusCheckRollup` field. Pure;
/// exposed for tests.
#[must_use]
pub fn parse_gh_pr_list_with_ci_status(stdout: &str) -> Vec<(PrSummary, Option<CiStatus>)> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let raw: Vec<GhPr> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    raw.into_iter()
        .map(|p| {
            let ci = aggregate_rollup_status(&p.status_check_rollup);
            (PrSummary::from(p), ci)
        })
        .collect()
}

/// Parse `gh pr list --json …` output into bare summaries. Convenience
/// wrapper around [`parse_gh_pr_list_with_ci_status`] for callers that
/// don't need the rollup hint.
#[must_use]
pub fn parse_gh_pr_list(stdout: &str) -> Vec<PrSummary> {
    parse_gh_pr_list_with_ci_status(stdout)
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// Map the rollup entries `gh pr list` emits onto a single
/// [`CiStatus`]: `Failure` if any failed, otherwise `Pending` if any
/// in-progress, otherwise `Success` if everything completed, else
/// `None` (no checks at all).
fn aggregate_rollup_status(rollup: &[GhRollupEntry]) -> Option<CiStatus> {
    if rollup.is_empty() {
        return None;
    }
    let mut saw_failure = false;
    let mut saw_pending = false;
    let mut saw_success = false;
    for entry in rollup {
        match bucket_to_ci_status(entry.bucket.as_deref()) {
            Some(CiStatus::Failure) => saw_failure = true,
            Some(CiStatus::Pending) => saw_pending = true,
            Some(CiStatus::Success) => saw_success = true,
            Some(CiStatus::Skipped) | None => {}
        }
    }
    if saw_failure {
        Some(CiStatus::Failure)
    } else if saw_pending {
        Some(CiStatus::Pending)
    } else if saw_success {
        Some(CiStatus::Success)
    } else {
        None
    }
}

/// `gh pr view <n> --json …` shape. Errors out on parse failure so
/// the bridge / `read_pr` caller can distinguish "no PR" from "auth
/// failed" — same rationale as `parse_gh_view_output` in the tracker.
pub fn parse_gh_pr_view(stdout: &str) -> Result<PrDetail> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("`gh pr view` returned no output"));
    }
    let raw: GhPrView = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing `gh pr view --json …` payload: {trimmed:?}"))?;
    let summary = PrSummary {
        number: raw.number,
        title: raw.title,
        head_ref: raw.head_ref_name,
        head_sha: raw.head_ref_oid,
        base_ref: raw.base_ref_name,
        state: parse_pr_state(&raw.state),
        draft: raw.is_draft,
        url: raw.url,
        labels: raw.labels.into_iter().map(|l| l.name).collect(),
    };
    Ok(PrDetail {
        summary,
        body: raw.body,
        author: raw.author.login,
    })
}

/// Parse `gh pr checks <n> --json name,bucket,state,link,workflowName`.
/// Tolerant: malformed entries are skipped, not raised — matches the
/// `parse_gh_output` policy on the issue side. Returns an empty
/// `ChecksSummary` for unparseable input rather than failing the
/// whole `pr_checks` call.
#[must_use]
pub fn parse_gh_pr_checks(stdout: &str) -> ChecksSummary {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return empty_checks_summary();
    }
    let raw: Vec<GhCheck> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return empty_checks_summary(),
    };
    let checks: Vec<Check> = raw
        .into_iter()
        .map(|c| {
            let conclusion = bucket_to_check_conclusion(c.bucket.as_deref(), c.state.as_deref());
            let run_id = c
                .link
                .as_deref()
                .and_then(run_id_from_details_url);
            Check {
                name: c.name,
                conclusion,
                details_url: c.link,
                run_id,
            }
        })
        .collect();
    let failed_count = u32::try_from(
        checks
            .iter()
            .filter(|c| c.conclusion == CheckConclusion::Failure)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let pending_count = u32::try_from(
        checks
            .iter()
            .filter(|c| c.conclusion == CheckConclusion::Pending)
            .count(),
    )
    .unwrap_or(u32::MAX);
    ChecksSummary {
        has_failures: failed_count > 0,
        failed_count,
        pending_count,
        checks,
    }
}

/// Parse the URL out of `gh pr create`'s stdout. `gh` prints a single
/// line URL like `https://github.com/owner/repo/pull/42`. Same
/// approach as `parse_gh_create_output` on the issue side.
#[must_use]
pub fn parse_gh_pr_create_url(stdout: &str) -> Option<u32> {
    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    let last_segment = last_line.trim().rsplit('/').next()?;
    last_segment.parse::<u32>().ok()
}

fn empty_checks_summary() -> ChecksSummary {
    ChecksSummary {
        checks: Vec::new(),
        has_failures: false,
        failed_count: 0,
        pending_count: 0,
    }
}

fn bucket_to_ci_status(bucket: Option<&str>) -> Option<CiStatus> {
    match bucket? {
        "pass" => Some(CiStatus::Success),
        "fail" => Some(CiStatus::Failure),
        "pending" => Some(CiStatus::Pending),
        "skipping" | "skip" | "skipped" => Some(CiStatus::Skipped),
        _ => None,
    }
}

fn bucket_to_check_conclusion(bucket: Option<&str>, state: Option<&str>) -> CheckConclusion {
    match bucket {
        Some("pass") => CheckConclusion::Success,
        Some("fail") => CheckConclusion::Failure,
        Some("pending") => CheckConclusion::Pending,
        Some("skipping" | "skip" | "skipped") => CheckConclusion::Skipped,
        Some("cancelled") => CheckConclusion::Cancelled,
        Some("neutral") => CheckConclusion::Neutral,
        _ => match state {
            Some("SUCCESS") => CheckConclusion::Success,
            Some("FAILURE") => CheckConclusion::Failure,
            Some("IN_PROGRESS" | "QUEUED" | "PENDING") => CheckConclusion::Pending,
            Some("SKIPPED") => CheckConclusion::Skipped,
            Some("CANCELLED") => CheckConclusion::Cancelled,
            _ => CheckConclusion::Neutral,
        },
    }
}

fn parse_pr_state(s: &str) -> PrState {
    match s.to_uppercase().as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        // CLOSED is the fallback — gh emits "CLOSED" for explicitly
        // closed (un-merged) PRs and we want any unrecognised string
        // to stay safely "closed" rather than masquerading as open.
        _ => PrState::Closed,
    }
}

/// POSIX-shell single-quote escape — same shape as
/// `tracker::github::shell_quote_str` but private to this module.
fn shell_quote_str(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[derive(Deserialize)]
struct GhPr {
    number: u32,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "headRefName")]
    head_ref_name: String,
    #[serde(default, rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(default, rename = "baseRefName")]
    base_ref_name: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    #[serde(default)]
    url: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default, rename = "statusCheckRollup")]
    status_check_rollup: Vec<GhRollupEntry>,
}

#[derive(Deserialize)]
struct GhLabel {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct GhRollupEntry {
    #[serde(default)]
    bucket: Option<String>,
}

#[derive(Deserialize)]
struct GhPrView {
    number: u32,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "headRefName")]
    head_ref_name: String,
    #[serde(default, rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(default, rename = "baseRefName")]
    base_ref_name: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    #[serde(default)]
    url: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    author: GhAuthor,
}

#[derive(Deserialize, Default)]
struct GhAuthor {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct GhCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    link: Option<String>,
}

impl From<GhPr> for PrSummary {
    fn from(g: GhPr) -> Self {
        Self {
            number: g.number,
            title: g.title,
            head_ref: g.head_ref_name,
            head_sha: g.head_ref_oid,
            base_ref: g.base_ref_name,
            state: parse_pr_state(&g.state),
            draft: g.is_draft,
            url: g.url,
            labels: g.labels.into_iter().map(|l| l.name).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    fn invoker_returning(stdout: &'static str) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(move |_, _| Ok(stdout.to_string()));
        Arc::new(mock)
    }

    fn expect_shell_cmd(
        mock: &mut MockProcessInvoker,
        repo_root: &'static str,
        expected_cmd: &'static str,
        reply: &'static str,
    ) {
        let expected_arg = format!("cd '{repo_root}' && {expected_cmd}");
        mock.expect_run()
            .with(eq("sh"), eq(vec!["-c".to_string(), expected_arg]))
            .returning(move |_, _| Ok(reply.to_string()));
    }

    #[test]
    fn name_is_github() {
        let h = GitHubCodeHost::new(invoker_returning(""));
        assert_eq!(h.name(), "github");
    }

    #[test]
    fn list_prs_parses_gh_pr_list_payload() {
        let json = r#"[
            {"number":42,"title":"fix x","headRefName":"feat/x","headRefOid":"abc",
             "baseRefName":"main","state":"OPEN","isDraft":false,
             "url":"https://github.com/o/r/pull/42",
             "labels":[{"name":"bug"}],"statusCheckRollup":[{"bucket":"fail"}]},
            {"number":7,"title":"chore","headRefName":"feat/y","headRefOid":"def",
             "baseRefName":"main","state":"OPEN","isDraft":true,
             "url":"https://github.com/o/r/pull/7","labels":[],
             "statusCheckRollup":[{"bucket":"pass"}]}
        ]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let prs = h.list_prs(Path::new("/repo"), &PrFilter::default()).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].head_ref, "feat/x");
        assert_eq!(prs[0].labels, vec!["bug"]);
        assert!(!prs[0].draft);
        assert!(prs[1].draft);
    }

    #[test]
    fn list_prs_filters_by_ci_status_failure() {
        let json = r#"[
            {"number":42,"title":"x","headRefName":"f","headRefOid":"a","baseRefName":"m",
             "state":"OPEN","isDraft":false,"url":"u","labels":[],
             "statusCheckRollup":[{"bucket":"fail"}]},
            {"number":7,"title":"y","headRefName":"g","headRefOid":"b","baseRefName":"m",
             "state":"OPEN","isDraft":false,"url":"u","labels":[],
             "statusCheckRollup":[{"bucket":"pass"}]}
        ]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let prs = h
            .list_prs(
                Path::new("/repo"),
                &PrFilter {
                    ci_status: Some(CiStatus::Failure),
                    state: PrState::Open,
                    labels: vec![],
                },
            )
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
    }

    #[test]
    fn list_prs_filters_by_label_or_match() {
        let json = r#"[
            {"number":42,"title":"x","headRefName":"f","headRefOid":"a","baseRefName":"m",
             "state":"OPEN","isDraft":false,"url":"u",
             "labels":[{"name":"needs-review"}],"statusCheckRollup":[]},
            {"number":7,"title":"y","headRefName":"g","headRefOid":"b","baseRefName":"m",
             "state":"OPEN","isDraft":false,"url":"u","labels":[],
             "statusCheckRollup":[]}
        ]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let prs = h
            .list_prs(
                Path::new("/repo"),
                &PrFilter {
                    ci_status: None,
                    state: PrState::Open,
                    labels: vec!["needs-review".into()],
                },
            )
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
    }

    #[test]
    fn list_prs_invokes_cd_wrapped_shell_with_state_open_by_default() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "gh pr list --json number,title,headRefName,headRefOid,baseRefName,state,\
             isDraft,url,labels,statusCheckRollup --limit 200 --state open",
            "[]",
        );
        let h = GitHubCodeHost::new(Arc::new(mock));
        let prs = h.list_prs(Path::new("/repo"), &PrFilter::default()).unwrap();
        assert!(prs.is_empty());
    }

    #[test]
    fn read_pr_parses_gh_pr_view_payload() {
        let json = r#"{"number":42,"title":"fix x","headRefName":"feat/x",
            "headRefOid":"abc123","baseRefName":"main","state":"OPEN","isDraft":false,
            "url":"https://github.com/o/r/pull/42","labels":[{"name":"bug"}],
            "body":"detailed description","author":{"login":"alice"}}"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let detail = h.read_pr(Path::new("/repo"), 42).unwrap();
        assert_eq!(detail.summary.number, 42);
        assert_eq!(detail.summary.head_sha, "abc123");
        assert_eq!(detail.body, "detailed description");
        assert_eq!(detail.author, "alice");
        assert_eq!(detail.summary.labels, vec!["bug"]);
    }

    #[test]
    fn pr_checks_aggregates_rollup_with_run_id_extraction() {
        let json = r#"[
            {"name":"build","bucket":"fail","state":"FAILURE",
             "link":"https://github.com/o/r/actions/runs/123/job/9","workflowName":"ci"},
            {"name":"lint","bucket":"pass","state":"SUCCESS",
             "link":"https://github.com/o/r/actions/runs/124/job/9","workflowName":"ci"},
            {"name":"deploy","bucket":"pending","state":"IN_PROGRESS",
             "link":"https://example.com/check/0","workflowName":"deploy"}
        ]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let summary = h.pr_checks(Path::new("/repo"), 42).unwrap();
        assert_eq!(summary.checks.len(), 3);
        assert!(summary.has_failures);
        assert_eq!(summary.failed_count, 1);
        assert_eq!(summary.pending_count, 1);
        let build = &summary.checks[0];
        assert_eq!(build.name, "build");
        assert_eq!(build.conclusion, CheckConclusion::Failure);
        assert_eq!(build.run_id, Some(123));
        let deploy = &summary.checks[2];
        assert_eq!(deploy.run_id, None);
    }

    #[test]
    fn pr_check_logs_falls_back_when_no_run_id_available() {
        let json = r#"[{"name":"deploy","bucket":"pending","state":"IN_PROGRESS",
            "link":"https://example.com/check/0","workflowName":"deploy"}]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let logs = h.pr_check_logs(Path::new("/repo"), 42, "deploy").unwrap();
        assert!(logs.contains("logs unavailable"));
        assert!(logs.contains("deploy"));
    }

    #[test]
    fn pr_check_logs_reports_when_check_missing() {
        let json = r#"[{"name":"build","bucket":"pass","state":"SUCCESS",
            "link":"https://github.com/o/r/actions/runs/1/job/2","workflowName":"ci"}]"#;
        let h = GitHubCodeHost::new(invoker_returning(json));
        let logs = h.pr_check_logs(Path::new("/repo"), 42, "nope").unwrap();
        assert!(logs.contains("no check named `nope`"));
        assert!(logs.contains("build"));
    }

    #[test]
    fn pr_comment_invokes_cd_wrapped_gh_pr_comment() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "gh pr comment 42 --body 'hello world'",
            "",
        );
        let h = GitHubCodeHost::new(Arc::new(mock));
        h.pr_comment(Path::new("/repo"), 42, "hello world").unwrap();
    }

    #[test]
    fn create_pr_invokes_gh_pr_create_with_all_flags_and_round_trips_via_view() {
        // gh pr create prints the new URL; we then call read_pr to
        // recover head_sha. Two shell calls.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "gh pr create --title 'fleet change' --body 'see commit' \
             --base 'main' --head 'feat/x' --draft",
            "https://github.com/o/r/pull/42\n",
        );
        let view_json = r#"{"number":42,"title":"fleet change","headRefName":"feat/x",
            "headRefOid":"deadbeef","baseRefName":"main","state":"OPEN","isDraft":true,
            "url":"https://github.com/o/r/pull/42","labels":[],"body":"see commit",
            "author":{"login":"fleet-bot"}}"#;
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "gh pr view 42 --json number,title,headRefName,headRefOid,baseRefName,\
             state,isDraft,url,labels,body,author",
            view_json,
        );
        let h = GitHubCodeHost::new(Arc::new(mock));
        let summary = h
            .create_pr(
                Path::new("/repo"),
                "fleet change",
                "see commit",
                "main",
                "feat/x",
                true,
            )
            .unwrap();
        assert_eq!(summary.number, 42);
        assert_eq!(summary.head_sha, "deadbeef");
        assert!(summary.draft);
    }

    #[test]
    fn parse_gh_pr_create_url_recovers_number_from_trailing_segment() {
        assert_eq!(
            parse_gh_pr_create_url("https://github.com/o/r/pull/42\n"),
            Some(42)
        );
        assert_eq!(parse_gh_pr_create_url(""), None);
    }

    #[test]
    fn parse_gh_pr_checks_returns_empty_summary_on_malformed_input() {
        let s = parse_gh_pr_checks("not json");
        assert!(s.checks.is_empty());
        assert!(!s.has_failures);
    }

    #[test]
    fn aggregate_rollup_status_picks_failure_over_pending_over_success() {
        let bucket = |b: &str| GhRollupEntry {
            bucket: Some(b.to_string()),
        };
        assert_eq!(
            aggregate_rollup_status(&[bucket("pass"), bucket("fail"), bucket("pending")]),
            Some(CiStatus::Failure)
        );
        assert_eq!(
            aggregate_rollup_status(&[bucket("pass"), bucket("pending")]),
            Some(CiStatus::Pending)
        );
        assert_eq!(
            aggregate_rollup_status(&[bucket("pass"), bucket("pass")]),
            Some(CiStatus::Success)
        );
        assert_eq!(aggregate_rollup_status(&[]), None);
    }
}
