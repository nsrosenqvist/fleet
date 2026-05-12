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
use super::preflight::{self, MissingDep};
use super::subprocess::suspend_around;
use super::ui;

const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Enter the TUI and drive the event loop. Returns the process exit code
/// the caller should propagate.
///
/// Runs a host-dependency preflight first. If anything is missing the user
/// gets a blocking modal explaining what to install; any key quits with
/// exit code 1. We enter the alt screen *before* the probe so the modal
/// renders consistently with the rest of the UI (same rounded borders,
/// same theme).
pub(super) fn run(app: &mut App) -> Result<i32> {
    let failures = preflight::check();
    let mut term = enter_terminal()?;
    if !failures.is_empty() {
        let result = run_preflight_modal(&mut term, &failures);
        leave_terminal()?;
        return result;
    }
    app.spawn_refresh_thread();
    let result = drive(app, &mut term);
    app.shutdown_refresh_thread();
    leave_terminal()?;
    result
}

/// Block on the preflight modal until the user dismisses it. Exit code 1
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
