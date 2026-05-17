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
        // GitHub has no native parent/child edge; the v1 convention is
        // a task-list line on the parent's body so the relationship
        // surfaces in GitHub's "Tasklists" UI and trackers like the
        // orchestrator agent can recreate it from the canonical source.
        //
        // Read-modify-write via gh: read the parent's body, append a
        // `- [ ] #<child>` line if not already present, write back.
        // Two shellouts because gh has no "append-to-body" verb.
        let view_cmd = format!("gh issue view {} --json body", shell_quote_str(parent_id));
        let body_json = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &view_cmd))
            .with_context(|| format!("invoking `gh issue view {parent_id} --json body`"))?;
        let current = parse_gh_body_field(&body_json).with_context(|| {
            format!("parsing body field from `gh issue view {parent_id}` output")
        })?;
        let new_body = append_task_list_line(&current, child_id);
        let edit_cmd = format!(
            "gh issue edit {} --body {}",
            shell_quote_str(parent_id),
            shell_quote_str(&new_body),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &edit_cmd))
            .with_context(|| format!("invoking `gh issue edit {parent_id} --body`"))?;
        Ok(())
    }
}

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

/// Parse the `body` field out of a `gh issue view --json body` payload.
/// Returns the body as a String, or an error when the payload doesn't
/// shape-match (missing field, malformed JSON, …).
#[allow(dead_code)] // Caller is `link_parent`, dead until Phase 2's tracker-create.
fn parse_gh_body_field(stdout: &str) -> Result<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("`gh issue view --json body` returned no output"));
    }
    #[derive(Deserialize)]
    struct BodyOnly {
        #[serde(default)]
        body: String,
    }
    let raw: BodyOnly = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing body-only payload: {trimmed:?}"))?;
    Ok(raw.body)
}

/// Append `- [ ] #<child_id>` to `current` if it isn't already
/// referenced. Idempotent: re-linking the same child is a no-op. A
/// trailing newline is normalised so the appended line always lives
/// on its own row even if the parent's body didn't end in `\n`.
#[must_use]
#[allow(dead_code)] // Caller is `link_parent`, dead until Phase 2's tracker-create.
fn append_task_list_line(current: &str, child_id: &str) -> String {
    let needle = format!("#{child_id}");
    // Whole-token match — a body that mentions `#43` shouldn't be
    // treated as already linking `#4`. lines() splits on `\n`/`\r\n`;
    // for each line, split_whitespace gives us tokens we can compare.
    let already_linked = current
        .lines()
        .any(|line| line.split_whitespace().any(|tok| tok == needle));
    if already_linked {
        return current.to_string();
    }
    let mut out = current.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("- [ ] ");
    out.push_str(&needle);
    out.push('\n');
    out
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
    fn link_parent_reads_then_appends_task_list_line_and_writes_back() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue view '100' --json body",
            r#"{"body": "Parent description"}"#,
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue edit '100' --body 'Parent description\n- [ ] #57\n'",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/r"), "100", "57").unwrap();
    }

    #[test]
    fn link_parent_is_idempotent_when_child_already_referenced() {
        // No second shellout should fire because the body already
        // contains `#57`. MockProcessInvoker would fail the test if a
        // second call appeared without an expect_shell_cmd for it.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue view '100' --json body",
            r#"{"body": "Existing: - [x] #57 done"}"#,
        );
        expect_shell_cmd(
            &mut mock,
            "/r",
            "gh issue edit '100' --body 'Existing: - [x] #57 done'",
            "",
        );
        let t = GitHubTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/r"), "100", "57").unwrap();
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
    fn append_task_list_line_appends_when_absent() {
        assert_eq!(
            append_task_list_line("Old body", "42"),
            "Old body\n- [ ] #42\n"
        );
        // Trailing newline preserved.
        assert_eq!(
            append_task_list_line("Old body\n", "42"),
            "Old body\n- [ ] #42\n"
        );
    }

    #[test]
    fn append_task_list_line_is_idempotent_when_present() {
        assert_eq!(
            append_task_list_line("- [ ] #42\nother stuff", "42"),
            "- [ ] #42\nother stuff"
        );
    }

    #[test]
    fn append_task_list_line_does_not_match_partial_number() {
        // A body that mentions `#43` shouldn't be considered to
        // already link `#4`.
        let body = append_task_list_line("references #43", "4");
        assert!(body.ends_with("- [ ] #4\n"), "{body}");
    }

    #[test]
    fn append_task_list_line_handles_empty_body() {
        assert_eq!(append_task_list_line("", "42"), "- [ ] #42\n");
    }
}
