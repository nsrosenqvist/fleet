//! `fleet autonomous run …` — the [`crate::autonomous::AutonomousEngine`]
//! driven from the command line instead of the TUI's `Shift+A`.
//!
//! Two modes:
//! - `--once`: run one supervisor tick (reap stale sessions, scan the
//!   tracker, maybe spawn one workflow), print the outcome, exit. The
//!   shape cron/systemd-timer/CI hooks want.
//! - `--watch`: loop forever, sleeping `autonomous.scan_interval_secs`
//!   between ticks. Soft-handles tracker errors (logs + continues) so
//!   a transient `gh` outage doesn't tear down the supervisor.
//!
//! ## What this module does *not* own
//! - The decision logic — that's [`AutonomousEngine::step`]. This
//!   module is just the I/O glue (config load, tracker build, session
//!   read, subprocess detach, sleep).
//! - The subprocess plumbing — that's [`crate::tui::build_workflow_run_command`].
//!   Reused as-is so the TUI and CLI surfaces stay aligned: the same
//!   `fleet workflow run …` command shape is fired by both.
//! - Reaping crashed sessions — that's [`crate::session::reaper::reap`].
//!   Called once at startup so a session whose previous driver died
//!   doesn't occupy a slot forever.

use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::autonomous::{AutonomousEngine, AutonomousOutcome, PauseReason, SpawnCommand};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::session::reaper::{self, RealPidProbe};
use crate::session::store::SessionStore;
use crate::session::{IssueContext, now_ms};
use crate::tracker::{self, Tracker};
use crate::tui::build_workflow_run_command;

/// CLI entry. `--once` xor `--watch` must be set.
pub fn run(once: bool, watch: bool) -> Result<i32> {
    if once == watch {
        bail!("fleet autonomous run: pass exactly one of --once or --watch");
    }
    let setup = Setup::from_cwd()?;
    if once {
        Ok(run_once(&setup))
    } else {
        run_watch(&setup)
    }
}

/// One supervisor tick + exit. Tracker errors are fatal here — a cron
/// invocation reports its setup failure via exit code 1.
///
/// Returns the exit code directly (no `Result`): every error path
/// inside one tick is reachable as a [`PauseReason::TrackerError`]
/// outcome and surfaced through the normal renderer, so wrapping in
/// `Result` would be misleading. The `run` wrapper converts to the
/// dispatch's `Result<i32>` shape.
fn run_once(setup: &Setup) -> i32 {
    sweep_crashed(setup);
    let mut engine = AutonomousEngine::new();
    engine.toggle();
    let sessions = load_sessions(&setup.store);
    let outcome = tick(
        &mut engine,
        Instant::now(),
        setup,
        &sessions,
        &RealSpawner {
            binary: std::env::current_exe().ok(),
        },
    );
    print!("{}", render_outcome(&outcome));
    match &outcome {
        AutonomousOutcome::WaitedFor(PauseReason::TrackerError(_)) => 1,
        _ => 0,
    }
}

/// Loop forever, sleeping between ticks. Tracker errors are logged but
/// don't terminate the loop — the supervisor is allowed to ride out a
/// transient outage. The only way out is `Ctrl-C` / `SIGTERM` to the
/// fleet process itself.
fn run_watch(setup: &Setup) -> Result<i32> {
    sweep_crashed(setup);
    let mut engine = AutonomousEngine::new();
    engine.toggle();
    let spawner = RealSpawner {
        binary: std::env::current_exe().ok(),
    };
    let scan_interval = Duration::from_secs(u64::from(setup.config.autonomous.scan_interval_secs));
    loop {
        let sessions = load_sessions(&setup.store);
        let outcome = tick(&mut engine, Instant::now(), setup, &sessions, &spawner);
        let line = render_outcome(&outcome);
        // One line per tick, prefixed with the engine's last status so
        // a `journalctl -f` reader can scan progress at a glance.
        print!("{line}");
        std::thread::sleep(scan_interval);
    }
}

/// Bag of process-wide deps for a single tick. Held by both `run_once`
/// and `run_watch` so the inner per-tick logic stays the same.
struct Setup {
    store: SessionStore,
    config: RepoConfig,
    tracker: Arc<dyn Tracker>,
    repo_root: std::path::PathBuf,
}

impl Setup {
    fn from_cwd() -> Result<Self> {
        let cwd = std::env::current_dir().context("reading current directory")?;
        let root = repo::fleet_root(&cwd);
        let config = RepoConfig::load(root.join(".fleet/config.yaml")).unwrap_or_default();
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        let tracker = tracker::build(config.tracker, invoker).ok_or_else(|| {
            anyhow::anyhow!(
                "tracker `{}` has no fleet plugin yet — pick git-bug or github in .fleet/config.yaml",
                config.tracker.as_str(),
            )
        })?;
        Ok(Self {
            store: SessionStore::for_repo(&root),
            config,
            tracker: Arc::from(tracker),
            repo_root: root,
        })
    }
}

/// Caller for an actual subprocess detach. Behind a trait so tests can
/// observe what would have been spawned without firing real children.
trait Spawner {
    fn spawn_workflow(&self, cmd: &SpawnCommand) -> Result<()>;
}

struct RealSpawner {
    binary: Option<std::path::PathBuf>,
}

impl Spawner for RealSpawner {
    fn spawn_workflow(&self, cmd: &SpawnCommand) -> Result<()> {
        let binary = self
            .binary
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve fleet binary path (current_exe)"))?;
        let mut child =
            build_workflow_run_command(binary, &cmd.workflow, Some(&cmd.issue.human_id));
        child
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        child
            .spawn()
            .with_context(|| format!("spawning `fleet workflow run {}`", cmd.workflow))?;
        Ok(())
    }
}

/// Run a single supervisor tick. Owns the dispatch decision: on a
/// [`AutonomousOutcome::Spawn`] it fires the spawner and returns the
/// outcome verbatim; on other outcomes it just returns. Test seam: the
/// `spawner` parameter is the only side-effecting input.
fn tick(
    engine: &mut AutonomousEngine,
    now: Instant,
    setup: &Setup,
    sessions: &[crate::session::Session],
    spawner: &dyn Spawner,
) -> AutonomousOutcome {
    // Reconcile plan-item states against current sessions before
    // the scan picks a candidate. This catches the "session finished
    // between ticks" case so the plan-aware ranker sees the
    // updated state (a Completed item doesn't get re-prioritized).
    let plan_store_pre = crate::plans::store::PlanStore::for_repo(&setup.repo_root);
    if let Err(err) = crate::autonomous::reconcile_plans_from_sessions(
        sessions,
        &plan_store_pre,
        crate::session::now_ms(),
    ) {
        tracing::warn!(error = %err, "autonomous tick: plan reconcile failed");
    }
    let tracker = Arc::clone(&setup.tracker);
    let root = setup.repo_root.clone();
    let list_open = move || -> Result<Vec<IssueContext>, String> {
        let issues = tracker.list_issues(&root).map_err(|e| format!("{e:#}"))?;
        let open: Vec<IssueContext> = issues
            .into_iter()
            .filter(|i| i.status == "open")
            .map(|i| IssueContext {
                id: i.id,
                human_id: i.human_id,
                title: i.title,
                labels: i.labels,
            })
            .collect();
        // Reorder so active-plan items come first (oldest-plan
        // priority), and drop tickets currently blocked by an open
        // dependency or a freeform tag. Plan/deps load failures
        // collapse to empty — the supervisor falls back to the
        // pre-plans "any open issue" path rather than aborting the
        // tick over a corrupt local file.
        let plans = crate::plans::store::PlanStore::for_repo(&root)
            .list_active()
            .unwrap_or_default();
        let deps = crate::deps::DepsStore::for_repo(&root)
            .load()
            .unwrap_or_default();
        Ok(crate::autonomous::rank_candidates_by_plan(
            open, &plans, &deps,
        ))
    };
    let outcome = engine.step(now, &setup.config.autonomous, sessions, list_open);
    if let AutonomousOutcome::Spawn(cmd) = &outcome {
        if let Err(err) = spawner.spawn_workflow(cmd) {
            // Returning the original Spawn outcome would falsely signal
            // success; promote it to a TrackerError-like WaitedFor so
            // the caller's exit-code logic reflects the failure. Use a
            // synthesised error string because PauseReason carries one.
            return AutonomousOutcome::WaitedFor(PauseReason::TrackerError(format!(
                "spawn failed: {err:#}"
            )));
        }
    }
    outcome
}

/// Side-effecting helper: walk the session store and reap any stuck
/// session whose driver process is gone. Failures are logged but
/// non-fatal — a missing `.fleet/sessions/` directory shouldn't keep
/// the supervisor from starting. Leaked containers from prior crashes
/// are stopped here too via the repo's configured runtime adapter; if
/// the adapter can't be built (misconfigured engine), the stopper
/// falls back to a noop and the markers are merely cleared.
fn sweep_crashed(setup: &Setup) {
    let stopper = crate::runtime::factory::build_stopper(&setup.repo_root);
    if let Err(err) = reaper::reap(&setup.store, &RealPidProbe, stopper.as_ref(), now_ms()) {
        tracing::warn!(error = %err, "autonomous: reap sweep at startup failed");
    }
}

/// Read every session from disk. A half-written meta.json shouldn't
/// poison the tick; tolerate per-session load errors.
fn load_sessions(store: &SessionStore) -> Vec<crate::session::Session> {
    let ids = match store.list() {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "autonomous: listing sessions failed");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Ok(s) = store.load(&id) {
            out.push(s);
        }
    }
    out
}

/// Pure renderer for a tick's outcome. Stable wording so logs are
/// greppable across releases.
#[must_use]
pub fn render_outcome(outcome: &AutonomousOutcome) -> String {
    match outcome {
        AutonomousOutcome::Idle => "idle\n".to_string(),
        AutonomousOutcome::Spawn(cmd) => format!(
            "spawn workflow={} issue={}\n",
            cmd.workflow, cmd.issue.human_id
        ),
        AutonomousOutcome::WaitedFor(reason) => format!("wait {}\n", render_reason(reason)),
    }
}

fn render_reason(reason: &PauseReason) -> String {
    match reason {
        PauseReason::SlotsExhausted { in_flight, cap } => {
            format!("slots-exhausted in_flight={in_flight} cap={cap}")
        }
        PauseReason::NoUnclaimedIssues => "no-unclaimed-issues".to_string(),
        PauseReason::TrackerError(msg) => format!("tracker-error {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionId};
    use crate::tracker::Issue;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn run_rejects_both_flags() {
        let err = run(true, true).unwrap_err();
        assert!(format!("{err}").contains("exactly one of"));
    }

    #[test]
    fn run_rejects_neither_flag() {
        let err = run(false, false).unwrap_err();
        assert!(format!("{err}").contains("exactly one of"));
    }

    #[test]
    fn render_outcome_idle() {
        assert_eq!(render_outcome(&AutonomousOutcome::Idle), "idle\n");
    }

    #[test]
    fn render_outcome_spawn_shows_workflow_and_issue() {
        let cmd = SpawnCommand {
            workflow: "standard".to_string(),
            issue: IssueContext {
                id: "gh:42".to_string(),
                human_id: "42".to_string(),
                title: "x".to_string(),
                labels: Vec::new(),
            },
        };
        assert_eq!(
            render_outcome(&AutonomousOutcome::Spawn(cmd)),
            "spawn workflow=standard issue=42\n"
        );
    }

    #[test]
    fn render_outcome_wait_slots_exhausted() {
        let r = render_outcome(&AutonomousOutcome::WaitedFor(PauseReason::SlotsExhausted {
            in_flight: 3,
            cap: 3,
        }));
        assert_eq!(r, "wait slots-exhausted in_flight=3 cap=3\n");
    }

    #[test]
    fn render_outcome_wait_no_issues() {
        let r = render_outcome(&AutonomousOutcome::WaitedFor(
            PauseReason::NoUnclaimedIssues,
        ));
        assert_eq!(r, "wait no-unclaimed-issues\n");
    }

    #[test]
    fn render_outcome_wait_tracker_error_carries_message() {
        let r = render_outcome(&AutonomousOutcome::WaitedFor(PauseReason::TrackerError(
            "gh: 401".to_string(),
        )));
        assert_eq!(r, "wait tracker-error gh: 401\n");
    }

    /// Captures every spawn the supervisor decides to do; never errors.
    struct RecordingSpawner {
        calls: Mutex<Vec<SpawnCommand>>,
    }

    impl RecordingSpawner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
        fn take(&self) -> Vec<SpawnCommand> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl Spawner for RecordingSpawner {
        fn spawn_workflow(&self, cmd: &SpawnCommand) -> Result<()> {
            self.calls.lock().unwrap().push(cmd.clone());
            Ok(())
        }
    }

    /// Spawner that always returns an error; verifies the tick demotes
    /// `Spawn` to a `TrackerError`-flavoured pause so exit codes track.
    struct FailingSpawner;

    impl Spawner for FailingSpawner {
        fn spawn_workflow(&self, _cmd: &SpawnCommand) -> Result<()> {
            Err(anyhow::anyhow!("disk full"))
        }
    }

    /// Tiny stub tracker so `tick` can be exercised end-to-end. Returns
    /// whatever issue list was queued at construction.
    struct StubTracker {
        issues: Vec<Issue>,
    }

    impl Tracker for StubTracker {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn list_issues(&self, _root: &Path) -> Result<Vec<Issue>> {
            Ok(self.issues.clone())
        }
    }

    fn setup_with_tracker(tracker: Arc<dyn Tracker>) -> (tempfile::TempDir, Setup) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let config = RepoConfig::default();
        (
            dir,
            Setup {
                store,
                config,
                tracker,
                repo_root: std::path::PathBuf::from("/repo"),
            },
        )
    }

    fn open_issue(human_id: &str) -> Issue {
        Issue {
            id: format!("gh:{human_id}"),
            human_id: human_id.to_string(),
            title: format!("issue {human_id}"),
            status: "open".to_string(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn tick_spawns_when_engine_says_spawn() {
        let tracker = Arc::new(StubTracker {
            issues: vec![open_issue("1")],
        });
        let (_d, setup) = setup_with_tracker(tracker);
        let mut engine = AutonomousEngine::new();
        engine.toggle();
        let spawner = RecordingSpawner::new();
        let outcome = tick(&mut engine, Instant::now(), &setup, &[], &spawner);
        match outcome {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(cmd.issue.human_id, "1");
                assert_eq!(cmd.workflow, "standard");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        let calls = spawner.take();
        assert_eq!(
            calls.len(),
            1,
            "spawner must have been invoked exactly once"
        );
    }

    #[test]
    fn tick_does_not_spawn_when_engine_disabled() {
        // Engine stays off => Idle, no spawn attempt.
        let tracker = Arc::new(StubTracker {
            issues: vec![open_issue("1")],
        });
        let (_d, setup) = setup_with_tracker(tracker);
        let mut engine = AutonomousEngine::new();
        // Do NOT toggle.
        let spawner = RecordingSpawner::new();
        let outcome = tick(&mut engine, Instant::now(), &setup, &[], &spawner);
        assert_eq!(outcome, AutonomousOutcome::Idle);
        assert!(spawner.take().is_empty());
    }

    #[test]
    fn tick_does_not_spawn_when_slots_exhausted() {
        // Default AutonomousConfig.max_parallel is 3. Stage 3 in-flight
        // sessions so the engine refuses to spawn.
        let tracker = Arc::new(StubTracker {
            issues: vec![open_issue("1")],
        });
        let (_d, setup) = setup_with_tracker(tracker);
        let mut engine = AutonomousEngine::new();
        engine.toggle();
        let spawner = RecordingSpawner::new();
        let sessions: Vec<Session> = (0..3)
            .map(|i| {
                let mut s = Session::new(SessionId::new(format!("s-{i}")), "standard", 1);
                s.state = crate::session::SessionState::Running;
                s
            })
            .collect();
        let outcome = tick(&mut engine, Instant::now(), &setup, &sessions, &spawner);
        match outcome {
            AutonomousOutcome::WaitedFor(PauseReason::SlotsExhausted { in_flight, cap }) => {
                assert_eq!(in_flight, 3);
                assert_eq!(cap, 3);
            }
            other => panic!("expected SlotsExhausted, got {other:?}"),
        }
        assert!(spawner.take().is_empty());
    }

    #[test]
    fn tick_promotes_spawn_failure_to_tracker_error_outcome() {
        // A failing spawner must not let an outwardly-successful Spawn
        // outcome through — otherwise --once would exit 0 despite the
        // run never starting. Verify the demotion.
        let tracker = Arc::new(StubTracker {
            issues: vec![open_issue("1")],
        });
        let (_d, setup) = setup_with_tracker(tracker);
        let mut engine = AutonomousEngine::new();
        engine.toggle();
        let outcome = tick(&mut engine, Instant::now(), &setup, &[], &FailingSpawner);
        match outcome {
            AutonomousOutcome::WaitedFor(PauseReason::TrackerError(msg)) => {
                assert!(msg.contains("disk full"), "got: {msg}");
                assert!(msg.starts_with("spawn failed:"), "got: {msg}");
            }
            other => panic!("expected TrackerError, got {other:?}"),
        }
    }

    #[test]
    fn tick_filters_to_open_issues_only() {
        // The tracker returns mixed open + closed. The engine must see
        // only the open ones (filtering is the tick's responsibility,
        // not the engine's).
        let tracker = Arc::new(StubTracker {
            issues: vec![
                Issue {
                    id: "gh:99".to_string(),
                    human_id: "99".to_string(),
                    title: "closed".to_string(),
                    status: "closed".to_string(),
                    labels: Vec::new(),
                },
                open_issue("1"),
            ],
        });
        let (_d, setup) = setup_with_tracker(tracker);
        let mut engine = AutonomousEngine::new();
        engine.toggle();
        let spawner = RecordingSpawner::new();
        let outcome = tick(&mut engine, Instant::now(), &setup, &[], &spawner);
        match outcome {
            AutonomousOutcome::Spawn(cmd) => {
                assert_eq!(cmd.issue.human_id, "1", "must pick the open issue");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// Tracker whose `list_issues` returns Err — exercises the
    /// engine's `TrackerError` pause path.
    struct ErroringTracker;

    impl Tracker for ErroringTracker {
        fn name(&self) -> &'static str {
            "err"
        }
        fn list_issues(&self, _root: &Path) -> Result<Vec<Issue>> {
            anyhow::bail!("gh: rate limited")
        }
    }

    #[test]
    fn tick_surfaces_tracker_error_as_wait_outcome() {
        let (_d, setup) = setup_with_tracker(Arc::new(ErroringTracker));
        let mut engine = AutonomousEngine::new();
        engine.toggle();
        let spawner = RecordingSpawner::new();
        let outcome = tick(&mut engine, Instant::now(), &setup, &[], &spawner);
        match outcome {
            AutonomousOutcome::WaitedFor(PauseReason::TrackerError(msg)) => {
                assert!(msg.contains("rate limited"), "got: {msg}");
            }
            other => panic!("expected TrackerError, got {other:?}"),
        }
        assert!(spawner.take().is_empty());
    }

    #[test]
    fn render_reason_covers_every_variant() {
        // Exhaustive coverage so a new variant breaks this test until
        // the match is updated.
        assert_eq!(
            render_reason(&PauseReason::SlotsExhausted {
                in_flight: 1,
                cap: 2
            }),
            "slots-exhausted in_flight=1 cap=2"
        );
        assert_eq!(
            render_reason(&PauseReason::NoUnclaimedIssues),
            "no-unclaimed-issues"
        );
        assert_eq!(
            render_reason(&PauseReason::TrackerError("e".to_string())),
            "tracker-error e"
        );
    }
}
