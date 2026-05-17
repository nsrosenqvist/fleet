//! `fleet deps …` — CLI surface over [`crate::deps::DepsStore`].
//!
//! Three subcommands: `list`, `add`, `remove`. Targets the
//! orchestrator-agent use case primarily — when filing a contracts
//! ticket + implementation tickets, the agent needs a clean way to
//! record the `blocked_on` edges without hand-editing
//! `.fleet/deps.json`. Same shape as `fleet plan` and
//! `fleet issues`: render helpers are pure functions over
//! `DepsDoc`, the `run_*` wrappers do the I/O.
//!
//! Reads tolerate a missing deps file (empty doc); writes
//! materialise the parent directory and atomic-rename the new
//! body in place via the store. Cycles are refused at `add` time
//! by pre-checking [`crate::deps::would_create_cycle`] against
//! the proposed edge.
//!
//! `add` also posts a pair of cross-link comments — one on the
//! `blocked` ticket, one on the `blocked_on` ticket — so a human
//! reading either ticket in the tracker sees the relationship
//! without consulting fleet's deps render. Freeform-tag edges
//! only comment on the `blocked` side (the right-hand is a slug,
//! not a ticket). Unlike the `tracker-create` workflow node, `add`
//! does not call `Tracker::link_parent`: that encodes a sub-issue
//! hierarchy and would misrepresent a plain blocks-relationship
//! between two independently-scoped tickets.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::deps::{BlockedReason, DepEdge, DepsDoc, DepsStore, would_create_cycle};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::session::now_ms;

/// `fleet deps list` — human-readable view of every edge in the
/// current repo. Replaces `cat .fleet/deps.json` for the
/// orchestrator agent (and any human reading the graph).
pub fn run_list() -> Result<i32> {
    let store = open_store()?;
    let doc = store.load().context("loading deps file")?;
    print!("{}", render_list(&doc));
    Ok(0)
}

/// `fleet deps add <blocked> --blocked-on <id>` or
/// `fleet deps add <blocked> --blocked-on-tag <slug>`.
///
/// Exactly one of `blocked_on` / `blocked_on_tag` must be
/// non-empty. Bails on cycle creation (the proposed edge would
/// close a path from the blocked side back to itself).
/// Idempotent: re-adding the same edge is a no-op, preserving the
/// original `created_at_ms` and skipping comment posting so retries
/// don't spam the ticket thread.
///
/// Thin shell: resolves the deps store + tracker from the repo
/// root and delegates the actual work to [`add_dep_edge`] so tests
/// can drive that function against a tempdir + mock tracker
/// without touching cwd or `.fleet/config.yaml`.
pub fn run_add(
    blocked: &str,
    blocked_on: Option<&str>,
    blocked_on_tag: Option<&str>,
) -> Result<i32> {
    let edge = build_edge(blocked, blocked_on, blocked_on_tag)?;
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = DepsStore::for_repo(&root);

    // Resolve the tracker for cross-link comments. Best-effort: a
    // missing config or an unimplemented plugin (Linear / Jira)
    // leaves the edge recordable without comments — the deps graph
    // is still useful even when the narrative breadcrumb can't be
    // posted.
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .with_context(|| format!("loading repo config under {}", root.display()))?;
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let tracker = crate::tracker::build(config.tracker, invoker);

    let outcome = add_dep_edge(&store, tracker.as_deref(), &root, edge)?;
    print!("{}", render_add_outcome(&outcome));
    Ok(0)
}

/// Result of an [`add_dep_edge`] call. Pure value object so tests
/// (and any future caller wanting a structured report) don't have
/// to screen-scrape [`render_add_outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub blocked: String,
    pub blocked_on: String,
    pub reason: BlockedReason,
    pub already_present: bool,
    /// 0 if the edge was already present (re-add skips commenting)
    /// or if no tracker was configured; 1 for a freeform-tag edge
    /// (only the `blocked` side has a ticket to comment on); 2 for
    /// a ticket→ticket edge.
    pub comments_posted: u8,
    /// `true` when `Tracker::link_blocks` was called and succeeded —
    /// surfacing a native blocks/blocked-by relationship in the host
    /// tracker UI (GitHub Issue Dependencies / a git-bug label pair).
    /// Always `false` for freeform-tag edges (no ticket on the right)
    /// and for re-adds (nothing to record).
    pub linked_in_tracker: bool,
    /// `false` when the repo's tracker config didn't resolve to an
    /// adapter (Linear / Jira today). Lets the renderer explain why
    /// `comments_posted == 0` to the user.
    pub tracker_available: bool,
}

/// Record a deps edge plus its cross-link comments and the native
/// tracker dependency edge. Bails on cycle creation. Side effects are
/// ordered comments → `link_blocks` → write so a transient tracker
/// failure leaves `.fleet/deps.json` untouched — the user re-runs
/// and the whole pipeline executes again.
///
/// Re-adding an already-present edge is a no-op: no comments, no
/// tracker calls, no store-write churn. This keeps `fleet deps add`
/// safe to re-run in scripts and stops a retry from spamming the
/// ticket with duplicate breadcrumbs.
pub fn add_dep_edge(
    store: &DepsStore,
    tracker: Option<&dyn crate::tracker::Tracker>,
    repo_root: &Path,
    edge: DepEdge,
) -> Result<AddOutcome> {
    let doc = store.load().context("loading deps file")?;
    if would_create_cycle(&doc, &edge) {
        bail!(
            "refusing to add edge `{} → {}`: it would create a cycle in the deps graph",
            edge.blocked,
            edge.blocked_on
        );
    }
    let already_present = doc
        .edges
        .iter()
        .any(|e| e.blocked == edge.blocked && e.blocked_on == edge.blocked_on);

    let tracker_available = tracker.is_some();
    let mut comments_posted: u8 = 0;
    let mut linked_in_tracker = false;

    if !already_present {
        if let Some(tracker) = tracker {
            let blocked_body = match edge.reason {
                BlockedReason::Ticket => {
                    format!("fleet: marked as blocked by #{}", edge.blocked_on)
                }
                BlockedReason::Freeform => {
                    // Strip the on-disk `free:` prefix for display:
                    // a human reader thinks of the tag as the slug
                    // they passed on the CLI, not the internal form.
                    let tag = edge
                        .blocked_on
                        .strip_prefix("free:")
                        .unwrap_or(&edge.blocked_on);
                    format!("fleet: marked as blocked on external tag `{tag}`")
                }
            };
            tracker
                .comment(repo_root, &edge.blocked, &blocked_body)
                .with_context(|| {
                    format!(
                        "posting cross-link comment on `{}` (blocked side)",
                        edge.blocked
                    )
                })?;
            comments_posted += 1;

            // The right-hand side only has a ticket when the edge is
            // ticket→ticket. Freeform tags are slugs, not tickets, so
            // both the second comment and the native tracker link are
            // skipped for them.
            if matches!(edge.reason, BlockedReason::Ticket) {
                let blocked_on_body = format!("fleet: marked as blocking #{}", edge.blocked);
                tracker
                    .comment(repo_root, &edge.blocked_on, &blocked_on_body)
                    .with_context(|| {
                        format!(
                            "posting cross-link comment on `{}` (blocked_on side)",
                            edge.blocked_on
                        )
                    })?;
                comments_posted += 1;

                tracker
                    .link_blocks(repo_root, &edge.blocked, &edge.blocked_on)
                    .with_context(|| {
                        format!(
                            "linking `{}` as blocked by `{}` in the tracker",
                            edge.blocked, edge.blocked_on
                        )
                    })?;
                linked_in_tracker = true;
            }
        }
    }

    store
        .add_edge(edge.clone())
        .with_context(|| format!("recording edge `{} → {}`", edge.blocked, edge.blocked_on))?;

    Ok(AddOutcome {
        blocked: edge.blocked,
        blocked_on: edge.blocked_on,
        reason: edge.reason,
        already_present,
        comments_posted,
        linked_in_tracker,
        tracker_available,
    })
}

/// Render the user-visible summary of an [`AddOutcome`]. Always
/// ends in a single `\n` so the CLI `print!` matches the
/// `render_list` / `render_unblock_outcome` shape.
#[must_use]
pub fn render_add_outcome(o: &AddOutcome) -> String {
    if o.already_present {
        return format!(
            "edge `{} → {}` already recorded — left in place\n",
            o.blocked, o.blocked_on
        );
    }
    let suffix: String = if o.tracker_available {
        let mut parts: Vec<String> = Vec::new();
        match o.comments_posted {
            0 => {}
            1 => parts.push("posted 1 cross-link comment".to_string()),
            n => parts.push(format!("posted {n} cross-link comments")),
        }
        if o.linked_in_tracker {
            parts.push("recorded native tracker link".to_string());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("; {}", parts.join(", "))
        }
    } else {
        " (no tracker configured for this repo — cross-link comments and native tracker link skipped)".to_string()
    };
    format!("added edge: {} → {}{}\n", o.blocked, o.blocked_on, suffix)
}

/// `fleet deps remove <blocked> --blocked-on <id>` or
/// `fleet deps remove <blocked> --blocked-on-tag <slug>`.
///
/// Surgical: clears exactly the `(blocked, blocked_on)` edge.
/// Idempotent — removing an edge that isn't there isn't an error,
/// just printed as a no-op so the orchestrator agent can re-run
/// without worrying about state.
pub fn run_remove(
    blocked: &str,
    blocked_on: Option<&str>,
    blocked_on_tag: Option<&str>,
) -> Result<i32> {
    let resolved_blocked_on = resolve_blocked_on(blocked_on, blocked_on_tag)?;
    let store = open_store()?;
    let removed = store
        .remove_edge(blocked, &resolved_blocked_on)
        .with_context(|| format!("removing edge `{blocked} → {resolved_blocked_on}`"))?;
    if removed {
        println!("removed edge: {blocked} → {resolved_blocked_on}");
    } else {
        println!("no edge `{blocked} → {resolved_blocked_on}` to remove");
    }
    Ok(0)
}

// === Helpers ===

fn open_store() -> Result<DepsStore> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    Ok(DepsStore::for_repo(&root))
}

/// Resolve `--blocked-on <id>` vs `--blocked-on-tag <slug>` into
/// the on-disk `blocked_on` string. The freeform tag form gets a
/// `free:` prefix so the supervisor can distinguish tag edges from
/// ticket edges at scan time.
fn resolve_blocked_on(blocked_on: Option<&str>, blocked_on_tag: Option<&str>) -> Result<String> {
    match (blocked_on, blocked_on_tag) {
        (Some(id), None) if !id.is_empty() => Ok(id.to_string()),
        (None, Some(tag)) if !tag.is_empty() => Ok(format!("free:{tag}")),
        (Some(_), Some(_)) => {
            bail!("pass either --blocked-on <ticket-id> or --blocked-on-tag <slug>, not both")
        }
        _ => bail!("pass --blocked-on <ticket-id> or --blocked-on-tag <slug>"),
    }
}

fn build_edge(
    blocked: &str,
    blocked_on: Option<&str>,
    blocked_on_tag: Option<&str>,
) -> Result<DepEdge> {
    if blocked.is_empty() {
        bail!("the <blocked> ticket id must not be empty");
    }
    let (right_hand, reason) = match (blocked_on, blocked_on_tag) {
        (Some(id), None) if !id.is_empty() => (id.to_string(), BlockedReason::Ticket),
        (None, Some(tag)) if !tag.is_empty() => (format!("free:{tag}"), BlockedReason::Freeform),
        (Some(_), Some(_)) => {
            bail!("pass either --blocked-on <ticket-id> or --blocked-on-tag <slug>, not both")
        }
        _ => bail!("pass --blocked-on <ticket-id> or --blocked-on-tag <slug>"),
    };
    Ok(DepEdge {
        blocked: blocked.to_string(),
        blocked_on: right_hand,
        reason,
        created_at_ms: now_ms(),
    })
}

/// Pure: render the human-readable `deps list` output. Empty doc
/// prints a one-line marker so orchestrator + scripts see something
/// instead of nothing.
#[must_use]
pub fn render_list(doc: &DepsDoc) -> String {
    use std::fmt::Write as _;
    if doc.edges.is_empty() {
        return "(no deps edges recorded)\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(out, "{} edge{}:", doc.edges.len(), plural(doc.edges.len()));
    for edge in &doc.edges {
        let kind = match edge.reason {
            BlockedReason::Ticket => "ticket",
            BlockedReason::Freeform => "freeform",
        };
        let _ = writeln!(out, "  {} → {}  [{kind}]", edge.blocked, edge.blocked_on);
    }
    out
}

#[must_use]
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::{BlockedReason, DEPS_SCHEMA_VERSION, DepEdge, DepsDoc};

    fn edge(blocked: &str, blocked_on: &str, reason: BlockedReason) -> DepEdge {
        DepEdge {
            blocked: blocked.to_string(),
            blocked_on: blocked_on.to_string(),
            reason,
            created_at_ms: 1,
        }
    }

    #[test]
    fn render_list_says_so_on_empty_doc() {
        let out = render_list(&DepsDoc::empty());
        assert!(out.contains("no deps edges"), "got: {out}");
    }

    #[test]
    fn render_list_prints_edges_with_kind_marker() {
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![
                edge("42", "43", BlockedReason::Ticket),
                edge("44", "free:apt-mirror", BlockedReason::Freeform),
            ],
        };
        let out = render_list(&doc);
        assert!(out.contains("2 edges:"), "got: {out}");
        assert!(out.contains("42 → 43  [ticket]"), "got: {out}");
        assert!(
            out.contains("44 → free:apt-mirror  [freeform]"),
            "got: {out}"
        );
    }

    #[test]
    fn render_list_pluralisation_matches_count() {
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![edge("42", "43", BlockedReason::Ticket)],
        };
        let out = render_list(&doc);
        assert!(out.contains("1 edge:"), "got: {out}");
    }

    #[test]
    fn build_edge_with_blocked_on_creates_ticket_edge() {
        let e = build_edge("42", Some("43"), None).unwrap();
        assert_eq!(e.blocked, "42");
        assert_eq!(e.blocked_on, "43");
        assert!(matches!(e.reason, BlockedReason::Ticket));
    }

    #[test]
    fn build_edge_with_blocked_on_tag_creates_freeform_edge_with_prefix() {
        let e = build_edge("42", None, Some("apt-mirror")).unwrap();
        assert_eq!(e.blocked_on, "free:apt-mirror");
        assert!(matches!(e.reason, BlockedReason::Freeform));
    }

    #[test]
    fn build_edge_refuses_both_flags_passed() {
        let err = build_edge("42", Some("43"), Some("apt-mirror")).unwrap_err();
        assert!(err.to_string().contains("not both"), "got: {err}");
    }

    #[test]
    fn build_edge_refuses_neither_flag_passed() {
        let err = build_edge("42", None, None).unwrap_err();
        assert!(err.to_string().contains("--blocked-on"), "got: {err}");
    }

    #[test]
    fn build_edge_refuses_empty_blocked_id() {
        let err = build_edge("", Some("43"), None).unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn resolve_blocked_on_returns_ticket_id_unchanged() {
        let s = resolve_blocked_on(Some("43"), None).unwrap();
        assert_eq!(s, "43");
    }

    #[test]
    fn resolve_blocked_on_prefixes_freeform_tag() {
        let s = resolve_blocked_on(None, Some("apt-mirror")).unwrap();
        assert_eq!(s, "free:apt-mirror");
    }

    // ---- add_dep_edge ---------------------------------------------

    use crate::tracker::{Issue, Tracker};
    use std::sync::Mutex;

    /// Minimal mock that records every `comment` call. `fail_after`
    /// makes the *next* call return an error so tests can exercise
    /// the "tracker post failed, don't record edge" path. Other
    /// `Tracker` methods are left at the trait default (which bails)
    /// — anything that reaches for them is a test bug.
    struct CommentRecordingTracker {
        calls: Mutex<Vec<(String, String)>>,
        link_blocks_calls: Mutex<Vec<(String, String)>>,
        fail_after: Mutex<Option<usize>>,
        fail_link_blocks: Mutex<bool>,
    }

    impl CommentRecordingTracker {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                link_blocks_calls: Mutex::new(Vec::new()),
                fail_after: Mutex::new(None),
                fail_link_blocks: Mutex::new(false),
            }
        }

        fn failing_after(n: usize) -> Self {
            let t = Self::new();
            *t.fail_after.lock().unwrap() = Some(n);
            t
        }

        fn failing_link_blocks() -> Self {
            let t = Self::new();
            *t.fail_link_blocks.lock().unwrap() = true;
            t
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn link_blocks_calls(&self) -> Vec<(String, String)> {
            self.link_blocks_calls.lock().unwrap().clone()
        }
    }

    impl Tracker for CommentRecordingTracker {
        fn name(&self) -> &'static str {
            "mock-comment-recording"
        }
        fn list_issues(&self, _: &Path) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }
        fn comment(&self, _: &Path, id: &str, body: &str) -> Result<()> {
            let count = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((id.to_string(), body.to_string()));
                calls.len()
            };
            if let Some(n) = *self.fail_after.lock().unwrap() {
                if count > n {
                    bail!("forced comment failure");
                }
            }
            Ok(())
        }
        fn link_blocks(&self, _: &Path, blocked: &str, blocked_on: &str) -> Result<()> {
            self.link_blocks_calls
                .lock()
                .unwrap()
                .push((blocked.to_string(), blocked_on.to_string()));
            if *self.fail_link_blocks.lock().unwrap() {
                bail!("forced link_blocks failure");
            }
            Ok(())
        }
    }

    fn ticket_edge(blocked: &str, blocked_on: &str) -> DepEdge {
        DepEdge {
            blocked: blocked.to_string(),
            blocked_on: blocked_on.to_string(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }
    }

    fn freeform_edge(blocked: &str, tag: &str) -> DepEdge {
        DepEdge {
            blocked: blocked.to_string(),
            blocked_on: format!("free:{tag}"),
            reason: BlockedReason::Freeform,
            created_at_ms: 1,
        }
    }

    #[test]
    fn add_dep_edge_posts_a_comment_on_both_tickets_for_ticket_edge() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let tracker = CommentRecordingTracker::new();
        let outcome = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap();

        assert!(!outcome.already_present);
        assert_eq!(outcome.comments_posted, 2);
        assert!(outcome.tracker_available);

        let calls = tracker.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "42");
        assert!(calls[0].1.contains("blocked by #43"), "got: {}", calls[0].1);
        assert_eq!(calls[1].0, "43");
        assert!(calls[1].1.contains("blocking #42"), "got: {}", calls[1].1);

        // Edge was actually written.
        let doc = store.load().unwrap();
        assert_eq!(doc.edges.len(), 1);
        assert_eq!(doc.edges[0].blocked, "42");
        assert_eq!(doc.edges[0].blocked_on, "43");
    }

    #[test]
    fn add_dep_edge_calls_link_blocks_for_ticket_edges() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let tracker = CommentRecordingTracker::new();
        let outcome = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap();

        assert!(outcome.linked_in_tracker);
        let link_calls = tracker.link_blocks_calls();
        assert_eq!(link_calls.len(), 1);
        assert_eq!(link_calls[0], ("42".to_string(), "43".to_string()));
    }

    #[test]
    fn add_dep_edge_posts_only_blocked_side_comment_for_freeform_edge() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let tracker = CommentRecordingTracker::new();
        let outcome = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            freeform_edge("42", "apt-mirror"),
        )
        .unwrap();

        assert_eq!(outcome.comments_posted, 1);
        assert!(
            !outcome.linked_in_tracker,
            "freeform edges have no ticket to link against"
        );
        assert_eq!(
            tracker.link_blocks_calls().len(),
            0,
            "link_blocks must not fire for freeform edges"
        );
        let calls = tracker.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "42");
        // Comment uses the user-facing slug, not the on-disk `free:` form.
        assert!(
            calls[0].1.contains("`apt-mirror`"),
            "expected slug rendering, got: {}",
            calls[0].1
        );
        assert!(
            !calls[0].1.contains("free:apt-mirror"),
            "should strip the `free:` prefix for display, got: {}",
            calls[0].1
        );
    }

    #[test]
    fn add_dep_edge_skips_comments_and_link_blocks_on_idempotent_re_add() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let tracker = CommentRecordingTracker::new();

        // First call: posts both comments + link_blocks.
        add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap();
        assert_eq!(tracker.calls().len(), 2);
        assert_eq!(tracker.link_blocks_calls().len(), 1);

        // Second call with the same pair: no new comments, no
        // additional link_blocks invocation.
        let outcome = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap();
        assert!(outcome.already_present);
        assert_eq!(outcome.comments_posted, 0);
        assert!(!outcome.linked_in_tracker);
        assert_eq!(tracker.calls().len(), 2, "should not re-post on a re-add");
        assert_eq!(
            tracker.link_blocks_calls().len(),
            1,
            "should not re-link on a re-add"
        );
    }

    #[test]
    fn add_dep_edge_records_edge_when_no_tracker_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));

        let outcome = add_dep_edge(&store, None, dir.path(), ticket_edge("42", "43")).unwrap();

        assert!(!outcome.tracker_available);
        assert_eq!(outcome.comments_posted, 0);
        assert!(!outcome.linked_in_tracker);
        // The edge still landed; the dep graph is useful on its own.
        let doc = store.load().unwrap();
        assert_eq!(doc.edges.len(), 1);
    }

    #[test]
    fn add_dep_edge_bails_and_does_not_record_edge_when_comment_post_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        // First comment succeeds, second fails — exercises the "edge
        // not recorded after a partial comment posting" guarantee.
        let tracker = CommentRecordingTracker::failing_after(1);

        let err = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("blocked_on side"), "got: {err:#}");
        // link_blocks must not have fired — comments precede it in
        // the pipeline and a failure short-circuits the rest.
        assert_eq!(tracker.link_blocks_calls().len(), 0);

        // No edge was recorded — the user can retry once the tracker
        // is reachable again and both halves will land together.
        let doc = store.load().unwrap();
        assert_eq!(doc.edges.len(), 0);
    }

    #[test]
    fn add_dep_edge_bails_and_does_not_record_edge_when_link_blocks_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        // Comments succeed, link_blocks fails — the edge must not be
        // recorded so a retry sees no edge in deps.json and re-runs
        // the whole pipeline (comments included, accepting the
        // duplicate-comment risk in exchange for never leaving an
        // edge without a native tracker link).
        let tracker = CommentRecordingTracker::failing_link_blocks();

        let err = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("linking `42` as blocked by `43`"),
            "got: {err:#}"
        );
        assert_eq!(
            tracker.calls().len(),
            2,
            "both comments should fire before link_blocks"
        );
        assert_eq!(tracker.link_blocks_calls().len(), 1);
        let doc = store.load().unwrap();
        assert_eq!(doc.edges.len(), 0);
    }

    #[test]
    fn add_dep_edge_refuses_cycle_creating_edges_and_skips_comments() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        // Seed: 43 → 42 already exists; proposing 42 → 43 would close
        // a cycle.
        store.add_edge(ticket_edge("43", "42")).unwrap();
        let tracker = CommentRecordingTracker::new();

        let err = add_dep_edge(
            &store,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            ticket_edge("42", "43"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {err:#}");
        assert_eq!(
            tracker.calls().len(),
            0,
            "must not post comments before the cycle check passes"
        );
        assert_eq!(tracker.link_blocks_calls().len(), 0);
    }

    #[test]
    fn render_add_outcome_mentions_comments_and_tracker_link_for_ticket_edges() {
        let o = AddOutcome {
            blocked: "42".to_string(),
            blocked_on: "43".to_string(),
            reason: BlockedReason::Ticket,
            already_present: false,
            comments_posted: 2,
            linked_in_tracker: true,
            tracker_available: true,
        };
        let s = render_add_outcome(&o);
        assert!(s.contains("added edge: 42 → 43"), "got: {s}");
        assert!(s.contains("2 cross-link comments"), "got: {s}");
        assert!(s.contains("native tracker link"), "got: {s}");
    }

    #[test]
    fn render_add_outcome_omits_tracker_link_for_freeform_edges() {
        let o = AddOutcome {
            blocked: "42".to_string(),
            blocked_on: "free:apt-mirror".to_string(),
            reason: BlockedReason::Freeform,
            already_present: false,
            comments_posted: 1,
            linked_in_tracker: false,
            tracker_available: true,
        };
        let s = render_add_outcome(&o);
        assert!(s.contains("1 cross-link comment"), "got: {s}");
        assert!(
            !s.contains("native tracker link"),
            "freeform edges have no native link to mention, got: {s}"
        );
    }

    #[test]
    fn render_add_outcome_calls_out_missing_tracker() {
        let o = AddOutcome {
            blocked: "42".to_string(),
            blocked_on: "43".to_string(),
            reason: BlockedReason::Ticket,
            already_present: false,
            comments_posted: 0,
            linked_in_tracker: false,
            tracker_available: false,
        };
        let s = render_add_outcome(&o);
        assert!(s.contains("no tracker configured"), "got: {s}");
    }

    #[test]
    fn render_add_outcome_already_present_says_left_in_place() {
        let o = AddOutcome {
            blocked: "42".to_string(),
            blocked_on: "43".to_string(),
            reason: BlockedReason::Ticket,
            already_present: true,
            comments_posted: 0,
            linked_in_tracker: false,
            tracker_available: true,
        };
        let s = render_add_outcome(&o);
        assert!(s.contains("already recorded"), "got: {s}");
        assert!(s.contains("left in place"), "got: {s}");
    }
}
