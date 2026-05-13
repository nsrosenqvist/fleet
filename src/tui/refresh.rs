//! Background refresh thread.
//!
//! Lives off the UI thread so a slow limactl→ao roundtrip (100–300 ms is
//! typical, multi-second when the VM is paging) never blocks rendering or
//! input. Talks to the UI through two mpsc channels: a command channel for
//! `ForceRefresh` / `Shutdown` from the main loop, and an update channel for
//! state snapshots back. The UI drains updates between draws via
//! [`crate::tui::app::App::drain_updates`].

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use std::collections::HashMap;

use crate::ao::{Ao, EventInfo, SessionInfo, SessionMeta};
use crate::lima::{Lima, VmStatus};
use crate::process::ProcessInvoker;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
const VM_NAME: &str = "fleet-vm";
const AO_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const VM_PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Event log polling cadence. Slower than per-session refresh —
/// events arrive in bursts (spawn, transition, CI signal) and the
/// bottom-pane ticker doesn't need sub-second freshness.
const EVENTS_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Per-session lifecycle/agent-state probe cadence. Reads from JSON
/// files on the guest's disk (one bulk roundtrip per tick), so the
/// floor cost is comparable to the events probe. Drives the sidebar
/// `✓ done` / `crashed` badges, which don't need sub-second freshness
/// — the underlying state only changes when an agent reports or a
/// runtime dies.
const META_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// How often the background crash-session sweep runs. AO doesn't reap
/// session records on its own, so without periodic cleanup the
/// per-project `sessions/` and `worktrees/` dirs grow unboundedly.
/// Half-hour cadence keeps the disk drag bounded without firing
/// often enough to surprise an attached user.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Minimum time-since-termination before a crashed session is
/// eligible for sweep. Gives the user a window to attach to a
/// crashed runtime and salvage state before fleet captures it and
/// hands it to `ao session kill`. 10 minutes is generous enough
/// that an "I'm AFK" pause doesn't lose an investigation.
const CLEANUP_MIN_AGE_SECS: u64 = 10 * 60;
/// Time window passed to `ao events list --since`. An hour catches
/// "what just happened" without the panel scrolling back to last
/// week's noise.
const EVENTS_SINCE_WINDOW: &str = "1h";
/// `ao events list -n` limit. 100 rows easily fills the visible
/// 6-line strip, and keeps the lima→ao roundtrip bounded if AO
/// has been chatty.
const EVENTS_LIMIT: u32 = 100;

/// Messages sent from the background thread back to the UI.
pub enum RefreshUpdate {
    Sessions(Vec<SessionInfo>),
    PaneCapture {
        session_id: String,
        output: String,
    },
    Error(String),
    AoUp(bool),
    VmUp(VmStatus),
    Events(Vec<EventInfo>),
    /// Bulk per-session lifecycle/agent state, keyed by session id.
    /// Sourced from the VM's `~/.agent-orchestrator/projects/<p>/sessions/<id>.json`
    /// files (one bulk read per tick), drives the sidebar badges
    /// (`✓ done` / `crashed`).
    SessionMeta(HashMap<String, SessionMeta>),
}

/// Messages sent from the UI to the refresh thread.
pub enum RefreshCommand {
    ForceRefresh,
    Shutdown,
}

/// Spawn the refresh thread. Returns the two endpoints + join handle the
/// caller stores on `App` for the lifetime of the session.
///
/// `pane_size` carries the output pane's current inner dimensions (packed
/// `(w << 16) | h`, zero = no render yet) so the refresh thread can pin
/// each newly-observed session's tmux window to the panel width on its
/// first capture. Crucial for the orchestrator session, which AO spawns
/// before fleet runs and which would otherwise stay at AO's default
/// width forever.
pub fn spawn(
    repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,
    pane_size: Arc<AtomicU32>,
) -> (
    mpsc::Sender<RefreshCommand>,
    mpsc::Receiver<RefreshUpdate>,
    JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    let handle =
        thread::spawn(move || refresh_loop(repo_root, invoker, pane_size, cmd_rx, &update_tx));
    (cmd_tx, update_rx, handle)
}

// Args are passed by value because the thread takes ownership at spawn;
// clippy::needless_pass_by_value doesn't account for that pattern.
#[allow(clippy::needless_pass_by_value)]
fn refresh_loop(
    _repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,
    pane_size: Arc<AtomicU32>,
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
    let mut next_events_probe = Instant::now();
    let mut next_meta_probe = Instant::now();
    // Sessions we've already pinned to the panel width this run. The
    // orchestrator session (and any AO worker that existed before we
    // started) gets resized on its first capture; subsequent captures
    // skip the resize so we don't burn a `limactl shell` round-trip per
    // tick per session. Stale ids are evicted when their session
    // disappears from AO's listing. After an attach the size will drift
    // up, but the attach handler issues its own resize on detach.
    let mut resized: HashSet<String> = HashSet::new();
    // Packed `pane_size` value the entries in `resized` are pinned to.
    // When the UI's `pane_size` atomic changes (host terminal resize,
    // pane reflow), we clear `resized` so the next status pass re-pins
    // every session to the new dims. Cheaper than checking each
    // session's own size — a single atomic load + compare per tick, no
    // tmux round-trip unless the value actually drifts.
    let mut last_resize_size: u32 = 0;
    // Cleanup fires immediately on startup (so accumulated litter
    // settles before the user sees the sidebar) and then on
    // CLEANUP_INTERVAL. ForceRefresh deliberately doesn't bump it
    // — destructive ops shouldn't ride user-driven refreshes.
    let mut next_cleanup = Instant::now();

    loop {
        let now = Instant::now();
        let wait_for = [
            next_status,
            next_ao_probe,
            next_vm_probe,
            next_events_probe,
            next_meta_probe,
            next_cleanup,
        ]
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
                next_events_probe = Instant::now();
                next_meta_probe = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        if now >= next_status {
            // Snapshot pane_size once per tick; if it's drifted since
            // we last pinned, invalidate the per-session cache so the
            // probe re-resizes every session to the new dims.
            let packed = pane_size.load(Ordering::Relaxed);
            if packed != last_resize_size {
                resized.clear();
                last_resize_size = packed;
            }
            if run_status_probe(&lima, &ao_workdir, packed, &mut resized, updates).is_err() {
                return;
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

        if now >= next_events_probe {
            // Event log: spawns, kills, lifecycle transitions, CI
            // failures, review activity. Errors here are quiet —
            // events are nice-to-have for the bottom-pane ticker;
            // a failed probe shouldn't drown the status bar in
            // refresh-error flashes alongside the session probe.
            let ao = Ao::new(&lima, &ao_workdir);
            if let Ok(events) = ao.events_list(EVENTS_SINCE_WINDOW, EVENTS_LIMIT)
                && updates.send(RefreshUpdate::Events(events)).is_err()
            {
                return;
            }
            next_events_probe = now + EVENTS_PROBE_INTERVAL;
        }

        if now >= next_meta_probe {
            if run_meta_probe(&lima, &ao_workdir, updates).is_err() {
                return;
            }
            next_meta_probe = now + META_PROBE_INTERVAL;
        }

        if now >= next_cleanup {
            if run_cleanup_tick(&ao_workdir, updates).is_err() {
                return;
            }
            next_cleanup = Instant::now() + CLEANUP_INTERVAL;
        }
    }
}

/// `ao session ls --all --json` plus a per-session tmux pane capture
/// for each result. Sends `Sessions` first so the sidebar's structure
/// snaps in immediately, then streams `PaneCapture` updates as each
/// capture lands. Failures land in the refresh-error channel as a
/// single-line message. `--all` includes orchestrator entries that
/// plain `ao status --json` would skip.
///
/// Returns `Err(())` only when the updates channel has been dropped.
fn run_status_probe(
    lima: &Lima,
    ao_workdir: &std::path::Path,
    pane_size: u32,
    resized: &mut HashSet<String>,
    updates: &mpsc::Sender<RefreshUpdate>,
) -> std::result::Result<(), ()> {
    let ao = Ao::new(lima, ao_workdir);
    match ao.session_ls_all() {
        Ok(r) => {
            let session_ids: Vec<String> = r.data.iter().filter_map(|s| s.id.clone()).collect();
            // Drop stale resize markers so a recycled session id gets
            // pinned again on its next first observation.
            resized.retain(|id| session_ids.iter().any(|s| s == id));
            if updates.send(RefreshUpdate::Sessions(r.data)).is_err() {
                return Err(());
            }
            let packed = pane_size;
            for id in session_ids {
                // First time we see this session id, pin its tmux window
                // to the output pane's width. AO created the session at
                // whatever default it picked; without this the
                // orchestrator pane would never fit. Skip when pane_size
                // is zero (UI hasn't rendered yet — happens for a tick
                // or two at startup; we'll catch up on the next pass).
                if packed != 0 && !resized.contains(&id) {
                    let w = u16::try_from(packed >> 16).unwrap_or(0);
                    let h = u16::try_from(packed & 0xFFFF).unwrap_or(0);
                    let _ = crate::tmux::resize_window(lima, ao_workdir, &id, w, h);
                    resized.insert(id.clone());
                }
                // 200 lines of scrollback is enough to fill any
                // realistic output panel. tmux capture inside the VM
                // is fast (~30 ms); we tail at render time.
                if let Ok(out) = crate::tmux::capture_pane(lima, ao_workdir, &id, 200)
                    && updates
                        .send(RefreshUpdate::PaneCapture {
                            session_id: id,
                            output: out,
                        })
                        .is_err()
                {
                    return Err(());
                }
            }
        }
        Err(e) => {
            let msg = format!("{e:#}").lines().next().unwrap_or("").to_string();
            if updates.send(RefreshUpdate::Error(msg)).is_err() {
                return Err(());
            }
        }
    }
    Ok(())
}

/// One per-session meta probe — bulk read of lifecycle JSON inside
/// the VM, parsed to a map keyed by session id. Failures are quiet
/// (badges are nice-to-have; a fluky node spawn shouldn't drown the
/// status bar in errors alongside the actual session probe).
///
/// Returns `Err(())` only when the updates channel has been dropped.
fn run_meta_probe(
    lima: &Lima,
    ao_workdir: &std::path::Path,
    updates: &mpsc::Sender<RefreshUpdate>,
) -> std::result::Result<(), ()> {
    let ao = Ao::new(lima, ao_workdir);
    let Ok(map) = ao.session_meta_bulk() else {
        return Ok(());
    };
    if updates.send(RefreshUpdate::SessionMeta(map)).is_err() {
        return Err(());
    }
    Ok(())
}

/// One cleanup pass — re-reads the AO config (so projects added via
/// `c` get picked up without a fleet restart), iterates projects,
/// invokes [`super::cleanup::sweep`] for each. Per-project errors are
/// surfaced via the normal error channel rather than fatal so a
/// permission glitch on one repo doesn't kill the refresh thread.
///
/// Returns `Err(())` only when the updates channel has been dropped —
/// the caller uses that as a signal to exit the refresh loop.
fn run_cleanup_tick(
    ao_workdir: &std::path::Path,
    updates: &mpsc::Sender<RefreshUpdate>,
) -> std::result::Result<(), ()> {
    let Ok(Some((_, cfg))) = crate::ao::config::AoConfig::load() else {
        return Ok(());
    };
    for (project_key, project) in &cfg.projects {
        if let Err(e) =
            super::cleanup::sweep(&project.path, ao_workdir, project_key, CLEANUP_MIN_AGE_SECS)
        {
            let msg = format!(
                "cleanup({project_key}): {}",
                format!("{e:#}").lines().next().unwrap_or("")
            );
            if updates.send(RefreshUpdate::Error(msg)).is_err() {
                return Err(());
            }
        }
    }
    Ok(())
}
