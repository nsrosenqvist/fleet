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

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use crate::process::ProcessInvoker;
use crate::repo_config::Tracker as TrackerChoice;

pub use git_bug::GitBugTracker;
pub use github::GitHubTracker;

/// Normalised lifecycle state for a ticket. Trackers map this onto
/// whatever native primitive they have: GitHub treats `InProgress` as
/// an `in-progress` label (no first-class state); git-bug uses its
/// own status verbs plus label conventions. The trait hides the
/// difference so workflow + bridge code never reaches for tracker
/// specifics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Open,
    InProgress,
    Closed,
}

/// A single comment on a ticket as returned by [`Tracker::read`].
/// `created_at` is the tracker's own timestamp string (ISO-8601 in
/// practice); fleet does not parse it because the only consumer today
/// is the agent prompt context, which displays it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// A ticket plus its rendered description and the comment thread.
/// Returned by [`Tracker::read`] and surfaced verbatim to bridge
/// callers so agents see the same story a human would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub issue: Issue,
    pub body: String,
    pub comments: Vec<Comment>,
}

/// Uniform issue shape across plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
/// dispatch into a worker thread for async UX (and because the bridge
/// listener holds an `Arc<dyn Tracker>` shared across thread bounds).
///
/// Write methods (`comment`, `set_status`, `add_label`, `remove_label`,
/// `create`, `link_parent`) and the rich-read method (`read`) carry
/// default impls that bail with a clear error. Trackers without
/// concrete support (e.g. a stub used in a test) get the bail-by-name
/// behaviour for free; the two production impls (`GitBugTracker` and
/// `GitHubTracker`) override every method.
pub trait Tracker: Send + Sync {
    fn name(&self) -> &'static str;
    /// Issues for the project rooted at `repo_root`. Sorted with open
    /// issues first, then closed, then alphabetic by `human_id` — single
    /// order all plugins share so UI code stays plugin-agnostic.
    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>>;

    /// Read a single ticket with its body and comment thread. Used by
    /// the bridge's `GET /read` route to feed an agent the context it
    /// needs about its bound ticket.
    fn read(&self, _repo_root: &Path, _issue_id: &str) -> Result<IssueDetail> {
        bail!("tracker `{}` does not implement read", self.name())
    }

    /// Append a comment to `issue_id`. The bridge writes the agent's
    /// summary / progress notes via this method; the workflow engine
    /// uses it for cross-link comments produced by `tracker-create`.
    fn comment(&self, _repo_root: &Path, _issue_id: &str, _body: &str) -> Result<()> {
        bail!("tracker `{}` does not implement comment", self.name())
    }

    /// Move `issue_id` to `status`. The trait abstracts over each
    /// tracker's native primitive: GitHub uses `gh issue close/reopen`
    /// for Open/Closed and an `in-progress` label for `InProgress`;
    /// git-bug has first-class status verbs plus its own label
    /// convention.
    fn set_status(&self, _repo_root: &Path, _issue_id: &str, _status: Status) -> Result<()> {
        bail!("tracker `{}` does not implement set_status", self.name())
    }

    /// Add a single label. Idempotent on both backends: re-adding an
    /// existing label is not an error.
    fn add_label(&self, _repo_root: &Path, _issue_id: &str, _label: &str) -> Result<()> {
        bail!("tracker `{}` does not implement add_label", self.name())
    }

    /// Remove a single label. Idempotent on both backends: removing a
    /// label the ticket doesn't have is not an error.
    fn remove_label(&self, _repo_root: &Path, _issue_id: &str, _label: &str) -> Result<()> {
        bail!("tracker `{}` does not implement remove_label", self.name())
    }

    /// Create a new ticket. Supervisor / brainstorm authority only —
    /// never reachable through the bridge. The trait surface lives here
    /// because `tracker-create` workflow nodes (Phase 2) and brainstorm
    /// tool endpoints (Phase 5) both consume it.
    #[allow(dead_code)]
    fn create(
        &self,
        _repo_root: &Path,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> Result<Issue> {
        bail!("tracker `{}` does not implement create", self.name())
    }

    /// Link `child_id` to `parent_id`. Mapping varies per tracker —
    /// GitHub appends a task-list line to the parent's body; git-bug
    /// records a `parent:<id>` label on the child.
    #[allow(dead_code)]
    fn link_parent(&self, _repo_root: &Path, _parent_id: &str, _child_id: &str) -> Result<()> {
        bail!("tracker `{}` does not implement link_parent", self.name())
    }
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

    /// Bare-bones `Tracker` that only implements the two required
    /// methods. Used to assert the default impl of every write method
    /// bails with a clear error rather than panicking.
    struct StubTracker;
    impl Tracker for StubTracker {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn list_issues(&self, _repo_root: &Path) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn default_write_methods_bail_with_tracker_name_in_message() {
        let t = StubTracker;
        let repo = Path::new("/repo");
        for (op, err) in [
            ("read", t.read(repo, "1").err()),
            ("comment", t.comment(repo, "1", "hi").err()),
            ("set_status", t.set_status(repo, "1", Status::Closed).err()),
            ("add_label", t.add_label(repo, "1", "x").err()),
            ("remove_label", t.remove_label(repo, "1", "x").err()),
            ("create", t.create(repo, "t", "b", &[]).err()),
            ("link_parent", t.link_parent(repo, "p", "c").err()),
        ] {
            let msg = format!(
                "{:#}",
                err.unwrap_or_else(|| panic!("expected {op} to bail"))
            );
            assert!(
                msg.contains("stub"),
                "{op}: tracker name missing from message — {msg}"
            );
            assert!(
                msg.contains(op),
                "{op}: operation name missing from message — {msg}"
            );
        }
    }

    #[test]
    fn build_returns_none_for_unimplemented_plugins() {
        let invoker = Arc::new(MockProcessInvoker::new()) as Arc<dyn ProcessInvoker>;
        assert!(build(TrackerChoice::Linear, Arc::clone(&invoker)).is_none());
        assert!(build(TrackerChoice::Jira, invoker).is_none());
    }

    #[test]
    fn status_round_trips_through_kebab_case_serde() {
        // Bridge HTTP routes encode status as kebab-case JSON
        // (`"in-progress"` not `"InProgress"`). Lock the encoding so
        // a future serde-rename slip can't silently break the wire
        // contract between fleet-tracker and the bridge server.
        assert_eq!(serde_json::to_string(&Status::Open).unwrap(), "\"open\"");
        assert_eq!(
            serde_json::to_string(&Status::InProgress).unwrap(),
            "\"in-progress\""
        );
        assert_eq!(
            serde_json::to_string(&Status::Closed).unwrap(),
            "\"closed\""
        );
        assert_eq!(
            serde_json::from_str::<Status>("\"in-progress\"").unwrap(),
            Status::InProgress
        );
    }

    #[test]
    fn issue_detail_round_trips_through_serde() {
        let detail = IssueDetail {
            issue: Issue {
                id: "gh:42".into(),
                human_id: "42".into(),
                title: "fix parser".into(),
                status: "open".into(),
                labels: vec!["bug".into()],
            },
            body: "long description".into(),
            comments: vec![Comment {
                author: "alice".into(),
                body: "first take".into(),
                created_at: "2026-05-15T10:00:00Z".into(),
            }],
        };
        let json = serde_json::to_string(&detail).unwrap();
        let back: IssueDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(detail, back);
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
