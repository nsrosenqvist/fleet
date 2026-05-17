//! Host-side `git-bug` tracker.
//!
//! Shells `git-bug` directly on the host through a [`ProcessInvoker`].
//! `git-bug` is project-local — the binary reads the project's git
//! history — so no auth handling is needed. The user installs
//! `git-bug` themselves (e.g. `brew install git-bug`); a missing
//! binary surfaces as the invoker's error on first `fleet issues
//! list`.
//!
//! Argv shapes follow the real CLI (git-bug ≥ 0.10): top-level verbs
//! like `add`, `comment`, `status`, `label`, `show`, `ls` — there is
//! no `bug` infix despite what some older docs (and an earlier
//! version of this codebase) claimed. Mutating commands pass
//! `--non-interactive` so they never spawn an editor and hang the
//! workflow.
//!
//! Defensive parsing handles `git-bug`'s quirks: it emits `null`
//! (not `[]`) for labels-less issues, and may print an empty body
//! or the literal `null` for an empty repo. The body of a ticket is
//! the first comment in the data model — `read` lifts it out into
//! `IssueDetail::body` so the agent-facing shape matches the
//! GitHub-style "body + comments" mental model.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use super::{Comment, Issue, IssueDetail, Status, Tracker, sort_open_first};
use crate::process::ProcessInvoker;

pub struct GitBugTracker {
    invoker: Arc<dyn ProcessInvoker>,
}

impl GitBugTracker {
    pub const fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { invoker }
    }

    /// Build `sh -c "cd <repo> && <cmd>"`. The cd-wrapping mirrors
    /// `LocalAdapter::exec`'s shape — git-bug operates on the cwd's git
    /// repo, but the invoker abstraction is cwd-agnostic, so every
    /// invocation enters the repo first.
    fn shell_cmd(repo_root: &Path, cmd: &str) -> Vec<String> {
        let wrapped = format!("cd {} && {cmd}", super::shell_quote_path(repo_root));
        vec!["-c".to_string(), wrapped]
    }
}

impl Tracker for GitBugTracker {
    fn name(&self) -> &'static str {
        "git-bug"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        let stdout = self
            .invoker
            .run(
                "sh",
                Self::shell_cmd(repo_root, "git-bug ls --format json --status open,closed"),
            )
            .context("invoking `git-bug ls --format json`")?;
        Ok(parse_git_bug_output(&stdout))
    }

    fn read(&self, repo_root: &Path, issue_id: &str) -> Result<IssueDetail> {
        let stdout = self
            .invoker
            .run(
                "sh",
                Self::shell_cmd(
                    repo_root,
                    &format!("git-bug show {} --format json", shell_quote_str(issue_id)),
                ),
            )
            .with_context(|| format!("invoking `git-bug show {issue_id} --format json`"))?;
        parse_git_bug_show_output(&stdout)
    }

    fn comment(&self, repo_root: &Path, issue_id: &str, body: &str) -> Result<()> {
        let cmd = format!(
            "git-bug comment add --non-interactive {} -m {}",
            shell_quote_str(issue_id),
            shell_quote_str(body),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `git-bug comment add {issue_id}`"))?;
        Ok(())
    }

    fn set_status(&self, repo_root: &Path, issue_id: &str, status: Status) -> Result<()> {
        // git-bug has first-class `status open` and `status close` verbs.
        // It has no first-class "in-progress" state; the v1 convention
        // (matching the gh side) is an `in-progress` label.
        let cmd = match status {
            Status::Open => format!("git-bug status open {}", shell_quote_str(issue_id)),
            Status::Closed => format!("git-bug status close {}", shell_quote_str(issue_id)),
            Status::InProgress => {
                format!(
                    "git-bug label add {} in-progress",
                    shell_quote_str(issue_id)
                )
            }
        };
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `git-bug status/label` for {issue_id}"))?;
        Ok(())
    }

    fn add_label(&self, repo_root: &Path, issue_id: &str, label: &str) -> Result<()> {
        let cmd = format!(
            "git-bug label add {} {}",
            shell_quote_str(issue_id),
            shell_quote_str(label),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `git-bug label add {issue_id} {label}`"))?;
        Ok(())
    }

    fn remove_label(&self, repo_root: &Path, issue_id: &str, label: &str) -> Result<()> {
        let cmd = format!(
            "git-bug label rm {} {}",
            shell_quote_str(issue_id),
            shell_quote_str(label),
        );
        self.invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `git-bug label rm {issue_id} {label}`"))?;
        Ok(())
    }

    fn create(
        &self,
        repo_root: &Path,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<Issue> {
        let cmd = format!(
            "git-bug add --non-interactive --title {} --message {}",
            shell_quote_str(title),
            shell_quote_str(body),
        );
        let stdout = self
            .invoker
            .run("sh", Self::shell_cmd(repo_root, &cmd))
            .with_context(|| format!("invoking `git-bug add --title {title}`"))?;
        let new_human_id = parse_git_bug_add_output(&stdout).ok_or_else(|| {
            anyhow!("`git-bug add` did not print a recognisable id; stdout: {stdout:?}")
        })?;
        // Labels arrive after creation because `git-bug add` doesn't
        // take `--label`. Each application is idempotent so a partial
        // failure can be retried by re-running create with the same
        // labels — but that's a future concern; v1 surfaces the first
        // error.
        for label in labels {
            self.add_label(repo_root, &new_human_id, label)?;
        }
        Ok(Issue {
            id: new_human_id.clone(),
            human_id: new_human_id,
            title: title.to_string(),
            status: "open".to_string(),
            labels: labels.to_vec(),
        })
    }

    fn link_parent(&self, repo_root: &Path, parent_id: &str, child_id: &str) -> Result<()> {
        // git-bug has no native parent edge; the v1 convention is a
        // `parent:<id>` label on the child. Symmetric with what other
        // git-bug-using projects (and the orchestrator agent's epic
        // recreation in Phase 5) expect.
        let label = format!("parent:{parent_id}");
        self.add_label(repo_root, child_id, &label)
    }

    fn link_blocks(
        &self,
        repo_root: &Path,
        blocked_id: &str,
        blocked_on_id: &str,
    ) -> Result<()> {
        // git-bug has no native blocks/blocked-by edge; mirror
        // `link_parent`'s label convention with a symmetric pair so
        // either side of the relationship is queryable via
        // `git-bug ls --label`. `add_label` is idempotent on git-bug,
        // so this call is safe to re-run.
        self.add_label(repo_root, blocked_id, &format!("blocked-by:{blocked_on_id}"))?;
        self.add_label(repo_root, blocked_on_id, &format!("blocks:{blocked_id}"))?;
        Ok(())
    }
}

/// Parse `git-bug ls --format json` output into normalised `Issue`s.
/// Pure; exposed for unit tests that feed canned JSON without invoking
/// `git-bug`.
#[must_use]
pub fn parse_git_bug_output(stdout: &str) -> Vec<Issue> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Vec::new();
    }
    let raw: Vec<GitBugIssue> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut issues: Vec<Issue> = raw.into_iter().map(Into::into).collect();
    sort_open_first(&mut issues);
    issues
}

/// Parse `git-bug show <id> --format json` output into an
/// [`IssueDetail`]. The body is lifted out of the first comment (git-bug
/// stores the ticket description as comment-zero); the remaining
/// comments are surfaced verbatim.
///
/// `created_at` on each `Comment` is empty because git-bug's show JSON
/// doesn't expose per-comment timestamps. A future git-bug release or
/// a separate `--field` traversal could fill them in; v1 leaves them
/// empty so the data model is honest about what's available.
pub fn parse_git_bug_show_output(stdout: &str) -> Result<IssueDetail> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("`git-bug show` returned no output"));
    }
    let raw: GitBugShow = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing `git-bug show --format json` payload: {trimmed:?}"))?;
    let (body, rest_comments) = match raw.comments.split_first() {
        Some((first, rest)) => (first.message.clone(), rest.to_vec()),
        None => (String::new(), Vec::new()),
    };
    let comments = rest_comments
        .into_iter()
        .map(|c| Comment {
            author: c.author.name,
            body: c.message,
            created_at: String::new(),
        })
        .collect();
    let labels = raw.labels.unwrap_or_default();
    Ok(IssueDetail {
        issue: Issue {
            id: raw.id,
            human_id: raw.human_id,
            title: raw.title,
            status: raw.status.to_lowercase(),
            labels,
        },
        body,
        comments,
    })
}

/// Parse `git-bug add`'s stdout for the new bug's short id.
/// Output shape: `<human_id> created\n`. Returns `None` for any
/// shape that doesn't have a non-empty first whitespace-separated
/// token, so callers can produce their own error.
#[must_use]
#[allow(dead_code)] // Production caller is `Tracker::create`, dead until Phase 2.
pub fn parse_git_bug_add_output(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// POSIX-shell single-quote escape for an arbitrary string. Matches the
/// `shell_quote_path` helper in this module's parent — duplicated rather
/// than refactored because the parent's takes `&Path`, not `&str`.
fn shell_quote_str(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// `git-bug ls --format json` row (subset). Every field uses
/// `null_as_default` because git-bug emits `null` (not `[]`/`""`)
/// for missing values — naive serde derive would refuse to parse.
#[derive(Deserialize)]
struct GitBugIssue {
    id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    human_id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    title: String,
    #[serde(default, deserialize_with = "null_as_default")]
    status: String,
    #[serde(default, deserialize_with = "null_as_default")]
    labels: Vec<String>,
}

/// `git-bug show --format json` payload (subset). git-bug models the
/// ticket body as the first comment, so `body` is derived in
/// [`parse_git_bug_show_output`] rather than mapped from a field here.
#[derive(Deserialize)]
struct GitBugShow {
    id: String,
    #[serde(default)]
    human_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    /// `null` for labels-less issues; kept as `Option` so callers
    /// substitute `Vec::new` without leaning on `null_as_default`.
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    comments: Vec<GitBugShowComment>,
}

#[derive(Deserialize, Clone)]
struct GitBugShowComment {
    #[serde(default)]
    author: GitBugShowAuthor,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize, Default, Clone)]
struct GitBugShowAuthor {
    #[serde(default)]
    name: String,
}

fn null_as_default<'de, D, T>(de: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Option::unwrap_or_default)
}

impl From<GitBugIssue> for Issue {
    fn from(g: GitBugIssue) -> Self {
        Self {
            id: g.id,
            human_id: g.human_id,
            title: g.title,
            status: g.status.to_lowercase(),
            labels: g.labels,
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

    /// Assert exactly one `sh -c "cd '<repo>' && <expected>"` invocation
    /// against `MockProcessInvoker`. Returns the mock so callers can
    /// chain additional invocations. Stdout for the matched call is
    /// `reply`.
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
    fn name_is_git_bug() {
        let t = GitBugTracker::new(invoker_returning(""));
        assert_eq!(t.name(), "git-bug");
    }

    #[test]
    fn list_issues_returns_empty_for_empty_repo() {
        let t = GitBugTracker::new(invoker_returning("null"));
        assert!(t.list_issues(Path::new("/repo")).unwrap().is_empty());
    }

    #[test]
    fn list_issues_returns_empty_for_blank_output() {
        let t = GitBugTracker::new(invoker_returning(""));
        assert!(t.list_issues(Path::new("/repo")).unwrap().is_empty());
    }

    #[test]
    fn list_issues_parses_array_and_sorts_open_first() {
        let json = r#"[
            {"id":"a","human_id":"a1","title":"X","status":"closed","labels":[]},
            {"id":"b","human_id":"b9","title":"Y","status":"open","labels":[]},
            {"id":"c","human_id":"c0","title":"Z","status":"open","labels":["bug"]}
        ]"#;
        let t = GitBugTracker::new(invoker_returning(json));
        let issues = t.list_issues(Path::new("/repo")).unwrap();
        // Open entries first (c0, b9), then closed (a1).
        assert_eq!(issues[0].human_id, "b9");
        assert_eq!(issues[1].human_id, "c0");
        assert_eq!(issues[2].human_id, "a1");
        assert_eq!(issues[1].labels, vec!["bug"]);
    }

    #[test]
    fn list_issues_tolerates_null_labels() {
        // git-bug emits `labels: null`, not `[]`, for issues with no
        // labels — naive serde derive refuses to parse. Lock the
        // tolerance so a future serde upgrade doesn't quietly break it.
        let json = r#"[{"id":"a","human_id":"a1","title":"X","status":"open","labels":null}]"#;
        let t = GitBugTracker::new(invoker_returning(json));
        let issues = t.list_issues(Path::new("/repo")).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].labels.is_empty());
    }

    #[test]
    fn status_is_normalised_to_lowercase() {
        let json = r#"[{"id":"a","human_id":"a1","title":"X","status":"OPEN","labels":[]}]"#;
        let t = GitBugTracker::new(invoker_returning(json));
        let issues = t.list_issues(Path::new("/repo")).unwrap();
        assert_eq!(issues[0].status, "open");
    }

    #[test]
    fn list_issues_invocation_uses_real_ls_verb_at_repo_root() {
        // Pin the real-CLI shape: `ls`, not `bug`. An earlier version
        // of this file used `git-bug bug --format json`; that verb
        // doesn't exist in the released CLI and would fail at runtime.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/some/repo",
            "git-bug ls --format json --status open,closed",
            "null",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        t.list_issues(Path::new("/some/repo")).unwrap();
    }

    #[test]
    fn invoker_error_propagates_with_context() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("git-bug not found")));
        let t = GitBugTracker::new(Arc::new(mock));
        let err = t.list_issues(Path::new("/repo")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("git-bug ls"), "msg = {msg}");
    }

    #[test]
    fn malformed_json_yields_empty_list_rather_than_error() {
        // Conservative: parser tolerance. A future user might invoke
        // fleet against a git-bug release that changes its JSON shape;
        // surfacing "no issues" beats hard-failing the picker.
        let t = GitBugTracker::new(invoker_returning("not json at all"));
        assert!(t.list_issues(Path::new("/repo")).unwrap().is_empty());
    }

    // ---- write methods ----

    #[test]
    fn comment_shells_comment_add_with_non_interactive_and_message() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug comment add --non-interactive 'abc123' -m 'hello world'",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        t.comment(Path::new("/repo"), "abc123", "hello world")
            .unwrap();
    }

    #[test]
    fn comment_with_single_quote_in_body_is_safely_escaped() {
        // Single quotes inside the body would close the POSIX-shell
        // single-quoted region; the escape sequence `'\''` reopens it
        // safely. Lock the rendering so a future refactor of
        // `shell_quote_str` can't break it.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            r"git-bug comment add --non-interactive 'id' -m 'it'\''s fine'",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        t.comment(Path::new("/repo"), "id", "it's fine").unwrap();
    }

    #[test]
    fn set_status_open_uses_status_open_verb() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/repo", "git-bug status open 'abc'", "");
        let t = GitBugTracker::new(Arc::new(mock));
        t.set_status(Path::new("/repo"), "abc", Status::Open)
            .unwrap();
    }

    #[test]
    fn set_status_closed_uses_status_close_verb() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/repo", "git-bug status close 'abc'", "");
        let t = GitBugTracker::new(Arc::new(mock));
        t.set_status(Path::new("/repo"), "abc", Status::Closed)
            .unwrap();
    }

    #[test]
    fn set_status_in_progress_falls_back_to_label_convention() {
        // git-bug has no native "in-progress" status; the convention
        // matches gh's `in-progress` label so workflow YAML doesn't
        // have to fork on tracker type.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug label add 'abc' in-progress",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        t.set_status(Path::new("/repo"), "abc", Status::InProgress)
            .unwrap();
    }

    #[test]
    fn add_label_shells_label_add() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/repo", "git-bug label add 'abc' 'bug'", "");
        let t = GitBugTracker::new(Arc::new(mock));
        t.add_label(Path::new("/repo"), "abc", "bug").unwrap();
    }

    #[test]
    fn remove_label_shells_label_rm() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(&mut mock, "/repo", "git-bug label rm 'abc' 'bug'", "");
        let t = GitBugTracker::new(Arc::new(mock));
        t.remove_label(Path::new("/repo"), "abc", "bug").unwrap();
    }

    #[test]
    fn link_parent_attaches_parent_label_to_child() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug label add 'child123' 'parent:parent456'",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        t.link_parent(Path::new("/repo"), "parent456", "child123")
            .unwrap();
    }

    #[test]
    fn link_blocks_attaches_a_symmetric_label_pair() {
        // `blocked-by:<blocked_on>` on the blocked side and
        // `blocks:<blocked>` on the blocked_on side — two label-add
        // shellouts in that order. The mock framework fails the test
        // if either is missing or out of order.
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug label add 'a' 'blocked-by:b'",
            "",
        );
        expect_shell_cmd(&mut mock, "/repo", "git-bug label add 'b' 'blocks:a'", "");
        let t = GitBugTracker::new(Arc::new(mock));
        t.link_blocks(Path::new("/repo"), "a", "b").unwrap();
    }

    #[test]
    fn create_shells_add_then_returns_parsed_new_id() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug add --non-interactive --title 'new title' --message 'body'",
            "abc123 created\n",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        let issue = t
            .create(Path::new("/repo"), "new title", "body", &[])
            .unwrap();
        assert_eq!(issue.human_id, "abc123");
        assert_eq!(issue.id, "abc123");
        assert_eq!(issue.title, "new title");
        assert_eq!(issue.status, "open");
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn create_with_labels_invokes_label_add_per_label() {
        let mut mock = MockProcessInvoker::new();
        // First: the add itself.
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug add --non-interactive --title 't' --message 'b'",
            "newid created\n",
        );
        // Then one label-add per label, in input order.
        expect_shell_cmd(&mut mock, "/repo", "git-bug label add 'newid' 'bug'", "");
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug label add 'newid' 'needs-review'",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        let issue = t
            .create(
                Path::new("/repo"),
                "t",
                "b",
                &["bug".to_string(), "needs-review".to_string()],
            )
            .unwrap();
        assert_eq!(issue.labels, vec!["bug", "needs-review"]);
    }

    #[test]
    fn create_errors_when_add_stdout_is_unrecognisable() {
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug add --non-interactive --title 't' --message 'b'",
            "",
        );
        let t = GitBugTracker::new(Arc::new(mock));
        let err = t.create(Path::new("/repo"), "t", "b", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("did not print a recognisable id"));
    }

    #[test]
    fn read_shells_show_with_format_json() {
        let payload = r#"{
            "id": "fullhash",
            "human_id": "abc123",
            "title": "T",
            "status": "open",
            "labels": null,
            "comments": [
                {"author": {"name": "alice"}, "message": "the body"},
                {"author": {"name": "bob"}, "message": "a reply"}
            ]
        }"#;
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug show 'abc123' --format json",
            payload,
        );
        let t = GitBugTracker::new(Arc::new(mock));
        let detail = t.read(Path::new("/repo"), "abc123").unwrap();
        assert_eq!(detail.issue.human_id, "abc123");
        assert_eq!(detail.issue.title, "T");
        assert_eq!(detail.issue.status, "open");
        assert!(detail.issue.labels.is_empty());
        // First comment becomes the body in git-bug's model.
        assert_eq!(detail.body, "the body");
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].author, "bob");
        assert_eq!(detail.comments[0].body, "a reply");
        // git-bug's show JSON doesn't expose per-comment timestamps;
        // surfacing an empty string is honest about what's available.
        assert_eq!(detail.comments[0].created_at, "");
    }

    #[test]
    fn read_handles_issue_with_no_comments_by_returning_empty_body() {
        let payload = r#"{
            "id": "fullhash",
            "human_id": "abc",
            "title": "T",
            "status": "open",
            "labels": null,
            "comments": []
        }"#;
        let mut mock = MockProcessInvoker::new();
        expect_shell_cmd(
            &mut mock,
            "/repo",
            "git-bug show 'abc' --format json",
            payload,
        );
        let t = GitBugTracker::new(Arc::new(mock));
        let detail = t.read(Path::new("/repo"), "abc").unwrap();
        assert_eq!(detail.body, "");
        assert!(detail.comments.is_empty());
    }

    #[test]
    fn parse_git_bug_add_output_extracts_first_token() {
        assert_eq!(
            parse_git_bug_add_output("abc123 created\n"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_git_bug_add_output("  hash  created\n"),
            Some("hash".to_string())
        );
        assert_eq!(parse_git_bug_add_output(""), None);
        assert_eq!(parse_git_bug_add_output("   "), None);
    }
}
