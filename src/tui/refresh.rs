//! Background refresh thread for the orchestrator's pane preview.
//!
//! Owns a thread that lives off the UI event loop so a tmux roundtrip
//! (~5–30 ms locally — much faster than the old Lima-shell era, but
//! still enough to drop frames if it lived on the draw path) doesn't
//! stall rendering. Talks to the UI through two mpsc channels: a
//! command channel for `Repin` / `Shutdown` from the main loop, and an
//! update channel carrying [`RefreshUpdate::OrchestratorPane`] snapshots
//! back. The UI drains updates between draws via
//! [`crate::tui::app::AppState::drain_refresh_updates`].
//!
//! `pane_size` carries the output sub-pane's current inner dimensions
//! (packed `(w << 16) | h`, zero = no render yet) so the thread can pin
//! the orchestrator's tmux window to those dims. Without that pin the
//! session stays at whatever size tmux defaulted to when `fleet
//! orchestrator` spawned it, and `capture-pane` returns lines wider
//! than the panel — Claude Code's TUI then re-renders broken.
//!
//! `Repin` is sent by the UI after the user detaches from `tmux
//! attach` so the next tick re-pins the window (the attach script
//! flipped `window-size latest` to let the user see the session at
//! their host terminal's full width).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::orchestrator::TMUX_SESSION_NAME;

/// How often the refresh thread polls `tmux capture-pane`. 1.5s mirrors
/// the old Lima-era cadence — snappy enough that scrolling output stays
/// readable in the preview, slow enough that ~40 tmux RPC/min is
/// negligible on a local socket.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// How far back into the orchestrator pane's scrollback `capture-pane`
/// reads. 200 lines easily fills the output sub-pane on any realistic
/// terminal; the UI tails further at render time.
const CAPTURE_SCROLLBACK_LINES: u32 = 200;

/// Messages sent from the UI to the refresh thread.
pub enum RefreshCommand {
    /// Tear down. Sent on TUI quit; the thread joins inside
    /// [`AppState::shutdown_refresh_thread`].
    Shutdown,
    /// User just detached from the orchestrator. The attach script
    /// flipped `window-size latest`, so re-pin the window dims on the
    /// next tick before the next capture.
    Repin,
}

/// Messages sent from the refresh thread back to the UI.
pub enum RefreshUpdate {
    /// Latest capture of the orchestrator pane. `None` means the tmux
    /// session doesn't exist (closed, never spawned) — the UI renders
    /// a muted placeholder.
    OrchestratorPane(Option<String>),
}

/// Spawn the refresh thread. Returns the command sender, update
/// receiver, and join handle. The caller stores all three on
/// [`AppState`] for the lifetime of the TUI.
pub fn spawn(
    pane_size: Arc<AtomicU32>,
) -> (
    mpsc::Sender<RefreshCommand>,
    mpsc::Receiver<RefreshUpdate>,
    JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    let handle = thread::spawn(move || refresh_loop(&pane_size, &cmd_rx, &update_tx));
    (cmd_tx, update_rx, handle)
}

fn refresh_loop(
    pane_size: &Arc<AtomicU32>,
    cmd_rx: &mpsc::Receiver<RefreshCommand>,
    update_tx: &mpsc::Sender<RefreshUpdate>,
) {
    // The size the orchestrator window is currently pinned to. Diverges
    // from `pane_size` when the host terminal resizes (changing the
    // sub-pane's inner dims) or after an attach (which flipped window-
    // size to `latest`); both cases trigger a re-pin on the next tick.
    let mut last_pinned_size: u32 = 0;
    let mut needs_repin = false;

    loop {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(RefreshCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(RefreshCommand::Repin) => {
                needs_repin = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let packed = pane_size.load(Ordering::Relaxed);
        if packed == 0 {
            // UI hasn't rendered the output sub-pane yet (happens for a
            // tick or two at startup); capturing at the tmux default
            // 80x24 would produce a meaningless preview. Skip until
            // the panel reports a real size.
            continue;
        }

        if packed != last_pinned_size || needs_repin {
            let width = u16::try_from(packed >> 16).unwrap_or(0);
            let height = u16::try_from(packed & 0xFFFF).unwrap_or(0);
            if width > 0 && height > 0 {
                let _ = pin_window_size(TMUX_SESSION_NAME, width, height);
            }
            last_pinned_size = packed;
            needs_repin = false;
        }

        let captured = capture_pane(TMUX_SESSION_NAME).ok();
        if update_tx
            .send(RefreshUpdate::OrchestratorPane(captured))
            .is_err()
        {
            // UI dropped the receiver — TUI is shutting down before
            // our Shutdown command arrived. Exit cleanly.
            return;
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

/// Pin the orchestrator's tmux window to `(width, height)`. Two
/// `set-option`+`resize-window` calls because tmux needs `window-size
/// manual` first or it will refuse to honour `resize-window` while no
/// client is attached. Both are best-effort: a missing session (rare,
/// the UI checks `orchestrators` non-empty before scheduling) silently
/// no-ops rather than poisoning the next capture's error path.
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
