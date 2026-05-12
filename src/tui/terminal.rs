//! Crossterm lifecycle + event loop + command executor.
//!
//! Owns the alt-screen / raw-mode handshake, drains state updates from the
//! refresh thread between draws, polls crossterm events, dispatches them to
//! the per-mode handler in [`crate::tui::input`], and runs queued
//! [`Command`]s after each draw. All terminal-suspending side effects
//! (attach, edit config, …) live here so the input layer never sees the
//! terminal handle.

use anyhow::Result;
use crossterm::{event, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use super::app::{App, Command};
use super::input;
use super::preflight::{self, MissingDep, Preflight, VM_NAME};
use super::subprocess::suspend_around;
use super::ui;

const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Enter the TUI and drive the event loop. Returns the process exit code
/// the caller should propagate.
///
/// Runs preflight first. The host-bin case is a dead-end modal (any key
/// quits); the VM cases are actionable — `y` suspends to `limactl start
/// fleet-vm`, then we loop back through preflight in case the VM came up
/// stopped (rare but possible if start failed mid-cloud-init).
pub(super) fn run(app: &mut App) -> Result<i32> {
    let mut term = enter_terminal()?;
    let outcome = settle_preflight(&mut term);
    let result = match outcome {
        PreflightOutcome::Proceed => {
            app.spawn_refresh_thread();
            let res = drive(app, &mut term);
            app.shutdown_refresh_thread();
            res
        }
        PreflightOutcome::Quit(code) => Ok(code),
        PreflightOutcome::Err(e) => Err(e),
    };
    leave_terminal()?;
    result
}

enum PreflightOutcome {
    Proceed,
    Quit(i32),
    Err(anyhow::Error),
}

/// Loop preflight → modal → remediation until the user either lands in
/// an `Ok` state or quits. Remediation runs `limactl start fleet-vm`
/// suspended; any keystroke other than `y` in a VM-setup modal is a
/// quit, matching the existing kill/stop confirm convention.
fn settle_preflight(term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> PreflightOutcome {
    loop {
        match preflight::check() {
            Preflight::Ok => return PreflightOutcome::Proceed,
            Preflight::HostBinsMissing(deps) => {
                return match run_preflight_modal(term, &deps) {
                    Ok(code) => PreflightOutcome::Quit(code),
                    Err(e) => PreflightOutcome::Err(e),
                };
            }
            Preflight::VmMissing => match prompt_vm_action(term, VmAction::Create) {
                Ok(true) => match bring_up_vm(term) {
                    Ok(()) => {} // re-check
                    Err(e) => return PreflightOutcome::Err(e),
                },
                Ok(false) => return PreflightOutcome::Quit(1),
                Err(e) => return PreflightOutcome::Err(e),
            },
            Preflight::VmStopped => match prompt_vm_action(term, VmAction::Start) {
                Ok(true) => match bring_up_vm(term) {
                    Ok(()) => {} // re-check
                    Err(e) => return PreflightOutcome::Err(e),
                },
                Ok(false) => return PreflightOutcome::Quit(1),
                Err(e) => return PreflightOutcome::Err(e),
            },
        }
    }
}

#[derive(Clone, Copy)]
enum VmAction {
    Create,
    Start,
}

/// Block on the host-bins modal until the user dismisses it. Exit code 1
/// so calling shells / CI can tell a preflight-failed run apart from a
/// clean quit.
fn run_preflight_modal(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    failures: &[MissingDep],
) -> Result<i32> {
    loop {
        term.draw(|f| ui::render_preflight(f, failures))?;
        if event::poll(TICK_INTERVAL)?
            && let event::Event::Key(k) = event::read()?
            && k.kind == event::KeyEventKind::Press
        {
            return Ok(1);
        }
    }
}

/// Draw the VM-setup modal (missing or stopped variant) and wait for a
/// y/n decision. Returns `Ok(true)` for "yes, remediate" and `Ok(false)`
/// for "quit"; anything that isn't an explicit `y`/`Y` is a quit, same
/// as the in-app confirm dialogs.
fn prompt_vm_action(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: VmAction,
) -> Result<bool> {
    loop {
        term.draw(|f| match action {
            VmAction::Create => ui::render_vm_missing(f),
            VmAction::Start => ui::render_vm_stopped(f),
        })?;
        if event::poll(TICK_INTERVAL)?
            && let event::Event::Key(k) = event::read()?
            && k.kind == event::KeyEventKind::Press
        {
            return Ok(matches!(k.code, event::KeyCode::Char('y' | 'Y')));
        }
    }
}

/// Suspend the alt screen and run `limactl start fleet-vm` interactively.
/// Lima prompts for template choice on first run and streams cloud-init
/// progress over many minutes — letting it own the terminal during that
/// window is far less surprising than trying to surface a fraction of
/// the output in a TUI pane. On return we redraw and let `settle_preflight`
/// re-check VM status.
fn bring_up_vm(term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    suspend_around(term, || {
        crate::process::run_interactive(
            "limactl",
            &["start".to_string(), VM_NAME.to_string()],
            &[],
            &[],
        )
        .map(|_| ())
    })?;
    Ok(())
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        event::EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn leave_terminal() -> Result<()> {
    execute!(
        io::stdout(),
        event::DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal::disable_raw_mode()?;
    Ok(())
}

fn drive(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<i32> {
    while !app.should_quit {
        // 1. Fold any state updates from the refresh thread.
        app.drain_updates();

        // 2. Paint.
        term.draw(|f| ui::render(app, f))?;

        // 3. Run queued commands. Some suspend the alt screen — doing so
        //    between frames means the next loop iteration repaints cleanly.
        for cmd in app.drain_commands() {
            run_command(app, term, cmd);
        }
        if app.should_quit {
            break;
        }

        // 4. Wait for input (capped at TICK_INTERVAL so the refresh thread's
        //    updates still surface even when the user is idle).
        if event::poll(TICK_INTERVAL)? {
            dispatch_event(app, &event::read()?);
        }
    }
    Ok(0)
}

fn dispatch_event(app: &mut App, ev: &event::Event) {
    match ev {
        event::Event::Key(k) if k.kind == event::KeyEventKind::Press => {
            // Mode-first dispatch. A pending confirm always intercepts.
            if app.confirm.is_some() {
                input::handle_key_confirm(app, *k);
            } else {
                input::handle_key_normal(app, *k);
            }
        }
        // Mouse is ignored while a confirm is pending — keyboard-only
        // resolution avoids accidentally killing a session by clicking
        // somewhere unrelated.
        event::Event::Mouse(m) if app.confirm.is_none() => {
            input::handle_mouse_normal(app, *m);
        }
        _ => {}
    }
}

fn run_command(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>, cmd: Command) {
    match cmd {
        Command::AttachSelected => attach_selected(app, term),
        Command::EditConfig => edit_config(app, term),
        Command::TrackerPreview => tracker_preview(app, term),
        Command::StartAo => start_ao(app, term),
        Command::OpenWeb => open_web(app),
        Command::KillSession(id) => kill_session(app, term, id),
        Command::StopAo => stop_ao(app, term),
        Command::RequestRefresh => app.request_refresh(),
    }
}

// ---- command bodies ------------------------------------------------------

fn attach_selected(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let Some(target) = app.selected_session_id() else {
        return;
    };
    let repo_root: PathBuf = app.repo_root.clone();
    let res = suspend_around(term, move || {
        crate::cli::attach::run(&repo_root, &target).map(|_| ())
    });
    // Re-render on resume. State refreshes on the next tick anyway.
    if let Err(e) = res {
        app.flash_err(format!("attach failed: {e:#}"));
    }
}

fn edit_config(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let yaml_path = app.repo_root.join("agent-orchestrator.yaml");
    let res = suspend_around(term, || {
        crate::process::run_interactive(&editor, &[yaml_path.display().to_string()], &[], &[])
            .map(|_| ())
    });
    app.flash_result("edited config".to_string(), res);
}

fn tracker_preview(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    // The sandbox project uses git-bug. When more trackers exist, dispatch
    // on `project.tracker.plugin` (read from agent-orchestrator.yaml).
    let res = suspend_around(term, || {
        crate::process::run_interactive(
            "limactl",
            &[
                "shell".to_string(),
                "--workdir".to_string(),
                "/Users/niklas/Code/agent-team/tmp/git-bug-sandbox".to_string(),
                "fleet-vm".to_string(),
                "git-bug".to_string(),
                "termui".to_string(),
            ],
            &[("TERM", "xterm-256color")],
            &[],
        )
        .map(|_| ())
    });
    app.flash_result("closed tracker preview".to_string(), res);
}

fn start_ao(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        crate::cli::spawn::run_start(&repo_root, false, false).map(|_| ())
    });
    app.flash_result("started AO".to_string(), res);
    app.request_refresh();
}

fn open_web(app: &mut App) {
    let url = app.selected_session().map_or_else(
        || "http://localhost:3000".to_string(),
        |s| {
            let project = s.project_id.as_deref().unwrap_or("default");
            let id = s.id.as_deref().unwrap_or("");
            format!("http://localhost:3000/projects/{project}/sessions/{id}")
        },
    );
    if !app.ao_up {
        app.flash_err(format!(
            "AO dashboard not running — start with Shift+S, then retry [Shift+W] (url: {url})"
        ));
        return;
    }
    match webbrowser::open(&url) {
        Ok(()) => app.flash_ok(format!("opened {url}")),
        Err(e) => app.flash_err(format!("open: {e}")),
    }
}

fn kill_session(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>, id: String) {
    let repo_root = app.repo_root.clone();
    let killed_id = id.clone();
    let res = suspend_around(term, || {
        crate::cli::passthrough::run_with_prefix(
            &repo_root,
            "session",
            &["kill".to_string(), id],
        )
        .map(|_| ())
    });
    app.flash_result(format!("killed {killed_id}"), res);
    app.request_refresh();
}

fn stop_ao(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        crate::cli::passthrough::run(&repo_root, &["stop".to_string()]).map(|_| ())
    });
    app.flash_result("stopped AO".to_string(), res);
    app.request_refresh();
}
