//! Crash forensics for sessions whose driver process died unexpectedly.
//!
//! The workflow executor stamps its own pid into [`Session::driver_pid`]
//! while it's driving a session, and clears it on every legitimate exit
//! (`Completed`, `Failed`, `AwaitingGate`). If the fleet process is killed
//! mid-run — `SIGKILL`, OOM, host reboot, terminal close — the on-disk
//! `meta.json` is frozen with `state = Running` and `driver_pid =
//! Some(<dead pid>)`. The TUI then either spins waiting on a corpse or
//! refuses to free up the autonomous-mode slot.
//!
//! The reaper closes this gap. At fleet startup, [`reap`] walks the
//! session store, finds sessions in [`SessionState::Running`] whose
//! `driver_pid` is missing or no longer alive, transitions them to
//! [`SessionState::Crashed`], and drops a forensic snapshot
//! (`crash.json`) alongside the session's `meta.json`. The snapshot
//! contains the last few lines of every per-node log so the user can
//! see what was happening when the driver died without needing to
//! reconstruct logs from scratch.
//!
//! [`AwaitingGate`](SessionState::AwaitingGate) is deliberately left
//! alone. By design, no fleet process is driving a gated session —
//! `clear_driver_pid` was called in [`crate::workflow::executor`] before
//! the executor returned. The user (or a future scheduled resume)
//! re-enters the loop via `fleet workflow resume`. Reaping gated
//! sessions would punish workflows whose users go to lunch.
//!
//! ## PID reuse
//!
//! `kill -0` checks for the *existence* of a process with that pid;
//! Unix can reuse pids after the original exits. If fleet crashed at
//! pid 42 and the host has long since reassigned pid 42 to an unrelated
//! process, this probe says "alive" and the reaper leaves the session
//! stuck. Mitigations:
//! - The reaper is run again on every fleet startup; the window for
//!   this collision is short.
//! - The user can force a reap from the CLI (`fleet sessions reap`).
//! - A future enhancement could combine pid with start-time (`/proc/<pid>/stat`
//!   field 22 on Linux, `proc_pidinfo` on macOS); v1 keeps things simple.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use super::store::SessionStore;
use super::{Session, SessionId, SessionState};

/// Probe for whether a process id is currently alive on the host. Behind
/// a trait so the reaper can be exercised in tests without spawning
/// child processes — and so future implementations (start-time hashing,
/// /proc inspection) drop in without touching the reaper.
pub trait PidProbe: Send + Sync {
    fn is_alive(&self, pid: u32) -> bool;
}

/// Production [`PidProbe`]. Shells out to `kill -0 <pid>`, which sends
/// no signal but returns exit 0 iff a process with that pid exists and
/// is signalable. `EPERM` (exists but unsignalable by us) maps to
/// "alive" because the alternative — treating EPERM as "dead" — would
/// wrongly reap sessions driven by a second fleet instance running
/// under a different uid in the same repo. That is not a supported
/// configuration, but we still prefer false-negative (no reap) over
/// false-positive (wrong reap) when in doubt.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealPidProbe;

impl PidProbe for RealPidProbe {
    fn is_alive(&self, pid: u32) -> bool {
        // `kill -0` is shell-builtin in most shells but we invoke the
        // standalone binary directly to avoid shell-quoting surprises.
        // A spawn failure is conservatively treated as "alive": we'd
        // rather skip a reap than incorrectly mark a live session
        // crashed when the probe itself broke. Stderr is silenced
        // because the standalone `kill` binary prints "No such process"
        // for dead pids — useful as an exit-code signal, noisy as a
        // user-facing line.
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_or(true, |s| s.success())
    }
}

/// Why a session was reaped. Surfaced in the [`ReapReport`] returned to
/// the caller and persisted into `crash.json` for post-hoc inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReapReason {
    /// Session was [`SessionState::Running`] with no `driver_pid` ever
    /// stamped. Means the driver crashed between transitioning to
    /// Running and persisting its pid — narrow window, but covered.
    NoDriverPid,
    /// Session was [`SessionState::Running`] with a `driver_pid` that
    /// no longer points to a live process.
    DeadDriver { pid: u32 },
}

impl ReapReason {
    /// Short human-readable form used in CLI / TUI summaries.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::NoDriverPid => "no driver_pid recorded".to_string(),
            Self::DeadDriver { pid } => format!("driver pid {pid} is dead"),
        }
    }
}

/// One reaped session. Returned in [`ReapReport`] so the caller can
/// print or render a summary; no caller is expected to act on this
/// programmatically beyond that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedSession {
    pub id: SessionId,
    pub reason: ReapReason,
}

/// Outcome of a [`reap`] sweep. `scanned` is the total number of
/// sessions inspected; `reaped` is the ones that transitioned to
/// `Crashed`. `scanned >= reaped.len()` always — sessions in terminal
/// states are scanned but not reaped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub scanned: usize,
    pub reaped: Vec<ReapedSession>,
}

/// Walk the session store and reap any session whose driver is gone.
/// One sweep, fully sequential, no concurrency — fleet startup latency
/// is dominated by container/image probes elsewhere; a few `kill -0`
/// invocations are noise.
///
/// Errors from individual sessions (unreadable meta.json, save failure)
/// are logged via `tracing::warn` and the sweep continues — one bad
/// session must not stop the rest from being recovered.
pub fn reap(
    store: &SessionStore,
    probe: &dyn PidProbe,
    now_ms: u64,
) -> Result<ReapReport> {
    let mut report = ReapReport::default();
    let ids = store
        .list()
        .with_context(|| format!("listing sessions under {}", store.root().display()))?;
    for id in ids {
        report.scanned += 1;
        let mut session = match store.load(&id) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(session = %id, error = %err, "reaper: skipping unreadable session");
                continue;
            }
        };

        let Some(reason) = classify(&session, probe) else {
            continue;
        };

        // Transition first, then snapshot, then save. If the snapshot
        // write fails we still want the state transition persisted so a
        // subsequent reap doesn't loop on this session forever.
        if let Err(err) = session.transition_to(SessionState::Crashed, now_ms) {
            tracing::warn!(session = %id, error = %err, "reaper: Running -> Crashed rejected");
            continue;
        }
        session.clear_driver_pid(now_ms);

        if let Err(err) = write_crash_snapshot(store, &session, &reason) {
            tracing::warn!(session = %id, error = %err, "reaper: writing crash.json failed");
        }

        if let Err(err) = store.save(&session) {
            tracing::warn!(session = %id, error = %err, "reaper: saving Crashed session failed");
            continue;
        }

        report.reaped.push(ReapedSession {
            id: session.id.clone(),
            reason,
        });
    }
    Ok(report)
}

/// Pure classifier: what reason (if any) would justify reaping this
/// session right now? Factored out of [`reap`] so the policy is unit-
/// testable without filesystem fixtures.
fn classify(session: &Session, probe: &dyn PidProbe) -> Option<ReapReason> {
    if session.state != SessionState::Running {
        return None;
    }
    match session.driver_pid {
        None => Some(ReapReason::NoDriverPid),
        Some(pid) if !probe.is_alive(pid) => Some(ReapReason::DeadDriver { pid }),
        Some(_) => None,
    }
}

/// The on-disk forensic record. Persisted next to `meta.json` so a
/// future TUI "show crash details" pane can render it without
/// reconstructing state.
#[derive(Debug, Serialize)]
struct CrashSnapshot<'a> {
    session_id: &'a str,
    workflow: &'a str,
    current_node: Option<&'a str>,
    reason: &'a ReapReason,
    /// Last node the session reported it was running. Same value as
    /// `current_node`; named separately for readability of the JSON.
    node_at_crash: Option<&'a str>,
    /// Wallclock at the moment the reaper transitioned the session.
    crashed_at_ms: u64,
    /// Map of node-log filename (e.g. `"plan.log"`) → last N lines of
    /// its contents. Bounded so a runaway agent's log doesn't bloat
    /// the snapshot.
    log_tails: BTreeMap<String, String>,
}

const LOG_TAIL_LINES: usize = 50;

fn write_crash_snapshot(
    store: &SessionStore,
    session: &Session,
    reason: &ReapReason,
) -> Result<()> {
    let log_tails = collect_log_tails(&store.session_dir(&session.id).join("logs"))?;
    let snapshot = CrashSnapshot {
        session_id: session.id.as_str(),
        workflow: &session.workflow,
        current_node: session.current_node.as_deref(),
        reason,
        node_at_crash: session.current_node.as_deref(),
        crashed_at_ms: session.updated_at_ms,
        log_tails,
    };
    let path = store.session_dir(&session.id).join("crash.json");
    let body = serde_json::to_string_pretty(&snapshot)
        .context("serialising crash snapshot")?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Read each per-node log file under `logs_dir` and keep the last N
/// lines. Missing `logs/` directory is not an error — a session that
/// crashed in `Created` may never have written a log file.
fn collect_log_tails(logs_dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(anyhow::Error::from(err).context(format!("reading {}", logs_dir.display()))),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("scanning {}", logs_dir.display()))?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let body = match std::fs::read_to_string(entry.path()) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(log = %entry.path().display(), error = %err, "reaper: skipping unreadable log");
                continue;
            }
        };
        out.insert(name, tail_lines(&body, LOG_TAIL_LINES));
    }
    Ok(out)
}

/// Keep the last `n` newline-separated lines of `s`. Free function so
/// the truncation policy can be unit-tested against the contract.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{IssueContext, SessionId};
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// Test pid probe: configurable answers per-pid.
    struct ScriptedProbe {
        alive: HashSet<u32>,
    }

    impl ScriptedProbe {
        fn with_alive<I: IntoIterator<Item = u32>>(pids: I) -> Self {
            Self {
                alive: pids.into_iter().collect(),
            }
        }
    }

    impl PidProbe for ScriptedProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
    }

    /// Records every pid asked about, so we can assert the reaper
    /// doesn't probe unnecessarily.
    struct RecordingProbe {
        asked: Mutex<Vec<u32>>,
    }

    impl RecordingProbe {
        fn new() -> Self {
            Self {
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl PidProbe for RecordingProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.asked.lock().unwrap().push(pid);
            false
        }
    }

    fn fresh_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::at(dir.path().to_path_buf());
        (dir, s)
    }

    fn create_session(store: &SessionStore, id: &str, state: SessionState, driver_pid: Option<u32>) -> Session {
        let mut s = Session::new(SessionId::new(id), "standard", 1_000);
        store.create(&s).unwrap();
        if state != SessionState::Created {
            s.transition_to(SessionState::Running, 2_000).unwrap();
            // Walk through any further legal transitions to reach the
            // requested target state.
            match state {
                SessionState::Running => {}
                SessionState::AwaitingGate => {
                    s.transition_to(SessionState::AwaitingGate, 3_000).unwrap();
                }
                SessionState::Completed => {
                    s.transition_to(SessionState::Completed, 3_000).unwrap();
                }
                SessionState::Failed => {
                    s.transition_to(SessionState::Failed, 3_000).unwrap();
                }
                SessionState::Crashed => {
                    s.transition_to(SessionState::Crashed, 3_000).unwrap();
                }
                SessionState::Created => unreachable!(),
            }
        }
        s.driver_pid = driver_pid;
        store.save(&s).unwrap();
        s
    }

    #[test]
    fn empty_store_returns_empty_report() {
        let (_d, store) = fresh_store();
        let probe = ScriptedProbe::with_alive([]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.scanned, 0);
        assert!(r.reaped.is_empty());
    }

    #[test]
    fn classify_skips_terminal_states() {
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        let probe = ScriptedProbe::with_alive([]);
        for state in [SessionState::Completed, SessionState::Failed, SessionState::Crashed] {
            s.state = state;
            s.driver_pid = Some(42);
            assert!(classify(&s, &probe).is_none(), "{state:?} must not be reaped");
        }
    }

    #[test]
    fn classify_skips_created_state() {
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        s.state = SessionState::Created;
        let probe = ScriptedProbe::with_alive([]);
        assert!(classify(&s, &probe).is_none());
    }

    #[test]
    fn classify_skips_awaiting_gate() {
        // The most important leave-alone case: a workflow paused at a
        // gate has no driver by design.
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        s.state = SessionState::AwaitingGate;
        s.driver_pid = None;
        let probe = ScriptedProbe::with_alive([]);
        assert!(classify(&s, &probe).is_none());
    }

    #[test]
    fn classify_reaps_running_without_driver_pid() {
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        s.state = SessionState::Running;
        s.driver_pid = None;
        let probe = ScriptedProbe::with_alive([]);
        assert_eq!(classify(&s, &probe), Some(ReapReason::NoDriverPid));
    }

    #[test]
    fn classify_reaps_running_with_dead_driver() {
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        s.state = SessionState::Running;
        s.driver_pid = Some(99_999);
        let probe = ScriptedProbe::with_alive([]);
        assert_eq!(
            classify(&s, &probe),
            Some(ReapReason::DeadDriver { pid: 99_999 })
        );
    }

    #[test]
    fn classify_leaves_running_with_live_driver_alone() {
        let mut s = Session::new(SessionId::new("s-x"), "wf", 1);
        s.state = SessionState::Running;
        s.driver_pid = Some(42);
        let probe = ScriptedProbe::with_alive([42]);
        assert!(classify(&s, &probe).is_none());
    }

    #[test]
    fn reap_transitions_orphaned_running_to_crashed() {
        let (_d, store) = fresh_store();
        create_session(&store, "s-orphan", SessionState::Running, Some(99_999));
        let probe = ScriptedProbe::with_alive([]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.scanned, 1);
        assert_eq!(r.reaped.len(), 1);
        assert_eq!(r.reaped[0].id, SessionId::new("s-orphan"));
        assert_eq!(
            r.reaped[0].reason,
            ReapReason::DeadDriver { pid: 99_999 }
        );
        let loaded = store.load(&SessionId::new("s-orphan")).unwrap();
        assert_eq!(loaded.state, SessionState::Crashed);
        assert_eq!(loaded.driver_pid, None);
        assert_eq!(loaded.updated_at_ms, 9_000);
    }

    #[test]
    fn reap_writes_crash_snapshot_next_to_meta() {
        let (_d, store) = fresh_store();
        let s = create_session(&store, "s-snap", SessionState::Running, Some(99_999));
        // Drop a log file so the snapshot picks something up.
        std::fs::write(
            store.session_dir(&s.id).join("logs").join("plan.log"),
            "line 1\nline 2\nline 3\n",
        )
        .unwrap();
        let probe = ScriptedProbe::with_alive([]);
        let _ = reap(&store, &probe, 9_000).unwrap();
        let snap_path = store.session_dir(&s.id).join("crash.json");
        assert!(snap_path.exists(), "crash.json must be written next to meta.json");
        let body = std::fs::read_to_string(&snap_path).unwrap();
        // Spot-check the high-level shape.
        assert!(body.contains("\"session_id\": \"s-snap\""), "got: {body}");
        assert!(body.contains("\"workflow\": \"standard\""));
        assert!(body.contains("\"dead_driver\""), "reason discriminant missing");
        assert!(body.contains("\"pid\": 99999"));
        assert!(body.contains("plan.log"));
        assert!(body.contains("line 3"));
    }

    #[test]
    fn reap_leaves_completed_failed_awaiting_gate_alone() {
        let (_d, store) = fresh_store();
        create_session(&store, "s-done", SessionState::Completed, None);
        create_session(&store, "s-fail", SessionState::Failed, None);
        create_session(&store, "s-gate", SessionState::AwaitingGate, None);
        let probe = ScriptedProbe::with_alive([]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.scanned, 3);
        assert!(r.reaped.is_empty(), "terminal + gated sessions must not be reaped");
        // States unchanged.
        assert_eq!(
            store.load(&SessionId::new("s-done")).unwrap().state,
            SessionState::Completed
        );
        assert_eq!(
            store.load(&SessionId::new("s-gate")).unwrap().state,
            SessionState::AwaitingGate
        );
    }

    #[test]
    fn reap_skips_running_with_live_driver() {
        let (_d, store) = fresh_store();
        create_session(&store, "s-alive", SessionState::Running, Some(42));
        let probe = ScriptedProbe::with_alive([42]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.scanned, 1);
        assert!(r.reaped.is_empty());
        assert_eq!(
            store.load(&SessionId::new("s-alive")).unwrap().state,
            SessionState::Running
        );
    }

    #[test]
    fn reap_reaps_running_with_no_driver_pid() {
        let (_d, store) = fresh_store();
        create_session(&store, "s-nopid", SessionState::Running, None);
        let probe = ScriptedProbe::with_alive([]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.reaped.len(), 1);
        assert_eq!(r.reaped[0].reason, ReapReason::NoDriverPid);
    }

    #[test]
    fn reap_picks_only_the_dead_among_mixed_sessions() {
        let (_d, store) = fresh_store();
        create_session(&store, "s-alive", SessionState::Running, Some(7));
        create_session(&store, "s-dead", SessionState::Running, Some(99_999));
        create_session(&store, "s-done", SessionState::Completed, None);
        create_session(&store, "s-gate", SessionState::AwaitingGate, None);
        let probe = ScriptedProbe::with_alive([7]);
        let r = reap(&store, &probe, 9_000).unwrap();
        assert_eq!(r.scanned, 4);
        assert_eq!(r.reaped.len(), 1);
        assert_eq!(r.reaped[0].id, SessionId::new("s-dead"));
    }

    #[test]
    fn reap_does_not_probe_terminal_session_pids() {
        // Avoid wasting forks on sessions where the answer doesn't
        // matter — a terminal session's pid field is informational at
        // best.
        let (_d, store) = fresh_store();
        let mut s = Session::new(SessionId::new("s-done"), "standard", 1_000);
        store.create(&s).unwrap();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        s.transition_to(SessionState::Completed, 3_000).unwrap();
        s.driver_pid = Some(42);
        store.save(&s).unwrap();
        let probe = RecordingProbe::new();
        let _ = reap(&store, &probe, 9_000).unwrap();
        assert!(
            probe.asked.lock().unwrap().is_empty(),
            "completed session should not be probed; asked = {:?}",
            probe.asked.lock().unwrap()
        );
    }

    #[test]
    fn reaped_session_retains_issue_context_for_post_mortem() {
        // The crash snapshot doesn't currently include issue context,
        // but the on-disk meta.json must still — the user looks at
        // `fleet sessions show` after a crash to learn what was being
        // worked on.
        let (_d, store) = fresh_store();
        let mut s = Session::new(SessionId::new("s-iss"), "standard", 1_000);
        store.create(&s).unwrap();
        s.transition_to(SessionState::Running, 2_000).unwrap();
        s.driver_pid = Some(99_999);
        s.issue = Some(IssueContext {
            id: "gh:42".to_string(),
            human_id: "42".to_string(),
            title: "Fix the parser".to_string(),
            labels: Vec::new(),
        });
        store.save(&s).unwrap();
        let probe = ScriptedProbe::with_alive([]);
        let _ = reap(&store, &probe, 9_000).unwrap();
        let loaded = store.load(&SessionId::new("s-iss")).unwrap();
        assert_eq!(loaded.state, SessionState::Crashed);
        assert!(loaded.issue.is_some(), "issue context must survive reap");
    }

    #[test]
    fn tail_lines_returns_all_lines_when_under_limit() {
        assert_eq!(tail_lines("a\nb\nc", 10), "a\nb\nc");
    }

    #[test]
    fn tail_lines_returns_last_n_when_over_limit() {
        let text = (1..=100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(&text, 3);
        assert_eq!(out, "98\n99\n100");
    }

    #[test]
    fn tail_lines_handles_empty_input() {
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn collect_log_tails_returns_empty_when_logs_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = collect_log_tails(&dir.path().join("no-such")).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn collect_log_tails_reads_each_log_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.log"), "a-1\na-2\n").unwrap();
        std::fs::write(dir.path().join("b.log"), "b-1\n").unwrap();
        let out = collect_log_tails(dir.path()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out.get("a.log").unwrap(), "a-1\na-2");
        assert_eq!(out.get("b.log").unwrap(), "b-1");
    }

}
