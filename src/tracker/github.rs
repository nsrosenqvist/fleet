//! Host-side GitHub tracker.
//!
//! Shells `gh issue list --json …` directly on the host through a
//! [`ProcessInvoker`]. Auth is the user's `gh auth login` — fleet
//! doesn't manage it. The host's `~/.config/gh/hosts.yml` is the only
//! configuration consulted; no VM round-trip.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use super::{Issue, Tracker, sort_open_first};
use crate::process::ProcessInvoker;

pub struct GitHubTracker {
    invoker: Arc<dyn ProcessInvoker>,
}

impl GitHubTracker {
    pub const fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { invoker }
    }
}

impl Tracker for GitHubTracker {
    fn name(&self) -> &'static str {
        "github"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        // gh auto-detects the repo from the cwd's git remote, so we cd
        // into the repo before invoking.
        let cmd = format!(
            "cd {} && gh issue list --json number,title,state,labels --limit 200 --state all",
            super::shell_quote_path(repo_root)
        );
        let stdout = self
            .invoker
            .run("sh", vec!["-c".to_string(), cmd])
            .context("invoking `gh issue list`")?;
        Ok(parse_gh_output(&stdout))
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
        mock.expect_run()
            .with(
                eq("sh"),
                eq(vec![
                    "-c".to_string(),
                    "cd '/r' && gh issue list --json number,title,state,labels --limit 200 --state all"
                        .to_string(),
                ]),
            )
            .returning(|_, _| Ok("[]".to_string()));
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
}
