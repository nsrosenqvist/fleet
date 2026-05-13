//! Tracker queries fleet runs from the host.
//!
//! Today the only backend is git-bug (the in-repo tracker AO ships
//! with by default). Listing happens via `limactl shell --workdir
//! <repo> fleet-vm git-bug bug --format json`, parsed against
//! git-bug's documented summary shape. Other tracker plugins (github,
//! linear, …) would land here when they grow support.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// One row in a tracker listing. The summary subset fleet's spawn
/// modal needs — `human_id` is what AO's spawn command expects, the
/// title goes in the picker, status filters out completed bugs from
/// the default view.
#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    /// Full hash. Stable id; rarely shown to humans.
    pub id: String,
    /// Short hash like `abc1234`. What we pass to `ao spawn`.
    #[serde(rename = "human_id", default)]
    pub human_id: String,
    /// One-line description.
    #[serde(default)]
    pub title: String,
    /// `"open"` / `"closed"` / etc.
    #[serde(default)]
    pub status: String,
    /// Tags from the tracker — not yet used by fleet but worth
    /// preserving for future filter chips.
    #[serde(default)]
    pub labels: Vec<String>,
}

impl Issue {
    /// Substring match against the filter string. Case-insensitive
    /// on both id (so `abc` finds `ABC1234`) and title (free-form
    /// text). Empty filter matches everything.
    pub fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let f = filter.to_lowercase();
        self.human_id.to_lowercase().contains(&f) || self.title.to_lowercase().contains(&f)
    }
}

/// `git-bug bug --format json` inside the VM. Returns parsed
/// summaries sorted by status (open first, then closed) so the spawn
/// picker leads with what the user almost certainly wants.
pub fn list_git_bug_issues(repo_root: &Path) -> Result<Vec<Issue>> {
    let output = std::process::Command::new("limactl")
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

    // git-bug returns null (not []) for an empty repo. Map that to
    // an empty vec so callers don't have to handle two shapes.
    let stdout = output.stdout;
    let trimmed = std::str::from_utf8(&stdout)
        .context("non-UTF-8 git-bug output")?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }

    let mut issues: Vec<Issue> =
        serde_json::from_str(trimmed).context("parse git-bug summary JSON")?;
    issues.sort_by(|a, b| {
        // open before closed, then by human_id alphabetically as a
        // tiebreaker so the order is stable across calls.
        let a_open = a.status == "open";
        let b_open = b.status == "open";
        b_open.cmp(&a_open).then_with(|| a.human_id.cmp(&b.human_id))
    });
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(human_id: &str, title: &str) -> Issue {
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
        assert!(issue("abc", "anything").matches(""));
    }

    #[test]
    fn filter_matches_human_id_substring() {
        assert!(issue("abc1234", "Fix the parser").matches("abc"));
        assert!(issue("abc1234", "Fix the parser").matches("1234"));
        assert!(!issue("abc1234", "Fix the parser").matches("zzz"));
    }

    #[test]
    fn filter_matches_title_substring() {
        assert!(issue("abc", "Fix the parser").matches("parser"));
        assert!(issue("abc", "Fix the parser").matches("PARS"));
    }
}
