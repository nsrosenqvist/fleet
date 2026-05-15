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

use std::time::{Duration, Instant};

use crate::repo_config::AutonomousConfig;
use crate::session::{IssueContext, Session, SessionState};

/// State carried across ticks. Created once, toggled on/off via
/// [`Self::toggle`], and stepped each event-loop iteration.
pub struct AutonomousEngine {
    enabled: bool,
    last_scan_at: Option<Instant>,
    last_spawn_at: Option<Instant>,
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
        self.status = if self.enabled {
            String::from("autonomous: ON")
        } else {
            String::from("autonomous: OFF")
        };
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
        let claimed: std::collections::HashSet<String> = in_flight
            .iter()
            .filter_map(|s| s.issue.as_ref().map(|i| i.id.clone()))
            .collect();
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

        // Found a candidate. Set the spawn timestamp BEFORE returning
        // so a panic in the caller's spawn path doesn't leak into a
        // tight retry loop on the next tick.
        self.last_spawn_at = Some(now);
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
}
