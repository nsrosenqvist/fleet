//! Crossterm lifecycle, event loop, and subprocess-suspend helper.
//!
//! The TUI's terminal mode is owned here:
//! - `run` does setup → loop → teardown. Errors from the loop body
//!   propagate, but the terminal is always restored first so a `?`
//!   short-circuit can't strand the user in raw alt-screen mode.
//! - `event_loop` is the per-tick draw + poll + dispatch hot path.
//! - `run_suspended` is the alt-screen suspend/resume dance for forking
//!   off an interactive subprocess: `fleet orchestrator` (tmux takeover)
//!   and the tracker TUIs (`git-bug termui`, `gh dash`).

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::repo;
use crate::runtime::factory::build_stopper;
use crate::session::now_ms;
use crate::session::reaper::{self, RealPidProbe};
use crate::session::store::SessionStore;

use super::app::{Action, AppState, DoctorSnapshot};
use super::input;
use super::ui;

const POLL_TIMEOUT: Duration = Duration::from_millis(100);

/// Entry point for `fleet ui`. Resolves the current repo's fleet root,
/// opens the session store, sets up the terminal, runs the event loop.
/// Always tears the terminal down before returning so a panic or `?`
/// short-circuit doesn't leave the user's terminal in raw mode.
pub fn run() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = SessionStore::for_repo(&root);

    // Sweep crashed sessions *before* taking over the terminal: a
    // session whose driver died left meta.json stuck in `running`, and
    // the TUI must show its correct (crashed) state from the first
    // frame. Errors here are logged but non-fatal — the TUI still opens.
    let stopper = build_stopper(&root);
    let reap_report = match reaper::reap(&store, &RealPidProbe, stopper.as_ref(), now_ms()) {
        Ok(r) => Some(r),
        Err(err) => {
            tracing::warn!(error = %err, "reaper sweep failed at TUI startup");
            None
        }
    };

    let mut stdout = io::stdout();
    enable_raw_mode().context("enabling terminal raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("constructing ratatui terminal")?;

    // Run the loop, but always tear down terminal state before
    // propagating its result — otherwise an early return leaves the
    // user staring at a useless raw-mode terminal.
    let loop_result = event_loop(&mut terminal, &root, &store, reap_report);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    loop_result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    root: &Path,
    store: &SessionStore,
    reap_report: Option<reaper::ReapReport>,
) -> Result<i32> {
    let mut state = AppState::new(root.to_path_buf(), store)?;
    // Eagerly probe the runtime adapter so the breadcrumb's
    // `runtime:●` badge is meaningful from the very first frame. The
    // doctor view re-probes on entry, so this initial snapshot can
    // legitimately go stale across long sessions — that's fine, the
    // badge is a hint, not the source of truth. Tests construct
    // `AppState` directly and skip this path, so the probe doesn't
    // burden the suite.
    state.doctor = Some(DoctorSnapshot::probe(root.to_path_buf()));
    // Surface the reap outcome in the status bar so the user sees what
    // happened without scrolling through logs. Silent when nothing was
    // reaped — the default "N sessions" line is more useful.
    if let Some(report) = reap_report {
        if !report.reaped.is_empty() {
            state.status.flash(format!(
                " reaped {} crashed session(s) on startup ",
                report.reaped.len()
            ));
        }
    }
    // Background thread polls the orchestrator pane and the panel-size
    // atomic. Started after the initial reload + reaper sweep so the
    // first capture has a real session to look at; joined on Quit /
    // ?-propagation through `loop_result` (see end of function).
    state.spawn_refresh_thread();
    let loop_result = drive(terminal, &mut state, store);
    state.shutdown_refresh_thread();
    loop_result
}

fn drive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    store: &SessionStore,
) -> Result<i32> {
    loop {
        // Pull anything the refresh thread emitted since the last
        // draw (orchestrator pane snapshots), so the next render
        // shows the freshest output.
        state.drain_refresh_updates();
        terminal.draw(|f| ui::render(f, state))?;
        if event::poll(POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                match input::handle_key(state, key, store) {
                    Action::Quit => return Ok(0),
                    Action::None => {}
                    Action::OpenOrchestrator => {
                        // Single-session model: `fleet orchestrator`
                        // is reuse-or-spawn — it will mint the
                        // orchestrator on first invocation, respawn
                        // the agent if the pane is dead, or just
                        // attach if everything's alive.
                        // Use the same defensive path resolution the spawn
                        // dispatchers do — `current_exe()` returns
                        // `<path> (deleted)` after a `cargo build` while
                        // the TUI is running, and Linux can't exec from
                        // that. `resolve_fleet_binary` strips the suffix
                        // and verifies the new path exists.
                        match crate::tui::app::resolve_fleet_binary() {
                            Ok(exe) => {
                                if let Err(err) = run_suspended(terminal, &exe, &["orchestrator"]) {
                                    state.status.flash(format!(" orchestrator failed: {err:#} "));
                                }
                            }
                            Err(err) => {
                                state
                                    .status
                                    .flash(format!(" orchestrator failed: {err:#} "));
                            }
                        }
                        // Detach left the tmux session at `window-size
                        // latest` (host terminal width). Ask the
                        // refresh thread to re-pin to the output
                        // sub-pane's dims on its next tick so the
                        // preview doesn't show wrap-broken lines.
                        state.request_focused_repin();
                        // Meta may have flipped to Active/Detached/
                        // Closed on detach; reload so the sidebar
                        // marker is current.
                        if let Err(err) = state.reload(store) {
                            state.status.flash(format!(" reload failed: {err:#} "));
                        }
                    }
                    Action::OpenTrackerTui { exe, args } => {
                        // Suspend into the tracker's terminal UI
                        // (`git-bug termui`, `gh dash`). The binary
                        // lives on PATH on the host — no Lima shell
                        // dance any more — so we can launch it
                        // directly.
                        if let Err(err) = run_suspended(terminal, std::path::Path::new(exe), args) {
                            state.status.flash(format!(" {exe} failed: {err:#} "));
                        }
                    }
                    Action::AttachWorker { tmux_name } => {
                        // Suspend → run the same styled-status-bar
                        // attach script the orchestrator uses → on
                        // detach, repin the focused window. Same
                        // shape as `OpenOrchestrator`; only the tmux
                        // target name differs.
                        if let Err(err) = run_attach_script(terminal, &tmux_name) {
                            state.status.flash(format!(" attach failed: {err:#} "));
                        }
                        state.request_focused_repin();
                        if let Err(err) = state.reload(store) {
                            state.status.flash(format!(" reload failed: {err:#} "));
                        }
                    }
                }
            }
        }
        // After event handling (or after the poll timed out), let the
        // autonomous supervisor decide whether to fire anything. The
        // engine debounces internally so this is cheap to call every
        // iteration.
        let tick_now = std::time::Instant::now();
        state.autonomous_tick(store, tick_now);
        state.scheduler_tick(store, tick_now);
        // Reap any workflow-run subprocess we kicked off — surfaces
        // non-zero exits as an error overlay before the next draw.
        state.poll_pending_spawn();
        // Promote the spawn-picker's tracker fetch from `Loading` to
        // `Loaded` / `Error` as soon as the background thread sends.
        // No-op when the picker is closed or the channel already
        // drained — cheap to call every iteration.
        state.drain_spawn_fetch();
    }
}

/// Suspend the alternate-screen + raw-mode dance, run `exe` with
/// `args` inheriting stdio, then re-enter the alternate screen. The
/// terminal is left in a usable raw-mode alternate-screen state
/// whether or not the subprocess succeeds.
///
/// Used both for `fleet orchestrator` (caller passes
/// [`std::env::current_exe`] so cargo-run and release installs both
/// work) and for the tracker TUIs (`git-bug termui`, `gh dash`,
/// resolved from PATH).
/// Suspend the TUI and run the orchestrator's bash attach script
/// against `tmux_name`. The script sets the styled status bar with
/// the detach hint, then `exec tmux attach` — same dance as `fleet
/// orchestrator` uses, just parametrised over the tmux session name
/// so workers can reuse it.
fn run_attach_script(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tmux_name: &str,
) -> Result<()> {
    let script = crate::cli::orchestrator::build_attach_script(tmux_name);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    let result = std::process::Command::new("bash")
        .args(["-c", &script])
        .status()
        .with_context(|| format!("running attach script for `{tmux_name}`"));

    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = enable_raw_mode();
    let _ = terminal.clear();

    let status = result?;
    if !status.success() {
        bail!("tmux attach -t {tmux_name} exited with {status}");
    }
    Ok(())
}

fn run_suspended(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    exe: &Path,
    args: &[&str],
) -> Result<()> {
    // Tear down — order mirrors the setup in `run` (reverse).
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    let exe_display = exe.display().to_string();
    let args_joined = args.join(" ");
    let result = std::process::Command::new(exe)
        .args(args)
        .status()
        .with_context(|| format!("running {exe_display} {args_joined}"));

    // Re-enter even if the subprocess errored — otherwise the user
    // is dropped into a half-broken terminal.
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = enable_raw_mode();
    let _ = terminal.clear();

    let status = result?;
    if !status.success() {
        bail!("{exe_display} {args_joined} exited with {status}");
    }
    Ok(())
}
