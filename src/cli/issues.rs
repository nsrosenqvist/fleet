//! `fleet issues list` — host-side tracker browser.
//!
//! Reads `.fleet/config.yaml`'s `tracker:` choice, builds the matching
//! impl from [`crate::tracker::build`], and prints one line per issue.
//! For unimplemented trackers (Linear / Jira today) the command prints an
//! actionable fallback message instead of failing — same shape as
//! `fleet runtime doctor`'s "next steps" UX.
//!
//! The render path is a pure function over the issue list so tests can
//! assert on output without a real `git-bug` or `gh` binary.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::tracker::{Issue, build};

/// CLI entry point for `fleet issues list`. Returns 0 on a clean listing
/// (including the empty case), 1 on any tracker/IO failure.
pub fn run_list() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .context("loading .fleet/config.yaml")?;
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let Some(tracker) = build(config.tracker, invoker) else {
        // Unimplemented plugin. Surface the actionable message instead of
        // letting the user wonder why nothing prints.
        eprintln!(
            "fleet issues list: tracker `{}` is not yet implemented — pick git-bug or github in .fleet/config.yaml",
            config.tracker.as_str()
        );
        return Ok(1);
    };
    let issues = tracker
        .list_issues(&root)
        .with_context(|| format!("listing issues via `{}`", tracker.name()))?;
    print!("{}", render_issue_list(tracker.name(), &root, &issues));
    Ok(0)
}

/// Pure renderer. Tested directly so the output format stays a UI
/// contract.
#[must_use]
pub fn render_issue_list(tracker_name: &str, repo_root: &Path, issues: &[Issue]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "fleet issues ({tracker_name}) in {}",
        repo_root.display()
    );
    if issues.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    // Compute column widths so the table looks tidy without dragging in a
    // table crate. `human_id` is short and bounded; `status` is one of
    // "open" / "closed" (5 chars + padding).
    let id_width = issues.iter().map(|i| i.human_id.len()).max().unwrap_or(2);
    for issue in issues {
        let labels = if issue.labels.is_empty() {
            String::new()
        } else {
            format!("  [{}]", issue.labels.join(", "))
        };
        let _ = writeln!(
            out,
            "  {status:<6}  {id:<id_width$}  {title}{labels}",
            status = issue.status,
            id = issue.human_id,
            title = issue.title,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(human_id: &str, title: &str, status: &str, labels: &[&str]) -> Issue {
        Issue {
            id: format!("{human_id}-full"),
            human_id: human_id.to_string(),
            title: title.to_string(),
            status: status.to_string(),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn render_empty_shows_header_and_none() {
        let out = render_issue_list("git-bug", Path::new("/r"), &[]);
        assert!(out.contains("fleet issues (git-bug) in /r"));
        assert!(out.contains("(none)"));
    }

    #[test]
    fn render_shows_one_line_per_issue_with_status_id_title() {
        let issues = vec![
            issue("42", "Fix parser", "open", &["bug"]),
            issue("1", "Update docs", "closed", &[]),
        ];
        let out = render_issue_list("github", Path::new("/r"), &issues);
        assert!(out.contains("open    42  Fix parser  [bug]"), "got: {out}");
        assert!(out.contains("closed  1   Update docs"), "got: {out}");
    }

    #[test]
    fn render_pads_id_column_to_widest_entry() {
        let issues = vec![
            issue("9", "short id", "open", &[]),
            issue("12345", "long id", "open", &[]),
        ];
        let out = render_issue_list("git-bug", Path::new("/r"), &issues);
        // Width = max id length = 5; the `9` line gets 4 spaces of padding.
        assert!(out.contains("open    9      short id"), "got: {out}");
        assert!(out.contains("open    12345  long id"), "got: {out}");
    }

    #[test]
    fn render_omits_labels_section_when_empty() {
        let issues = vec![issue("1", "No labels", "open", &[])];
        let out = render_issue_list("git-bug", Path::new("/r"), &issues);
        assert!(!out.contains('['));
    }

    #[test]
    fn render_joins_multiple_labels_with_comma() {
        let issues = vec![issue("1", "x", "open", &["bug", "p1"])];
        let out = render_issue_list("git-bug", Path::new("/r"), &issues);
        assert!(out.contains("[bug, p1]"));
    }
}
