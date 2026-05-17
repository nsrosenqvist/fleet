//! Crossterm lifecycle, event loop, and subprocess-suspend helper.
//!
//! The TUI's terminal mode is owned here:
//! - `run` does setup → loop → teardown. Errors from the loop body
//!   propagate, but the terminal is always restored first so a `?`
//!   short-circuit can't strand the user in raw alt-screen mode.
//! - `event_loop` is the per-tick draw + poll + dispatch hot path.
//! - `run_brainstorm_suspended` is the alt-screen suspend/resume dance
//!   for forking off `fleet brainstorm` (which itself takes over the
//!   terminal with tmux).

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
use crate::session::reaper::{self, RealPidProbe};
use crate::session::store::SessionStore;
use crate::session::{now_ms};
use crate::runtime::factory::build_stopper;

use super::app::{Action, AppState};
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
    // Surface the reap outcome in the status bar so the user sees what
    // happened without scrolling through logs. Silent when nothing was
    // reaped — the default "N sessions" line is more useful.
    if let Some(report) = reap_report {
        if !report.reaped.is_empty() {
            state.status_line = format!(
                " reaped {} crashed session(s) on startup ",
                report.reaped.len()
            );
        }
    }
    loop {
        terminal.draw(|f| ui::render(f, &state))?;
        if event::poll(POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                match input::handle_key(&mut state, key, store) {
                    Action::Quit => return Ok(0),
                    Action::None => {}
                    Action::NewBrainstorm => {
                        if let Err(err) = run_brainstorm_suspended(terminal, &["brainstorm"]) {
                            state.status_line = format!(" brainstorm failed: {err:#} ");
                        }
                        // After the subprocess returns, the
                        // brainstorm session list on disk has
                        // changed (a new one was created). Refresh
                        // so the sidebar reflects it.
                        if let Err(err) = state.reload(store) {
                            state.status_line = format!(" reload failed: {err:#} ");
                        }
                    }
                    Action::AttachBrainstorm(id) => {
                        let id_str = id.as_str().to_string();
                        if let Err(err) = run_brainstorm_suspended(
                            terminal,
                            &["brainstorm", "attach", id_str.as_str()],
                        ) {
                            state.status_line = format!(" brainstorm attach failed: {err:#} ");
                        }
                        // Brainstorm meta may have flipped to
                        // Detached/Closed on detach; reload so the
                        // marker is current.
                        if let Err(err) = state.reload(store) {
                            state.status_line = format!(" reload failed: {err:#} ");
                        }
                    }
                }
            }
        }
        // After event handling (or after the poll timed out), let the
        // autonomous supervisor decide whether to fire anything. The
        // engine debounces internally so this is cheap to call every
        // iteration.
        state.autonomous_tick(store, std::time::Instant::now());
        // Reap any workflow-run subprocess we kicked off — surfaces
        // non-zero exits as an error overlay before the next draw.
        state.poll_pending_spawn();
    }
}

/// Suspend the alternate-screen + raw-mode dance, run the current
/// fleet binary with `args` inheriting stdio, then re-enter the
/// alternate screen. The terminal is left in a usable raw-mode
/// alternate-screen state whether or not the subprocess succeeds.
///
/// Uses [`std::env::current_exe`] to locate the binary so the
/// behaviour works under `cargo run` as well as a release install.
fn run_brainstorm_suspended(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    args: &[&str],
) -> Result<()> {
    let exe = std::env::current_exe().context("locating current fleet binary")?;
    // Tear down — order mirrors the setup in `run` (reverse).
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    let exe_display = exe.display().to_string();
    let args_joined = args.join(" ");
    let result = std::process::Command::new(&exe)
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
