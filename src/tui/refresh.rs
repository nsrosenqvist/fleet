//! Background refresh thread for tmux-pane previews.
//!
//! Owns a thread that lives off the UI event loop so a tmux roundtrip
//! (~5–30 ms locally — much faster than the old Lima-shell era, but
//! still enough to drop frames if it lived on the draw path) doesn't
//! stall rendering. Talks to the UI through two mpsc channels: a
//! command channel for `Repin` / `Shutdown` from the main loop, and an
//! update channel carrying [`RefreshUpdate`] variants back. The UI
//! drains updates between draws via
//! [`crate::tui::app::AppState::drain_refresh_updates`].
//!
//! Two pane sources today:
//! - The orchestrator's fixed `fl-orchestrator` session (always
//!   captured if it exists).
//! - Each currently-tracked worker session — `fleet-<session-id>` —
//!   published from the UI through `worker_targets` so the thread
//!   knows which to poll. Workers that aren't tmux-backed (foreground
//!   `fleet workflow run`) simply never appear in that list.
//!
//! `pane_size` carries the output sub-pane's current inner dimensions
//! (packed `(w << 16) | h`, zero = no render yet) so the thread can pin
//! the *currently focused* tmux window to those dims. The focused
//! target is also published from the UI so we don't waste resizes on
//! sessions the user isn't looking at.
//!
//! `Repin` is sent by the UI after the user detaches from `tmux
//! attach` so the next tick re-pins the focused window (the attach
//! script flipped `window-size latest` to let the user see the
//! session at their host terminal's full width).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::orchestrator::TMUX_SESSION_NAME;
use crate::session::{SessionId, worker_tmux_name};

/// How often the refresh thread polls `tmux capture-pane`. 1.5s mirrors
/// the old Lima-era cadence — snappy enough that scrolling output stays
/// readable in the preview, slow enough that ~40 tmux RPC/min is
/// negligible on a local socket.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// How far back into a tmux pane's scrollback `capture-pane` reads.
/// 200 lines easily fills the output sub-pane on any realistic
/// terminal; the UI tails further at render time.
const CAPTURE_SCROLLBACK_LINES: u32 = 200;

/// What the user is currently looking at on the right pane. Drives
/// which tmux window the thread pins to the panel dims (resizing a
/// pane the user can't see wastes RPC + may surprise an attached
/// session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusedTarget {
    /// Right pane is showing the orchestrator preview.
    Orchestrator,
    /// Right pane is showing a worker preview.
    Worker(SessionId),
    /// No tmux-backed preview in focus (worker without a tmux pane,
    /// orchestrator absent, empty selection). Thread captures but
    /// doesn't resize anything.
    None,
}

/// Shared handle the UI uses to publish refresh-thread inputs.
/// Cloneable so both `AppState` (writer) and the thread (reader) hold
/// references without juggling raw `Arc<…>` triplets at every call
/// site.
#[derive(Clone)]
pub struct RefreshInputs {
    pub pane_size: Arc<AtomicU32>,
    /// Session ids of workers whose tmux pane should be polled. The
    /// UI rebuilds this on each `reload` (only sessions in `Running`
    /// state with a live tmux pane). `Mutex` (not `RwLock`) because
    /// writes are infrequent and the read path takes the lock
    /// briefly to clone.
    pub worker_targets: Arc<Mutex<Vec<SessionId>>>,
    /// What the user is currently focused on. Drives the per-tick
    /// resize decision. `RwLock` so the UI's brief write doesn't
    /// contend with the thread's read.
    pub focused: Arc<RwLock<FocusedTarget>>,
}

impl Default for RefreshInputs {
    fn default() -> Self {
        Self {
            pane_size: Arc::new(AtomicU32::new(0)),
            worker_targets: Arc::new(Mutex::new(Vec::new())),
            focused: Arc::new(RwLock::new(FocusedTarget::None)),
        }
    }
}

/// Messages sent from the UI to the refresh thread.
pub enum RefreshCommand {
    /// Tear down. Sent on TUI quit; the thread joins inside
    /// [`crate::tui::app::AppState::shutdown_refresh_thread`].
    Shutdown,
    /// User just detached from a tmux session (orchestrator or
    /// worker). The attach script flipped `window-size latest`, so
    /// re-pin the focused window's dims on the next tick.
    Repin,
}

/// Messages sent from the refresh thread back to the UI.
pub enum RefreshUpdate {
    /// Latest capture of the orchestrator pane. `None` means the tmux
    /// session doesn't exist (closed, never spawned) — the UI renders
    /// a muted placeholder.
    OrchestratorPane(Option<String>),
    /// Latest capture of a worker pane keyed by its session id.
    /// `None` body means the tmux session has gone away (workflow
    /// completed, killed, or was foreground-only) — the UI evicts
    /// the cached snapshot and falls back to the log tail.
    WorkerPane {
        session_id: SessionId,
        output: Option<String>,
    },
}

/// Spawn the refresh thread. Returns the command sender, update
/// receiver, and join handle. The caller stores all three on
/// [`crate::tui::app::AppState`] for the lifetime of the TUI.
pub fn spawn(
    inputs: RefreshInputs,
) -> (
    mpsc::Sender<RefreshCommand>,
    mpsc::Receiver<RefreshUpdate>,
    JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    let handle = thread::spawn(move || refresh_loop(&inputs, &cmd_rx, &update_tx));
    (cmd_tx, update_rx, handle)
}

fn refresh_loop(
    inputs: &RefreshInputs,
    cmd_rx: &mpsc::Receiver<RefreshCommand>,
    update_tx: &mpsc::Sender<RefreshUpdate>,
) {
    // The (target, size) pair the focused window was last pinned to.
    // Diverges from `(focused, pane_size)` when the host terminal
    // resizes, when the focus changes, or after an attach (which
    // flipped window-size to `latest`); each case triggers a re-pin
    // on the next tick.
    let mut last_pin: Option<(FocusedTarget, u32)> = None;
    let mut needs_repin = false;

    loop {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(RefreshCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(RefreshCommand::Repin) => {
                needs_repin = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let packed = inputs.pane_size.load(Ordering::Relaxed);
        let focused = inputs
            .focused
            .read()
            .ok()
            .map_or(FocusedTarget::None, |g| g.clone());

        // Pin the focused window to panel dims when something
        // changed since last time. `packed == 0` means the UI hasn't
        // rendered the output sub-pane yet (a tick or two at
        // startup); skip — capturing at the tmux default 80x24 would
        // produce a meaningless preview.
        if packed != 0 {
            let pin_target = match &focused {
                FocusedTarget::Orchestrator => Some(TMUX_SESSION_NAME.to_string()),
                FocusedTarget::Worker(id) => Some(worker_tmux_name(id)),
                FocusedTarget::None => None,
            };
            let current_key = (focused.clone(), packed);
            if pin_target.is_some() && (needs_repin || last_pin.as_ref() != Some(&current_key)) {
                if let Some(name) = pin_target {
                    let width = u16::try_from(packed >> 16).unwrap_or(0);
                    let height = u16::try_from(packed & 0xFFFF).unwrap_or(0);
                    if width > 0 && height > 0 {
                        let _ = pin_window_size(&name, width, height);
                    }
                }
                last_pin = Some(current_key);
                needs_repin = false;
            }
        }

        // Always capture the orchestrator pane (even when a worker
        // is focused) so flipping focus surfaces a fresh snapshot
        // immediately rather than waiting an extra tick.
        let orch_snapshot = capture_pane(TMUX_SESSION_NAME).ok();
        if update_tx
            .send(RefreshUpdate::OrchestratorPane(orch_snapshot))
            .is_err()
        {
            return;
        }

        // Worker captures: snapshot the published id list under a
        // brief lock, then run captures without holding it (each
        // shell-out is ~5–30 ms; we don't want to block the UI's
        // publishes).
        let targets: Vec<SessionId> = inputs
            .worker_targets
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.clone());
        for id in targets {
            let name = worker_tmux_name(&id);
            let snapshot = capture_pane(&name).ok();
            if update_tx
                .send(RefreshUpdate::WorkerPane {
                    session_id: id,
                    output: snapshot,
                })
                .is_err()
            {
                return;
            }
        }
    }
}

/// `tmux capture-pane -p -e -t <target> -S -<lines>`. `-e` preserves
/// ANSI SGR escapes (the UI parses them via `ansi-to-tui`). `-S
/// -<lines>` rolls the start back into the scrollback so we get more
/// than just the current viewport. Returns the raw bytes verbatim —
/// no trim, since the trailing newlines matter for line counting
/// during render.
fn capture_pane(target: &str) -> std::io::Result<String> {
    let out = std::process::Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-e",
            "-t",
            target,
            "-S",
            &format!("-{CAPTURE_SCROLLBACK_LINES}"),
        ])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "tmux capture-pane exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pin a tmux window to `(width, height)`. Two `set-option` +
/// `resize-window` calls because tmux needs `window-size manual`
/// first or it will refuse to honour `resize-window` while no client
/// is attached. Both are best-effort: a missing session (rare, the
/// UI only adds live workers to `worker_targets`) silently no-ops
/// rather than poisoning the next capture's error path.
fn pin_window_size(target: &str, width: u16, height: u16) -> std::io::Result<()> {
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", target, "window-size", "manual"])
        .output()?;
    let _ = std::process::Command::new("tmux")
        .args([
            "resize-window",
            "-t",
            target,
            "-x",
            &width.to_string(),
            "-y",
            &height.to_string(),
        ])
        .output()?;
    Ok(())
}
