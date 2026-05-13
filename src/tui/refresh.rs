//! Background refresh thread.
//!
//! Lives off the UI thread so a slow limactl→ao roundtrip (100–300 ms is
//! typical, multi-second when the VM is paging) never blocks rendering or
//! input. Talks to the UI through two mpsc channels: a command channel for
//! `ForceRefresh` / `Shutdown` from the main loop, and an update channel for
//! state snapshots back. The UI drains updates between draws via
//! [`crate::tui::app::App::drain_updates`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::ao::{Ao, SessionInfo};
use crate::lima::{Lima, VmStatus};
use crate::process::ProcessInvoker;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
const VM_NAME: &str = "fleet-vm";
const AO_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const VM_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Messages sent from the background thread back to the UI.
pub enum RefreshUpdate {
    Sessions(Vec<SessionInfo>),
    PaneCapture { session_id: String, output: String },
    Error(String),
    AoUp(bool),
    VmUp(VmStatus),
}

/// Messages sent from the UI to the refresh thread.
pub enum RefreshCommand {
    ForceRefresh,
    Shutdown,
}

/// Spawn the refresh thread. Returns the two endpoints + join handle the
/// caller stores on `App` for the lifetime of the session.
pub fn spawn(
    repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,
) -> (
    mpsc::Sender<RefreshCommand>,
    mpsc::Receiver<RefreshUpdate>,
    JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    let handle = thread::spawn(move || refresh_loop(repo_root, invoker, cmd_rx, &update_tx));
    (cmd_tx, update_rx, handle)
}

// Args are passed by value because the thread takes ownership at spawn;
// clippy::needless_pass_by_value doesn't account for that pattern.
#[allow(clippy::needless_pass_by_value)]
fn refresh_loop(
    _repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,
    cmds: mpsc::Receiver<RefreshCommand>,
    updates: &mpsc::Sender<RefreshUpdate>,
) {
    // AO discovers its config from cwd only, so every `ao …` and
    // `tmux …` we run inside the VM has to land in the directory
    // holding the canonical XDG yaml. Resolving once at the top of
    // the loop keeps the contract identical across iterations and
    // surfaces a misconfigured environment immediately rather than
    // every refresh tick.
    let Some(ao_workdir) = crate::ao::config::AoConfig::workdir() else {
        let _ = updates.send(RefreshUpdate::Error(
            "no $HOME / $XDG_CONFIG_HOME — can't locate AO config dir".to_string(),
        ));
        return;
    };
    let lima = Lima::new(invoker.clone(), VM_NAME);
    let mut next_status = Instant::now();
    let mut next_ao_probe = Instant::now();
    let mut next_vm_probe = Instant::now();

    loop {
        let now = Instant::now();
        let wait_for = [next_status, next_ao_probe, next_vm_probe]
            .into_iter()
            .map(|t| t.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_millis(100));

        match cmds.recv_timeout(wait_for) {
            Ok(RefreshCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(RefreshCommand::ForceRefresh) => {
                next_status = Instant::now();
                next_ao_probe = Instant::now();
                next_vm_probe = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        if now >= next_status {
            let ao = Ao::new(&lima, &ao_workdir);
            // `session ls --all --json` includes both worker and
            // orchestrator entries so the sidebar can group them
            // separately. `ao status --json` skips orchestrators
            // (they show only in the human-readable banner).
            match ao.session_ls_all() {
                Ok(r) => {
                    // Send Sessions first so the UI sees the structure
                    // immediately, then capture each pane sequentially and
                    // send incremental PaneCapture updates as they finish.
                    let session_ids: Vec<String> =
                        r.data.iter().filter_map(|s| s.id.clone()).collect();
                    if updates.send(RefreshUpdate::Sessions(r.data)).is_err() {
                        return;
                    }
                    for id in session_ids {
                        // 200 lines of scrollback is enough to fill any
                        // realistic output panel. tmux capture inside the VM
                        // is fast (~30 ms); we tail at render time.
                        if let Ok(out) = crate::tmux::capture_pane(&lima, &ao_workdir, &id, 200)
                            && updates
                                .send(RefreshUpdate::PaneCapture {
                                    session_id: id,
                                    output: out,
                                })
                                .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if updates
                        .send(RefreshUpdate::Error(
                            format!("{e:#}").lines().next().unwrap_or("").to_string(),
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            next_status = now + REFRESH_INTERVAL;
        }

        if now >= next_ao_probe {
            // Cheap TCP probe — the AO dashboard listens on localhost:3000
            // when `ao start` is up. 200ms timeout so a hung probe doesn't
            // stall the refresh loop.
            let up = std::net::TcpStream::connect_timeout(
                &"127.0.0.1:3000".parse().unwrap(),
                Duration::from_millis(200),
            )
            .is_ok();
            if updates.send(RefreshUpdate::AoUp(up)).is_err() {
                return;
            }
            next_ao_probe = now + AO_PROBE_INTERVAL;
        }

        if now >= next_vm_probe {
            let status = lima.status();
            if updates.send(RefreshUpdate::VmUp(status)).is_err() {
                return;
            }
            next_vm_probe = now + VM_PROBE_INTERVAL;
        }
    }
}
