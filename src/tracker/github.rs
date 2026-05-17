//! Host-side GitHub tracker.
//!
//! Shells `gh` directly on the host through a [`ProcessInvoker`]. Auth
//! is the user's `gh auth login` — fleet doesn't manage it. The host's
//! `~/.config/gh/hosts.yml` is the only configuration consulted; no VM
//! round-trip.
//!
//! Argv shapes follow the released `gh` CLI: `gh issue list/view/comment/
//! edit/create/close/reopen`. Labels are accumulated via repeated
//! `--label` flags. Mutating commands never trigger an interactive
//! editor because every body/title is supplied via `--body` / `--title`.
//!
//! Issue identifiers cross fleet's boundary as `human_id` (the bare
//! number, e.g. `"42"`), not URLs. `gh` accepts either; fleet hands it
//! the number so logs and error messages stay short.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use super::{Comment, Issue, IssueDetail, Status, Tracker, sort_open_first};
use crate::process::ProcessInvoker;

pub struct GitHubTracker {
    invoker: Arc<dyn ProcessInvoker>,
}

impl GitHubTracker {
    pub const fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { invoker }
    }

    /// Build `sh -c "cd <repo> && <cmd>"`. `gh` auto-detects the repo
    /// from the cwd's git remote, so every invocation enters the repo
    /// first.
    fn shell_cmd(repo_root: &Path, cmd: &str) -> Vec<String> {
        let wrapped = format!("cd {} && {cmd}", super::shell_quote_path(repo_root));
        vec!["-c".to_string(), wrapped]
    }
}

impl Tracker for GitHubTracker {
    fn name(&self) -> &'static str {
        "github"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        let stdout = self
            .invoker
            .run(
                "sh",
                Self::shell_cmd(
                    repo_root,
                    "gh issue list --json number,title,state,labels --limit 200 --state all",
                ),
            )
            .context("invoking `gh issue list`")?;
        Ok(parse_gh_output(&stdout))
    }

    fn read(&self, repo_root: &Path, issue_id: &str) -> Result<IssueDetail> {
        let cmd = format!(
            "gh issue view {} --json number,title,state,labels,body,comments",
            shell_quote_str(issue_id)
        );
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh issue view {issue_id}`"))?;
        parse_gh_view_output(&stdout)
    }

    fn comment(&self, repo_root: &Path, issue_id: &str, body: &str) -> Result<()> {
        let cmd = format!(
            "gh issue comment {} --body {}",
            shell_quote_str(issue_id),
            shell_quote_str(body),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh issue comment {issue_id}`"))?;
        Ok(())
    }

    fn set_status(&self, repo_root: &Path, issue_id: &str, status: Status) -> Result<()> {
        // GitHub has no first-class "in-progress" state; the convention
        // is an `in-progress` label so workflow YAML doesn't have to
        // fork on tracker type.
        let cmd = match status {
            Status::Open => format!("gh issue reopen {}", shell_quote_str(issue_id)),
            Status::Closed => format!("gh issue close {}", shell_quote_str(issue_id)),
            Status::InProgress => format!(
                "gh issue edit {} --add-label in-progress",
                shell_quote_str(issue_id)
            ),
        };
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking gh status verb for {issue_id}"))?;
        Ok(())
    }

    fn add_label(&self, repo_root: &Path, issue_id: &str, label: &str) -> Result<()> {
        let cmd = format!(
            "gh issue edit {} --add-label {}",
            shell_quote_str(issue_id),
            shell_quote_str(label),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh issue edit --add-label` for {issue_id}"))?;
        Ok(())
    }

    fn remove_label(&self, repo_root: &Path, issue_id: &str, label: &str) -> Result<()> {
        let cmd = format!(
            "gh issue edit {} --remove-label {}",
            shell_quote_str(issue_id),
            shell_quote_str(label),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh issue edit --remove-label` for {issue_id}"))?;
        Ok(())
    }

    fn create(
        &self,
        repo_root: &Path,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<Issue> {
        let mut cmd = format!(
            "gh issue create --title {} --body {}",
            shell_quote_str(title),
            shell_quote_str(body),
        );
        // gh accumulates labels via repeated --label flags. Empty
        // label slice → no flag, which gh accepts.
        for label in labels {
            cmd.push_str(" --label ");
            cmd.push_str(&shell_quote_str(label));
        }
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `gh issue create --title {title}`"))?;
        let number = parse_gh_create_output(&stdout).ok_or_else(|| {
            anyhow!("`gh issue create` did not print a recognisable URL; stdout: {stdout:?}")
        })?;
        Ok(Issue {
            id: format!("gh:{number}"),
            human_id: number.to_string(),
            title: title.to_string(),
            status: "open".to_string(),
            labels: labels.to_vec(),
        })
    }

    fn link_parent(&self, repo_root: &Path, parent_id: &str, child_id: &str) -> Result<()> {
        // GitHub's native sub-issues feature (REST API surface added
        // in api version `2026-03-10`, exposed in the UI as the
        // "Sub-issues" panel on the parent). Three `gh api` shellouts:
        //
        //   1. list existing sub-issues so re-linking is a no-op
        //      (the POST endpoint isn't idempotent — it would return
        //      422 on a duplicate, and we want `link_parent` to stay
        //      cheap to re-run);
        //   2. resolve the child issue's database id, because the POST
        //      body takes the numeric `id`, not the user-facing issue
        //      `number`;
        //   3. POST the link.
        //
        // `{owner}/{repo}` are placeholders that `gh api` substitutes
        // from the current git remote, same as fleet's other `gh`
        // shellouts (which all `cd` into the repo first).
        let existing_numbers = {
            let cmd = format!(
                "gh api {API_VERSION_HEADER} \
                 repos/{{owner}}/{{repo}}/issues/{}/sub_issues",
                shell_quote_str(parent_id),
            );
            let stdout = self
                .invoker
                .run("sh", Self::shell_cmd(repo_root, &cmd))
                .with_context(|| {
                    format!("listing sub-issues of #{parent_id} for idempotence check")
                })?;
            parse_issue_number_list(&stdout)
        };
        if existing_numbers
            .iter()
            .any(|n| n.to_string() == child_id)
        {
            return Ok(());
        }

        let child_db_id = {
            let cmd = format!(
                "gh api {API_VERSION_HEADER} \
                 repos/{{owner}}/{{repo}}/issues/{} --jq .id",
                shell_quote_str(child_id),
            );
            let stdout = self
                .invoker
                .run("sh", Self::shell_cmd(repo_root, &cmd))
                .with_context(|| format!("resolving database id of #{child_id}"))?;
            stdout.trim().parse::<u64>().with_context(|| {
                format!(
                    "parsing database id for #{child_id}: expected an integer, got {:?}",
                    stdout.trim()
                )
            })?
        };

        let post_cmd = format!(
            "gh api {API_VERSION_HEADER} -X POST -F sub_issue_id={child_db_id} \
             repos/{{owner}}/{{repo}}/issues/{}/sub_issues",
            shell_quote_str(parent_id),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &post_cmd))
            .with_context(|| format!("linking #{child_id} as a sub-issue of #{parent_id}"))?;
        Ok(())
    }

    fn link_blocks(
        &self,
        repo_root: &Path,
        blocked_id: &str,
        blocked_on_id: &str,
    ) -> Result<()> {
        // GitHub's native Issue Dependencies REST API. Same three-step
        // pattern as `link_parent`'s sub-issues flow because the POST
        // endpoint isn't idempotent (duplicate links return 422):
        //
        //   1. list `blocked_by` deps on the blocked issue;
        //   2. resolve the blocked_on issue's database id (the POST
        //      body takes `issue_id`, not the user-facing number);
        //   3. POST the dep.
        //
        // POSTing to `…/issues/<blocked>/dependencies/blocked_by` with
        // `issue_id: <blocked_on>` is symmetric: GitHub records the
        // inverse "blocking" relationship on the blocked_on issue
        // automatically, so we don't have to make the call twice.
        let existing_numbers = {
            let cmd = format!(
                "gh api {API_VERSION_HEADER} \
                 repos/{{owner}}/{{repo}}/issues/{}/dependencies/blocked_by",
                shell_quote_str(blocked_id),
            );
            let stdout = self
                .invoker
                .run("sh", Self::shell_cmd(repo_root, &cmd))
                .with_context(|| {
                    format!("listing blocked-by deps of #{blocked_id} for idempotence check")
                })?;
            parse_issue_number_list(&stdout)
        };
        if existing_numbers
            .iter()
            .any(|n| n.to_string() == blocked_on_id)
        {
            return Ok(());
        }

        let blocked_on_db_id = {
            let cmd = format!(
                "gh api {API_VERSION_HEADER} \
                 repos/{{owner}}/{{repo}}/issues/{} --jq .id",
                shell_quote_str(blocked_on_id),
            );
            let stdout = self
                .invoker
                .run("sh", Self::shell_cmd(repo_root, &cmd))
                .with_context(|| format!("resolving database id of #{blocked_on_id}"))?;
            stdout.trim().parse::<u64>().with_context(|| {
                format!(
                    "parsing database id for #{blocked_on_id}: expected an integer, got {:?}",
                    stdout.trim()
                )
            })?
        };

        let post_cmd = format!(
            "gh api {API_VERSION_HEADER} -X POST -F issue_id={blocked_on_db_id} \
             repos/{{owner}}/{{repo}}/issues/{}/dependencies/blocked_by",
            shell_quote_str(blocked_id),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &post_cmd))
            .with_context(|| {
                format!("linking #{blocked_id} as blocked by #{blocked_on_id}")
            })?;
        Ok(())
    }
}

/// API version header passed to every sub-issues / dependencies
/// `gh api` call. Both endpoints entered general availability under
/// this version; pinning it keeps fleet's behaviour stable when
/// GitHub bumps the default version for unrelated reasons.
const API_VERSION_HEADER: &str = "-H 'X-GitHub-Api-Version: 2026-03-10'";

/// Parse `gh issue list --json …` output into normalised `Issue`s.
/// Pure; exposed for tests.
#[must_use]
pub fn parse_gh_output(stdout: &str) -> Vec<Issue> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let raw: Vec<GhIssue> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut issues: Vec<Issue> = raw.into_iter().map(Into::into).collect();
    sort_open_first(&mut issues);
    issues
}

/// Parse `gh issue view <id> --json …` output into an [`IssueDetail`].
/// Surfaces a parse error rather than silently returning an empty
/// detail, because callers (the bridge `GET /read` route, the agent's
/// `fleet-tracker read`) need to distinguish "no body" from "request
/// failed".
pub fn parse_gh_view_output(stdout: &str) -> Result<IssueDetail> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("`gh issue view` returned no output"));
    }
    let raw: GhView = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing `gh issue view --json …` payload: {trimmed:?}"))?;
    let labels = raw.labels.into_iter().map(|l| l.name).collect();
    let comments = raw
        .comments
        .into_iter()
        .map(|c| Comment {
            author: c.author.login,
            body: c.body,
            created_at: c.created_at,
        })
        .collect();
    Ok(IssueDetail {
        issue: Issue {
            id: format!("gh:{}", raw.number),
            human_id: raw.number.to_string(),
            title: raw.title,
            status: raw.state.to_lowercase(),
            labels,
        },
        body: raw.body,
        comments,
    })
}

/// Parse the issue number out of `gh issue create`'s stdout. gh prints
/// a single line containing the new issue URL, e.g.
/// `https://github.com/owner/repo/issues/42`. The trailing path
/// component after the final `/` is the number.
#[must_use]
#[allow(dead_code)] // Production caller is `Tracker::create`, dead until Phase 2.
pub fn parse_gh_create_output(stdout: &str) -> Option<u64> {
    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    let last_segment = last_line.trim().rsplit('/').next()?;
    last_segment.parse::<u64>().ok()
}

/// Parse the `[{number, ...}, ...]` JSON returned by GitHub's
/// sub-issues and dependencies list endpoints into a flat list of
/// issue numbers. Pure; used by both [`Tracker::link_parent`] and
/// [`Tracker::link_blocks`] for their idempotence checks.
///
/// Tolerant on shape mismatch (returns an empty list) for the same
/// reason [`parse_gh_output`] is: a parse failure on the read side
/// shouldn't block the POST that follows — the POST itself will
/// surface any real auth/repo error with a clearer message than a
/// JSON parse failure on a list response would.
#[must_use]
fn parse_issue_number_list(stdout: &str) -> Vec<u64> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    #[derive(Deserialize)]
    struct Ref {
        number: u64,
    }
    let raw: Vec<Ref> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    raw.into_iter().map(|r| r.number).collect()
}

/// POSIX-shell single-quote escape for an arbitrary string. Matches
/// the helper in `super::shell_quote_path` but takes `&str`.
fn shell_quote_str(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

#[derive(Deserialize)]
struct GhLabel {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct GhView {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    comments: Vec<GhComment>,
}

#[derive(Deserialize)]
struct GhComment {
    #[serde(default)]
    author: GhCommentAuthor,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
}

#[derive(Deserialize, Default)]
struct GhCommentAuthor {
    #[serde(default)]
    login: String,
}

impl From<GhIssue> for Issue {
    fn from(g: GhIssue) -> Self {
        Self {
            id: format!("gh:{}", g.number),
            human_id: g.number.to_string(),
            title: g.title,
            status: g.state.to_lowercase(),
            labels: g.labels.into_iter().map(|l| l.name).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::{always, eq};

    fn invoker_returning(stdout: &'static str) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |_, _| Ok(stdout.to_string()));
        Arc::new(mock)
    }

    /// Assert exactly one `sh -c "cd '<repo>' && <expected>"` against
    /// `MockProcessInvoker`. Stdout for the matched call is `reply`.
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
        let t = GitHubTracker::new(invoker_returning("[]"));
        assert_eq!(t.name(), "github");
    }

    #[test]
    fn list_issues_returns_empty_for_blank_output() {
        let t = GitHubTracker::new(invoker_returning(""));
        assert!(t.list_issues(Path::new("/repo")).unwrap().is_empty());
    }

    #[test]
    fn list_issues_parses_gh_json_and_sorts() {
        let json = r#"[
            {"number":42,"title":"Z","state":"OPEN","labels":[{"name":"bug"}]},
            {"number":1,"title":"A","state":"CLOSED","labels":[]},
            {"number":7,"title":"M","state":"OPEN","labels":[]}
        ]"#;
        let t = GitHubTracker::new(invoker_returning(json));
        let issues = t.list_issues(Path::new("/repo")).unwrap();
        // Open first, then alphabetic by human_id (lexicographic on "42", "7").
        assert_eq!(issues[0].human_id, "42");
        assert_eq!(issues[1].human_id, "7");
        assert_eq!(issues[2].human_id, "1");
        assert_eq!(issues[0].labels, vec!["bug"]);
    }

    #[test]
    fn id_prefix_is_gh() {
        let json = r#"[{"number":42,"title":"x","state":"open","labels":[]}]"#;
        let t = GitHubTracker::new(invoker_returning(json));
        assert_eq!(t.list_issues(Path::new("/r")).unwrap()[0].id, "gh:42");
    }

    #[test]
    fn status_is_normalised_to_lowercase() {
        let json = r#"[{"number":1,"title":"x","state":"OPEN","labels":[]}]"#;
        let t = GitHubTracker::new(invoker_returning(json));
        assert_eq!(t.list_issues(Path::new("/r")).unwrap()[0].status, "open");
    }

    #[test]
    fn invocation_uses_cd_wrapped_shell_at_repo_root() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue list --json number,title,state,labels --limit 200 --state all",
            "[]",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.list_issues(Path::new("/r")).unwrap();
    }

    #[test]
    fn invoker_error_propagates_with_context() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("gh: not found")));
        let t = GitHubTracker::new(Arc::new(mock));
        let err = t.list_issues(Path::new("/r")).unwrap_err();
        assert!(format!("{err:#}").contains("gh issue list"));
    }

    #[test]
    fn malformed_json_yields_empty_list_rather_than_error() {
        let t = GitHubTracker::new(invoker_returning("not json"));
        assert!(t.list_issues(Path::new("/r")).unwrap().is_empty());
    }

    #[test]
    fn empty_array_is_handled_explicitly() {
        let t = GitHubTracker::new(invoker_returning("[]"));
        assert!(t.list_issues(Path::new("/r")).unwrap().is_empty());
    }

    // ---- write methods ----

    #[test]
    fn comment_uses_issue_comment_body_flag() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue comment '42' --body 'hello world'",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.comment(Path::new("/r"), "42", "hello world").unwrap();
    }

    #[test]
    fn comment_body_with_single_quote_is_safely_escaped() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            r"gh issue comment '42' --body 'it'\''s fine'",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.comment(Path::new("/r"), "42", "it's fine").unwrap();
    }

    #[test]
    fn set_status_open_uses_issue_reopen() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/r", "gh issue reopen '42'", "");
        let t = GitHubTracker::new(Arc::new(mock));
        t.set_status(Path::new("/r"), "42", Status::Open).unwrap();
    }

    #[test]
    fn set_status_closed_uses_issue_close() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/r", "gh issue close '42'", "");
        let t = GitHubTracker::new(Arc::new(mock));
        t.set_status(Path::new("/r"), "42", Status::Closed).unwrap();
    }

    #[test]
    fn set_status_in_progress_adds_in_progress_label() {
        // Mirrors git-bug's fallback so the workflow YAML doesn't have
        // to fork on tracker type.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue edit '42' --add-label in-progress",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.set_status(Path::new("/r"), "42", Status::InProgress)
            .unwrap();
    }

    #[test]
    fn add_label_uses_issue_edit_add_label() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/r", "gh issue edit '42' --add-label 'bug'", "");
        let t = GitHubTracker::new(Arc::new(mock));
        t.add_label(Path::new("/r"), "42", "bug").unwrap();
    }

    #[test]
    fn remove_label_uses_issue_edit_remove_label() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue edit '42' --remove-label 'bug'",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.remove_label(Path::new("/r"), "42", "bug").unwrap();
    }

    #[test]
    fn create_without_labels_parses_url_for_new_number() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue create --title 'new title' --body 'body'",
            "https://github.com/owner/repo/issues/57\n",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let issue = t.create(Path::new("/r"), "new title", "body", &[]).unwrap();
        assert_eq!(issue.human_id, "57");
        assert_eq!(issue.id, "gh:57");
        assert_eq!(issue.title, "new title");
        assert_eq!(issue.status, "open");
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn create_with_labels_appends_one_label_flag_per_label_in_order() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue create --title 't' --body 'b' --label 'bug' --label 'needs-review'",
            "https://github.com/o/r/issues/3\n",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let issue = t
            .create(
                Path::new("/r"),
                "t",
                "b",
                &["bug".to_string(), "needs-review".to_string()],
            )
            .unwrap();
        assert_eq!(issue.human_id, "3");
        assert_eq!(issue.labels, vec!["bug", "needs-review"]);
    }

    #[test]
    fn create_errors_when_stdout_has_no_url_with_a_trailing_number() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue create --title 't' --body 'b'",
            "Creating issue in owner/repo\n",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let err = t.create(Path::new("/r"), "t", "b", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("did not print a recognisable URL"));
    }

    #[test]
    fn read_uses_issue_view_with_full_json_field_list() {
        let payload = r#"{
            "number": 42,
            "title": "fix parser",
            "state": "OPEN",
            "body": "long description",
            "labels": [{"name": "bug"}],
            "comments": [
                {"author": {"login": "alice"}, "body": "first take", "createdAt": "2026-05-15T10:00:00Z"}
            ]
        }"#;
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue view '42' --json number,title,state,labels,body,comments",
            payload,
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let detail = t.read(Path::new("/r"), "42").unwrap();
        assert_eq!(detail.issue.id, "gh:42");
        assert_eq!(detail.issue.human_id, "42");
        assert_eq!(detail.issue.title, "fix parser");
        assert_eq!(detail.issue.status, "open");
        assert_eq!(detail.issue.labels, vec!["bug"]);
        assert_eq!(detail.body, "long description");
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].author, "alice");
        assert_eq!(detail.comments[0].body, "first take");
        assert_eq!(detail.comments[0].created_at, "2026-05-15T10:00:00Z");
    }

    #[test]
    fn link_parent_lists_sub_issues_then_resolves_database_id_then_posts() {
        // Three shellouts, in order: list (returns empty array so the
        // child is not yet linked), GET the child to resolve its
        // database id, POST the link. Each `expect_shell_cmd` matches
        // exactly one invocation; the `MockProcessInvoker` framework
        // fails the test if any extra calls fire.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            "[]",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'57' --jq .id",
            "9876543210\n",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' -X POST -F sub_issue_id=9876543210 \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/r"), "100", "57").unwrap();
    }

    #[test]
    fn link_parent_is_idempotent_when_child_already_linked() {
        // The list call returns the child already on the parent — no
        // database-id lookup or POST should fire. Only one
        // `expect_shell_cmd` is set up; if `link_parent` does a second
        // invocation the mock framework fails the test.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            r#"[{"number": 57, "id": 9876543210}]"#,
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/r"), "100", "57").unwrap();
    }

    #[test]
    fn link_parent_idempotence_is_whole_number_match_not_substring() {
        // Parent already has #43 as a sub-issue; linking #4 must
        // proceed (resolve id + POST), not short-circuit. The mock
        // framework fails the test if the resolve / post calls
        // don't fire — proving the substring trap is avoided.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            r#"[{"number": 43}]"#,
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'4' --jq .id",
            "1111111\n",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' -X POST -F sub_issue_id=1111111 \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/r"), "100", "4").unwrap();
    }

    #[test]
    fn link_parent_surfaces_a_non_integer_database_id_as_a_typed_error() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'100'/sub_issues",
            "[]",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'57' --jq .id",
            "not-a-number\n",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let err = t.link_parent(Path::new("/r"), "100", "57").unwrap_err();
        assert!(
            format!("{err:#}").contains("expected an integer"),
            "got: {err:#}"
        );
    }

    #[test]
    fn link_blocks_lists_dependencies_then_resolves_id_then_posts() {
        // Same three-shellout pattern as `link_parent` but against the
        // `/dependencies/blocked_by` endpoints. POST body uses
        // `issue_id` (not `sub_issue_id`).
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'42'/dependencies/blocked_by",
            "[]",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'43' --jq .id",
            "5550000\n",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' -X POST -F issue_id=5550000 \
             repos/{owner}/{repo}/issues/'42'/dependencies/blocked_by",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_blocks(Path::new("/r"), "42", "43").unwrap();
    }

    #[test]
    fn link_blocks_is_idempotent_when_dependency_already_recorded() {
        // The list response already contains the proposed blocked_on
        // ticket; no resolve / POST should fire.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'42'/dependencies/blocked_by",
            r#"[{"number": 43, "id": 5550000}]"#,
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_blocks(Path::new("/r"), "42", "43").unwrap();
    }

    #[test]
    fn link_blocks_surfaces_a_non_integer_database_id_as_a_typed_error() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'42'/dependencies/blocked_by",
            "[]",
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh api -H 'X-GitHub-Api-Version: 2026-03-10' \
             repos/{owner}/{repo}/issues/'43' --jq .id",
            "not-a-number\n",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        let err = t.link_blocks(Path::new("/r"), "42", "43").unwrap_err();
        assert!(
            format!("{err:#}").contains("expected an integer"),
            "got: {err:#}"
        );
    }

    // ---- pure helpers ----

    #[test]
    fn parse_gh_create_output_extracts_number_from_url() {
        assert_eq!(
            parse_gh_create_output("https://github.com/owner/repo/issues/42\n"),
            Some(42)
        );
        // Multi-line: the URL is the last non-empty line.
        assert_eq!(
            parse_gh_create_output(
                "Creating issue in owner/repo\n\nhttps://github.com/owner/repo/issues/7\n"
            ),
            Some(7)
        );
        // No trailing number → None.
        assert_eq!(
            parse_gh_create_output("https://github.com/owner/repo/issues/\n"),
            None
        );
        assert_eq!(parse_gh_create_output(""), None);
    }

    #[test]
    fn parse_issue_number_list_extracts_issue_numbers_from_list_response() {
        let json = r#"[
            {"number": 57, "id": 9876, "title": "child A"},
            {"number": 91, "id": 1234, "title": "child B"}
        ]"#;
        assert_eq!(parse_issue_number_list(json), vec![57, 91]);
    }

    #[test]
    fn parse_issue_number_list_returns_empty_for_empty_array() {
        assert_eq!(parse_issue_number_list("[]"), Vec::<u64>::new());
    }

    #[test]
    fn parse_issue_number_list_returns_empty_on_blank_or_unparseable_output() {
        assert_eq!(parse_issue_number_list(""), Vec::<u64>::new());
        assert_eq!(parse_issue_number_list("not json"), Vec::<u64>::new());
    }
}
