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

use anyhow::{Context, Result, bail};

use crate::deps::{BlockedReason, DepEdge, DepsDoc, DepsStore, would_create_cycle};
use crate::repo;
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
/// original `created_at_ms`.
pub fn run_add(
    blocked: &str,
    blocked_on: Option<&str>,
    blocked_on_tag: Option<&str>,
) -> Result<i32> {
    let edge = build_edge(blocked, blocked_on, blocked_on_tag)?;
    let store = open_store()?;
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
    store
        .add_edge(edge.clone())
        .with_context(|| format!("recording edge `{} → {}`", edge.blocked, edge.blocked_on))?;
    if already_present {
        println!(
            "edge `{} → {}` already recorded — left in place",
            edge.blocked, edge.blocked_on
        );
    } else {
        println!("added edge: {} → {}", edge.blocked, edge.blocked_on);
    }
    Ok(0)
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
fn resolve_blocked_on(
    blocked_on: Option<&str>,
    blocked_on_tag: Option<&str>,
) -> Result<String> {
    match (blocked_on, blocked_on_tag) {
        (Some(id), None) if !id.is_empty() => Ok(id.to_string()),
        (None, Some(tag)) if !tag.is_empty() => Ok(format!("free:{tag}")),
        (Some(_), Some(_)) => bail!(
            "pass either --blocked-on <ticket-id> or --blocked-on-tag <slug>, not both"
        ),
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
        (Some(_), Some(_)) => bail!(
            "pass either --blocked-on <ticket-id> or --blocked-on-tag <slug>, not both"
        ),
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
        let _ = writeln!(
            out,
            "  {} → {}  [{kind}]",
            edge.blocked, edge.blocked_on
        );
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
        assert!(out.contains("44 → free:apt-mirror  [freeform]"), "got: {out}");
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
        assert!(
            err.to_string().contains("not both"),
            "got: {err}"
        );
    }

    #[test]
    fn build_edge_refuses_neither_flag_passed() {
        let err = build_edge("42", None, None).unwrap_err();
        assert!(
            err.to_string().contains("--blocked-on"),
            "got: {err}"
        );
    }

    #[test]
    fn build_edge_refuses_empty_blocked_id() {
        let err = build_edge("", Some("43"), None).unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "got: {err}"
        );
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
}
