//! `fleet issues …` — host-side tracker browser + writer.
//!
//! Reads `.fleet/config.yaml`'s `tracker:` choice, builds the matching
//! impl from [`crate::tracker::build`], and dispatches to the trait's
//! methods. For unimplemented trackers (Linear / Jira today) commands
//! print an actionable fallback message instead of failing — same
//! shape as `fleet runtime doctor`'s "next steps" UX.
//!
//! Subcommands:
//! - `list` — every issue, one line each.
//! - `create <title> [--body <body>] [--label <label>]...` — file a new
//!   ticket; prints the new id on stdout.
//! - `comment <id> <body>` — post a comment.
//! - `set-status <id> <status>` — open / in-progress / closed.
//! - `add-label <id> <label>` / `remove-label <id> <label>`.
//!
//! Write commands are the host-side mirror of what the per-session
//! bridge exposes to workflow containers (and what the orchestrator
//! agent invokes from inside its tmux pane). Render paths are pure
//! functions over value objects so tests assert on output text
//! without a real `git-bug` / `gh`.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::tracker::{Issue, Status, Tracker, build};

/// CLI entry point for `fleet issues list`. Returns 0 on a clean listing
/// (including the empty case), 1 on any tracker/IO failure.
pub fn run_list() -> Result<i32> {
    let (root, tracker, _config) = open_tracker("list")?;
    let issues = tracker
        .list_issues(&root)
        .with_context(|| format!("listing issues via `{}`", tracker.name()))?;
    print!("{}", render_issue_list(tracker.name(), &root, &issues));
    Ok(0)
}

/// `fleet issues create <title> [--body …] [--label …]…` — file a new
/// ticket via the configured tracker; print the new id on stdout. The
/// repo's `autonomous.filter_labels` are unioned into `labels` before
/// the create call so a fleet-spawned ticket passes the supervisor's
/// label gate on the next tick. Empty `filter_labels` = pre-feature
/// behaviour (caller-supplied labels pass through unchanged).
pub fn run_create(title: &str, body: Option<&str>, labels: &[String]) -> Result<i32> {
    if title.trim().is_empty() {
        anyhow::bail!("issue title must be non-empty");
    }
    let (root, tracker, config) = open_tracker("create")?;
    let body = body.unwrap_or("");
    let stamped = config.autonomous.stamp_creation_labels(labels);
    let issue = tracker
        .create(&root, title, body, &stamped)
        .with_context(|| format!("creating issue via `{}` (title: {title:?})", tracker.name()))?;
    println!("{}", issue.human_id);
    Ok(0)
}

/// `fleet issues comment <id> <body>` — comment on a ticket.
pub fn run_comment(id: &str, body: &str) -> Result<i32> {
    let (root, tracker, _config) = open_tracker("comment")?;
    tracker
        .comment(&root, id, body)
        .with_context(|| format!("commenting on `{id}` via `{}`", tracker.name()))?;
    Ok(0)
}

/// `fleet issues set-status <id> <open|in-progress|closed>`.
pub fn run_set_status(id: &str, status_word: &str) -> Result<i32> {
    let status = parse_status(status_word)?;
    let (root, tracker, _config) = open_tracker("set-status")?;
    tracker
        .set_status(&root, id, status)
        .with_context(|| format!("setting status on `{id}` via `{}`", tracker.name()))?;
    Ok(0)
}

/// `fleet issues add-label <id> <label>`.
pub fn run_add_label(id: &str, label: &str) -> Result<i32> {
    if label.trim().is_empty() {
        anyhow::bail!("label must be non-empty");
    }
    let (root, tracker, _config) = open_tracker("add-label")?;
    tracker
        .add_label(&root, id, label)
        .with_context(|| format!("adding label `{label}` to `{id}` via `{}`", tracker.name()))?;
    Ok(0)
}

/// `fleet issues remove-label <id> <label>`.
pub fn run_remove_label(id: &str, label: &str) -> Result<i32> {
    if label.trim().is_empty() {
        anyhow::bail!("label must be non-empty");
    }
    let (root, tracker, _config) = open_tracker("remove-label")?;
    tracker.remove_label(&root, id, label).with_context(|| {
        format!(
            "removing label `{label}` from `{id}` via `{}`",
            tracker.name()
        )
    })?;
    Ok(0)
}

/// Build the configured tracker + return the loaded `RepoConfig` so
/// callers needing autonomous-block knobs (e.g. `run_create`'s label
/// stamp) don't have to re-load the file. Bails with the same
/// "not implemented" wording for every subcommand so the message
/// stays consistent.
fn open_tracker(subcommand: &str) -> Result<(std::path::PathBuf, Box<dyn Tracker>, RepoConfig)> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config =
        RepoConfig::load(root.join(".fleet/config.yaml")).context("loading .fleet/config.yaml")?;
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let tracker = build(config.tracker, Arc::clone(&invoker)).ok_or_else(|| {
        anyhow::anyhow!(
            "fleet issues {subcommand}: tracker `{}` is not yet implemented \
             — pick git-bug or github in .fleet/config.yaml",
            config.tracker.as_str()
        )
    })?;
    Ok((root, tracker, config))
}

/// Parse the CLI `<status>` argument into the typed `Status` enum.
/// Same wording the bridge + orchestrator server use, so the surface
/// matches what users see in the docs.
fn parse_status(word: &str) -> Result<Status> {
    match word {
        "open" => Ok(Status::Open),
        "in-progress" => Ok(Status::InProgress),
        "closed" => Ok(Status::Closed),
        other => anyhow::bail!("unknown status `{other}` (allowed: open / in-progress / closed)"),
    }
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

    #[test]
    fn parse_status_recognises_three_canonical_words() {
        assert!(matches!(parse_status("open").unwrap(), Status::Open));
        assert!(matches!(
            parse_status("in-progress").unwrap(),
            Status::InProgress
        ));
        assert!(matches!(parse_status("closed").unwrap(), Status::Closed));
    }

    #[test]
    fn parse_status_rejects_unknown_with_allowed_list_in_message() {
        let err = parse_status("frozen").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("frozen"), "got: {msg}");
        assert!(
            msg.contains("open") && msg.contains("in-progress") && msg.contains("closed"),
            "got: {msg}"
        );
    }
}
