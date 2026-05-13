//! Issue trackers fleet queries to populate the spawn picker.
//!
//! Each tracker plugin (matched against the `tracker.plugin` field
//! in `agent-orchestrator.yaml`) implements the [`Tracker`] trait —
//! one method, `list_issues`, returns a uniform [`Issue`] shape.
//! [`build`] is the factory: it maps a plugin-name string to a
//! concrete impl, returning `None` for plugins fleet doesn't yet
//! speak so callers can fall back to manual id entry without
//! special-casing each plugin at the call site.
//!
//! Today fleet ships two backends. Both run **inside the Lima VM**
//! via `limactl shell --workdir <repo>` — keeps the install story
//! uniform (fleet's Lima template provisions git-bug + gh; no
//! host-side tracker dependencies) and lets gh-in-VM reuse the
//! host's `gh auth login` via Lima's home bind-mount (gh's config
//! at `~/.config/gh/hosts.yml` is the same file in both
//! filesystems).
//!
//! - [`GitBugTracker`] — `git-bug bug --format json`. Data lives in
//!   the project's git history.
//! - [`GitHubTracker`] — `gh issue list --json …`. gh talks to
//!   github.com directly, auto-detects the repo from the project
//!   path's git remote.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// Uniform issue shape fleet stores after normalising whatever the
/// tracker plugin returned. `human_id` is what we hand to `ao
/// spawn`; the rest decorates the picker.
#[derive(Debug, Clone)]
pub struct Issue {
    /// Stable opaque id (full hash for git-bug, "gh:<number>" for
    /// GitHub). Not shown to users; useful as a future cache key.
    pub id: String,
    /// What the user sees and what gets passed to `ao spawn`.
    pub human_id: String,
    /// One-line description.
    pub title: String,
    /// Normalised lowercase status: `open` / `closed`.
    pub status: String,
    /// Tags from the tracker. The picker surfaces the first one as
    /// a trailing chip; full list reserved for future filter UX.
    pub labels: Vec<String>,
}

impl Issue {
    /// Substring match for the spawn picker's filter input.
    /// Case-insensitive on both id and title. Empty filter matches.
    pub fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let f = filter.to_lowercase();
        self.human_id.to_lowercase().contains(&f) || self.title.to_lowercase().contains(&f)
    }
}

/// Plugin-agnostic tracker query. Sync — the caller spins this in a
/// thread for async UX (see `App::open_spawn_prompt`). `Send + Sync`
/// because the spawn prompt's fetcher thread takes ownership of the
/// `Box`.
pub trait Tracker: Send + Sync {
    /// Stable identifier matching `tracker.plugin` in the AO yaml.
    fn name(&self) -> &'static str;
    /// Issues for the project rooted at `repo_root`, sorted with
    /// open before closed and then by `human_id` so the picker order
    /// is stable across calls.
    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>>;
}

/// Map a `tracker.plugin` string to a backend. `None` means "fleet
/// doesn't speak this plugin yet" — callers should fall back to
/// manual id entry.
pub fn build(plugin: &str) -> Option<Box<dyn Tracker>> {
    match plugin {
        "git-bug" => Some(Box::new(GitBugTracker)),
        "github" => Some(Box::new(GitHubTracker)),
        _ => None,
    }
}

// ─────────────────────── git-bug ───────────────────────

pub struct GitBugTracker;

impl Tracker for GitBugTracker {
    fn name(&self) -> &'static str {
        "git-bug"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        let output = Command::new("limactl")
            .arg("shell")
            .arg("--workdir")
            .arg(repo_root)
            .arg("fleet-vm")
            .arg("git-bug")
            .arg("bug")
            .arg("--format")
            .arg("json")
            .output()
            .context("invoke `git-bug bug` via limactl shell")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git-bug bug failed: {}",
                stderr.lines().next().unwrap_or("(no stderr)")
            );
        }

        // git-bug emits `null` (not `[]`) for an empty repo; map to
        // empty vec so the picker just shows "(no issues)".
        let trimmed = std::str::from_utf8(&output.stdout)
            .context("non-UTF-8 git-bug output")?
            .trim();
        if trimmed.is_empty() || trimmed == "null" {
            return Ok(Vec::new());
        }

        let raw: Vec<GitBugIssue> =
            serde_json::from_str(trimmed).context("parse git-bug summary JSON")?;
        let mut issues: Vec<Issue> = raw.into_iter().map(Into::into).collect();
        sort_open_first(&mut issues);
        Ok(issues)
    }
}

/// `git-bug bug --format json` row (subset). `git-bug` uses
/// `snake_case` even though its `--field` selector is `camelCase` — we
/// use the JSON form here so serde's renames stay minimal.
#[derive(Deserialize)]
struct GitBugIssue {
    id: String,
    #[serde(default)]
    human_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: Vec<String>,
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

// ─────────────────────── github ───────────────────────

pub struct GitHubTracker;

impl Tracker for GitHubTracker {
    fn name(&self) -> &'static str {
        "github"
    }

    fn list_issues(&self, repo_root: &Path) -> Result<Vec<Issue>> {
        // gh runs in the VM (like git-bug) for a uniform install
        // story — fleet's Lima template provisions both, no host-side
        // tracker deps. The host's `gh auth login` config lives at
        // `~/.config/gh/hosts.yml`, which is bind-mounted into the VM
        // at the same path, so gh-in-guest reads the same creds —
        // the user doesn't have to re-auth inside the VM.
        let output = Command::new("limactl")
            .arg("shell")
            .arg("--workdir")
            .arg(repo_root)
            .arg("fleet-vm")
            .arg("gh")
            .arg("issue")
            .arg("list")
            .arg("--json")
            .arg("number,title,state,labels")
            .arg("--limit")
            .arg("200")
            .arg("--state")
            .arg("all")
            .output()
            .context("invoke `gh issue list` via limactl shell")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "gh issue list failed: {}",
                stderr.lines().next().unwrap_or("(no stderr)")
            );
        }

        let trimmed = std::str::from_utf8(&output.stdout)
            .context("non-UTF-8 gh output")?
            .trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let raw: Vec<GhIssue> =
            serde_json::from_str(trimmed).context("parse `gh issue list` JSON")?;
        let mut issues: Vec<Issue> = raw.into_iter().map(Into::into).collect();
        sort_open_first(&mut issues);
        Ok(issues)
    }
}

/// `gh issue list --json …` row. State is upper-case (`OPEN` /
/// `CLOSED`), labels are objects not strings, numbers are u64.
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

// ─────────────────────── helpers ───────────────────────

/// Open issues first, then closed, then alphabetic by `human_id`.
/// Single ordering all trackers use so the picker behaves the same
/// regardless of plugin.
fn sort_open_first(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        let a_open = a.status == "open";
        let b_open = b.status == "open";
        b_open.cmp(&a_open).then_with(|| a.human_id.cmp(&b.human_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_filter_matches_everything() {
        assert!(open("abc", "anything").matches(""));
    }

    #[test]
    fn filter_matches_human_id_substring_case_insensitive() {
        assert!(open("abc1234", "Fix parser").matches("abc"));
        assert!(open("abc1234", "Fix parser").matches("1234"));
        assert!(open("abc1234", "Fix parser").matches("ABC"));
        assert!(!open("abc1234", "Fix parser").matches("zzz"));
    }

    #[test]
    fn filter_matches_title_case_insensitive() {
        assert!(open("abc", "Fix the parser").matches("parser"));
        assert!(open("abc", "Fix the parser").matches("PARS"));
    }

    #[test]
    fn build_returns_known_plugins() {
        assert!(build("git-bug").is_some());
        assert!(build("github").is_some());
        assert!(build("linear").is_none());
        assert!(build("").is_none());
    }

    #[test]
    fn git_bug_issue_mapping() {
        let raw: GitBugIssue = serde_json::from_str(
            r#"{"id":"deadbeef","human_id":"abc1234","title":"Fix","status":"OPEN","labels":["bug"]}"#,
        )
        .expect("parse");
        let issue: Issue = raw.into();
        assert_eq!(issue.human_id, "abc1234");
        // Status normalises to lowercase across all plugins so the
        // picker's colouring branches stay simple.
        assert_eq!(issue.status, "open");
        assert_eq!(issue.labels, vec!["bug".to_string()]);
    }

    #[test]
    fn gh_issue_mapping() {
        let raw: GhIssue = serde_json::from_str(
            r#"{"number":42,"title":"Fix","state":"OPEN","labels":[{"name":"bug"}]}"#,
        )
        .expect("parse");
        let issue: Issue = raw.into();
        assert_eq!(issue.human_id, "42");
        assert_eq!(issue.id, "gh:42");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.labels, vec!["bug".to_string()]);
    }

    #[test]
    fn sort_places_open_before_closed() {
        let mut issues = vec![
            Issue {
                id: "1".into(),
                human_id: "1".into(),
                title: "closed-a".into(),
                status: "closed".into(),
                labels: Vec::new(),
            },
            Issue {
                id: "2".into(),
                human_id: "2".into(),
                title: "open-b".into(),
                status: "open".into(),
                labels: Vec::new(),
            },
            Issue {
                id: "3".into(),
                human_id: "3".into(),
                title: "open-a".into(),
                status: "open".into(),
                labels: Vec::new(),
            },
        ];
        sort_open_first(&mut issues);
        // Open entries lead, sorted alphabetically by human_id.
        assert_eq!(issues[0].human_id, "2");
        assert_eq!(issues[1].human_id, "3");
        assert_eq!(issues[2].human_id, "1");
    }
}
