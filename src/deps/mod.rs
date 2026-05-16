//! Cross-session dependency graph persisted at `.fleet/deps.json`.
//!
//! One file per repo, owned by the workflow engine + the autonomous
//! supervisor. The store records "ticket X is blocked on Y" edges so
//! the supervisor's scheduler can skip blocked tickets and the TUI
//! can surface the relationship.
//!
//! The shape is intentionally narrow: a flat list of edges with a
//! version stamp on the outer document. Cycle detection, advanced
//! queries, and freeform-edge auto-clear all land in later phases —
//! v2 of the doc format can extend the schema without breaking
//! older fleet builds because deserialisation tolerates unknown
//! fields (`#[serde(default)]` plus the version stamp).
//!
//! Single-writer semantics: same as `SessionStore`. Multiple fleet
//! processes mutating the same repo concurrently is out of scope
//! for v1; we rely on the existing one-fleet-per-repo invariant.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Why the edge exists. Drives later auto-clearing behaviour: a
/// `Ticket` edge clears when the supervisor sees the referenced
/// ticket close; a `Freeform` edge only clears when a user calls
/// `fleet sessions unblock`. The supervisor consumes that
/// distinction in Phase 4 — for v1, the store just records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockedReason {
    Ticket,
    Freeform,
}

/// One blocked-on edge: ticket `blocked` cannot proceed until
/// `blocked_on` resolves. `blocked_on` is a ticket id when `reason
/// == Ticket`, or a `free:<slug>` tag when `reason == Freeform`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepEdge {
    pub blocked: String,
    pub blocked_on: String,
    pub reason: BlockedReason,
    pub created_at_ms: u64,
}

/// On-disk document shape. `version` is bumped whenever the schema
/// breaks compatibility; v1 fleet only writes v1 docs and refuses
/// to load anything with a higher number so a downgrade doesn't
/// silently truncate edges it doesn't understand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepsDoc {
    pub version: u32,
    #[serde(default)]
    pub edges: Vec<DepEdge>,
}

/// Current schema version. Bump on any incompatible change to
/// [`DepEdge`] or [`DepsDoc`]. Backwards-compatible additions (new
/// optional fields) don't require a bump.
pub const DEPS_SCHEMA_VERSION: u32 = 1;

impl DepsDoc {
    /// Fresh document with no edges. Used as the implicit value
    /// returned by [`DepsStore::load`] when the on-disk file doesn't
    /// exist yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: DEPS_SCHEMA_VERSION,
            edges: Vec::new(),
        }
    }
}

impl Default for DepsDoc {
    fn default() -> Self {
        Self::empty()
    }
}

/// Persistence adapter for `.fleet/deps.json`. Holds the file path;
/// stateless across calls (the document is read/written each time
/// rather than cached in memory) because writes are infrequent and
/// the alternative (in-memory caching with manual invalidation)
/// doesn't pay for itself yet.
#[derive(Debug, Clone)]
pub struct DepsStore {
    path: PathBuf,
}

impl DepsStore {
    /// Open a store at an explicit file path. Tests use this to point
    /// at a tempdir; the production CLI uses [`Self::for_repo`].
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Open a store rooted at `<fleet_root>/.fleet/deps.json` — the
    /// canonical location. Not yet called from production code path
    /// (the executor derives the deps path from its `SessionStore`
    /// to avoid plumbing another value through `ExecuteRequest`);
    /// the CLI / TUI consume this directly in later phases.
    #[must_use]
    #[allow(dead_code)]
    pub fn for_repo(fleet_root: impl AsRef<Path>) -> Self {
        Self::at(fleet_root.as_ref().join(".fleet").join("deps.json"))
    }

    /// Path of the backing JSON file. Exposed for diagnostics
    /// (`fleet doctor` and friends) and tests.
    #[must_use]
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the current document. A missing file is not an error —
    /// it returns [`DepsDoc::empty`], matching the "no edges yet"
    /// state new repos start in. Any other I/O or parse failure
    /// surfaces with file-path context so the caller sees where to
    /// look. Refuses documents with a higher schema version than
    /// this fleet build knows about, to avoid silently truncating
    /// edges fleet doesn't understand on the rewrite path.
    pub fn load(&self) -> Result<DepsDoc> {
        let body = match std::fs::read_to_string(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DepsDoc::empty()),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("reading deps file at {}", self.path.display()));
            }
        };
        let doc: DepsDoc = serde_json::from_str(&body)
            .with_context(|| format!("parsing deps JSON at {}", self.path.display()))?;
        if doc.version > DEPS_SCHEMA_VERSION {
            anyhow::bail!(
                "deps file at {} has schema version {} but this fleet build only \
                 understands up to {}; refusing to mutate to avoid truncating \
                 unknown fields",
                self.path.display(),
                doc.version,
                DEPS_SCHEMA_VERSION
            );
        }
        Ok(doc)
    }

    /// Atomically replace the on-disk document. Materialises the
    /// parent directory if needed (the typical `.fleet/` hierarchy
    /// already exists, but a brand-new repo's first save creates
    /// it). Writes via a tmp file + rename for the
    /// crash-during-write story.
    pub fn save(&self, doc: &DepsDoc) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory at {}", parent.display()))?;
        }
        let body =
            serde_json::to_string_pretty(doc).with_context(|| "serialising deps doc to JSON")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing tmp deps file at {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), self.path.display()))?;
        Ok(())
    }

    /// Convenience: load → push edge → save. The plan's typical
    /// caller — `tracker-create` filing a parent→child block — only
    /// ever appends, so wrapping the read-modify-write keeps that
    /// site one line. De-duplicates on the exact `(blocked,
    /// blocked_on)` tuple so a re-run of the same workflow node
    /// doesn't double-count the edge; the existing entry's
    /// `created_at_ms` is preserved.
    pub fn add_edge(&self, edge: DepEdge) -> Result<()> {
        let mut doc = self.load()?;
        let already = doc
            .edges
            .iter()
            .any(|e| e.blocked == edge.blocked && e.blocked_on == edge.blocked_on);
        if !already {
            doc.edges.push(edge);
            self.save(&doc)?;
        }
        Ok(())
    }

    /// Remove every edge where `blocked_id` appears on the
    /// `blocked` side. Used by `fleet sessions unblock` to clear
    /// all dependencies a stuck session is waiting on, in one
    /// atomic write. Returns the count of edges removed; zero is
    /// not an error (the session may have had no edges to begin
    /// with).
    ///
    /// `blocked_on` (the right-hand side) is left untouched —
    /// auto-clearing a `Ticket` edge when its target ticket closes
    /// is the supervisor's job (Phase 4), not this store's.
    #[allow(dead_code)]
    pub fn remove_edges_for_blocked(&self, blocked_id: &str) -> Result<usize> {
        let mut doc = self.load()?;
        let before = doc.edges.len();
        doc.edges.retain(|e| e.blocked != blocked_id);
        let removed = before - doc.edges.len();
        if removed > 0 {
            self.save(&doc)?;
        }
        Ok(removed)
    }
}

/// Would adding `proposed` to `doc` close a cycle? Returns `true`
/// iff a path already exists from `proposed.blocked_on` back to
/// `proposed.blocked` — adding the new edge would then form
/// `proposed.blocked → proposed.blocked_on → … → proposed.blocked`.
///
/// Pure function over the document so callers (tracker-create's
/// plan-injector, future manual-unblock surgery) can dry-run a
/// proposed mutation before persisting. BFS over the edge list;
/// freeform-tag right-hand sides terminate naturally because no
/// edge has `blocked == "free:<slug>"`.
///
/// Self-loops (`A → A`) are reported as cycles — `A` is trivially
/// reachable from itself, and a self-blocked ticket isn't useful
/// to record either way.
#[must_use]
#[allow(dead_code)]
pub fn would_create_cycle(doc: &DepsDoc, proposed: &DepEdge) -> bool {
    if proposed.blocked == proposed.blocked_on {
        return true;
    }
    // BFS from `proposed.blocked_on` outward, walking
    // `blocked -> blocked_on` edges. If we reach
    // `proposed.blocked`, the proposed edge would close a cycle.
    let mut frontier: Vec<&str> = vec![proposed.blocked_on.as_str()];
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    seen.insert(proposed.blocked_on.as_str());
    while let Some(current) = frontier.pop() {
        if current == proposed.blocked {
            return true;
        }
        for edge in &doc.edges {
            if edge.blocked == current && seen.insert(edge.blocked_on.as_str()) {
                frontier.push(edge.blocked_on.as_str());
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edge(blocked: &str, blocked_on: &str) -> DepEdge {
        DepEdge {
            blocked: blocked.to_string(),
            blocked_on: blocked_on.to_string(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn load_returns_empty_doc_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let doc = store.load().unwrap();
        assert_eq!(doc.version, DEPS_SCHEMA_VERSION);
        assert!(doc.edges.is_empty());
    }

    #[test]
    fn for_repo_anchors_at_fleet_deps_json() {
        let store = DepsStore::for_repo("/repo");
        assert_eq!(store.path(), Path::new("/repo/.fleet/deps.json"));
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        let mut doc = DepsDoc::empty();
        doc.edges.push(sample_edge("42", "43"));
        doc.edges.push(sample_edge("42", "44"));
        store.save(&doc).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 2);
        assert_eq!(loaded.edges[0].blocked, "42");
        assert_eq!(loaded.edges[1].blocked_on, "44");
    }

    #[test]
    fn save_creates_parent_directories_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Path nested two levels deep — the typical `.fleet/`
        // hierarchy may not exist on a brand-new repo until fleet
        // first writes to it.
        let store = DepsStore::at(dir.path().join("a").join("b").join("deps.json"));
        store.save(&DepsDoc::empty()).unwrap();
        assert!(store.path().exists());
    }

    #[test]
    fn add_edge_appends_when_new() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("42", "43")).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
        assert_eq!(loaded.edges[0].blocked, "42");
        assert_eq!(loaded.edges[0].blocked_on, "43");
    }

    #[test]
    fn add_edge_is_idempotent_on_repeated_calls() {
        // Re-running a tracker-create node mustn't double-count the
        // edge — the second insert with the same (blocked,
        // blocked_on) tuple is a no-op.
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("42", "43")).unwrap();
        store.add_edge(sample_edge("42", "43")).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
    }

    #[test]
    fn add_edge_distinguishes_different_blocked_on_for_same_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("42", "43")).unwrap();
        store.add_edge(sample_edge("42", "44")).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 2);
    }

    #[test]
    fn add_edge_preserves_existing_timestamp_on_duplicate_insert() {
        // The dedupe is by `(blocked, blocked_on)`, so an attempted
        // re-insert with a fresher timestamp must NOT overwrite —
        // the edge's identity is its endpoints, not its time.
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store
            .add_edge(DepEdge {
                blocked: "42".to_string(),
                blocked_on: "43".to_string(),
                reason: BlockedReason::Ticket,
                created_at_ms: 1_000,
            })
            .unwrap();
        store
            .add_edge(DepEdge {
                blocked: "42".to_string(),
                blocked_on: "43".to_string(),
                reason: BlockedReason::Ticket,
                created_at_ms: 2_000,
            })
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
        assert_eq!(loaded.edges[0].created_at_ms, 1_000);
    }

    #[test]
    fn load_refuses_future_schema_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deps.json");
        std::fs::write(
            &path,
            r#"{"version":99,"edges":[{"blocked":"42","blocked_on":"43","reason":"ticket","created_at_ms":1}]}"#,
        )
        .unwrap();
        let store = DepsStore::at(&path);
        let err = store.load().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("schema version 99"), "got: {msg}");
        assert!(msg.contains("refusing to mutate"), "got: {msg}");
    }

    #[test]
    fn freeform_reason_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store
            .add_edge(DepEdge {
                blocked: "42".to_string(),
                blocked_on: "free:apt-mirror".to_string(),
                reason: BlockedReason::Freeform,
                created_at_ms: 1,
            })
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges[0].reason, BlockedReason::Freeform);
        // And the on-disk JSON spells it lowercase, matching the
        // serde attribute on the enum.
        let body = std::fs::read_to_string(store.path()).unwrap();
        assert!(body.contains(r#""reason": "freeform""#), "body: {body}");
    }

    #[test]
    fn remove_edges_for_blocked_clears_matches_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("42", "43")).unwrap();
        store.add_edge(sample_edge("42", "44")).unwrap();
        store.add_edge(sample_edge("43", "44")).unwrap();
        let removed = store.remove_edges_for_blocked("42").unwrap();
        assert_eq!(removed, 2);
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
        // The unrelated 43→44 edge survives.
        assert_eq!(loaded.edges[0].blocked, "43");
        assert_eq!(loaded.edges[0].blocked_on, "44");
    }

    #[test]
    fn remove_edges_for_blocked_returns_zero_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("42", "43")).unwrap();
        assert_eq!(store.remove_edges_for_blocked("999").unwrap(), 0);
        // File untouched because no save happens.
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
    }

    #[test]
    fn remove_edges_for_blocked_only_clears_left_hand_side() {
        // The 43→42 edge should *not* be removed when we unblock 42 —
        // its right-hand side mentions 42, but auto-clearing
        // those is the supervisor's job, not the store's.
        let dir = tempfile::tempdir().unwrap();
        let store = DepsStore::at(dir.path().join("deps.json"));
        store.add_edge(sample_edge("43", "42")).unwrap();
        store.add_edge(sample_edge("42", "44")).unwrap();
        let removed = store.remove_edges_for_blocked("42").unwrap();
        assert_eq!(removed, 1);
        let loaded = store.load().unwrap();
        assert_eq!(loaded.edges.len(), 1);
        assert_eq!(loaded.edges[0].blocked, "43");
        assert_eq!(loaded.edges[0].blocked_on, "42");
    }

    #[test]
    fn would_create_cycle_detects_direct_back_edge() {
        // Existing: 43→42. Proposing 42→43 closes a 2-cycle.
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![sample_edge("43", "42")],
        };
        assert!(would_create_cycle(&doc, &sample_edge("42", "43")));
    }

    #[test]
    fn would_create_cycle_detects_transitive_cycle() {
        // Chain: 43→44, 44→42. Proposing 42→43 closes
        // 42→43→44→42.
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![sample_edge("43", "44"), sample_edge("44", "42")],
        };
        assert!(would_create_cycle(&doc, &sample_edge("42", "43")));
    }

    #[test]
    fn would_create_cycle_returns_false_for_a_safe_new_edge() {
        // Existing: 43→44. Proposing 42→43 is safe (no back-path).
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![sample_edge("43", "44")],
        };
        assert!(!would_create_cycle(&doc, &sample_edge("42", "43")));
    }

    #[test]
    fn would_create_cycle_returns_false_for_an_empty_graph() {
        let doc = DepsDoc::empty();
        assert!(!would_create_cycle(&doc, &sample_edge("42", "43")));
    }

    #[test]
    fn would_create_cycle_treats_freeform_tags_as_terminal() {
        // `free:apt-mirror` has no outgoing edge (nothing blocks
        // *on* it being something else), so a proposed
        // 42 → free:apt-mirror is always safe.
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![DepEdge {
                blocked: "43".to_string(),
                blocked_on: "42".to_string(),
                reason: BlockedReason::Ticket,
                created_at_ms: 1,
            }],
        };
        let proposed = DepEdge {
            blocked: "42".to_string(),
            blocked_on: "free:apt-mirror".to_string(),
            reason: BlockedReason::Freeform,
            created_at_ms: 1,
        };
        assert!(!would_create_cycle(&doc, &proposed));
    }

    #[test]
    fn would_create_cycle_reports_self_loops_as_cycles() {
        // Self-loop is degenerate but unambiguously cyclic — refuse.
        let doc = DepsDoc::empty();
        assert!(would_create_cycle(&doc, &sample_edge("42", "42")));
    }

    #[test]
    fn would_create_cycle_handles_branching_paths_without_false_positives() {
        // 43 → 44; 43 → 45; 45 → 50. Proposing 42→43 is safe (no
        // path back to 42 from any branch).
        let doc = DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges: vec![
                sample_edge("43", "44"),
                sample_edge("43", "45"),
                sample_edge("45", "50"),
            ],
        };
        assert!(!would_create_cycle(&doc, &sample_edge("42", "43")));
    }

    #[test]
    fn load_tolerates_missing_edges_array() {
        // Older fleet builds (pre this commit) never wrote a
        // deps.json. Future versions may write one with just
        // `{"version":1}` (e.g. as a marker). The default tag on
        // `edges` covers that case — the field is optional.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deps.json");
        std::fs::write(&path, r#"{"version":1}"#).unwrap();
        let store = DepsStore::at(&path);
        let doc = store.load().unwrap();
        assert!(doc.edges.is_empty());
    }
}
