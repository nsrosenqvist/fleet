//! Plans: named, ordered ticket lists the autonomous supervisor walks
//! through in sequence. The data layer for the orchestrator's
//! preference-vs-requirement story — plans encode *preference*
//! (this is the order I want these worked on); the deps graph in
//! [`crate::deps`] encodes *requirement* (this can't run until that
//! closes). The two compose at supervisor-scheduling time (Phase 4).
//!
//! Module shape mirrors [`crate::session`] and [`crate::deps`]:
//! - value-object types + serde here,
//! - [`store::PlanStore`] handles `.fleet/plans/<id>.yaml` persistence,
//! - id minting goes through [`PlanIdSource`] / [`ClockPlanIdSource`]
//!   so tests can pin id values without touching the wallclock.
//!
//! The supervisor's plan-aware scheduling and the TUI Plans view both
//! depend on this module but neither lives here.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::SessionId;

pub mod store;

/// Stable identifier for a plan. Same opaque-newtype contract as
/// [`SessionId`]: callers pass it around and key off it. Serialises
/// transparently as a JSON / YAML string so on-disk plan files
/// stay human-readable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(String);

impl PlanId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle state of a plan. `Active` is the only state the
/// supervisor considers for spawning; the other three are terminal
/// or paused. Transitions are user-driven via the `fleet plan`
/// CLI; the supervisor doesn't move plans between states by itself
/// (it only marks individual items completed/failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanState {
    Active,
    Paused,
    Completed,
    Abandoned,
}

/// What happens when a plan item's session fails. Configurable per
/// plan because some plans (a refactor where each step is
/// independent) want `continue`; most (a chained migration) want
/// `stop` so a human can decide before fleet proceeds.
///
/// `Stop` is the conservative default — chains where a mid-item
/// failure would corrupt later items shouldn't be override-able by
/// accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemFailurePolicy {
    /// Plan transitions to `Paused`, reason recorded. Default.
    #[default]
    Stop,
    /// Supervisor moves to the next item.
    Continue,
    /// Re-mark the item `Pending` once; second failure behaves as
    /// `Stop`.
    RetryOnce,
}

/// Per-item state. The supervisor advances items between states as
/// sessions complete; the `fleet plan` CLI surface lets users
/// mutate them directly (e.g. `fleet plan inject` adds a `Pending`
/// item, `abandon` doesn't change per-item state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanItemState {
    Pending,
    InProgress,
    Completed,
    Skipped,
    Failed,
}

/// One item in a plan: a single ticket the supervisor will spawn
/// a workflow against in sequence. `session_id` is populated by the
/// supervisor once it starts working the item; `injected` is `true`
/// for items the `tracker-create` workflow node added mid-flight
/// (so the TUI / CLI can flag them as "added during execution"
/// rather than part of the original plan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub ticket_id: String,
    pub state: PlanItemState,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub injected: bool,
}

impl PlanItem {
    /// Build a fresh pending item from a ticket id. Most callers
    /// (the `fleet plan new` CLI, the brainstorm agent) construct
    /// items this way; only the supervisor and the tracker-create
    /// plan-injector set the other fields directly.
    #[must_use]
    pub fn pending(ticket_id: impl Into<String>) -> Self {
        Self {
            ticket_id: ticket_id.into(),
            state: PlanItemState::Pending,
            session_id: None,
            injected: false,
        }
    }

    /// Same as [`Self::pending`] but stamped as injected by the
    /// workflow engine (tracker-create node).
    #[must_use]
    pub fn pending_injected(ticket_id: impl Into<String>) -> Self {
        Self {
            ticket_id: ticket_id.into(),
            state: PlanItemState::Pending,
            session_id: None,
            injected: true,
        }
    }
}

/// Reference to the tracker-native epic primitive this plan
/// shadows. GitHub uses "tracking issues" with task lists; Linear
/// has actual epics; git-bug uses a `parent:<id>` label
/// convention. v1 fleet doesn't auto-create these — the field is
/// reserved space the brainstorm agent populates in Phase 5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpicRef {
    /// Tracker plugin name (`"github"` / `"git-bug"` / future).
    pub tracker: String,
    /// Tracker-native identifier of the epic.
    pub id: String,
}

/// A complete plan: id, name, ordered items, current lifecycle
/// state, and timestamps. Persisted at
/// `.fleet/plans/<id>.yaml` (one file per plan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub name: String,
    pub items: Vec<PlanItem>,
    pub state: PlanState,
    #[serde(default)]
    pub on_item_failure: ItemFailurePolicy,
    #[serde(default)]
    pub epic_ref: Option<EpicRef>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Plan {
    /// Build a new plan from a name and an ordered ticket list.
    /// State defaults to `Active` — the `fleet plan new` CLI
    /// surface assumes the user wants the supervisor to pick the
    /// plan up immediately. Use the explicit constructor below
    /// (`paused`) when that isn't the case.
    #[must_use]
    pub fn new(
        id: PlanId,
        name: impl Into<String>,
        tickets: Vec<String>,
        created_at_ms: u64,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            items: tickets.into_iter().map(PlanItem::pending).collect(),
            state: PlanState::Active,
            on_item_failure: ItemFailurePolicy::default(),
            epic_ref: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }

    /// Position of the item with `ticket_id` in `items`, if any.
    /// Used by the tracker-create plan-injector to find where to
    /// slot a new follow-up ticket.
    #[must_use]
    pub fn position_of(&self, ticket_id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.ticket_id == ticket_id)
    }

    /// True iff every item is `Completed`. Used by the supervisor
    /// to advance the plan to `PlanState::Completed` when it
    /// finishes the last item.
    #[must_use]
    pub fn all_items_done(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .iter()
                .all(|i| i.state == PlanItemState::Completed)
    }

    /// (#completed, #total). The Plans CLI / TUI surface this as
    /// `n/N` for progress display.
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .items
            .iter()
            .filter(|i| i.state == PlanItemState::Completed)
            .count();
        (done, self.items.len())
    }
}

/// Source for newly-minted plan ids. Behind a trait so tests don't
/// depend on wallclock time and aren't subject to collisions when
/// many plans are minted in the same millisecond. Same shape as
/// [`crate::session::IdSource`].
pub trait PlanIdSource: Send + Sync {
    fn mint(&self) -> PlanId;
}

/// Production ID source: millisecond Unix timestamp + an in-process
/// monotonic counter, packed as `plan-<13 hex>-<4 hex>`. Mirrors
/// the session id source exactly.
#[derive(Debug, Default)]
pub struct ClockPlanIdSource;

impl PlanIdSource for ClockPlanIdSource {
    fn mint(&self) -> PlanId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        PlanId::new(format!("plan-{ms:013x}-{n:04x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_id_round_trips_through_string() {
        let id = PlanId::new("plan-abc");
        assert_eq!(id.as_str(), "plan-abc");
        assert_eq!(format!("{id}"), "plan-abc");
    }

    #[test]
    fn plan_id_serialises_transparently_as_a_string() {
        let id = PlanId::new("plan-1");
        let yaml = serde_yml::to_string(&id).unwrap();
        // serde_yml renders unquoted bare strings when possible.
        assert!(yaml.contains("plan-1"), "yaml: {yaml}");
        let round: PlanId = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(round, id);
    }

    #[test]
    fn clock_plan_id_source_mints_unique_ids_in_a_burst() {
        // Many mints in the same ms must stay unique thanks to the
        // counter half of the format.
        let src = ClockPlanIdSource;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            assert!(seen.insert(src.mint()));
        }
    }

    #[test]
    fn clock_plan_id_source_uses_the_plan_prefix_and_hex_segments() {
        let src = ClockPlanIdSource;
        let id = src.mint();
        let s = id.as_str();
        assert!(
            s.starts_with("plan-"),
            "id should be `plan-<hex>-<hex>`, got {s}"
        );
        let rest = &s["plan-".len()..];
        let mut parts = rest.split('-');
        let ms = parts.next().expect("ms segment");
        let counter = parts.next().expect("counter segment");
        assert!(parts.next().is_none(), "exactly two hex segments");
        assert_eq!(
            ms.len(),
            13,
            "ms segment should be 13 hex chars, got `{ms}`"
        );
        assert_eq!(
            counter.len(),
            4,
            "counter should be 4 hex chars, got `{counter}`"
        );
        assert!(ms.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(counter.chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn sample_plan() -> Plan {
        Plan::new(
            PlanId::new("plan-1"),
            "Parser refactor",
            vec!["42".into(), "43".into(), "44".into()],
            1_700_000_000_000,
        )
    }

    #[test]
    fn plan_new_defaults_state_to_active() {
        let p = sample_plan();
        assert_eq!(p.state, PlanState::Active);
    }

    #[test]
    fn plan_new_defaults_on_item_failure_to_stop() {
        let p = sample_plan();
        assert_eq!(p.on_item_failure, ItemFailurePolicy::Stop);
    }

    #[test]
    fn plan_new_constructs_pending_items_in_declared_order() {
        let p = sample_plan();
        assert_eq!(p.items.len(), 3);
        for (idx, expected) in ["42", "43", "44"].iter().enumerate() {
            assert_eq!(p.items[idx].ticket_id, *expected);
            assert_eq!(p.items[idx].state, PlanItemState::Pending);
            assert!(p.items[idx].session_id.is_none());
            assert!(!p.items[idx].injected);
        }
    }

    #[test]
    fn position_of_returns_index_for_known_ticket() {
        let p = sample_plan();
        assert_eq!(p.position_of("43"), Some(1));
        assert_eq!(p.position_of("nope"), None);
    }

    #[test]
    fn all_items_done_is_false_for_a_fresh_plan() {
        assert!(!sample_plan().all_items_done());
    }

    #[test]
    fn all_items_done_is_true_only_when_every_item_completed() {
        let mut p = sample_plan();
        for item in &mut p.items {
            item.state = PlanItemState::Completed;
        }
        assert!(p.all_items_done());
        // One item flipped back → no longer done.
        p.items[1].state = PlanItemState::InProgress;
        assert!(!p.all_items_done());
    }

    #[test]
    fn all_items_done_is_false_for_an_empty_plan() {
        // An items-less plan is degenerate; treat it as not-done so
        // a future supervisor doesn't auto-transition empty plans
        // to Completed.
        let mut p = sample_plan();
        p.items.clear();
        assert!(!p.all_items_done());
    }

    #[test]
    fn progress_counts_completed_items() {
        let mut p = sample_plan();
        p.items[0].state = PlanItemState::Completed;
        p.items[1].state = PlanItemState::InProgress;
        assert_eq!(p.progress(), (1, 3));
    }

    #[test]
    fn plan_item_pending_injected_sets_the_flag() {
        let item = PlanItem::pending_injected("51");
        assert!(item.injected);
        assert_eq!(item.state, PlanItemState::Pending);
        assert_eq!(item.ticket_id, "51");
    }

    #[test]
    fn plan_round_trips_through_yaml_with_full_field_set() {
        // Lock in the on-disk shape: kebab-case state strings,
        // optional epic_ref, item injected flag.
        let mut p = sample_plan();
        p.items[0].state = PlanItemState::Completed;
        p.items[0].session_id = Some(SessionId::new("s-abc"));
        p.items[1].injected = true;
        p.state = PlanState::Paused;
        p.on_item_failure = ItemFailurePolicy::RetryOnce;
        p.epic_ref = Some(EpicRef {
            tracker: "github".into(),
            id: "200".into(),
        });
        p.updated_at_ms = 1_700_000_001_000;

        let yaml = serde_yml::to_string(&p).unwrap();
        let round: Plan = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(round, p);
        // Spot-check the enum rendering — the YAML must use the
        // tagged-lowercase form so external tools can grep `state:
        // paused` reliably.
        assert!(yaml.contains("state: paused"), "yaml: {yaml}");
        assert!(yaml.contains("on_item_failure: retry-once"), "yaml: {yaml}");
        assert!(yaml.contains("state: completed"), "yaml: {yaml}");
        assert!(yaml.contains("state: pending"), "yaml: {yaml}");
    }

    #[test]
    fn plan_yaml_omits_default_epic_ref_but_retains_it_when_set() {
        let p_without = sample_plan();
        let yaml = serde_yml::to_string(&p_without).unwrap();
        // serde's `#[serde(default)]` doesn't *skip* a None
        // serialisation by default; we accept either "epic_ref: null"
        // or absence. Round-trip is the contract — either way the
        // loaded value matches.
        let round: Plan = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(round.epic_ref, None);
    }

    #[test]
    fn plan_yaml_tolerates_missing_optional_fields_on_load() {
        // A minimal hand-written plan file the user might commit
        // before the brainstorm agent fills in epic_ref. Optional
        // fields default cleanly.
        let yaml = "\
id: plan-1
name: x
items:
  - { ticket_id: '42', state: pending }
state: active
created_at_ms: 0
updated_at_ms: 0
";
        let p: Plan = serde_yml::from_str(yaml).unwrap();
        assert_eq!(p.items.len(), 1);
        assert!(p.items[0].session_id.is_none());
        assert!(!p.items[0].injected);
        assert_eq!(p.on_item_failure, ItemFailurePolicy::Stop);
        assert!(p.epic_ref.is_none());
    }
}
