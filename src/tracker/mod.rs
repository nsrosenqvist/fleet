//! Host-side issue trackers.
//!
//! [`GitBugTracker`] and [`GitHubTracker`] shell directly to `git-bug` /
//! `gh` through a [`ProcessInvoker`](crate::process::ProcessInvoker) so
//! both are testable without the real binaries installed.
//!
//! Trackers **read** — they list issues. Closing / commenting is the
//! workflow engine's job, so the trait stays narrow.

pub mod git_bug;
pub mod github;

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::process::ProcessInvoker;
use crate::repo_config::Tracker as TrackerChoice;

pub use git_bug::GitBugTracker;
pub use github::GitHubTracker;

/// Uniform issue shape across plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable opaque id (full hash for git-bug, `"gh:<number>"` for
    /// GitHub).
    pub id: String,
    /// What the user sees and what gets passed to spawn commands.
    pub human_id: String,
    pub title: String,
    /// Normalised lowercase status: `open` / `closed`.
    pub status: String,
    pub labels: Vec<String>,
}

impl Issue {
    /// Substring match for picker-style filtering. Case-insensitive on
    /// both id and title. Empty filter matches everything. Reserved
    /// for the future TUI spawn picker; no current binary caller.
    #[must_use]
    #[allow(dead_code)]
    pub fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let f = filter.to_lowercase();
        self.human_id.to_lowercase().contains(&f) || self.title.to_lowercase().contains(&f)
    }
}

/// Plugin-agnostic tracker query. `Send + Sync` because callers may
/// dispatch into a worker thread for async UX.
pub trait Tracker: Send + Sync {
    fn name(&self) -> &'static str;
    /// Issues for the project rooted at `repo_root`. Sorted with open
    /// issues first, then closed, then alphabetic by `human_id` — single
    /// order all plugins share so UI code stays plugin-agnostic.
    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>>;
}

/// Build the tracker selected by `.fleet/config.yaml`. Returns `None` for
/// plugins fleet doesn't yet implement (Linear, Jira) so callers can
/// gracefully fall back to manual entry.
#[must_use]
pub fn build(choice: TrackerChoice, invoker: Arc<dyn ProcessInvoker>) -> Option<Box<dyn Tracker>> {
    match choice {
        TrackerChoice::GitBug => Some(Box::new(GitBugTracker::new(invoker))),
        TrackerChoice::Github => Some(Box::new(GitHubTracker::new(invoker))),
        TrackerChoice::Linear | TrackerChoice::Jira => None,
    }
}

/// POSIX-shell single-quote escape for paths spliced into `sh -c`.
/// Same shape as the helpers in [`crate::runtime::local`] and
/// [`crate::workflow::executor`]; duplicated rather than re-exported
/// because each module's escape is private and small.
pub fn shell_quote_path(p: &Path) -> String {
    let s = p.display().to_string();
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Sort issues with open first, then alphabetic by `human_id`. Exposed
/// for impls' use; not part of the trait so tests can call it directly.
pub fn sort_open_first(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        let a_open = a.status == "open";
        let b_open = b.status == "open";
        b_open
            .cmp(&a_open)
            .then_with(|| a.human_id.cmp(&b.human_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;

    fn open(human_id: &str, title: &str) -> Issue {
        Issue {
            id: format!("{human_id}-full"),
            human_id: human_id.to_string(),
            title: title.to_string(),
            status: "open".to_string(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn issue_matches_empty_filter() {
        assert!(open("a", "b").matches(""));
    }

    #[test]
    fn issue_matches_human_id_case_insensitive() {
        assert!(open("abc1", "fix").matches("ABC"));
        assert!(!open("abc1", "fix").matches("xyz"));
    }

    #[test]
    fn issue_matches_title_case_insensitive() {
        assert!(open("a", "Fix parser").matches("PARS"));
    }

    #[test]
    fn build_returns_concrete_impls_for_known_plugins() {
        let invoker = Arc::new(MockProcessInvoker::new()) as Arc<dyn ProcessInvoker>;
        let gb = build(TrackerChoice::GitBug, Arc::clone(&invoker)).unwrap();
        assert_eq!(gb.name(), "git-bug");
        let gh = build(TrackerChoice::Github, Arc::clone(&invoker)).unwrap();
        assert_eq!(gh.name(), "github");
    }

    #[test]
    fn build_returns_none_for_unimplemented_plugins() {
        let invoker = Arc::new(MockProcessInvoker::new()) as Arc<dyn ProcessInvoker>;
        assert!(build(TrackerChoice::Linear, Arc::clone(&invoker)).is_none());
        assert!(build(TrackerChoice::Jira, invoker).is_none());
    }

    #[test]
    fn sort_open_first_places_open_before_closed_then_alpha() {
        let mut issues = vec![
            Issue {
                id: "1".into(),
                human_id: "5".into(),
                title: "closed".into(),
                status: "closed".into(),
                labels: vec![],
            },
            Issue {
                id: "2".into(),
                human_id: "9".into(),
                title: "open-b".into(),
                status: "open".into(),
                labels: vec![],
            },
            Issue {
                id: "3".into(),
                human_id: "1".into(),
                title: "open-a".into(),
                status: "open".into(),
                labels: vec![],
            },
        ];
        sort_open_first(&mut issues);
        assert_eq!(issues[0].human_id, "1");
        assert_eq!(issues[1].human_id, "9");
        assert_eq!(issues[2].human_id, "5");
    }
}
