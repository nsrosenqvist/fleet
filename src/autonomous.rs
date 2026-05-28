//! Autonomous-mode supervisor engine.
//!
//! Drives the TUI's `Shift+A` mode: scan the configured tracker for
//! open issues, pick one that isn't already claimed by an in-flight
//! session, and surface a [`SpawnCommand`] the caller can detach as a
//! `fleet workflow run` subprocess.
//!
//! Pure state machine. The engine doesn't talk to the tracker or
//! spawn subprocesses itself — it takes an in-flight session slice
//! and a `list_open_issues` closure, returns an
//! [`AutonomousOutcome`], and mutates its own debounce state. The
//! caller (the TUI today, a CLI daemon in Phase 3) does the I/O.
//!
//! Debouncing:
//! - **Scan interval** (configurable, default 10s): minimum time
//!   between tracker calls. Real `gh` / `git-bug` shells cost
//!   hundreds of ms each; the TUI's event loop ticks ~10× per
//!   second, so without the interval we'd hammer the tracker.
//! - **Spawn cooldown** (configurable, default 2s): how long to
//!   wait after firing a spawn before considering the next slot.
//!   `fleet workflow run` takes a moment to create its session row
//!   on disk; the cooldown gives the in-flight count time to
//!   reflect reality before claim-avoidance runs again.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::deps::{BlockedReason, DepsDoc};
use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanItemState, PlanState};
use crate::repo_config::AutonomousConfig;
use crate::session::{IssueContext, Session, SessionState};

/// How long an in-engine spawn claim survives before being treated
/// as stale (the subprocess presumably crashed during fork and
/// never wrote meta.json). 5 minutes covers the worst-case fresh
/// devcontainer build + image pull without forever blocking a
/// genuinely failed spawn from being retried.
const SPAWN_GRACE: Duration = Duration::from_secs(5 * 60);

/// State carried across ticks. Created once, toggled on/off via
/// [`Self::toggle`], and stepped each event-loop iteration.
pub struct AutonomousEngine {
    enabled: bool,
    last_scan_at: Option<Instant>,
    last_spawn_at: Option<Instant>,
    /// Tickets the engine has emitted a `Spawn` outcome for
    /// recently. Bridges the window between the caller forking the
    /// subprocess and the subprocess writing `meta.json` — during
    /// that window the `in_flight` session list contains no row for
    /// the new session, so the claim filter alone can't see the
    /// pending work. Entries expire after [`SPAWN_GRACE`] so a
    /// genuinely failed spawn doesn't block the ticket forever.
    pending_claims: HashMap<String, Instant>,
    /// Human-readable line for the TUI status bar. Updated by
    /// [`Self::step`] each time it decides anything interesting.
    status: String,
}

impl Default for AutonomousEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AutonomousEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: false,
            last_scan_at: None,
            last_spawn_at: None,
            pending_claims: HashMap::new(),
            status: String::from("autonomous: OFF"),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Latest human-readable status. The TUI surfaces this in the
    /// bottom bar so the operator knows why nothing is happening
    /// (or that everything is).
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Flip the mode. Resets the debounce clocks so the very next
    /// `step` after enabling runs a fresh scan instead of waiting
    /// out a stale interval.
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.last_scan_at = None;
        self.last_spawn_at = None;
        // Drop stale pending claims on toggle — a flip to OFF then
        // back ON is the user explicitly resetting, and any stuck
        // claim from a previous run shouldn't carry over.
        self.pending_claims.clear();
        self.status = if self.enabled {
            String::from("autonomous: ON")
        } else {
            String::from("autonomous: OFF")
        };
    }

    /// Release the engine's pending claim on `ticket_id`. Called by
    /// the dispatcher when the actual `subprocess.spawn()` fails —
    /// without this the engine would refuse to retry the same ticket
    /// until [`SPAWN_GRACE`] elapsed.
    pub fn clear_pending_claim(&mut self, ticket_id: &str) {
        self.pending_claims.remove(ticket_id);
    }

    /// Override the status line. Used by the TUI when the engine is
    /// disabled but the supervisor needs to surface info (e.g.
    /// "tracker unavailable; cannot enable").
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
    }

    /// Drive one supervisor tick. Returns the action the caller should
    /// take. Idempotent within a debounce window — calling it 100
    /// times in a row with the same `now` won't trigger extra scans
    /// or spawns.
    ///
    /// `list_open_issues` is a closure rather than a trait dependency
    /// so tests stub it out without standing up a fake tracker.
    pub fn step(
        &mut self,
        now: Instant,
        config: &AutonomousConfig,
        in_flight: &[Session],
        list_open_issues: impl FnOnce() -> Result<Vec<IssueContext>, String>,
    ) -> AutonomousOutcome {
        if !self.enabled {
            return AutonomousOutcome::Idle;
        }

        // Spawn cooldown: never spawn twice in quick succession even
        // if the in-flight count somehow looks slot-available.
        if let Some(last) = self.last_spawn_at {
            let cooldown = Duration::from_secs(u64::from(config.spawn_cooldown_secs));
            if now.duration_since(last) < cooldown {
                // No status update — we're just waiting for the
                // child's session row to materialise.
                return AutonomousOutcome::Idle;
            }
        }

        // Scan interval: skip the (potentially expensive) tracker
        // call if we ran one recently.
        if let Some(last) = self.last_scan_at {
            let interval = Duration::from_secs(u64::from(config.scan_interval_secs));
            if now.duration_since(last) < interval {
                return AutonomousOutcome::Idle;
            }
        }
        self.last_scan_at = Some(now);

        // Slot check: count Running + AwaitingGate as occupying a slot.
        // Created is transient (Session::new -> Running happens within
        // one event-loop tick); terminal states free their slot.
        let in_flight_count = in_flight
            .iter()
            .filter(|s| matches!(s.state, SessionState::Running | SessionState::AwaitingGate))
            .count();
        let cap_usize = config.max_parallel as usize;
        if in_flight_count >= cap_usize {
            self.status = format!(
                "autonomous: ON · {in_flight_count}/{cap} in-flight (max reached)",
                cap = config.max_parallel,
            );
            return AutonomousOutcome::WaitedFor(PauseReason::SlotsExhausted {
                in_flight: in_flight_count,
                cap: config.max_parallel,
            });
        }

        // Tracker scan + claim filter.
        let open_issues = match list_open_issues() {
            Ok(v) => v,
            Err(err) => {
                self.status = format!("autonomous: ON · tracker error: {err}");
                return AutonomousOutcome::WaitedFor(PauseReason::TrackerError(err));
            }
        };
        // Sweep expired pending claims before reading them so a stuck
        // ticket can be retried after the grace period without an
        // explicit `clear_pending_claim` call.
        self.pending_claims
            .retain(|_, t| now.duration_since(*t) < SPAWN_GRACE);
        // Only sessions still doing work hold their issue's claim.
        // Failed / Completed / Crashed sessions are terminal — leaving
        // them in `claimed` would silently block a `fleet plan retry`
        // (or hand-restart of the same ticket) from ever spawning,
        // because the engine would treat the dead session's bound
        // issue as already-in-progress on every tick. Same predicate
        // as the slot-count above, intentionally — "in-flight for
        // slot accounting" and "in-flight for claim accounting" are
        // the same set.
        //
        // Union with `pending_claims`: tickets the engine has *just*
        // dispatched a Spawn for but whose subprocess hasn't written
        // meta.json yet. Without this the next tick's scan (10s by
        // default) can fire before the new session is visible on
        // disk and re-spawn the same ticket — observed as "two
        // workers on the same ticket in parallel."
        let mut claimed: std::collections::HashSet<String> = in_flight
            .iter()
            .filter(|s| matches!(s.state, SessionState::Running | SessionState::AwaitingGate))
            .filter_map(|s| s.issue.as_ref().map(|i| i.id.clone()))
            .collect();
        claimed.extend(self.pending_claims.keys().cloned());
        let Some(candidate) = open_issues.into_iter().find(|i| !claimed.contains(&i.id)) else {
            self.status = format!(
                "autonomous: ON · {in_flight_count}/{cap} in-flight · no unclaimed open issues",
                cap = config.max_parallel,
            );
            return AutonomousOutcome::WaitedFor(PauseReason::NoUnclaimedIssues);
        };

        // Pick a workflow for this specific issue. The default and
        // any user-defined `routing:` rules in config are consulted
        // by `resolve_workflow_for` — if no rule matches the issue's
        // labels, `config.workflow` is the fallback. This is the
        // only place the engine reads label data; rule order is the
        // config author's contract.
        let workflow = config.resolve_workflow_for(&candidate.labels).to_string();

        // Found a candidate. Set the spawn timestamp + pending claim
        // BEFORE returning so a panic in the caller's spawn path
        // doesn't leak into a tight retry loop on the next tick.
        // The caller is expected to call `clear_pending_claim` if
        // the subprocess.spawn() itself errors so the ticket frees
        // up before SPAWN_GRACE expires.
        self.last_spawn_at = Some(now);
        self.pending_claims.insert(candidate.id.clone(), now);
        self.status = format!(
            "autonomous: ON · spawned `{workflow}` for #{human}",
            human = candidate.human_id,
        );
        AutonomousOutcome::Spawn(SpawnCommand {
            workflow,
            issue: candidate,
        })
    }
}

/// Reorder candidate open issues so active-plan items come first,
/// in plan order, with the oldest plan considered first. Items that
/// would be blocked by an open dependency (or a freeform tag) are
/// dropped from the candidate list entirely — they're never going
/// to make progress on this tick.
///
/// Pure free function so both call sites (CLI's `tick` + TUI's
/// `autonomous_tick`) share the same logic, and tests can pin the
/// behaviour without standing up the full closure.
///
/// Plans considered: `Active` only. Items considered: `Pending`
/// only (items already `InProgress` are presumably claimed by an
/// in-flight session; `Completed`/`Skipped`/`Failed` are done). The
/// fallback "any open issue" tail preserves the pre-plans
/// behaviour for tickets not currently owned by any active plan.
#[must_use]
pub fn rank_candidates_by_plan(
    open: Vec<IssueContext>,
    plans: &[Plan],
    deps: &DepsDoc,
) -> Vec<IssueContext> {
    use std::collections::HashSet;
    // Tickets that some plan considers Completed. Used to logically
    // close out blockers that are still `open` in the tracker (the
    // workflow's create-pr / close-ticket step was skipped, or the
    // tracker is updated out-of-band) so downstream items can move.
    // Without this, "ticket A blocks B" keeps B pinned forever once
    // A is plan-Completed but the tracker hasn't caught up.
    let plan_completed: HashSet<String> = plans
        .iter()
        .flat_map(|p| p.items.iter())
        .filter(|it| it.state == PlanItemState::Completed)
        .map(|it| it.ticket_id.clone())
        .collect();
    // `open_set` for the blocker check subtracts plan-completed
    // tickets so a logically-done blocker no longer holds its
    // dependents. The candidate iteration below still walks the raw
    // `open` slice — we want to prioritize / pick from currently
    // tracker-open tickets, just not consider plan-done ones as
    // active blockers.
    let open_set: HashSet<String> = open
        .iter()
        .map(|i| i.human_id.clone())
        .filter(|id| !plan_completed.contains(id))
        .collect();
    let is_blocked = |ticket: &str| ticket_is_blocked(ticket, &open_set, deps);

    // Tickets that appear as a `Failed` item in any plan are
    // suppressed: a failure on those tickets means the user must
    // explicitly `fleet plan retry` (or Shift+R) before autonomous
    // re-tries. Without this gate the engine spins through fresh
    // sessions every scan interval against the same broken ticket,
    // accumulating Failed sessions on disk indefinitely. The
    // suppression covers all plan states — Failed items in
    // Active plans still need human attention; Paused plans signal
    // even more strongly that autonomous should keep its hands off.
    let suppressed_by_failure: HashSet<String> = plans
        .iter()
        .flat_map(|p| p.items.iter())
        .filter(|it| it.state == PlanItemState::Failed)
        .map(|it| it.ticket_id.clone())
        .collect();

    // Tickets that any plan has already addressed — Completed (a
    // prior session finished successfully), InProgress (a session is
    // working on it, the claim filter would also catch this but
    // belt-and-braces against a stale session pointer), or Skipped
    // (user marked it as not needed). Without this, a ticket the
    // tracker still shows as `open` (because the workflow's open_pr
    // step was skipped, or because the user closes tickets manually
    // out-of-band) silently gets re-spawned on the next tick after
    // its first session completes — observed as "two agents working
    // on the same plan item in parallel."
    let already_addressed: HashSet<String> = plans
        .iter()
        .flat_map(|p| p.items.iter())
        .filter(|it| {
            matches!(
                it.state,
                PlanItemState::Completed | PlanItemState::InProgress | PlanItemState::Skipped,
            )
        })
        .map(|it| it.ticket_id.clone())
        .collect();

    let mut prioritized: Vec<IssueContext> = Vec::new();
    let mut leftover = open;

    // Active plans, oldest-first. The plan author's expectation is
    // that A → B → C means A goes first; multi-plan ordering is
    // determined by which plan was created first (a v1 convention
    // that buys deterministic behaviour without needing a separate
    // priority field).
    let mut sorted_plans: Vec<&Plan> = plans
        .iter()
        .filter(|p| p.state == PlanState::Active)
        .collect();
    sorted_plans.sort_by_key(|p| p.created_at_ms);

    for plan in sorted_plans {
        for item in &plan.items {
            if item.state != PlanItemState::Pending {
                continue;
            }
            if is_blocked(&item.ticket_id) {
                continue;
            }
            if suppressed_by_failure.contains(&item.ticket_id) {
                continue;
            }
            if let Some(pos) = leftover.iter().position(|i| i.human_id == item.ticket_id) {
                prioritized.push(leftover.remove(pos));
            }
        }
    }
    // Append non-blocked leftover (the any-open-issue fallback) so
    // tickets not currently in any plan still get picked once the
    // plan queue is exhausted. `already_addressed` mirrors the
    // plan-loop's "Pending only" filter so a ticket the prioritized
    // pass *deliberately* skipped (because its item is already done)
    // doesn't sneak back in through the fallback.
    leftover.retain(|i| {
        !is_blocked(&i.human_id)
            && !suppressed_by_failure.contains(&i.human_id)
            && !already_addressed.contains(&i.human_id)
    });
    prioritized.extend(leftover);
    prioritized
}

/// Walk active plans and advance each item's state + `session_id` to
/// reflect the newest session bound to its ticket. Persists only
/// plans that actually changed; returns the count of plans saved.
///
/// Resolution rule: for a given (plan, item), the newest session
/// bound to `item.ticket_id` wins (highest `updated_at_ms`). This
/// makes re-spawn-after-Failed cleanly update the item to the new
/// state.
///
/// Mapping:
/// - `SessionState::Running` | `AwaitingGate` → `PlanItemState::InProgress`
/// - `SessionState::Completed` → `PlanItemState::Completed`
/// - `SessionState::Failed` | `Crashed` → `PlanItemState::Failed`
/// - `SessionState::Created` → no-op (transient — `Created` normally
///   transitions to `Running` within a tick; updating items on
///   `Created` would just flicker the state to `InProgress` and back).
///
/// Plan-level state transitions (Active → Paused on a Failed item,
/// per the `on_item_failure` policy) land in the next commit; this
/// one only advances *item* state.
pub fn reconcile_plans_from_sessions(
    sessions: &[Session],
    store: &PlanStore,
    now_ms: u64,
) -> Result<usize> {
    let plan_ids = store.list()?;
    let mut saved = 0;
    for plan_id in plan_ids {
        let mut plan = match store.load(&plan_id) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(plan = %plan_id, error = %err, "reconcile: skipping unreadable plan");
                continue;
            }
        };
        let changed = reconcile_one_plan(sessions, &mut plan);
        if changed {
            plan.updated_at_ms = now_ms;
            store.save(&plan).with_context_for_plan(&plan_id)?;
            saved += 1;
        }
    }
    Ok(saved)
}

/// Pure mutation half of [`reconcile_plans_from_sessions`]. Updates
/// `plan.items` against `sessions` and returns true iff anything
/// changed. Factored out for testability — the store-touching
/// half is then a thin wrapper.
///
/// When an item transitions from a non-Failed state into Failed,
/// the plan's `on_item_failure` policy fires:
///
/// - `Stop` (default): item recorded as Failed; plan transitions
///   to `PlanState::Paused`. User decides whether to retry / edit
///   / abandon.
/// - `Continue`: item recorded as Failed; plan stays `Active` so
///   the next pending item gets picked up.
/// - `RetryOnce`: on the first failure for this item, the item is
///   re-marked `Pending` and `retry_count` bumps to 1 so the
///   supervisor re-spawns it. A second failure (`retry_count >= 1`)
///   falls through to `Stop`.
#[must_use]
pub fn reconcile_one_plan(sessions: &[Session], plan: &mut Plan) -> bool {
    let mut changed = false;
    let policy = plan.on_item_failure;
    // Sessions older than the plan's last user-driven mutation are
    // "stale" — `fleet plan retry` / inject / pause-resume all bump
    // `plan.updated_at_ms`, and after a retry the dead Failed
    // session still lives on disk with its own (older) timestamp.
    // Without this filter, reconcile would walk the dead session,
    // see "newest for ticket #X is Failed", re-apply the Stop
    // policy, and quietly undo the retry the user just performed.
    let watermark = plan.updated_at_ms;
    for item_idx in 0..plan.items.len() {
        let ticket_id = plan.items[item_idx].ticket_id.clone();
        let newest = sessions
            .iter()
            .filter(|s| s.issue.as_ref().is_some_and(|i| i.human_id == ticket_id))
            .filter(|s| s.updated_at_ms > watermark)
            .max_by_key(|s| s.updated_at_ms);
        let Some(session) = newest else {
            continue;
        };
        let Some(target_state) = session_to_item_state(session.state) else {
            continue;
        };
        let was_state = plan.items[item_idx].state;
        let session_id_match = plan.items[item_idx].session_id.as_ref() == Some(&session.id);
        if was_state == target_state && session_id_match {
            continue;
        }

        // Newly transitioning into Failed → apply the failure
        // policy. `was_state == Failed` means the item was already
        // failed on a previous reconcile pass — the policy fired
        // then; don't fire it again.
        if target_state == PlanItemState::Failed && was_state != PlanItemState::Failed {
            apply_failure_policy(plan, item_idx, &session.id, policy);
            changed = true;
            continue;
        }

        let item = &mut plan.items[item_idx];
        item.state = target_state;
        item.session_id = Some(session.id.clone());
        changed = true;
    }
    changed
}

/// Apply the `on_item_failure` policy to an item that just
/// transitioned into Failed. See [`reconcile_one_plan`] for the
/// per-variant semantics.
fn apply_failure_policy(
    plan: &mut Plan,
    item_idx: usize,
    session_id: &crate::session::SessionId,
    policy: crate::plans::ItemFailurePolicy,
) {
    use crate::plans::ItemFailurePolicy;
    match policy {
        ItemFailurePolicy::Stop => {
            plan.items[item_idx].state = PlanItemState::Failed;
            plan.items[item_idx].session_id = Some(session_id.clone());
            plan.state = PlanState::Paused;
        }
        ItemFailurePolicy::Continue => {
            plan.items[item_idx].state = PlanItemState::Failed;
            plan.items[item_idx].session_id = Some(session_id.clone());
            // Plan stays Active — next pending item gets picked up.
        }
        ItemFailurePolicy::RetryOnce => {
            if plan.items[item_idx].retry_count == 0 {
                // First failure: mark Pending, bump counter, keep
                // the failed session id so the TUI / `fleet plan
                // show` can surface "tried once, failed". The
                // supervisor will re-spawn on the next tick.
                plan.items[item_idx].state = PlanItemState::Pending;
                plan.items[item_idx].session_id = Some(session_id.clone());
                plan.items[item_idx].retry_count = 1;
            } else {
                // Second failure: behave as Stop.
                plan.items[item_idx].state = PlanItemState::Failed;
                plan.items[item_idx].session_id = Some(session_id.clone());
                plan.state = PlanState::Paused;
            }
        }
    }
}

#[must_use]
fn session_to_item_state(state: SessionState) -> Option<PlanItemState> {
    match state {
        SessionState::Running | SessionState::AwaitingGate => Some(PlanItemState::InProgress),
        SessionState::Completed => Some(PlanItemState::Completed),
        SessionState::Failed | SessionState::Crashed => Some(PlanItemState::Failed),
        SessionState::Created => None,
    }
}

/// Internal extension trait that annotates save errors with the
/// plan id without sprinkling `with_context` calls everywhere.
trait WithPlanContext<T> {
    fn with_context_for_plan(self, plan_id: &crate::plans::PlanId) -> Result<T>;
}

impl<T> WithPlanContext<T> for Result<T> {
    fn with_context_for_plan(self, plan_id: &crate::plans::PlanId) -> Self {
        use anyhow::Context as _;
        self.with_context(|| format!("saving plan `{plan_id}` after reconcile"))
    }
}

/// `true` iff `ticket` is currently blocked per the deps graph:
/// either a `Freeform` edge keyed by it exists, or a `Ticket` edge
/// keyed by it points at a ticket that's still in `open_set`. A
/// `Ticket` edge whose target has closed (no longer open) is
/// considered cleared by the scheduler — the deps store keeps the
/// edge for audit, but it doesn't block scheduling any more.
#[must_use]
pub fn ticket_is_blocked(
    ticket: &str,
    open_set: &std::collections::HashSet<String>,
    deps: &DepsDoc,
) -> bool {
    deps.edges.iter().any(|e| {
        if e.blocked != ticket {
            return false;
        }
        match e.reason {
            BlockedReason::Freeform => true,
            BlockedReason::Ticket => open_set.contains(&e.blocked_on),
        }
    })
}

/// Returned by [`AutonomousEngine::step`]. Indicates whether the
/// caller should detach a workflow-run subprocess, and (when the
/// engine waited) why no action was taken.
#[derive(Debug, PartialEq, Eq)]
pub enum AutonomousOutcome {
    /// Engine disabled or debouncing; nothing to do this tick.
    Idle,
    /// Caller should fire `fleet workflow run <workflow> --issue <human_id>`.
    Spawn(SpawnCommand),
    /// Scan ran but didn't dispatch; the engine's `status` field
    /// carries the human-readable line.
    WaitedFor(PauseReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub workflow: String,
    pub issue: IssueContext,
}

/// Why a scan finished without spawning. Mirrored in
/// [`AutonomousEngine::status`] for the TUI; exposed as an enum so
/// future code (tests, structured logs, a TUI badge) can branch on
/// the reason.
#[derive(Debug, PartialEq, Eq)]
pub enum PauseReason {
    SlotsExhausted { in_flight: usize, cap: u32 },
    NoUnclaimedIssues,
    TrackerError(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_config::RoutingRule;
    use crate::session::SessionId;

    fn cfg() -> AutonomousConfig {
        AutonomousConfig::default()
    }

    fn t0() -> Instant {
        Instant::now()
    }

    fn issue(human: &str) -> IssueContext {
        IssueContext {
            id: format!("gh:{human}"),
            human_id: human.to_string(),
            title: format!("Issue {human}"),
            labels: Vec::new(),
        }
    }

    fn issue_with_labels(human: &str, labels: &[&str]) -> IssueContext {
        IssueContext {
            id: format!("gh:{human}"),
            human_id: human.to_string(),
            title: format!("Issue {human}"),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// Build an in-flight (Running) session bound to the given issue.
    fn in_flight_session(id: &str, claimed: Option<&str>) -> Session {
        let mut s = Session::new(SessionId::new(id), "standard", 0);
        s.transition_to(SessionState::Running, 1).unwrap();
        if let Some(h) = claimed {
            s.issue = Some(issue(h));
        }
        s
    }

    fn terminal_session(id: &str, claimed: Option<&str>, state: SessionState) -> Session {
        let mut s = Session::new(SessionId::new(id), "standard", 0);
        s.transition_to(SessionState::Running, 1).unwrap();
        s.transition_to(state, 2).unwrap();
        if let Some(h) = claimed {
            s.issue = Some(issue(h));
        }
        s
    }

    #[test]
    fn disabled_engine_is_idle_regardless_of_state() {
        let mut e = AutonomousEngine::new();
        assert!(!e.enabled());
        let out = e.step(t0(), &cfg(), &[], || Ok(vec![issue("1")]));
        assert_eq!(out, AutonomousOutcome::Idle);
        // Status untouched.
        assert_eq!(e.status(), "autonomous: OFF");
    }

    #[test]
    fn toggle_flips_enabled_and_resets_debounce_clocks() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        assert!(e.enabled());
        assert_eq!(e.status(), "autonomous: ON");
        e.toggle();
        assert!(!e.enabled());
    }

    #[test]
    fn step_spawns_for_first_unclaimed_open_issue() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let out = e.step(t0(), &cfg(), &[], || Ok(vec![issue("42"), issue("43")]));
        match out {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(cmd.workflow, "standard");
                assert_eq!(cmd.issue.human_id, "42");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert!(e.status().contains("spawned `standard` for #42"));
    }

    #[test]
    fn step_skips_issues_already_claimed_by_in_flight_sessions() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let in_flight = vec![in_flight_session("s-1", Some("42"))];
        let out = e.step(t0(), &cfg(), &in_flight, || {
            Ok(vec![issue("42"), issue("43")])
        });
        match out {
            AutonomousOutcome::Spawn(cmd) => assert_eq!(cmd.issue.human_id, "43"),
            other => panic!("expected Spawn(#43), got {other:?}"),
        }
    }

    #[test]
    fn step_suppresses_recently_spawned_ticket_until_grace_expires() {
        // Regression: between dispatch_autonomous_spawn forking the
        // subprocess and the subprocess writing meta.json, the
        // in_flight slice contains no session for the ticket. The
        // next tick (10s later) would see the ticket as unclaimed
        // and spawn a second worker against the same plan item.
        // The engine now records its own pending claim that bridges
        // the gap.
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg();
        let now = Instant::now();
        // First tick spawns for #42.
        let out = e.step(now, &cfg, &[], || Ok(vec![issue("42"), issue("43")]));
        assert!(matches!(out, AutonomousOutcome::Spawn(ref c) if c.issue.human_id == "42"));

        // Second tick, AFTER the scan interval but BEFORE the subprocess
        // has written meta.json (so in_flight is still empty). The
        // pending claim must suppress #42 — engine picks #43 instead.
        let out2 = e.step(
            now + Duration::from_secs(15),
            &cfg,
            &[],
            || Ok(vec![issue("42"), issue("43")]),
        );
        assert!(
            matches!(out2, AutonomousOutcome::Spawn(ref c) if c.issue.human_id == "43"),
            "expected Spawn(#43) — #42's pending claim should suppress it, got {out2:?}",
        );
    }

    #[test]
    fn step_pending_claim_expires_after_grace_period() {
        // If the subprocess crashes during fork and never writes
        // meta.json, the pending claim must eventually expire so the
        // ticket can be retried. 5 minutes is the grace.
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg();
        let now = Instant::now();
        let _ = e.step(now, &cfg, &[], || Ok(vec![issue("42")]));
        // 6 minutes later — grace expired, no real session ever
        // showed up, ticket should be eligible again.
        let out = e.step(
            now + Duration::from_secs(6 * 60),
            &cfg,
            &[],
            || Ok(vec![issue("42")]),
        );
        assert!(
            matches!(out, AutonomousOutcome::Spawn(ref c) if c.issue.human_id == "42"),
            "ticket should be eligible after grace period: {out:?}",
        );
    }

    #[test]
    fn clear_pending_claim_releases_ticket_for_immediate_retry() {
        // When the caller's subprocess.spawn() itself fails, the
        // dispatcher calls clear_pending_claim so the ticket isn't
        // stuck until SPAWN_GRACE elapses.
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg();
        let now = Instant::now();
        let out = e.step(now, &cfg, &[], || Ok(vec![issue("42")]));
        let cmd = match out {
            AutonomousOutcome::Spawn(c) => c,
            other => panic!("expected Spawn, got {other:?}"),
        };
        e.clear_pending_claim(&cmd.issue.id);
        // Immediately after the scan interval, #42 is eligible again.
        let out2 = e.step(now + Duration::from_secs(15), &cfg, &[], || {
            Ok(vec![issue("42")])
        });
        assert!(matches!(out2, AutonomousOutcome::Spawn(ref c) if c.issue.human_id == "42"));
    }

    #[test]
    fn step_terminal_sessions_do_not_block_their_issues_from_respawning() {
        // Regression for the retry-after-failure bug: a Failed session
        // bound to #42 used to keep #42 in the "claimed" set, so a
        // subsequent `fleet plan retry` (which flips the plan item
        // back to Pending but leaves the dead session in place for
        // forensics) would never see the engine respawn the ticket.
        // Only Running / AwaitingGate sessions hold the claim now.
        let mut e = AutonomousEngine::new();
        e.toggle();
        let sessions = vec![
            terminal_session("s-old", Some("42"), SessionState::Failed),
            terminal_session("s-crash", Some("42"), SessionState::Crashed),
        ];
        let out = e.step(t0(), &cfg(), &sessions, || Ok(vec![issue("42")]));
        match out {
            AutonomousOutcome::Spawn(cmd) => assert_eq!(cmd.issue.human_id, "42"),
            other => panic!("expected Spawn(#42), got {other:?}"),
        }
    }

    #[test]
    fn step_returns_no_unclaimed_when_every_open_issue_is_in_flight() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let in_flight = vec![
            in_flight_session("s-1", Some("42")),
            in_flight_session("s-2", Some("43")),
        ];
        let out = e.step(t0(), &cfg(), &in_flight, || {
            Ok(vec![issue("42"), issue("43")])
        });
        assert_eq!(
            out,
            AutonomousOutcome::WaitedFor(PauseReason::NoUnclaimedIssues)
        );
        assert!(
            e.status().contains("no unclaimed open issues"),
            "got: {}",
            e.status()
        );
    }

    #[test]
    fn step_returns_slots_exhausted_at_max_parallel() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.max_parallel = 1;
        let in_flight = vec![in_flight_session("s-1", Some("99"))];
        // Tracker has other issues, but we're at slot cap — must NOT
        // even call the closure (it's a FnOnce; tracking that here
        // would be brittle, so just assert the outcome).
        let out = e.step(t0(), &cfg, &in_flight, || Ok(vec![issue("42")]));
        assert!(
            matches!(
                out,
                AutonomousOutcome::WaitedFor(PauseReason::SlotsExhausted {
                    in_flight: 1,
                    cap: 1
                })
            ),
            "got: {out:?}"
        );
        assert!(e.status().contains("1/1 in-flight"), "got: {}", e.status());
    }

    #[test]
    fn step_terminal_sessions_do_not_count_against_slots() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.max_parallel = 1;
        // One completed (slot freed) + one failed → in-flight count is 0.
        let in_flight = vec![
            terminal_session("s-done", Some("1"), SessionState::Completed),
            terminal_session("s-fail", Some("2"), SessionState::Failed),
        ];
        let out = e.step(t0(), &cfg, &in_flight, || Ok(vec![issue("3")]));
        // Slot is open; spawns for issue 3.
        assert!(matches!(out, AutonomousOutcome::Spawn(_)));
    }

    #[test]
    fn step_awaiting_gate_sessions_count_against_slots() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.max_parallel = 1;
        let in_flight = vec![terminal_session(
            "s-paused",
            Some("1"),
            SessionState::AwaitingGate,
        )];
        let out = e.step(t0(), &cfg, &in_flight, || Ok(vec![issue("2")]));
        // Gated session still holds the slot.
        assert!(matches!(
            out,
            AutonomousOutcome::WaitedFor(PauseReason::SlotsExhausted { .. })
        ));
    }

    #[test]
    fn step_surfaces_tracker_errors_with_message() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let out = e.step(t0(), &cfg(), &[], || {
            Err("gh: not authenticated".to_string())
        });
        match out {
            AutonomousOutcome::WaitedFor(PauseReason::TrackerError(msg)) => {
                assert!(msg.contains("not authenticated"));
            }
            other => panic!("expected tracker error, got {other:?}"),
        }
        assert!(e.status().contains("tracker error"));
    }

    #[test]
    fn step_respects_scan_interval_between_calls() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg(); // 10s default
        let now = Instant::now();
        // First call hits the tracker (no last_scan_at yet).
        let _ = e.step(now, &cfg, &[], || Ok(vec![]));
        // Second call within the interval → Idle, closure NOT called.
        let mut called = false;
        let out = e.step(now + Duration::from_secs(1), &cfg, &[], || {
            called = true;
            Ok(vec![])
        });
        assert_eq!(out, AutonomousOutcome::Idle);
        assert!(
            !called,
            "tracker closure must NOT be called inside scan interval"
        );
    }

    #[test]
    fn step_runs_after_scan_interval_elapses() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg();
        let now = Instant::now();
        let _ = e.step(now, &cfg, &[], || Ok(vec![]));
        let mut called = false;
        let out = e.step(now + Duration::from_secs(11), &cfg, &[], || {
            called = true;
            Ok(vec![issue("1")])
        });
        assert!(called, "tracker must be called after the interval elapsed");
        assert!(matches!(out, AutonomousOutcome::Spawn(_)));
    }

    #[test]
    fn step_respects_spawn_cooldown_after_dispatch() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let cfg = cfg();
        let now = Instant::now();
        // First step spawns.
        let out = e.step(now, &cfg, &[], || Ok(vec![issue("1")]));
        assert!(matches!(out, AutonomousOutcome::Spawn(_)));
        // Within the cooldown, even though the scan interval may
        // have been bypassed by toggle's reset on next enable, we
        // still must NOT spawn again.
        let mut called = false;
        let out = e.step(now + Duration::from_millis(500), &cfg, &[], || {
            called = true;
            Ok(vec![issue("2")])
        });
        assert_eq!(out, AutonomousOutcome::Idle);
        assert!(!called, "tracker must NOT be called inside spawn cooldown");
    }

    #[test]
    fn step_resumes_scanning_after_spawn_cooldown_elapses() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.scan_interval_secs = 0; // strip the scan-interval gate for this test
        let now = Instant::now();
        let _ = e.step(now, &cfg, &[], || Ok(vec![issue("1")]));
        // After cooldown elapses, a second spawn is allowed (assuming
        // there's an unclaimed issue and a slot).
        let out = e.step(
            now + Duration::from_secs(3),
            &cfg,
            // Caller hasn't observed the previous spawn yet — pretend
            // no in-flight session exists. That's the worst case we
            // need to handle: cooldown lets the child write its row,
            // but we deliberately model the "still racing" window
            // here.
            &[],
            || Ok(vec![issue("2")]),
        );
        assert!(matches!(out, AutonomousOutcome::Spawn(cmd) if cmd.issue.human_id == "2"));
    }

    #[test]
    fn step_honours_configured_max_parallel() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.max_parallel = 5;
        let in_flight: Vec<Session> = (0..3)
            .map(|i| in_flight_session(&format!("s-{i}"), Some(&i.to_string())))
            .collect();
        // 3/5 in-flight → should still spawn.
        let out = e.step(t0(), &cfg, &in_flight, || Ok(vec![issue("99")]));
        assert!(matches!(out, AutonomousOutcome::Spawn(cmd) if cmd.issue.human_id == "99"));
    }

    #[test]
    fn step_picks_first_unclaimed_in_caller_supplied_order() {
        // The engine doesn't sort; it trusts the closure to return
        // issues in priority order (tracker plugins already
        // open-first-sort their results).
        let mut e = AutonomousEngine::new();
        e.toggle();
        let in_flight = vec![in_flight_session("s-1", Some("42"))];
        let out = e.step(t0(), &cfg(), &in_flight, || {
            Ok(vec![issue("42"), issue("99"), issue("100")])
        });
        match out {
            AutonomousOutcome::Spawn(cmd) => assert_eq!(cmd.issue.human_id, "99"),
            other => panic!("expected #99 (first unclaimed), got {other:?}"),
        }
    }

    #[test]
    fn step_uses_configured_workflow_name_for_spawn_command() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.workflow = "hotfix".to_string();
        let out = e.step(t0(), &cfg, &[], || Ok(vec![issue("7")]));
        match out {
            AutonomousOutcome::Spawn(cmd) => assert_eq!(cmd.workflow, "hotfix"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn set_status_overrides_for_tui_messaging() {
        let mut e = AutonomousEngine::new();
        e.set_status("autonomous: cannot enable — tracker unavailable");
        assert!(e.status().contains("cannot enable"));
    }

    #[test]
    fn step_routes_to_matching_workflow_based_on_issue_labels() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.routing = vec![RoutingRule {
            labels: vec!["bug".to_string()],
            workflow: "hotfix".to_string(),
        }];
        let out = e.step(t0(), &cfg, &[], || {
            Ok(vec![issue_with_labels("42", &["bug"])])
        });
        match out {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(cmd.workflow, "hotfix", "routing rule must override default");
                assert_eq!(cmd.issue.human_id, "42");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        // Status line mirrors the chosen workflow, not the default.
        assert!(
            e.status().contains("spawned `hotfix`"),
            "got: {}",
            e.status()
        );
    }

    #[test]
    fn step_falls_back_to_default_workflow_when_no_rule_matches() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.routing = vec![RoutingRule {
            labels: vec!["docs".to_string()],
            workflow: "docs-only".to_string(),
        }];
        // Issue has no `docs` label — should land on autonomous.workflow.
        let out = e.step(t0(), &cfg, &[], || {
            Ok(vec![issue_with_labels("42", &["bug"])])
        });
        match out {
            AutonomousOutcome::Spawn(cmd) => assert_eq!(cmd.workflow, "standard"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn step_uses_default_workflow_when_issue_has_no_labels() {
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.routing = vec![RoutingRule {
            labels: vec!["bug".to_string()],
            workflow: "hotfix".to_string(),
        }];
        let out = e.step(t0(), &cfg, &[], || Ok(vec![issue("42")]));
        match out {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(cmd.workflow, "standard");
                assert!(cmd.issue.labels.is_empty());
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn step_first_matching_rule_wins_for_multi_labeled_issue() {
        // Issue carrying multiple labels matches whichever rule fires
        // first in the config — locks the "config-author owns
        // precedence" contract.
        let mut e = AutonomousEngine::new();
        e.toggle();
        let mut cfg = cfg();
        cfg.routing = vec![
            RoutingRule {
                labels: vec!["hotfix".to_string()],
                workflow: "fast-track".to_string(),
            },
            RoutingRule {
                labels: vec!["docs".to_string()],
                workflow: "docs-only".to_string(),
            },
        ];
        let out = e.step(t0(), &cfg, &[], || {
            Ok(vec![issue_with_labels("42", &["docs", "hotfix"])])
        });
        match out {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(
                    cmd.workflow, "fast-track",
                    "rule order is config-author contract"
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    // ---- rank_candidates_by_plan / ticket_is_blocked ------------------

    use crate::deps::{DEPS_SCHEMA_VERSION, DepEdge, DepsDoc};
    use crate::plans::{Plan, PlanId};

    fn plan_with(id: &str, name: &str, tickets: &[&str], created_at_ms: u64) -> Plan {
        Plan::new(
            PlanId::new(id),
            name,
            tickets.iter().map(|s| (*s).to_string()).collect(),
            created_at_ms,
        )
    }

    fn empty_deps() -> DepsDoc {
        DepsDoc::empty()
    }

    fn deps_with(edges: Vec<DepEdge>) -> DepsDoc {
        DepsDoc {
            version: DEPS_SCHEMA_VERSION,
            edges,
        }
    }

    #[test]
    fn rank_candidates_by_plan_pulls_plan_items_to_the_front_in_order() {
        let open = vec![issue("99"), issue("43"), issue("42"), issue("100")];
        let plan = plan_with("plan-1", "p", &["42", "43"], 0);
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // Plan items first (42, 43 — in plan order), then leftover
        // (99, 100 — in their original arrival order).
        assert_eq!(ids, vec!["42", "43", "99", "100"]);
    }

    #[test]
    fn rank_candidates_by_plan_uses_oldest_plan_first_across_multiple() {
        let open = vec![issue("88"), issue("42"), issue("77")];
        let older = plan_with("plan-A", "first", &["42"], 0);
        let newer = plan_with("plan-B", "second", &["77"], 2_000);
        // Pass plans in the "wrong" order to confirm the sort fires.
        let ordered = rank_candidates_by_plan(open, &[newer, older], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        assert_eq!(ids, vec!["42", "77", "88"]);
    }

    #[test]
    fn rank_candidates_by_plan_skips_non_active_plans() {
        let open = vec![issue("99"), issue("42")];
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.state = PlanState::Paused;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // Paused plan doesn't prioritize anything; order is open's
        // natural order.
        assert_eq!(ids, vec!["99", "42"]);
    }

    #[test]
    fn rank_candidates_by_plan_skips_non_pending_items() {
        let open = vec![issue("99"), issue("42"), issue("43")];
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        // Item 0 (42) is already in progress; should not be
        // re-prioritized AND not appear in the leftover fallback.
        // Item 1 (43) stays pending.
        plan.items[0].state = PlanItemState::InProgress;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // Only 43 (Pending) is prioritized; 99 (not in any plan)
        // still falls through as leftover. 42 is fully suppressed —
        // the `already_addressed` filter mirrors the prioritized
        // pass's "Pending only" rule so a ticket the plan author
        // explicitly marked as InProgress doesn't get re-spawned
        // through the fallback path.
        assert_eq!(ids, vec!["43", "99"]);
    }

    #[test]
    fn rank_candidates_by_plan_suppresses_completed_items_to_prevent_respawn() {
        // Regression: after a session completed for a plan item, the
        // tracker still listed the ticket as `open` (workflow's
        // open_pr / close-ticket step skipped, manual close pending,
        // etc.). The Completed plan item was correctly skipped in
        // the prioritized pass but the leftover fallback let the
        // ticket through, so the next autonomous tick re-spawned a
        // second session against the same item.
        let open = vec![issue("42"), issue("43")];
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        plan.items[0].state = PlanItemState::Completed;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        assert_eq!(ids, vec!["43"], "Completed item must not appear in candidates");
    }

    #[test]
    fn rank_candidates_by_plan_suppresses_skipped_items() {
        // A plan item the user explicitly marked Skipped shouldn't
        // come back through the leftover fallback.
        let open = vec![issue("42")];
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.items[0].state = PlanItemState::Skipped;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        assert!(ordered.is_empty(), "Skipped item must not appear in candidates");
    }

    #[test]
    fn rank_candidates_by_plan_suppresses_tickets_with_failed_plan_items() {
        // Regression: when an item in a plan is Failed, the engine
        // used to keep respawning the same ticket on every scan
        // interval, accumulating Failed sessions on disk. Failed
        // items are now suppressed across all plan states until
        // the user runs `fleet plan retry` (which flips them back
        // to Pending).
        let open = vec![issue("42"), issue("43"), issue("44")];
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        plan.items[0].state = PlanItemState::Failed;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // 42 is filtered (Failed); 43 is still Pending; 44 is open
        // but not in any plan, so falls through as leftover.
        assert_eq!(ids, vec!["43", "44"]);
    }

    #[test]
    fn rank_candidates_by_plan_suppresses_failed_items_even_in_paused_plans() {
        // A Paused plan with a Failed item — the ticket should
        // still be suppressed, since pausing doesn't undo the
        // failure, it just stops the engine from picking new items.
        let open = vec![issue("42")];
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.items[0].state = PlanItemState::Failed;
        plan.state = PlanState::Paused;
        let ordered = rank_candidates_by_plan(open, &[plan], &empty_deps());
        assert!(ordered.is_empty(), "Failed-in-paused must be suppressed; got {ordered:?}");
    }

    #[test]
    fn rank_candidates_by_plan_drops_deps_blocked_tickets() {
        let open = vec![issue("99"), issue("42"), issue("43")];
        let plan = plan_with("plan-1", "p", &["42", "43"], 0);
        // 42 → blocked_on 43; 43 is still open, so 42 is blocked.
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "43".into(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }]);
        let ordered = rank_candidates_by_plan(open, &[plan], &deps);
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // 42 is filtered out; the plan-prioritized list contains just
        // 43; leftover (99) trails.
        assert_eq!(ids, vec!["43", "99"]);
    }

    #[test]
    fn rank_candidates_by_plan_clears_ticket_block_when_blocker_plan_completed() {
        // Mirror of the "tracker closed it" path but for the case
        // where the workflow's create-pr / close-ticket step never
        // ran (no code_host configured). The blocker is still
        // tracker-`open` but its plan item is Completed — the
        // scheduler should unblock dependents anyway so plan
        // progress doesn't stall waiting for an external close.
        let open = vec![issue("42"), issue("43")];
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        // 43 was completed by an earlier session; tracker hasn't
        // caught up (still listed in `open`).
        plan.items[1].state = PlanItemState::Completed;
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "43".into(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }]);
        let ordered = rank_candidates_by_plan(open, &[plan], &deps);
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        // 43 is suppressed via `already_addressed` (Completed); 42 is
        // unblocked because its blocker is plan-Completed even though
        // the tracker still says open.
        assert_eq!(ids, vec!["42"]);
    }

    #[test]
    fn rank_candidates_by_plan_clears_ticket_block_when_target_already_closed() {
        // 42 → blocked_on 99, but 99 isn't in the open list (it has
        // closed). The edge is still in deps.json (the store doesn't
        // auto-clear), but the scheduler treats it as cleared.
        let open = vec![issue("42")];
        let plan = plan_with("plan-1", "p", &["42"], 0);
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "99".into(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }]);
        let ordered = rank_candidates_by_plan(open, &[plan], &deps);
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        assert_eq!(ids, vec!["42"]);
    }

    #[test]
    fn rank_candidates_by_plan_freeform_edges_always_block() {
        // Freeform blockers (`free:apt-mirror`) only clear via
        // `fleet sessions unblock`. The scheduler never auto-clears
        // them.
        let open = vec![issue("42")];
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "free:apt-mirror".into(),
            reason: BlockedReason::Freeform,
            created_at_ms: 1,
        }]);
        let ordered = rank_candidates_by_plan(open, &[], &deps);
        assert!(ordered.is_empty(), "got: {ordered:?}");
    }

    #[test]
    fn rank_candidates_by_plan_with_no_plans_preserves_open_order() {
        let open = vec![issue("99"), issue("42"), issue("77")];
        let ordered = rank_candidates_by_plan(open, &[], &empty_deps());
        let ids: Vec<&str> = ordered.iter().map(|i| i.human_id.as_str()).collect();
        assert_eq!(ids, vec!["99", "42", "77"]);
    }

    // ---- reconcile_one_plan -------------------------------------------

    fn session_with_state(id: &str, ticket: &str, state: SessionState, updated_ms: u64) -> Session {
        let mut s = Session::new(SessionId::new(id), "standard", 0);
        if state != SessionState::Created {
            s.transition_to(SessionState::Running, 1).unwrap();
            if state == SessionState::Running {
                // updated_at_ms doesn't normally tick without a
                // transition; bump it via record_node_cost so tests
                // can pin which session is "newest".
                s.record_node_cost("noop", 0.0, updated_ms);
            } else {
                s.transition_to(state, updated_ms).unwrap();
            }
        }
        s.issue = Some(issue(ticket));
        s
    }

    #[test]
    fn reconcile_one_plan_promotes_pending_to_in_progress_when_session_running() {
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        let sessions = vec![session_with_state("s-1", "42", SessionState::Running, 100)];
        let changed = reconcile_one_plan(&sessions, &mut plan);
        assert!(changed);
        assert_eq!(plan.items[0].state, PlanItemState::InProgress);
        assert_eq!(
            plan.items[0].session_id.as_ref().map(SessionId::as_str),
            Some("s-1")
        );
        // Item 1 untouched.
        assert_eq!(plan.items[1].state, PlanItemState::Pending);
    }

    #[test]
    fn reconcile_one_plan_completes_item_when_session_completed() {
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        let sessions = vec![session_with_state(
            "s-1",
            "42",
            SessionState::Completed,
            200,
        )];
        let changed = reconcile_one_plan(&sessions, &mut plan);
        assert!(changed);
        assert_eq!(plan.items[0].state, PlanItemState::Completed);
    }

    #[test]
    fn reconcile_one_plan_marks_failed_for_failed_or_crashed_sessions() {
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        let sessions = vec![
            session_with_state("s-1", "42", SessionState::Failed, 100),
            session_with_state("s-2", "43", SessionState::Crashed, 100),
        ];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        assert_eq!(plan.items[0].state, PlanItemState::Failed);
        assert_eq!(plan.items[1].state, PlanItemState::Failed);
    }

    #[test]
    fn reconcile_one_plan_uses_newest_session_when_multiple_bind_same_ticket() {
        // A retry: an older session Failed at updated_ms=100, a newer
        // session is now Running at updated_ms=300. Item state should
        // reflect the newer session.
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        let sessions = vec![
            session_with_state("s-old", "42", SessionState::Failed, 100),
            session_with_state("s-new", "42", SessionState::Running, 300),
        ];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        assert_eq!(plan.items[0].state, PlanItemState::InProgress);
        assert_eq!(
            plan.items[0].session_id.as_ref().map(SessionId::as_str),
            Some("s-new")
        );
    }

    #[test]
    fn reconcile_one_plan_is_idempotent_when_state_already_matches() {
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.items[0].state = PlanItemState::Completed;
        plan.items[0].session_id = Some(SessionId::new("s-1"));
        let sessions = vec![session_with_state(
            "s-1",
            "42",
            SessionState::Completed,
            100,
        )];
        let changed = reconcile_one_plan(&sessions, &mut plan);
        assert!(!changed, "no save needed when state already matches");
    }

    #[test]
    fn reconcile_one_plan_ignores_sessions_with_no_bound_ticket() {
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        let mut s = Session::new(SessionId::new("s-orphan"), "standard", 0);
        s.transition_to(SessionState::Running, 1).unwrap();
        // session.issue = None
        let changed = reconcile_one_plan(&[s], &mut plan);
        assert!(!changed);
        assert_eq!(plan.items[0].state, PlanItemState::Pending);
    }

    #[test]
    fn reconcile_one_plan_skips_sessions_in_created_state() {
        // Created is transient — promoting items based on it would
        // flicker InProgress and back to Pending within a tick.
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        let s = session_with_state("s-1", "42", SessionState::Created, 100);
        let changed = reconcile_one_plan(&[s], &mut plan);
        assert!(!changed);
    }

    // ---- on_item_failure policies -----------------------------------

    use crate::plans::ItemFailurePolicy;

    #[test]
    fn reconcile_ignores_dead_sessions_older_than_plan_updated_at_ms() {
        // Regression: after `fleet plan retry` the failed session
        // stays on disk for forensics and the plan's updated_at_ms
        // jumps to "now". Reconcile used to find that dead Failed
        // session, see "newest session for ticket #X is Failed",
        // and re-apply the Stop policy — silently undoing the
        // retry. The watermark filter now skips sessions older
        // than the plan's last user-driven mutation.
        let mut plan = plan_with("plan-1", "p", &["42"], 100);
        plan.on_item_failure = ItemFailurePolicy::Stop;
        // Post-retry shape: item back to Pending, session_id kept
        // for forensics, plan.updated_at_ms = 500 (the retry
        // timestamp). The dead session's updated_at_ms is 200 —
        // older than the plan, so reconcile must ignore it.
        plan.items[0].session_id = Some(SessionId::new("s-old"));
        plan.updated_at_ms = 500;
        let sessions = vec![session_with_state(
            "s-old",
            "42",
            SessionState::Failed,
            200,
        )];
        let changed = reconcile_one_plan(&sessions, &mut plan);
        assert!(!changed, "stale Failed session must not re-trigger the policy");
        assert_eq!(plan.items[0].state, PlanItemState::Pending);
        assert_eq!(plan.state, PlanState::Active);
    }

    #[test]
    fn failure_policy_stop_pauses_plan_when_item_fails() {
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        plan.on_item_failure = ItemFailurePolicy::Stop;
        let sessions = vec![session_with_state("s-1", "42", SessionState::Failed, 100)];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        assert_eq!(plan.items[0].state, PlanItemState::Failed);
        assert_eq!(plan.state, PlanState::Paused);
    }

    #[test]
    fn failure_policy_continue_leaves_plan_active() {
        let mut plan = plan_with("plan-1", "p", &["42", "43"], 0);
        plan.on_item_failure = ItemFailurePolicy::Continue;
        let sessions = vec![session_with_state("s-1", "42", SessionState::Failed, 100)];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        assert_eq!(plan.items[0].state, PlanItemState::Failed);
        assert_eq!(plan.state, PlanState::Active);
    }

    #[test]
    fn failure_policy_retry_once_re_marks_item_pending_first_time() {
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.on_item_failure = ItemFailurePolicy::RetryOnce;
        let sessions = vec![session_with_state("s-1", "42", SessionState::Failed, 100)];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        // First Failed → Pending with retry_count=1; plan stays Active.
        assert_eq!(plan.items[0].state, PlanItemState::Pending);
        assert_eq!(plan.items[0].retry_count, 1);
        assert_eq!(plan.state, PlanState::Active);
        // session_id is still set to the failed session so the TUI
        // can show "tried once, failed".
        assert!(plan.items[0].session_id.is_some());
    }

    #[test]
    fn failure_policy_retry_once_falls_back_to_stop_on_second_failure() {
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.on_item_failure = ItemFailurePolicy::RetryOnce;
        // Simulate first-failure-already-applied state.
        plan.items[0].retry_count = 1;
        plan.items[0].state = PlanItemState::InProgress;
        plan.items[0].session_id = Some(SessionId::new("s-retry"));
        // Now the retry session fails too.
        let sessions = vec![session_with_state(
            "s-retry-2",
            "42",
            SessionState::Failed,
            200,
        )];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        // Second failure → Stop semantics.
        assert_eq!(plan.items[0].state, PlanItemState::Failed);
        assert_eq!(plan.state, PlanState::Paused);
    }

    #[test]
    fn failure_policy_does_not_re_fire_once_item_already_failed() {
        // Item is already Failed (the policy fired on a previous
        // reconcile). The next reconcile sees the same session
        // state and shouldn't re-pause the plan or otherwise touch
        // anything.
        let mut plan = plan_with("plan-1", "p", &["42"], 0);
        plan.on_item_failure = ItemFailurePolicy::Stop;
        plan.items[0].state = PlanItemState::Failed;
        plan.items[0].session_id = Some(SessionId::new("s-1"));
        plan.state = PlanState::Paused;
        let sessions = vec![session_with_state("s-1", "42", SessionState::Failed, 100)];
        let changed = reconcile_one_plan(&sessions, &mut plan);
        assert!(!changed);
        assert_eq!(plan.state, PlanState::Paused);
    }

    #[test]
    fn plan_aware_scheduling_block_then_clear_end_to_end() {
        // End-to-end story for Phase 4 acceptance: a plan [42, 43]
        // with 42 currently blocked on 51. First tick should
        // prioritize 43 (the second plan item) and drop 42; once 51
        // closes (out of the open set), a second tick should
        // prioritize 42 again in plan order.

        let plan = plan_with("plan-1", "p", &["42", "43"], 0);

        // First tick: 42 is blocked on the still-open 51.
        let open_with_blocker = vec![issue("42"), issue("43"), issue("51")];
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "51".into(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }]);
        let ranked = rank_candidates_by_plan(open_with_blocker, std::slice::from_ref(&plan), &deps);
        let ids: Vec<&str> = ranked.iter().map(|i| i.human_id.as_str()).collect();
        // 42 dropped (blocked); 43 prioritized (plan); 51 leftover.
        assert_eq!(ids, vec!["43", "51"]);

        // Now 51 closes — it's not in the open list any more. Even
        // though the deps edge is still on disk, the scheduler
        // treats it as cleared since `blocked_on` isn't open.
        let open_after_clear = vec![issue("42"), issue("43")];
        let ranked_after = rank_candidates_by_plan(open_after_clear, &[plan], &deps);
        let ids_after: Vec<&str> = ranked_after.iter().map(|i| i.human_id.as_str()).collect();
        // Plan order restored: 42 first, 43 second.
        assert_eq!(ids_after, vec!["42", "43"]);
    }

    #[test]
    fn plan_aware_scheduling_injected_ticket_runs_before_its_parent() {
        // Mirrors what `tracker-create`'s plan-injector achieves: a
        // newly-filed prerequisite is inserted before the bound
        // parent in the plan. The ranker should pick it up first on
        // the next tick.
        let mut plan = plan_with("plan-1", "p", &["42", "44"], 0);
        // Simulate tracker-create injecting #51 before #42.
        plan.items
            .insert(0, crate::plans::PlanItem::pending_injected("51"));
        let open = vec![issue("42"), issue("44"), issue("51")];
        // 42 is now blocked on the freshly filed 51.
        let deps = deps_with(vec![DepEdge {
            blocked: "42".into(),
            blocked_on: "51".into(),
            reason: BlockedReason::Ticket,
            created_at_ms: 1,
        }]);
        let ranked = rank_candidates_by_plan(open, &[plan], &deps);
        let ids: Vec<&str> = ranked.iter().map(|i| i.human_id.as_str()).collect();
        // 51 runs first (injected, position 0 in the plan).
        // 42 is blocked; dropped. 44 follows from the plan.
        assert_eq!(ids, vec!["51", "44"]);
    }

    #[test]
    fn failure_policy_continue_leaves_other_items_pending_for_next_spawn() {
        // The whole point of Continue: when item 0 fails, item 1
        // stays Pending and the supervisor's next tick will pick
        // it up.
        let mut plan = plan_with("plan-1", "p", &["42", "43", "44"], 0);
        plan.on_item_failure = ItemFailurePolicy::Continue;
        let sessions = vec![session_with_state("s-1", "42", SessionState::Failed, 100)];
        let _ = reconcile_one_plan(&sessions, &mut plan);
        assert_eq!(plan.items[1].state, PlanItemState::Pending);
        assert_eq!(plan.items[2].state, PlanItemState::Pending);
    }

    #[test]
    fn ticket_is_blocked_matches_specifically_keyed_edges() {
        let deps = deps_with(vec![
            DepEdge {
                blocked: "42".into(),
                blocked_on: "43".into(),
                reason: BlockedReason::Ticket,
                created_at_ms: 1,
            },
            DepEdge {
                blocked: "44".into(),
                blocked_on: "42".into(),
                reason: BlockedReason::Ticket,
                created_at_ms: 2,
            },
        ]);
        let open: std::collections::HashSet<String> =
            ["43".to_string(), "42".to_string()].into_iter().collect();
        // 42 is blocked (blocked_on 43, which is open).
        assert!(ticket_is_blocked("42", &open, &deps));
        // 44 is blocked (blocked_on 42, which is open).
        assert!(ticket_is_blocked("44", &open, &deps));
        // 43 isn't blocked — no edge has it as the LHS.
        assert!(!ticket_is_blocked("43", &open, &deps));
        // 99 isn't blocked.
        assert!(!ticket_is_blocked("99", &open, &deps));
    }
}
