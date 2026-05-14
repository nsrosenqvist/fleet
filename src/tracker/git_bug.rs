//! Host-side `git-bug` tracker.
//!
//! Shells `git-bug bug --format json` directly on the host through a
//! [`ProcessInvoker`]. `git-bug` is project-local — the binary reads
//! the project's git history — so no auth handling is needed; the
//! installation hint lives in `fleet runtime doctor` (next chunk).
//!
//! Defensive parsing matches the AO-era tracker's quirks: `git-bug`
//! emits `null` (not `[]`) for labels-less issues, and may print an
//! empty body or the literal `null` for an empty repo.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use super::{Issue, Tracker, sort_open_first};
use crate::process::ProcessInvoker;

pub struct GitBugTracker {
    invoker: Arc<dyn ProcessInvoker>,
}

impl GitBugTracker {
    pub const fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { invoker }
    }
}

impl Tracker for GitBugTracker {
    fn name(&self) -> &'static str {
        "git-bug"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        // `git-bug` operates on the cwd's git repo. We shell through
        // `sh -c "cd <repo> && git-bug bug --format json"` because the
        // invoker abstraction is cwd-agnostic. The cd-wrapping mirrors
        // what `LocalAdapter::exec` does — same shell-escape rules.
        let cmd = format!(
            "cd {} && git-bug bug --format json",
            super::shell_quote_path(repo_root)
        );
        let stdout = self
            .invoker
            .run("sh", vec!["-c".to_string(), cmd])
            .context("invoking `git-bug bug --format json`")?;
        Ok(parse_git_bug_output(&stdout))
    }
}

/// Parse `git-bug bug --format json` output into normalised `Issue`s.
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

/// `git-bug bug --format json` row (subset). Same defensive `null →
/// default` treatment as the AO-era impl — git-bug emits `null` (not
/// `[]`) for the labels field on issues with no labels.
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
        // Reproduces the AO-era regression: git-bug emits `labels: null`,
        // not `[]`, for issues with no labels. Must not fail parsing.
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
    fn invocation_uses_cd_wrapped_shell_command_at_repo_root() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("sh"),
                eq(vec![
                    "-c".to_string(),
                    "cd '/some/repo' && git-bug bug --format json".to_string(),
                ]),
            )
            .returning(|_, _| Ok("null".to_string()));
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
        assert!(msg.contains("git-bug bug"), "msg = {msg}");
    }

    #[test]
    fn malformed_json_yields_empty_list_rather_than_error() {
        // Conservative: parser tolerance. A future user might invoke
        // fleet against a git-bug release that changes its JSON shape;
        // surfacing "no issues" beats hard-failing the picker.
        let t = GitBugTracker::new(invoker_returning("not json at all"));
        assert!(t.list_issues(Path::new("/repo")).unwrap().is_empty());
    }
}
