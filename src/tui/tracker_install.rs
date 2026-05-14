//! Streaming subprocess wrapper for in-VM tracker installs.
//!
//! Mirrors the shape of [`crate::tui::bringup::BringUp`] — a piped
//! `limactl shell fleet-vm bash -c '<script>'` child whose stdout +
//! stderr fold into a bounded ring buffer the renderer reads each
//! tick. Different from `BringUp` only in the command it spawns
//! (apt / curl install scripts vs `limactl start`) and the fact
//! that it can chain multiple installs in one supervisor thread.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::preflight::{self, VM_NAME};

const TAIL_BUDGET: usize = 12;

enum InstallEvent {
    Line(String),
    /// One tool finished; carries the tool name and exit status so
    /// the renderer can show "✓ git-bug" / "✗ gh" between installs.
    StepDone {
        tool: String,
        code: i32,
    },
    /// Supervisor thread finished all installs.
    Finished(Result<()>),
}

/// Handle to an in-flight tracker install run. Public surface is the
/// same as `BringUp`: caller drives via `tick` + `is_finished` and
/// reads `tail_lines` for the progress modal body.
pub(super) struct TrackerInstall {
    rx: Receiver<InstallEvent>,
    join: Option<JoinHandle<()>>,
    tail: VecDeque<String>,
    started_at: Instant,
    /// Names of the tools the user asked to install. Used for the
    /// "installing N tools" header in the modal.
    tools: Vec<String>,
    /// Per-tool outcomes accumulated as `StepDone` events arrive.
    /// The renderer surfaces these as ticked rows above the live
    /// output tail.
    steps: Vec<StepOutcome>,
    finished: Option<Result<()>>,
}

#[derive(Clone)]
pub(super) struct StepOutcome {
    pub(super) tool: String,
    pub(super) code: i32,
}

impl TrackerInstall {
    /// Spawn one install per tool sequentially. Each install runs as
    /// a separate `limactl shell fleet-vm bash -c '<script>'`
    /// invocation — failure of one doesn't block the next (the user
    /// might recover one tool out of two and that's still progress).
    /// `preflight::install_command` is the source of the script.
    pub(super) fn spawn(tools: Vec<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let tools_for_thread = tools.clone();
        let join = thread::spawn(move || drive_supervisor(tools_for_thread, &tx));
        Self {
            rx,
            join: Some(join),
            tail: VecDeque::with_capacity(TAIL_BUDGET),
            started_at: Instant::now(),
            tools,
            steps: Vec::new(),
            finished: None,
        }
    }

    pub(super) fn tick(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(InstallEvent::Line(line)) => self.push_tail(line),
                Ok(InstallEvent::StepDone { tool, code }) => {
                    self.steps.push(StepOutcome { tool, code });
                }
                Ok(InstallEvent::Finished(res)) => {
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

    pub(super) fn tail_lines(&self) -> impl Iterator<Item = &str> {
        self.tail.iter().map(String::as_str)
    }

    pub(super) fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub(super) fn tools(&self) -> &[String] {
        &self.tools
    }

    pub(super) fn steps(&self) -> &[StepOutcome] {
        &self.steps
    }

    /// `Some(Ok(()))` means every install command ran to completion
    /// (some may have returned non-zero; check [`Self::steps`]).
    /// `Some(Err)` means a spawn failed catastrophically (the
    /// supervisor couldn't even launch limactl).
    pub(super) fn outcome(&self) -> Option<&Result<()>> {
        self.finished.as_ref()
    }

    fn push_tail(&mut self, line: String) {
        if self.tail.len() == TAIL_BUDGET {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }
}

fn drive_supervisor(tools: Vec<String>, tx: &Sender<InstallEvent>) {
    for tool in tools {
        let Some(install) = preflight::install_command(&tool) else {
            let _ = tx.send(InstallEvent::Line(format!(
                "fleet: no install command registered for `{tool}`"
            )));
            let _ = tx.send(InstallEvent::StepDone { tool, code: -1 });
            continue;
        };
        // Header line in the tail so the user can see which tool's
        // output is below.
        let _ = tx.send(InstallEvent::Line(format!(
            "── installing `{tool}` inside fleet-vm (as {}) ──",
            install.as_user
        )));
        let code = match run_one(install.script, install.as_user, tx) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(InstallEvent::Line(format!("fleet: install failed: {e:#}")));
                -1
            }
        };
        let _ = tx.send(InstallEvent::StepDone { tool, code });
    }
    let _ = tx.send(InstallEvent::Finished(Ok(())));
}

/// Spawn a single `limactl shell --user <as_user> fleet-vm bash -c
/// '<script>'` install. `as_user` is picked by [`preflight::install_command`]
/// — system-scope installs (apt, /usr/local/bin) run as `root` since
/// the lima user no longer has sudo; user-scope ones (gh extensions)
/// run as `lima` so they land in $HOME/.local/share/gh/extensions.
fn run_one(script: &str, as_user: &str, tx: &Sender<InstallEvent>) -> Result<i32> {
    let mut child = Command::new("limactl")
        .arg("shell")
        .arg("--user")
        .arg(as_user)
        .arg(VM_NAME)
        .arg("bash")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn `limactl shell --user {as_user} fleet-vm bash`"))?;
    let stdout = child.stdout.take().context("child stdout missing")?;
    let stderr = child.stderr.take().context("child stderr missing")?;
    forward_streams(child, stdout, stderr, tx)
}

fn forward_streams(
    mut child: Child,
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
    tx: &Sender<InstallEvent>,
) -> Result<i32> {
    let tx_out = tx.clone();
    let tx_err = tx.clone();
    let h_out = thread::spawn(move || forward_lines(stdout, &tx_out));
    let h_err = thread::spawn(move || forward_lines(stderr, &tx_err));
    let status = child.wait().context("wait on `limactl shell` child")?;
    let _ = h_out.join();
    let _ = h_err.join();
    Ok(status.code().unwrap_or(-1))
}

fn forward_lines(reader: impl Read, tx: &Sender<InstallEvent>) {
    let buf = BufReader::new(reader);
    for line in buf.lines().map_while(Result::ok) {
        if tx.send(InstallEvent::Line(line)).is_err() {
            return;
        }
    }
}
