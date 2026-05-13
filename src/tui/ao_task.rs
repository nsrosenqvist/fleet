//! Background subprocess for AO actions invoked from the TUI.
//!
//! Mirrors [`super::bringup::BringUp`]: a single owned `JoinHandle` drives the
//! child (or chain of children, in the multi-phase case), per-stream reader
//! threads forward output line-by-line into an mpsc channel, and the UI ticks
//! the resulting struct each frame to surface progress in the status bar.
//!
//! What this enables is the actions formerly run via [`super::subprocess::
//! suspend_around`] — `ao session kill`, `ao stop`, `ao start`, `ao spawn`,
//! the restart-for-orchestrator chain — running without tearing down the alt
//! screen. The TUI shows a spinner + label while the task runs and replaces
//! it with the normal green/red flash on completion. On failure the first
//! line of captured stderr is fed to `action_error` so the user still sees
//! the cause.
//!
//! Multi-phase support exists for the restart chain (`ao stop --all` followed
//! by `ao start <project>`): a `Vec<TaskPhase>` runs sequentially, aborting
//! on the first non-zero exit. Each phase optionally carries a sub-label
//! that overrides the task-wide label in the spinner while it runs.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// One step in an AO task. For single-step actions (kill, stop, spawn,
/// start) the `phases` vec passed to [`AoTask::spawn`] has one element;
/// for the restart chain it has two.
pub(super) struct TaskPhase {
    pub program: String,
    pub args: Vec<String>,
    /// `(name, value)` pairs set in the child's env. Carried as owned
    /// strings so the supervisor thread can take ownership; the TUI
    /// never logs these.
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    /// Sub-label shown in the spinner while this phase runs. Set for
    /// multi-phase tasks (e.g. "stopping AO" then "starting AO" for
    /// the restart chain); `None` for single-phase tasks leaves the
    /// task-wide label alone.
    pub label: Option<String>,
}

/// How many tail lines we keep for failure surfacing. 16 is enough to
/// catch the typical stack/diagnostic dump from `ao` / `limactl` while
/// keeping memory bounded.
const TAIL_BUDGET: usize = 16;

enum AoTaskEvent {
    PhaseStarted(Option<String>),
    Line(String),
    Exited(Result<i32>),
}

/// Handle to one in-flight AO subprocess (or sequential chain). Held
/// in `App::in_flight_ao`; ticked each main-loop iteration.
pub(super) struct AoTask {
    rx: Receiver<AoTaskEvent>,
    join: Option<JoinHandle<()>>,
    label: String,
    phase_label: Option<String>,
    tail: VecDeque<String>,
    started_at: Instant,
    finished: Option<Result<i32>>,
}

impl AoTask {
    /// Spawn the supervisor thread and return immediately. `label` is
    /// the task-wide identifier shown in the spinner and the
    /// completion flash ("killing sb-42", "starting AO", …).
    pub(super) fn spawn(label: impl Into<String>, phases: Vec<TaskPhase>) -> Result<Self> {
        if phases.is_empty() {
            anyhow::bail!("AoTask::spawn requires at least one phase");
        }
        let (tx, rx) = mpsc::channel();
        let join = thread::spawn(move || drive_phases(phases, &tx));
        Ok(Self {
            rx,
            join: Some(join),
            label: label.into(),
            phase_label: None,
            tail: VecDeque::with_capacity(TAIL_BUDGET),
            started_at: Instant::now(),
            finished: None,
        })
    }

    /// Drain queued events. Call between draws — non-blocking.
    pub(super) fn tick(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(AoTaskEvent::PhaseStarted(label)) => self.phase_label = label,
                Ok(AoTaskEvent::Line(line)) => self.push_tail(line),
                Ok(AoTaskEvent::Exited(res)) => {
                    self.finished = Some(res);
                    if let Some(join) = self.join.take() {
                        let _ = join.join();
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    pub(super) fn outcome(&self) -> Option<&Result<i32>> {
        self.finished.as_ref()
    }

    pub(super) fn label(&self) -> &str {
        &self.label
    }

    pub(super) fn phase_label(&self) -> Option<&str> {
        self.phase_label.as_deref()
    }

    pub(super) fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Best guess at the actual error line for the failure flash.
    /// Walks the tail buffer newest→oldest and picks the first line
    /// containing an explicit error marker (`Error:`, `✖`, `Failed`,
    /// …). Falls back to the last captured line if no marker is
    /// found, returns `None` when nothing was captured.
    ///
    /// The walk order matters: an `ao start` failure prints the
    /// `Error: Failed to setup orchestrator: …` line *before* a
    /// couple of harmless trailing lines (`[next] exited with code
    /// null` etc.), so the naive last-line surface would hide the
    /// real cause. Trailing noise after the marker is also typical
    /// for tools that emit a banner followed by post-error cleanup
    /// messages.
    pub(super) fn failure_hint(&self) -> Option<&str> {
        const MARKERS: &[&str] = &[
            "Error:", "error:", "ERROR:", "✖", "✗", "Failed", "failed:", "FAILED", "fatal:",
        ];
        for line in self.tail.iter().rev() {
            if MARKERS.iter().any(|m| line.contains(m)) {
                return Some(line.as_str());
            }
        }
        self.tail.back().map(String::as_str)
    }

    fn push_tail(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        if self.tail.len() == TAIL_BUDGET {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }
}

/// Supervisor: run each phase sequentially, abort on the first non-zero
/// exit or spawn / wait error. Sends `PhaseStarted` before each phase
/// and `Exited` exactly once at the end.
fn drive_phases(phases: Vec<TaskPhase>, tx: &Sender<AoTaskEvent>) {
    for phase in phases {
        // Best-effort phase announcement. If the receiver is gone we
        // bail out without spawning the child — the UI has dropped
        // the task.
        if tx
            .send(AoTaskEvent::PhaseStarted(phase.label.clone()))
            .is_err()
        {
            return;
        }
        match run_phase(&phase, tx) {
            Ok(0) => {}
            Ok(code) => {
                let _ = tx.send(AoTaskEvent::Exited(Ok(code)));
                return;
            }
            Err(e) => {
                let _ = tx.send(AoTaskEvent::Exited(Err(e)));
                return;
            }
        }
    }
    let _ = tx.send(AoTaskEvent::Exited(Ok(0)));
}

fn run_phase(phase: &TaskPhase, tx: &Sender<AoTaskEvent>) -> Result<i32> {
    let mut cmd = Command::new(&phase.program);
    cmd.args(&phase.args);
    for (k, v) in &phase.env_set {
        cmd.env(k, v);
    }
    for k in &phase.env_unset {
        cmd.env_remove(k);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn `{}`", phase.program))?;
    let stdout = child
        .stdout
        .take()
        .context("child has no stdout pipe (Stdio::piped requested)")?;
    let stderr = child
        .stderr
        .take()
        .context("child has no stderr pipe (Stdio::piped requested)")?;
    let tx_out = tx.clone();
    let tx_err = tx.clone();
    let h_out = thread::spawn(move || forward_lines(stdout, &tx_out));
    let h_err = thread::spawn(move || forward_lines(stderr, &tx_err));
    let status = child.wait().context("wait on child")?;
    let _ = h_out.join();
    let _ = h_err.join();
    Ok(status.code().unwrap_or(-1))
}

fn forward_lines(reader: impl Read, tx: &Sender<AoTaskEvent>) {
    let buf = BufReader::new(reader);
    for line in buf.lines().map_while(Result::ok) {
        if tx.send(AoTaskEvent::Line(line)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn drive_to_completion(task: &mut AoTask) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !task.is_finished() && Instant::now() < deadline {
            task.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        // One last drain after the supervisor exits.
        task.tick();
        assert!(task.is_finished(), "task did not finish within 5s");
    }

    #[test]
    fn single_phase_round_trip() {
        let mut task = AoTask::spawn(
            "echo hi",
            vec![TaskPhase {
                program: "sh".into(),
                args: vec!["-c".into(), "echo hi".into()],
                env_set: Vec::new(),
                env_unset: Vec::new(),
                label: None,
            }],
        )
        .expect("spawn");
        drive_to_completion(&mut task);
        match task.outcome() {
            Some(Ok(0)) => {}
            other => panic!("expected Ok(0), got {other:?}"),
        }
        assert_eq!(task.failure_hint(), Some("hi"));
        assert!(task.phase_label().is_none());
    }

    #[test]
    fn failure_hint_falls_back_to_last_line_without_markers() {
        let mut task = AoTask::spawn(
            "multi-line",
            vec![TaskPhase {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "echo first; echo middle; echo last; exit 9".into(),
                ],
                env_set: Vec::new(),
                env_unset: Vec::new(),
                label: None,
            }],
        )
        .expect("spawn");
        drive_to_completion(&mut task);
        assert_eq!(task.failure_hint(), Some("last"));
    }

    #[test]
    fn failure_hint_prefers_marked_line_over_trailing_noise() {
        // Reproduces the `ao start` failure shape: a meaningful
        // `Error: …` line followed by post-failure cleanup noise
        // (`[next] exited with code null`). The naive last-line
        // surface would hide the real cause; the marker walk picks
        // the Error: line.
        let script = "echo '[notifier-webhook] No url configured — notifications will be no-ops'; \
                      echo '✖ Orchestrator setup failed'; \
                      echo 'Error: Failed to setup orchestrator: Session fl-orchestrator cannot be restored'; \
                      echo '[next] exited with code null'; \
                      echo '[direct-terminal] exited with code null'; \
                      exit 1";
        let mut task = AoTask::spawn(
            "ao start",
            vec![TaskPhase {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                env_set: Vec::new(),
                env_unset: Vec::new(),
                label: None,
            }],
        )
        .expect("spawn");
        drive_to_completion(&mut task);
        let hint = task.failure_hint().expect("expected a hint");
        assert!(
            hint.starts_with("Error:"),
            "expected the Error: line, got: {hint}"
        );
        assert!(
            hint.contains("Session fl-orchestrator cannot be restored"),
            "expected the real cause to be surfaced, got: {hint}"
        );
    }

    #[test]
    fn non_zero_exit_surfaces_through_outcome() {
        let mut task = AoTask::spawn(
            "false",
            vec![TaskPhase {
                program: "sh".into(),
                args: vec!["-c".into(), "echo nope >&2; exit 3".into()],
                env_set: Vec::new(),
                env_unset: Vec::new(),
                label: None,
            }],
        )
        .expect("spawn");
        drive_to_completion(&mut task);
        match task.outcome() {
            Some(Ok(3)) => {}
            other => panic!("expected Ok(3), got {other:?}"),
        }
        assert_eq!(task.failure_hint(), Some("nope"));
    }

    #[test]
    fn second_phase_skipped_when_first_fails() {
        let mut task = AoTask::spawn(
            "chain",
            vec![
                TaskPhase {
                    program: "sh".into(),
                    args: vec!["-c".into(), "exit 7".into()],
                    env_set: Vec::new(),
                    env_unset: Vec::new(),
                    label: Some("first".into()),
                },
                TaskPhase {
                    program: "sh".into(),
                    args: vec!["-c".into(), "echo should-not-run".into()],
                    env_set: Vec::new(),
                    env_unset: Vec::new(),
                    label: Some("second".into()),
                },
            ],
        )
        .expect("spawn");
        drive_to_completion(&mut task);
        match task.outcome() {
            Some(Ok(7)) => {}
            other => panic!("expected Ok(7), got {other:?}"),
        }
        // The second phase never sent PhaseStarted because run_phase
        // returned non-zero and the supervisor aborted.
        assert_eq!(task.phase_label(), Some("first"));
        assert!(
            !task.tail.iter().any(|l| l.contains("should-not-run")),
            "second phase must not have executed"
        );
    }

    #[test]
    fn empty_phases_rejected() {
        let Err(err) = AoTask::spawn("noop", Vec::new()) else {
            panic!("expected an error for empty phases");
        };
        assert!(err.to_string().contains("at least one phase"));
    }
}
