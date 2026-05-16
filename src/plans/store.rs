//! Persistence for plans: one `<id>.yaml` per plan under
//! `<fleet_root>/.fleet/plans/`.
//!
//! Shape mirrors [`crate::session::store::SessionStore`] and
//! [`crate::deps::DepsStore`]: the store knows about disk layout and
//! atomic write semantics; the value object validates itself. The
//! `for_repo` / `at` split exposes the canonical layout to production
//! callers while keeping tests cheap (tempdir-rooted stores).
//!
//! Single-writer semantics: fleet is the only writer to a given
//! `.fleet/plans/` directory by design (the existing per-repo fleet
//! invariant). Concurrent fleet processes in the same repo aren't
//! defended against — that's a wider invariant covered by the same
//! reasoning behind `SessionStore`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use super::{Plan, PlanId, PlanState};

/// Persistence adapter for `<fleet_root>/.fleet/plans/<id>.yaml`. Holds
/// the directory; stateless across calls (each load/save reads/writes
/// the YAML file rather than caching in memory). Plans mutate
/// infrequently enough that the cache cost wouldn't pay off, and the
/// stateless shape keeps testing trivial.
///
/// Id minting lives at the call site (see [`super::PlanIdSource`] /
/// [`super::ClockPlanIdSource`]) rather than on the store, matching
/// the `SessionStore` shape — keeps the store dyn-Trait-free so it
/// can derive `Debug`/`Clone` and the `Send + Sync` contracts other
/// fleet stores inherit.
#[derive(Debug, Clone)]
pub struct PlanStore {
    root: PathBuf,
}

impl PlanStore {
    /// Open a store rooted at a specific directory. Tests use this
    /// to point at a tempdir; production goes through
    /// [`Self::for_repo`].
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Open a store anchored at `<fleet_root>/.fleet/plans/` — the
    /// canonical location alongside `sessions/` and `deps.json`.
    #[must_use]
    pub fn for_repo(fleet_root: impl AsRef<Path>) -> Self {
        Self::at(fleet_root.as_ref().join(".fleet").join("plans"))
    }

    /// Directory the store reads from / writes to. Exposed for
    /// diagnostics + tests.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the YAML file for a given plan id.
    #[must_use]
    pub fn plan_path(&self, id: &PlanId) -> PathBuf {
        self.root.join(format!("{}.yaml", id.as_str()))
    }

    /// Materialise a brand-new plan on disk. Errors when a plan
    /// with this id already exists — callers that meant to update
    /// should use [`Self::save`].
    pub fn create(&self, plan: &Plan) -> Result<()> {
        let path = self.plan_path(&plan.id);
        if path.exists() {
            bail!(
                "plan {} already exists on disk at {}",
                plan.id,
                path.display()
            );
        }
        self.save(plan)
    }

    /// Atomically replace the on-disk plan. Materialises the parent
    /// directory on first save. Writes via tmp + rename so a crash
    /// mid-write leaves the previous file intact.
    pub fn save(&self, plan: &Plan) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating plans dir at {}", self.root.display()))?;
        let path = self.plan_path(&plan.id);
        let body = serde_yml::to_string(plan)
            .with_context(|| format!("serialising plan {} to YAML", plan.id))?;
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing tmp plan file at {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Load a single plan by id. A missing file errors — unlike
    /// [`crate::deps::DepsStore::load`], plans don't have a
    /// "implicitly empty" fallback because the caller asked for a
    /// specific id and would only have that id if it was minted at
    /// some point.
    pub fn load(&self, id: &PlanId) -> Result<Plan> {
        let path = self.plan_path(id);
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading plan file at {}", path.display()))?;
        let plan: Plan = serde_yml::from_str(&body)
            .with_context(|| format!("parsing plan YAML at {}", path.display()))?;
        Ok(plan)
    }

    /// Ids of all plans on disk. Returns an empty Vec when the
    /// directory doesn't exist yet (fresh repo). Sorted by file name,
    /// which is lexicographically ≈ chronological thanks to the ms-
    /// prefixed id format.
    pub fn list(&self) -> Result<Vec<PlanId>> {
        let read = match std::fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("reading plans dir at {}", self.root.display()));
            }
        };
        let mut ids: Vec<PlanId> = Vec::new();
        for entry in read {
            let entry =
                entry.with_context(|| format!("reading entry in {}", self.root.display()))?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                // Non-UTF8 file names aren't ours — skip.
                continue;
            };
            // Skip the tmp side files atomic-write leaves on crash,
            // and anything that isn't a plan YAML.
            if let Some(stem) = name_str.strip_suffix(".yaml") {
                ids.push(PlanId::new(stem));
            }
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }

    /// Convenience: load every plan whose state is [`PlanState::Active`].
    /// Used by the supervisor's scheduling tick (Phase 4) and the
    /// tracker-create plan-injector. Plans that fail to load surface
    /// the error rather than being silently skipped — a broken plan
    /// file is a louder problem than a misordered queue.
    pub fn list_active(&self) -> Result<Vec<Plan>> {
        let mut active = Vec::new();
        for id in self.list()? {
            let plan = self.load(&id)?;
            if plan.state == PlanState::Active {
                active.push(plan);
            }
        }
        Ok(active)
    }

    /// First active plan containing `ticket_id`, with the matching
    /// item's index. Used by the `tracker-create` node's plan-
    /// injection path (Phase 3 commit 4). Returns `None` when no
    /// active plan owns the ticket — the new follow-up is filed
    /// without plan-side bookkeeping in that case.
    ///
    /// Plans-load failures bubble: if any plan file is unparseable,
    /// the caller sees the error rather than getting a stale "not
    /// found" answer.
    pub fn find_plan_containing(&self, ticket_id: &str) -> Result<Option<(Plan, usize)>> {
        for plan in self.list_active()? {
            if let Some(idx) = plan.position_of(ticket_id) {
                return Ok(Some((plan, idx)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::{ItemFailurePolicy, PlanItem, PlanItemState};

    fn sample_plan(id: &str) -> Plan {
        Plan::new(
            PlanId::new(id),
            "Parser refactor",
            vec!["42".into(), "43".into(), "44".into()],
            1_700_000_000_000,
        )
    }

    #[test]
    fn for_repo_anchors_at_fleet_plans() {
        let store = PlanStore::for_repo("/repo");
        assert_eq!(store.root(), Path::new("/repo/.fleet/plans"));
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let mut plan = sample_plan("plan-1");
        plan.items[0].state = PlanItemState::Completed;
        plan.on_item_failure = ItemFailurePolicy::Continue;
        store.save(&plan).unwrap();
        let loaded = store.load(&PlanId::new("plan-1")).unwrap();
        assert_eq!(loaded, plan);
    }

    #[test]
    fn save_creates_the_plans_directory_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path().join("nested-not-yet-existing"));
        store.save(&sample_plan("plan-1")).unwrap();
        assert!(store.root().is_dir());
    }

    #[test]
    fn create_errors_when_plan_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let plan = sample_plan("plan-existing");
        store.create(&plan).unwrap();
        let err = store.create(&plan).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn load_errors_with_path_context_for_missing_plan() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let err = store.load(&PlanId::new("plan-nope")).unwrap_err();
        assert!(format!("{err:#}").contains("plan-nope.yaml"));
    }

    #[test]
    fn list_returns_empty_for_a_missing_plans_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path().join("never-created"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_returns_ids_sorted_lexicographically() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        // Save in reverse-chronological order; list should still
        // return them sorted ascending. With the ms-prefixed id
        // format this matches chronological order in practice.
        store.save(&sample_plan("plan-3")).unwrap();
        store.save(&sample_plan("plan-1")).unwrap();
        store.save(&sample_plan("plan-2")).unwrap();
        let ids = store.list().unwrap();
        assert_eq!(
            ids,
            vec![
                PlanId::new("plan-1"),
                PlanId::new("plan-2"),
                PlanId::new("plan-3"),
            ]
        );
    }

    #[test]
    fn list_skips_non_yaml_entries_and_tmp_side_files() {
        // Atomic-write tmp files (`.yaml.tmp`) shouldn't show up in
        // `list`; readme-style sidecars shouldn't either.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("plan-1.yaml.tmp"), "junk").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        let store = PlanStore::at(dir.path());
        store.save(&sample_plan("plan-1")).unwrap();
        // Even though tmp + README exist, list returns only plan-1.
        assert_eq!(store.list().unwrap(), vec![PlanId::new("plan-1")]);
    }

    #[test]
    fn list_active_filters_to_active_plans_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let mut active = sample_plan("plan-a");
        let mut paused = sample_plan("plan-b");
        paused.state = PlanState::Paused;
        let mut completed = sample_plan("plan-c");
        completed.state = PlanState::Completed;
        active.state = PlanState::Active;
        store.save(&active).unwrap();
        store.save(&paused).unwrap();
        store.save(&completed).unwrap();
        let actives = store.list_active().unwrap();
        assert_eq!(actives.len(), 1);
        assert_eq!(actives[0].id, PlanId::new("plan-a"));
    }

    #[test]
    fn find_plan_containing_returns_first_active_match_with_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let plan = sample_plan("plan-1");
        store.save(&plan).unwrap();
        let (found, idx) = store.find_plan_containing("43").unwrap().unwrap();
        assert_eq!(found.id, PlanId::new("plan-1"));
        assert_eq!(idx, 1);
    }

    #[test]
    fn find_plan_containing_returns_none_when_no_plan_owns_the_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        store.save(&sample_plan("plan-1")).unwrap();
        assert!(store.find_plan_containing("999").unwrap().is_none());
    }

    #[test]
    fn find_plan_containing_skips_non_active_plans() {
        // A ticket living in a Paused plan shouldn't be reported as
        // a hit — the supervisor doesn't act on paused plans, and
        // the tracker-create plan-injector would otherwise inject
        // into a plan the user has explicitly halted.
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let mut plan = sample_plan("plan-1");
        plan.state = PlanState::Paused;
        store.save(&plan).unwrap();
        assert!(store.find_plan_containing("43").unwrap().is_none());
    }

    #[test]
    fn save_overwrites_existing_plan_atomically() {
        // The supervisor and the tracker-create plan-injector both
        // call save() multiple times on the same plan to update
        // item states. Lock the round-trip in.
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::at(dir.path());
        let mut plan = sample_plan("plan-1");
        store.save(&plan).unwrap();
        plan.items.push(PlanItem::pending_injected("99"));
        plan.updated_at_ms = 1_700_000_001_000;
        store.save(&plan).unwrap();
        let loaded = store.load(&PlanId::new("plan-1")).unwrap();
        assert_eq!(loaded.items.len(), 4);
        assert!(loaded.items[3].injected);
        assert_eq!(loaded.items[3].ticket_id, "99");
        assert_eq!(loaded.updated_at_ms, 1_700_000_001_000);
    }
}
