#![allow(dead_code)]
//! Loop-driven workflow scheduler.
//!
//! Parallel to [`crate::autonomous::AutonomousEngine`]: a pure state
//! machine that takes ticks, returns spawn commands, and never does
//! I/O. The TUI / CLI tick driver feeds it the current sessions list,
//! the parsed workflows, and a closure that lists PRs from the
//! configured code host; the engine decides which workflows are due,
//! whether overlap should suppress this firing, and how many sessions
//! to mint per due workflow.
//!
//! Persistent `last_run_at` per workflow lives on disk in
//! [`store::SchedulerState`] (`.fleet/scheduler_state.json`). The
//! engine reloads it each tick — cheap (a few small entries) and
//! avoids the in-memory-vs-disk staleness a long-lived per-process
//! cache would create.
//!
//! Why not fold into the autonomous engine: that engine fires per
//! open *issue*; this one fires per elapsed *interval*. They are
//! orthogonal triggers and should be independently togglable —
//! `fleet scheduler enable` lights this engine up without dragging
//! the autonomous loop with it.

pub mod dispatcher;
pub mod store;

use std::time::{Duration, Instant, SystemTime};

use crate::code_host::{PrFilter, PrSummary};
use crate::session::{Session, SessionState};
use crate::workflow::spec::{NodeKind, OnOverlap, PrBindMode, Workflow};

pub use dispatcher::{SchedulerSpawn, SpawnSeed};
pub use store::SchedulerState;

/// State carried across ticks. Created once, toggled on/off via
/// [`Self::toggle`], advanced via [`Self::step`].
pub struct SchedulerEngine {
    enabled: bool,
    /// TUI status-bar line. Updated by [`Self::step`] each time it
    /// decides anything interesting, the same UX pattern the
    /// autonomous engine uses.
    status: String,
    /// Wallclock of the last tick. Currently informational; reserved
    /// for future per-engine debouncing if the TUI tick cadence
    /// grows.
    #[allow(dead_code)]
    last_step_at: Option<Instant>,
}

impl Default for SchedulerEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: false,
            status: String::from("scheduler: OFF"),
            last_step_at: None,
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Human-readable status the TUI bottom bar surfaces.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Override the status line. Used by the CLI / TUI when the
    /// engine is off but the driver still wants to surface info
    /// ("scheduler: OFF · 2 workflows configured").
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
    }

    /// Flip the on/off flag. Resets `last_step_at` so a freshly-
    /// enabled engine doesn't wait out a stale debounce.
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.last_step_at = None;
        self.status = if self.enabled {
            String::from("scheduler: ON")
        } else {
            String::from("scheduler: OFF")
        };
    }

    /// Drive one tick. Pure modulo the `list_prs` closure. Returns
    /// the action the caller should take; the caller is responsible
    /// for both dispatching the spawns and persisting the updated
    /// [`SchedulerState`].
    ///
    /// `list_prs` is a closure rather than a trait dependency so the
    /// engine doesn't need to know about `CodeHost` (which would
    /// drag in the whole module). Tests stub it inline.
    pub fn step<F>(
        &mut self,
        now: SystemTime,
        now_inst: Instant,
        workflows: &[Workflow],
        sessions: &[Session],
        state: &SchedulerState,
        list_prs: F,
    ) -> SchedulerOutcome
    where
        F: Fn(&str, &PrFilter) -> Result<Vec<PrSummary>, String>,
    {
        self.last_step_at = Some(now_inst);
        if !self.enabled {
            return SchedulerOutcome::Idle;
        }

        let due: Vec<&Workflow> = workflows
            .iter()
            .filter(|w| w.loop_interval.is_some())
            .filter(|w| workflow_is_due(w, now, state))
            .collect();
        if due.is_empty() {
            self.status = Self::idle_status_for(workflows, now, state);
            return SchedulerOutcome::WaitedFor(SkipReason::NoWorkflowDue);
        }

        let mut spawns = Vec::new();
        let mut new_state_marks: Vec<(String, SystemTime)> = Vec::new();
        for wf in due {
            // Overlap policy: with `Skip` (the default), suppress this
            // tick when a session for this workflow is already in
            // `Running` / `AwaitingGate`. `Run` accepts overlap.
            if wf.on_overlap == OnOverlap::Skip && has_inflight_for(wf, sessions) {
                self.status = format!(
                    "scheduler: ON · `{}` due but a session is in flight (on_overlap: skip)",
                    wf.name,
                );
                // Don't advance last_run_at — we didn't actually run.
                continue;
            }

            match collect_spawns(wf, &list_prs) {
                Ok(workflow_spawns) => {
                    // Even when the PR list comes back empty, mark
                    // the workflow as run so we don't hot-spin on
                    // back-to-back ticks against an empty candidate
                    // set.
                    new_state_marks.push((wf.name.clone(), now));
                    spawns.extend(workflow_spawns);
                }
                Err(err) => {
                    self.status = format!(
                        "scheduler: ON · `{}` candidate query failed: {err}",
                        wf.name,
                    );
                    // Don't advance last_run_at — error is transient,
                    // the next tick should retry.
                }
            }
        }

        if spawns.is_empty() {
            // Either every due workflow was suppressed by overlap, or
            // every candidate query produced zero results — surface
            // both as a `Waited`. The caller still needs to persist
            // any `new_state_marks` so empty-result ticks advance
            // last_run_at.
            return SchedulerOutcome::WaitedFor(SkipReason::AllSuppressedOrEmpty {
                marks: new_state_marks,
            });
        }

        self.status = format!(
            "scheduler: ON · spawned {} session(s) for {} workflow(s)",
            spawns.len(),
            new_state_marks.len(),
        );
        SchedulerOutcome::Spawn {
            spawns,
            marks: new_state_marks,
        }
    }

    fn idle_status_for(
        workflows: &[Workflow],
        now: SystemTime,
        state: &SchedulerState,
    ) -> String {
        let looped: Vec<&Workflow> = workflows
            .iter()
            .filter(|w| w.loop_interval.is_some())
            .collect();
        if looped.is_empty() {
            return String::from("scheduler: ON · no workflows with `loop:` configured");
        }
        let soonest = looped
            .iter()
            .filter_map(|w| time_until_due(w, now, state).map(|d| (w.name.as_str(), d)))
            .min_by_key(|(_, d)| *d);
        match soonest {
            Some((name, remaining)) => format!(
                "scheduler: ON · next tick `{}` in ~{}s",
                name,
                remaining.as_secs(),
            ),
            None => String::from("scheduler: ON · no workflows due"),
        }
    }
}

/// Result of one [`SchedulerEngine::step`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum SchedulerOutcome {
    /// Engine is off, or has nothing to do.
    Idle,
    /// Engine ticked but produced no spawns this round (overlap
    /// suppressed every due workflow, or `pr-list` returned empty).
    /// `marks` is the list of `(workflow_name, ran_at)` updates the
    /// caller should persist so empty-result ticks still advance the
    /// clock and don't hot-spin.
    WaitedFor(SkipReason),
    /// Engine produced spawn commands. The caller dispatches them
    /// and persists `marks`.
    Spawn {
        spawns: Vec<SchedulerSpawn>,
        marks: Vec<(String, SystemTime)>,
    },
}

/// Why a tick produced no spawns. Surfaced for telemetry / tests.
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    NoWorkflowDue,
    AllSuppressedOrEmpty {
        marks: Vec<(String, SystemTime)>,
    },
}

fn workflow_is_due(wf: &Workflow, now: SystemTime, state: &SchedulerState) -> bool {
    let Some(interval) = wf.loop_interval else {
        return false;
    };
    // Negative duration (clock skew) — be conservative and treat as
    // "not due" rather than firing on a clock jump backwards.
    state.last_run_at.get(&wf.name).is_none_or(|last| {
        now.duration_since(*last).is_ok_and(|elapsed| elapsed >= interval)
    })
}

fn time_until_due(
    wf: &Workflow,
    now: SystemTime,
    state: &SchedulerState,
) -> Option<Duration> {
    let interval = wf.loop_interval?;
    let last = state.last_run_at.get(&wf.name).copied()?;
    let elapsed = now.duration_since(last).ok()?;
    Some(interval.checked_sub(elapsed).unwrap_or(Duration::ZERO))
}

fn has_inflight_for(wf: &Workflow, sessions: &[Session]) -> bool {
    sessions.iter().any(|s| {
        s.workflow == wf.name
            && matches!(
                s.state,
                SessionState::Running | SessionState::AwaitingGate
            )
    })
}

/// Collect the spawn commands a single due workflow produces. Returns
/// an empty Vec when the workflow's first node is `pr-list bind: per`
/// and the host returned no PRs — the caller treats that as "marked
/// run, but no sessions to spawn this tick."
fn collect_spawns<F>(wf: &Workflow, list_prs: &F) -> Result<Vec<SchedulerSpawn>, String>
where
    F: Fn(&str, &PrFilter) -> Result<Vec<PrSummary>, String>,
{
    if let Some(root) = wf.nodes.first() {
        if let NodeKind::PrList {
            filter,
            bind: Some(PrBindMode::Per),
        } = &root.kind
        {
            let prs = list_prs(&wf.name, filter)?;
            return Ok(prs
                .into_iter()
                .map(|pr| SchedulerSpawn {
                    workflow: wf.name.clone(),
                    start_after_node: Some(root.id.clone()),
                    seed: SpawnSeed::Pr(pr),
                })
                .collect());
        }
    }
    // Plain `loop:` workflow with no candidate enumeration — emit
    // exactly one anonymous spawn.
    Ok(vec![SchedulerSpawn {
        workflow: wf.name.clone(),
        start_after_node: None,
        seed: SpawnSeed::Anonymous,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_host::{PrFilter, PrState};
    use crate::session::{Session, SessionId};
    use crate::workflow::spec::Workflow;

    fn parse(yaml: &str) -> Workflow {
        Workflow::from_str_at(yaml, "/x").unwrap()
    }

    fn at_time(s: SystemTime, secs_added: u64) -> SystemTime {
        s + Duration::from_secs(secs_added)
    }

    fn sample_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    // The `_pr` arg / Result return type matches the closure signature
    // `SchedulerEngine::step` accepts; the function form keeps test
    // sites short and matches the trait's runtime shape.
    #[allow(clippy::unnecessary_wraps)]
    fn ok_list_prs(_name: &str, _filter: &PrFilter) -> Result<Vec<PrSummary>, String> {
        Ok(Vec::new())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn always_one_pr(_name: &str, _filter: &PrFilter) -> Result<Vec<PrSummary>, String> {
        Ok(vec![PrSummary {
            number: 42,
            title: "Fix x".into(),
            head_ref: "feat/x".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            state: PrState::Open,
            draft: false,
            url: "https://github.com/o/r/pull/42".into(),
            labels: vec![],
        }])
    }

    fn fixture_session(workflow: &str, state: SessionState) -> Session {
        let mut s = Session::new(SessionId::new(format!("s-{workflow}")), workflow, 1);
        if matches!(state, SessionState::Running | SessionState::AwaitingGate) {
            s.state = SessionState::Running;
            if state == SessionState::AwaitingGate {
                s.state = SessionState::AwaitingGate;
            }
        } else {
            s.state = state;
        }
        s
    }

    #[test]
    fn disabled_engine_is_idle() {
        let mut eng = SchedulerEngine::new();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[],
            &[],
            &SchedulerState::default(),
            ok_list_prs,
        );
        assert_eq!(outcome, SchedulerOutcome::Idle);
        assert!(!eng.enabled());
    }

    #[test]
    fn toggle_flips_enabled_and_resets_status() {
        let mut eng = SchedulerEngine::new();
        assert_eq!(eng.status(), "scheduler: OFF");
        eng.toggle();
        assert!(eng.enabled());
        assert_eq!(eng.status(), "scheduler: ON");
        eng.toggle();
        assert!(!eng.enabled());
    }

    #[test]
    fn step_returns_no_workflow_due_when_only_loopless_workflows_exist() {
        let wf = parse("name: a\nnodes:\n  - id: x\n    agent: claude\n");
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[],
            &SchedulerState::default(),
            ok_list_prs,
        );
        assert!(matches!(
            outcome,
            SchedulerOutcome::WaitedFor(SkipReason::NoWorkflowDue)
        ));
    }

    #[test]
    fn step_fires_workflow_when_due_with_no_prior_run() {
        let wf = parse(
            "\
name: hourly
loop: 1h
trigger: { issueless: true }
nodes:
  - id: scan
    agent: claude
",
        );
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[],
            &SchedulerState::default(),
            ok_list_prs,
        );
        match outcome {
            SchedulerOutcome::Spawn { spawns, marks } => {
                assert_eq!(spawns.len(), 1);
                assert_eq!(spawns[0].workflow, "hourly");
                assert!(matches!(spawns[0].seed, SpawnSeed::Anonymous));
                assert_eq!(marks.len(), 1);
                assert_eq!(marks[0].0, "hourly");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn step_skips_workflow_before_interval_elapses() {
        let wf = parse(
            "\
name: hourly
loop: 1h
trigger: { issueless: true }
nodes:
  - id: scan
    agent: claude
",
        );
        let base = sample_now();
        let mut state = SchedulerState::default();
        state.last_run_at.insert("hourly".to_string(), base);

        let mut eng = SchedulerEngine::new();
        eng.toggle();
        // 30 minutes later — not yet due.
        let workflows = std::slice::from_ref(&wf);
        let outcome = eng.step(
            at_time(base, 1_800),
            Instant::now(),
            workflows,
            &[],
            &state,
            ok_list_prs,
        );
        assert!(matches!(
            outcome,
            SchedulerOutcome::WaitedFor(SkipReason::NoWorkflowDue)
        ));

        // 61 minutes later — due.
        let outcome2 = eng.step(
            at_time(base, 3_661),
            Instant::now(),
            &[wf],
            &[],
            &state,
            ok_list_prs,
        );
        assert!(matches!(outcome2, SchedulerOutcome::Spawn { .. }));
    }

    #[test]
    fn step_with_pr_list_bind_per_fans_out_one_spawn_per_pr() {
        let wf = parse(
            "\
name: ci-fix
loop: 1h
trigger: { issueless: true }
nodes:
  - id: pick
    type: pr-list
    filter:
      ci_status: failure
    bind: per
  - id: fix
    depends_on: [pick]
    agent: claude
",
        );
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[],
            &SchedulerState::default(),
            always_one_pr,
        );
        match outcome {
            SchedulerOutcome::Spawn { spawns, marks } => {
                assert_eq!(spawns.len(), 1);
                assert_eq!(spawns[0].workflow, "ci-fix");
                assert_eq!(spawns[0].start_after_node.as_deref(), Some("pick"));
                match &spawns[0].seed {
                    SpawnSeed::Pr(pr) => assert_eq!(pr.number, 42),
                    SpawnSeed::Anonymous => panic!("expected Pr seed, got Anonymous"),
                }
                assert_eq!(marks.len(), 1);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn step_with_empty_pr_list_marks_workflow_run_but_emits_no_spawn() {
        let wf = parse(
            "\
name: ci-fix
loop: 1h
trigger: { issueless: true }
nodes:
  - id: pick
    type: pr-list
    bind: per
",
        );
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[],
            &SchedulerState::default(),
            ok_list_prs,
        );
        // The candidate set was empty → no spawns, but we still
        // advance last_run_at so the next tick won't immediately
        // re-query an empty PR list.
        match outcome {
            SchedulerOutcome::WaitedFor(SkipReason::AllSuppressedOrEmpty { marks }) => {
                assert_eq!(marks.len(), 1);
                assert_eq!(marks[0].0, "ci-fix");
            }
            other => panic!("expected AllSuppressedOrEmpty, got {other:?}"),
        }
    }

    #[test]
    fn step_skip_overlap_suppresses_when_session_in_flight() {
        let wf = parse(
            "\
name: hourly
loop: 1h
on_overlap: skip
trigger: { issueless: true }
nodes:
  - id: scan
    agent: claude
",
        );
        let session = fixture_session("hourly", SessionState::Running);
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[session],
            &SchedulerState::default(),
            ok_list_prs,
        );
        match outcome {
            SchedulerOutcome::WaitedFor(SkipReason::AllSuppressedOrEmpty { marks }) => {
                // Overlap skip: don't advance last_run_at — the
                // workflow gets a fresh shot next tick.
                assert!(marks.is_empty(), "skip-overlap must not advance the clock");
            }
            other => panic!("expected AllSuppressedOrEmpty, got {other:?}"),
        }
    }

    #[test]
    fn step_run_overlap_fires_anyway_when_session_in_flight() {
        let wf = parse(
            "\
name: hourly
loop: 1h
on_overlap: run
trigger: { issueless: true }
nodes:
  - id: scan
    agent: claude
",
        );
        let session = fixture_session("hourly", SessionState::Running);
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[session],
            &SchedulerState::default(),
            ok_list_prs,
        );
        assert!(matches!(outcome, SchedulerOutcome::Spawn { .. }));
    }

    #[test]
    fn step_list_prs_error_does_not_advance_clock() {
        let wf = parse(
            "\
name: ci-fix
loop: 1h
trigger: { issueless: true }
nodes:
  - id: pick
    type: pr-list
    bind: per
",
        );
        let failing = |_: &str, _: &PrFilter| -> Result<Vec<PrSummary>, String> {
            Err("gh: not authenticated".into())
        };
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        let outcome = eng.step(
            sample_now(),
            Instant::now(),
            &[wf],
            &[],
            &SchedulerState::default(),
            failing,
        );
        match outcome {
            SchedulerOutcome::WaitedFor(SkipReason::AllSuppressedOrEmpty { marks }) => {
                assert!(marks.is_empty(), "failed query must not advance the clock");
            }
            other => panic!("expected AllSuppressedOrEmpty, got {other:?}"),
        }
        assert!(eng.status().contains("gh: not authenticated"), "got: {}", eng.status());
    }

    #[test]
    fn step_two_workflows_independently_due() {
        let a = parse(
            "name: a\nloop: 1h\ntrigger: { issueless: true }\nnodes:\n  - id: n\n    agent: c\n",
        );
        let b = parse(
            "name: b\nloop: 2h\ntrigger: { issueless: true }\nnodes:\n  - id: n\n    agent: c\n",
        );
        let base = sample_now();
        let mut state = SchedulerState::default();
        state.last_run_at.insert("a".to_string(), base);
        state.last_run_at.insert("b".to_string(), base);
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        // 90 min later: `a` is due (>60m), `b` is not (>120m).
        let outcome = eng.step(
            at_time(base, 5_400),
            Instant::now(),
            &[a, b],
            &[],
            &state,
            ok_list_prs,
        );
        match outcome {
            SchedulerOutcome::Spawn { spawns, marks } => {
                assert_eq!(spawns.len(), 1);
                assert_eq!(spawns[0].workflow, "a");
                assert_eq!(marks.len(), 1);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn idle_status_includes_remaining_seconds_to_next_workflow() {
        let wf = parse(
            "name: hourly\nloop: 1h\ntrigger: { issueless: true }\nnodes:\n  - id: n\n    agent: c\n",
        );
        let base = sample_now();
        let mut state = SchedulerState::default();
        state.last_run_at.insert("hourly".to_string(), base);
        let mut eng = SchedulerEngine::new();
        eng.toggle();
        // 5 minutes elapsed — 55 min remain.
        let _ = eng.step(
            at_time(base, 300),
            Instant::now(),
            &[wf],
            &[],
            &state,
            ok_list_prs,
        );
        // Allow ±5 seconds for math
        assert!(
            eng.status().contains("3300s") || eng.status().contains("3301s"),
            "got: {}",
            eng.status()
        );
    }
}

