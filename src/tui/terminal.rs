//! Crossterm lifecycle + event loop + command executor.
//!
//! Owns the alt-screen / raw-mode handshake, drains state updates from the
//! refresh thread between draws, polls crossterm events, dispatches them to
//! the per-mode handler in [`crate::tui::input`], and runs queued
//! [`Command`]s after each draw. All terminal-suspending side effects
//! (attach, edit config, …) live here so the input layer never sees the
//! terminal handle.

use anyhow::{Context, Result};
use crossterm::{event, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use super::app::{App, Command};
use super::bringup::{BringUp, BringUpMode};
use super::input;
use super::preflight::{self, MissingDep, MissingTracker, Preflight};
use super::subprocess::suspend_around;
use super::tracker_install::TrackerInstall;
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
    let outcome = settle_preflight(&mut term, &app.repo_root);
    let result = match outcome {
        PreflightOutcome::Proceed => {
            // Pick up any AO yaml edits the user made via the
            // preflight "edit config" action — App was constructed
            // before the modal loop ran, so its cached config is
            // potentially stale.
            app.reload_ao_config();
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
/// an `Ok` state or quits. Each modal offers `c` (open the AO yaml in
/// `$EDITOR`) so a misconfigured project / tracker plugin can be fixed
/// without exiting fleet first.
fn settle_preflight(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    repo_root: &std::path::Path,
) -> PreflightOutcome {
    loop {
        match preflight::check() {
            Preflight::Ok => return PreflightOutcome::Proceed,
            Preflight::HostBinsMissing(deps) => match run_host_bins_modal(term, &deps) {
                Ok(HostBinsAction::EditConfig) => {
                    if let Err(e) = edit_ao_yaml(term, repo_root) {
                        return PreflightOutcome::Err(e);
                    }
                }
                Ok(HostBinsAction::Quit) => return PreflightOutcome::Quit(1),
                Err(e) => return PreflightOutcome::Err(e),
            },
            Preflight::VmMissing => match prompt_vm_action(term, VmAction::Create) {
                Ok(true) => match bring_up_vm(term, BringUpMode::Create) {
                    Ok(()) => {}
                    Err(e) => return PreflightOutcome::Err(e),
                },
                Ok(false) => return PreflightOutcome::Quit(1),
                Err(e) => return PreflightOutcome::Err(e),
            },
            Preflight::VmStopped => match prompt_vm_action(term, VmAction::Start) {
                Ok(true) => match bring_up_vm(term, BringUpMode::StartExisting) {
                    Ok(()) => {}
                    Err(e) => return PreflightOutcome::Err(e),
                },
                Ok(false) => return PreflightOutcome::Quit(1),
                Err(e) => return PreflightOutcome::Err(e),
            },
            Preflight::WorkspaceUnsafe { current } => {
                match run_workspace_unsafe_modal(term, current.as_deref()) {
                    Ok(HostBinsAction::EditConfig) => {
                        if let Err(e) = edit_ao_yaml(term, repo_root) {
                            return PreflightOutcome::Err(e);
                        }
                    }
                    Ok(HostBinsAction::Quit) => return PreflightOutcome::Quit(1),
                    Err(e) => return PreflightOutcome::Err(e),
                }
            }
            Preflight::TrackerWarnings(warnings) => {
                // No prompt — tracker tools are fleet's responsibility
                // since fleet manages the VM. Run the install with a
                // small in-TUI progress modal (so the screen doesn't
                // look frozen for 10–30s) and re-preflight; if a
                // residual error remains the user sees the spawn
                // picker's "tracker error" message rather than a
                // startup roadblock.
                if let Err(e) = run_tracker_install(term, &warnings) {
                    return PreflightOutcome::Err(e);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum VmAction {
    Create,
    Start,
}

/// User's choice from the host-bins modal. Edit-config loops back
/// through preflight after `$EDITOR` exits (in case the user dropped
/// or replaced a misconfigured project that depended on the missing
/// host bin — unlikely for `limactl` itself, but handy when the host
/// bins list grows).
enum HostBinsAction {
    EditConfig,
    Quit,
}

fn run_host_bins_modal(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    failures: &[MissingDep],
) -> Result<HostBinsAction> {
    loop {
        term.draw(|f| ui::render_preflight(f, failures))?;
        if event::poll(TICK_INTERVAL)?
            && let event::Event::Key(k) = event::read()?
            && k.kind == event::KeyEventKind::Press
        {
            return Ok(match k.code {
                event::KeyCode::Char('c' | 'C') => HostBinsAction::EditConfig,
                _ => HostBinsAction::Quit,
            });
        }
    }
}

/// Workspace-unsafe modal. Same key model as the host-bins one
/// (`c` → edit yaml, anything else → quit), so the user has a fast
/// remediation path without leaving the alt screen.
fn run_workspace_unsafe_modal(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    current: Option<&str>,
) -> Result<HostBinsAction> {
    loop {
        term.draw(|f| ui::render_workspace_unsafe(f, current))?;
        if event::poll(TICK_INTERVAL)?
            && let event::Event::Key(k) = event::read()?
            && k.kind == event::KeyEventKind::Press
        {
            return Ok(match k.code {
                event::KeyCode::Char('c' | 'C') => HostBinsAction::EditConfig,
                _ => HostBinsAction::Quit,
            });
        }
    }
}

/// Auto-install missing tracker tools inside the VM, rendering a
/// streaming progress modal until the supervisor reports done. No
/// user input — fleet manages the VM, so the tools are fleet's
/// responsibility to provision, not the user's chore. After the
/// modal closes, control returns to `settle_preflight` which probes
/// again; any residual error advances to the main TUI as a normal
/// status-bar flash rather than a startup-blocking modal.
fn run_tracker_install(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    warnings: &[MissingTracker],
) -> Result<()> {
    let tools: Vec<String> = warnings.iter().map(|w| w.tool.to_string()).collect();
    if tools.is_empty() {
        return Ok(());
    }
    let mut install = TrackerInstall::spawn(tools);
    loop {
        install.tick();
        term.draw(|f| ui::render_tracker_install(f, &install))?;
        if install.is_finished() {
            // One last drain so any final lines that arrived in the
            // same tick window land in the displayed tail.
            install.tick();
            term.draw(|f| ui::render_tracker_install(f, &install))?;
            return Ok(());
        }
        // Tracker installs don't accept user input (no [y]/[n] gate),
        // but we still drain crossterm events so the queue doesn't
        // accumulate while we wait.
        if event::poll(TICK_INTERVAL)? {
            let _ = event::read();
        }
    }
}

/// Suspend the alt screen, open the AO yaml in `$EDITOR`, return so
/// the preflight loop re-runs against the (possibly edited) file.
/// Always targets the canonical XDG yaml — fleet is distributed as a
/// binary users run inside their own repos, and dropping artefacts
/// into someone else's working tree is hostile. Same shape as the
/// in-TUI `c` keybind.
fn edit_ao_yaml(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    _repo_root: &std::path::Path,
) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let yaml_path = crate::ao::config::AoConfig::default_xdg_path()
        .context("no $HOME / $XDG_CONFIG_HOME — can't resolve AO config path")?;
    if let Some(parent) = yaml_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    suspend_around(term, || {
        crate::process::run_interactive(&editor, &[yaml_path.display().to_string()], &[], &[])
            .map(|_| ())
    })?;
    Ok(())
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

/// Run `limactl start --tty=false …` as a piped child and render a live
/// progress modal until it exits.
///
/// On `BringUpMode::Create`, fleet's bundled `templates/fleet-vm.yaml`
/// is written to a tempfile and passed in so Lima provisions a VM with
/// Node 20, git, tmux, gh, claude-code, and `@aoagents/ao`. The probe
/// in that template blocks Lima from reporting "Ready" until `ao` is
/// actually on PATH, so the first `ao status` call after bring-up
/// doesn't race against the npm install.
///
/// On `BringUpMode::StartExisting`, fleet just runs `limactl start
/// fleet-vm` against the existing instance config.
///
/// Either way: piping stdio keeps the user inside the TUI for the whole
/// boot window. On exit we drop the modal and let [`settle_preflight`]
/// re-check VM status — if limactl returned non-zero or the VM still
/// isn't running, the next iteration shows the relevant modal again
/// rather than blindly proceeding into a broken TUI.
fn bring_up_vm(term: &mut Terminal<CrosstermBackend<io::Stdout>>, mode: BringUpMode) -> Result<()> {
    let mut bringup = BringUp::spawn(mode)?;
    loop {
        bringup.tick();
        term.draw(|f| ui::render_vm_bringup(f, &bringup))?;
        if bringup.is_finished() {
            // One last drain after the Exited event to flush any trailing
            // log lines that arrived in the same tick window.
            bringup.tick();
            term.draw(|f| ui::render_vm_bringup(f, &bringup))?;
            // Surface a hard failure (spawn / wait error) immediately so
            // the caller doesn't loop preflight forever against a broken
            // limactl. Non-zero exit codes flow through the normal path —
            // preflight re-runs and either advances (VM came up despite
            // the code) or shows the relevant modal again.
            if let Some(Err(e)) = bringup.outcome() {
                return Err(anyhow::anyhow!("vm bring-up: {e}"));
            }
            return Ok(());
        }
        // Drain stray crossterm events while the child runs so a stuck
        // queue doesn't bloat. We don't act on them — the modal has no
        // keymap by design (cancelling a half-baked Lima create is worse
        // than waiting it out).
        if event::poll(TICK_INTERVAL)? {
            let _ = event::read();
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
        app.drain_spawn_fetch();

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
            // Any keystroke counts as "user is driving the UI again,"
            // so a stale flash dismisses immediately. Done *before*
            // the handler runs so a key that itself sets a fresh
            // flash (e.g. `c` → "edited /path/...") overrides cleanly
            // rather than getting wiped a tick later.
            //
            // Mouse events do *not* dismiss — a stray hover or scroll
            // would wipe an error before the user could read it (or
            // select-to-copy from it), which is exactly the bug
            // someone hit when an error vanished as they reached for
            // the trackpad to drag-select the message.
            app.dismiss_flash();
            // Mode-first dispatch. Order of precedence: spawn modal
            // (text input intercepts everything) → confirm (y/N gate)
            // → normal view keys.
            if app.secret_setup.is_some() {
                input::handle_key_secret_setup(app, *k);
            } else if app.spawn_prompt.is_some() {
                input::handle_key_spawn_prompt(app, *k);
            } else if app.confirm.is_some() {
                input::handle_key_confirm(app, *k);
            } else {
                input::handle_key_normal(app, *k);
            }
        }
        // Mouse is ignored while any modal is pending — keyboard-
        // only resolution avoids accidentally killing a session or
        // dismissing a spawn / token prompt with a stray click.
        event::Event::Mouse(m)
            if app.confirm.is_none()
                && app.spawn_prompt.is_none()
                && app.secret_setup.is_none() =>
        {
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
        Command::TrackerWeb => tracker_web(app),
        Command::StartAo => start_ao(app, term),
        Command::RestartAoForOrchestrator => restart_ao_for_orchestrator(app, term),
        Command::OpenWeb => open_web(app),
        Command::KillSession(id) => kill_session(app, term, id),
        Command::StopAo => stop_ao(app, term),
        Command::RequestRefresh => app.request_refresh(),
        Command::Spawn(issue) => spawn_session(app, term, &issue),
        Command::SaveOauthToken(token) => save_oauth_token(app, &token),
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

/// Hand the AO catalog yaml to `$EDITOR` (suspending the alt screen)
/// and reload the cached config when the user comes back, so any edits
/// they made take effect on the next session-list refresh without
/// needing a manual `r` press. Always targets the canonical XDG yaml
/// — fleet ships as a binary users run inside their own repos, so
/// dropping artefacts into someone else's working tree is off-limits.
fn edit_config(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let Some(yaml_path) = crate::ao::config::AoConfig::default_xdg_path() else {
        app.flash_err("no $HOME / $XDG_CONFIG_HOME — can't resolve AO config path");
        return;
    };
    // `$EDITOR` won't open a non-existent file gracefully in every
    // editor (vi handles it, some fancy ones complain); make sure the
    // parent dir exists so a brand-new XDG path is writable.
    if let Some(parent) = yaml_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        app.flash_err(format!("mkdir {}: {}", parent.display(), e));
        return;
    }
    let res = suspend_around(term, || {
        crate::process::run_interactive(&editor, &[yaml_path.display().to_string()], &[], &[])
            .map(|_| ())
    });
    app.flash_if_err(res);
    // Pick up any changes the user made (added projects, renamed,
    // moved paths, …) so the breadcrumb chip + cwd-scoped filter
    // reflect them immediately.
    app.reload_ao_config();
}

/// `t`: open the tracker's terminal UI for the current project.
/// Resolves the project's `tracker.plugin` from AO config and runs
/// the matching in-VM TUI (`git-bug termui`, `gh dash`). Web-only
/// plugins (none today, but conceptually a tracker without a
/// terminal UI) flash a "use Shift+T for the web view" hint.
fn tracker_preview(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let Some((project_path, plugin)) = current_project_tracker(app) else {
        app.flash_err(
            "no tracker configured for this project — set tracker.plugin in agent-orchestrator.yaml",
        );
        return;
    };

    let argv: Vec<String> = match plugin.as_str() {
        "git-bug" => {
            // git-bug refuses to start without an identity, and the
            // standard `git-bug user new` is interactive — bad fit for
            // the suspend-and-attach flow. Bootstrap one from the host
            // git config (the user already set name/email there for
            // their normal git commits) inline so termui just works.
            let script = match git_bug_bootstrap_script(&project_path) {
                Ok(s) => s,
                Err(e) => {
                    app.flash_err(format!("git-bug identity: {e:#}"));
                    return;
                }
            };
            vec![
                "shell".into(),
                "--workdir".into(),
                project_path.display().to_string(),
                "fleet-vm".into(),
                "bash".into(),
                "-c".into(),
                script,
            ]
        }
        "github" => vec![
            "shell".into(),
            "--workdir".into(),
            project_path.display().to_string(),
            "fleet-vm".into(),
            "gh".into(),
            "dash".into(),
        ],
        other => {
            app.flash_err(format!(
                "no terminal tracker preview for plugin `{other}` — use `c` to edit config"
            ));
            return;
        }
    };
    let res = suspend_around(term, || {
        crate::process::run_interactive("limactl", &argv, &[("TERM", "xterm-256color")], &[])
            .map(|_| ())
    });
    app.flash_if_err(res);
}

/// `Shift+T`: open the tracker's web view in the host browser.
/// Plugins without a web front-end (git-bug today) flash a hint
/// pointing at the lowercase `t` TUI variant. The legend doesn't
/// show the keybind for those plugins, but tolerate a stray press
/// gracefully.
fn tracker_web(app: &mut App) {
    let Some((project_path, plugin)) = current_project_tracker(app) else {
        app.flash_err(
            "no tracker configured for this project — set tracker.plugin in agent-orchestrator.yaml",
        );
        return;
    };
    match plugin.as_str() {
        "github" => match github_issues_url_for(&project_path) {
            Ok(url) => {
                if let Err(e) = webbrowser::open(&url) {
                    app.flash_err(format!("open browser: {e}"));
                }
            }
            Err(e) => app.flash_err(format!("github tracker: {e:#}")),
        },
        "git-bug" => {
            app.flash_err("git-bug has no remote site — press `t` for the local TUI");
        }
        other => {
            app.flash_err(format!("no web tracker for plugin `{other}`"));
        }
    }
}

/// `(project.path, project.tracker.plugin)` for the current project,
/// if any. `None` when fleet was launched outside a known project, or
/// when the project has no tracker block.
fn current_project_tracker(app: &App) -> Option<(std::path::PathBuf, String)> {
    let key = app.current_project_key.as_ref()?;
    let cfg = app.ao_config.as_ref()?;
    let project = cfg.projects.get(key)?;
    let plugin = project.tracker.as_ref()?.plugin.clone();
    Some((project.path.clone(), plugin))
}

/// Bash one-liner that opens `git-bug termui` for `project_path`,
/// auto-creating a git-bug identity from the host's `git config`
/// user.name / user.email when none exists yet. Without this the
/// user has to drop to a shell and run `git-bug user new` before
/// they can use the tracker, which defeats the point of the
/// keybind.
fn git_bug_bootstrap_script(project_path: &std::path::Path) -> Result<String> {
    let name = host_git_config(project_path, "user.name")?;
    let email = host_git_config(project_path, "user.email")?;
    let name_q = crate::cli::spawn::shell_quote_single(&name);
    let email_q = crate::cli::spawn::shell_quote_single(&email);
    // The `git-bug user ls | grep -q .` check returns 1 when the
    // repo has no users yet (output is empty); we only create on
    // that path. `git-bug user new --non-interactive` is the silent
    // form added in git-bug v0.10+.
    Ok(format!(
        "if ! git-bug user ls 2>/dev/null | grep -q .; then \
            git-bug user new --non-interactive --name {name_q} --email {email_q} >/dev/null || exit 1; \
         fi; \
         exec git-bug termui"
    ))
}

/// Read a `git config` value from the project's checkout. Falls back
/// to the global / system config when the local repo doesn't override
/// — git's normal precedence.
fn host_git_config(project_path: &std::path::Path, key: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["config", "--get", key])
        .output()
        .with_context(|| format!("git -C <path> config --get {key}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git config `{key}` is unset on the host — run `git config --global {key} '...'` first"
        );
    }
    let value = String::from_utf8(out.stdout)
        .context("non-UTF-8 git config value")?
        .trim()
        .to_string();
    if value.is_empty() {
        anyhow::bail!("git config `{key}` is empty");
    }
    Ok(value)
}

/// Derive `https://github.com/<owner>/<name>/issues` from a project's
/// `origin` git remote. Supports both ssh (`git@github.com:o/n.git`)
/// and https (`https://github.com/o/n.git` or without the `.git`).
fn github_issues_url_for(project_path: &std::path::Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .context("git -C <path> config remote.origin.url")?;
    if !out.status.success() {
        anyhow::bail!("no `origin` remote configured");
    }
    let remote = String::from_utf8(out.stdout)
        .context("non-UTF-8 remote URL")?
        .trim()
        .to_string();
    let trimmed = remote.trim_end_matches(".git");
    let path = trimmed
        .strip_prefix("git@github.com:")
        .or_else(|| trimmed.strip_prefix("https://github.com/"))
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .ok_or_else(|| anyhow::anyhow!("unsupported remote URL: {remote}"))?;
    Ok(format!("https://github.com/{path}/issues"))
}

/// `Shift+S` handler. AO's lifecycle is more involved than "start
/// when down, no-op when up" — the daemon can be running with no
/// orchestrator (the dashboard survives, the project's claude
/// session was killed), and running `ao start` in that state drops
/// the user into AO's interactive "already running" menu. The
/// branches below keep the TUI in charge:
///
/// - AO down: `ao start` as before (creates dashboard + orchestrator).
/// - AO up + orchestrator present: silent flash, no subprocess.
/// - AO up, no orchestrator: open a confirm modal offering to
///   restart the daemon. The actual restart runs from `Command::
///   RestartAoForOrchestrator` after the user accepts.
fn start_ao(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    if !ensure_oauth_token_configured(app) {
        return;
    }
    if app.ao_up {
        if app.has_orchestrator() {
            // Already in the state the user wants.
            return;
        }
        // Daemon up, orchestrator missing: prompt before destroying
        // the daemon (kills the dashboard and any worker sessions
        // along with it).
        app.confirm = Some(crate::tui::app::Confirm::RestartAoForOrchestrator);
        return;
    }
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        crate::cli::spawn::run_start(&repo_root, false, false).map(|_| ())
    });
    app.flash_if_err(res);
    app.request_refresh();
}

/// Tear AO down and start it fresh against the current project.
/// Bypasses `ao start`'s "already running" interactive menu when
/// the daemon is up but no orchestrator exists. Destructive (kills
/// dashboard + any running workers); only invoked after a y/N
/// confirm in [`start_ao`].
fn restart_ao_for_orchestrator(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    if !ensure_oauth_token_configured(app) {
        return;
    }
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        // `ao stop --all` releases the lock and frees the
        // dashboard's port so the subsequent `ao start` doesn't
        // hit the "already running" check.
        crate::cli::passthrough::run(&repo_root, &["stop".to_string(), "--all".to_string()])
            .map(|_| ())
            .and_then(|()| crate::cli::spawn::run_start(&repo_root, false, false).map(|_| ()))
    });
    app.flash_if_err(res);
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
    if let Err(e) = webbrowser::open(&url) {
        app.flash_err(format!("open: {e}"));
    }
}

fn kill_session(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>, id: String) {
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        crate::cli::passthrough::run_with_prefix(&repo_root, "session", &["kill".to_string(), id])
            .map(|_| ())
    });
    app.flash_if_err(res);
    app.request_refresh();
}

/// Spawn an AO worker session against the given issue id. Hands off
/// to `cli::spawn::run_spawn` (which carries the full OAuth handoff +
/// limactl shell + token-injection wrapper) via `suspend_around`, so
/// the user sees AO's output stream — and any failure surface — in
/// the terminal directly. On success we kick a refresh so the new
/// session appears in the sidebar without waiting for the next tick.
fn spawn_session(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>, issue: &str) {
    if !ensure_oauth_token_configured(app) {
        return;
    }
    let repo_root = app.repo_root.clone();
    let issue_owned = issue.to_string();
    let res = suspend_around(term, || {
        crate::cli::spawn::run_spawn(&repo_root, &issue_owned, None, None).map(|_| ())
    });
    app.flash_if_err(res);
    app.request_refresh();
}

/// Gate for any action that needs the Claude OAuth token (Shift+S
/// → start AO, n → spawn). Returns `true` when the action can
/// proceed; `false` after opening the in-TUI setup modal so the
/// user can configure the token without dropping to a shell. The
/// passthrough auth mode short-circuits to `true` since it doesn't
/// touch fleet's `claude_code_oauth_token` secret.
fn ensure_oauth_token_configured(app: &mut App) -> bool {
    let cfg = crate::config::Config::load(&app.repo_root).unwrap_or_default();
    if cfg.agent_auth().resolve(&app.repo_root) != crate::config::ResolvedAuthMode::ClaudeOauth {
        return true;
    }
    if cfg.secrets.contains_key("claude_code_oauth_token") {
        return true;
    }
    app.secret_setup = Some(crate::tui::app::SecretSetup::default());
    false
}

/// Save the user-provided OAuth token from the secret-setup modal
/// into the OS keychain (via the `keyring` crate, same backend the
/// existing `KeychainBackend` reads from at spawn time) and append
/// the matching config entry to the XDG fleet config so the next
/// run resolves it without prompting again.
fn save_oauth_token(app: &mut App, token: &str) {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    let service = "claude-code-oauth-token";

    // 1. Store the token via keyring.
    let entry = match keyring::Entry::new(service, &user) {
        Ok(e) => e,
        Err(e) => {
            if let Some(s) = app.secret_setup.as_mut() {
                s.error = Some(format!("open OS keyring: {e}"));
            }
            return;
        }
    };
    if let Err(e) = entry.set_password(token) {
        if let Some(s) = app.secret_setup.as_mut() {
            s.error = Some(format!("save to keyring: {e}"));
        }
        return;
    }

    // 2. Append a `[secrets.claude_code_oauth_token]` block to the
    //    fleet config so it resolves on the next run without
    //    re-prompting. We append-as-text to preserve any existing
    //    keys + comments — round-tripping via serde would strip
    //    them.
    if let Err(e) = append_keychain_secret_to_config(service) {
        if let Some(s) = app.secret_setup.as_mut() {
            s.error = Some(format!("write fleet config: {e:#}"));
        }
        return;
    }

    // Success: close the modal. No flash — the user just typed the
    // token, they know it succeeded when the modal vanishes.
    app.secret_setup = None;
}

fn append_keychain_secret_to_config(service: &str) -> Result<()> {
    let path = crate::config::Config::xdg_path()
        .context("no HOME / XDG_CONFIG_HOME — can't resolve fleet config path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains("[secrets.claude_code_oauth_token]") {
        // The precheck shouldn't have opened the modal if this
        // existed — but if it did (race against an external edit),
        // refuse to clobber.
        anyhow::bail!(
            "{} already has a `claude_code_oauth_token` entry; \
             edit it manually with `c` if you want to change backends",
            path.display()
        );
    }
    let separator = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let new_section = format!(
        "{separator}[secrets.claude_code_oauth_token]\nbackend = \"keychain\"\nservice = \"{service}\"\n",
    );
    let combined = format!("{existing}{new_section}");
    std::fs::write(&path, combined).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn stop_ao(app: &mut App, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let repo_root = app.repo_root.clone();
    let res = suspend_around(term, || {
        crate::cli::passthrough::run(&repo_root, &["stop".to_string()]).map(|_| ())
    });
    app.flash_if_err(res);
    app.request_refresh();
}
