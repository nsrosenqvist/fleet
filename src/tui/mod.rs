//! v2 ratatui session browser — `fleet ui`.
//!
//! Minimal Phase-1 surface: lists fleet sessions from the [`SessionStore`]
//! in a sidebar, shows the selected session's meta + most recent log tail
//! in a detail pane. Keybindings:
//!
//! - `q` / `Esc` — quit
//! - `j` / `↓` — next session
//! - `k` / `↑` — previous session
//! - `r` — reload from `.fleet/sessions/`
//! - `Shift+K` — mark the selected session `Failed` (the Phase-1 "kill"
//!   affordance: persists the transition; container-side stop is the
//!   operator's job via `fleet runtime stop <id>`)
//! - `n` — open the spawn picker: shows the workflows under
//!   `.fleet/workflows/`; Enter detaches a `fleet workflow run <name>`
//!   subprocess (inherits env + cwd), Esc cancels. The TUI itself
//!   doesn't drive the run — `r` reloads when you want to see the new
//!   session row.
//! - `Shift+A` — toggle autonomous mode. While ON, the supervisor in
//!   [`crate::autonomous`] periodically scans the configured tracker
//!   for open issues and detaches `fleet workflow run` subprocesses
//!   against unclaimed ones — bounded by `autonomous.max_parallel`
//!   from `.fleet/config.yaml`. The status bar surfaces the engine's
//!   live status line.
//! - `d` — toggle the doctor pane.
//!
//! The TUI is a *browser*, not a workflow driver — it does not run
//! containers itself. Spawning shells out to the fleet binary so the
//! TUI's event loop stays responsive; attaching is `fleet runtime
//! attach`. Keeps the TUI's surface area small enough that the
//! implementation fits in one file.
//!
//! Pure render helpers (`state_word`, `state_marker`, `sort_sessions`,
//! `tail_lines`) are unit-tested. The ratatui draw cycle and event loop
//! are exercised by manual smoke; the helpers carry the behavioural
//! contract.

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;

use crate::autonomous;
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::Capabilities;
use crate::runtime::detect::probe;
use crate::runtime::factory::{build_adapter, build_stopper};
use crate::session::reaper::{self, RealPidProbe, ReapReport};
use crate::session::store::SessionStore;
use crate::session::{Session, SessionState, now_ms};

const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const LOG_TAIL_LINES: usize = 25;

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
    reap_report: Option<ReapReport>,
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
        terminal.draw(|f| render(f, &state))?;
        if event::poll(POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                match state.handle_key(key, store) {
                    Action::Quit => return Ok(0),
                    Action::None => {}
                }
            }
        }
        // After event handling (or after the poll timed out), let the
        // autonomous supervisor decide whether to fire anything. The
        // engine debounces internally so this is cheap to call every
        // iteration.
        state.autonomous_tick(store, std::time::Instant::now());
    }
}

/// In-memory app state. Held by the event loop, mutated by key handlers,
/// snapshotted by the render functions.
struct AppState {
    root: PathBuf,
    sessions: Vec<Session>,
    list_state: ListState,
    /// Tail of the most-recently-touched log under the selected session.
    /// Re-read on selection change so it doesn't go stale across refreshes.
    log_tail: Vec<String>,
    last_selected_id: Option<String>,
    status_line: String,
    /// Which top-level view is active. Toggled by `d`.
    view: View,
    /// Lazily-computed doctor snapshot. `None` until the user enters
    /// the doctor view at least once; refreshed on every entry so the
    /// pane reflects current config + host probe state.
    doctor: Option<DoctorSnapshot>,
    /// Workflow names available to launch from this repo. Refreshed
    /// each time the spawn picker opens so a newly-added workflow
    /// file appears without restarting the TUI. Empty when no
    /// `.fleet/workflows/` exists.
    spawn_workflows: Vec<String>,
    /// Cursor for the spawn picker. Independent of `list_state` (which
    /// tracks the sessions sidebar) so reopening the picker doesn't
    /// nuke the session selection.
    spawn_list_state: ListState,
    /// Repo config snapshot. Reloaded on `r`. Carries the
    /// `autonomous:` bounds + the tracker choice the engine needs.
    config: RepoConfig,
    /// Autonomous supervisor. Off by default; toggled via `Shift+A`.
    autonomous: autonomous::AutonomousEngine,
    /// Tracker built lazily on first `Shift+A` — the `gh`/`git-bug`
    /// construction happens then, not at TUI startup, so users who
    /// never use autonomous mode aren't blocked by tracker setup.
    tracker: TrackerState,
}

/// Lifecycle of the lazily-built tracker. Three states because the
/// "tried and the plugin isn't implemented" outcome must be sticky:
/// re-pressing `Shift+A` after a Linear/Jira refusal shouldn't
/// silently retry the same build.
enum TrackerState {
    /// Not yet built. First `Shift+A` transitions out of this.
    Pending,
    /// Built and ready to scan.
    Built(Arc<dyn crate::tracker::Tracker>),
    /// Built failed — typically the configured plugin (Linear / Jira)
    /// has no impl yet. The toggle handler surfaces this in the
    /// engine status; subsequent presses don't retry.
    Unsupported,
}

/// Top-level view enum. `Sessions` is the default; `Doctor` shows the
/// adapter + tracker + agents introspection pane; `Spawn` shows the
/// workflow picker for kicking off a new `fleet workflow run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Sessions,
    Doctor,
    Spawn,
}

/// Resolved snapshot for the doctor pane. Computed by
/// [`DoctorSnapshot::probe`] against the current cwd.
#[derive(Debug, Clone)]
pub struct DoctorSnapshot {
    pub root: PathBuf,
    pub initialised: bool,
    pub configured_adapter: String,
    pub configured_hardening: String,
    pub tracker: String,
    /// Adapter name + capability descriptor when the factory built one.
    /// `Err(msg)` carries the user-facing reason the factory refused.
    pub adapter: Result<(String, Capabilities), String>,
    /// `(agent name, env passthrough whitelist)` rows from the registry,
    /// in stable iteration order.
    pub agents: Vec<(String, Vec<String>)>,
}

impl DoctorSnapshot {
    /// Build a snapshot for the repo rooted at `root`. Uses
    /// `RealProcessInvoker` to do the host probe + (potentially) the
    /// Docker rootless check. Pure with respect to its arguments;
    /// failure modes resolve into the struct rather than propagating.
    pub fn probe(root: PathBuf) -> Self {
        let initialised = repo::is_initialised(&root);
        let config = RepoConfig::load(root.join(".fleet/config.yaml")).unwrap_or_default();
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        let probe_report = probe(invoker.as_ref());
        let adapter = match build_adapter(&config.runtime, &probe_report, Arc::clone(&invoker)) {
            Ok(adapter) => Ok((adapter.name().to_string(), adapter.capabilities())),
            Err(err) => Err(format!("{err:#}")),
        };
        let agents: Vec<(String, Vec<String>)> = config
            .agents
            .registry
            .iter()
            .map(|(name, spec)| (name.to_string(), spec.env_passthrough.clone()))
            .collect();
        Self {
            root,
            initialised,
            configured_adapter: config.runtime.adapter.as_str().to_string(),
            configured_hardening: config.runtime.hardening.as_str().to_string(),
            tracker: config.tracker.as_str().to_string(),
            adapter,
            agents,
        }
    }
}

impl AppState {
    fn new(root: PathBuf, store: &SessionStore) -> Result<Self> {
        let config = RepoConfig::load(root.join(".fleet/config.yaml")).unwrap_or_default();
        let mut state = Self {
            root,
            sessions: Vec::new(),
            list_state: ListState::default(),
            log_tail: Vec::new(),
            last_selected_id: None,
            status_line: " ready ".to_string(),
            view: View::Sessions,
            doctor: None,
            spawn_workflows: Vec::new(),
            spawn_list_state: ListState::default(),
            config,
            autonomous: autonomous::AutonomousEngine::new(),
            tracker: TrackerState::Pending,
        };
        state.reload(store)?;
        Ok(state)
    }

    fn reload(&mut self, store: &SessionStore) -> Result<()> {
        let ids = store.list().context("listing sessions")?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            // Tolerate per-session load errors: a half-written meta.json
            // shouldn't block the rest of the browser.
            if let Ok(s) = store.load(&id) {
                sessions.push(s);
            }
        }
        sort_sessions(&mut sessions);
        // Preserve the previously selected session by id; on no match,
        // select the first if any.
        let prev = self.list_state.selected().and_then(|i| self.sessions.get(i)).map(|s| s.id.to_string());
        self.sessions = sessions;
        let new_index = prev
            .and_then(|id| self.sessions.iter().position(|s| s.id.to_string() == id))
            .or(if self.sessions.is_empty() { None } else { Some(0) });
        self.list_state.select(new_index);
        self.refresh_log_tail();
        self.status_line = render_status_line(&self.sessions);
        Ok(())
    }

    fn selected(&self) -> Option<&Session> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let len = isize::try_from(self.sessions.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.list_state.selected().unwrap_or(0))
            .unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.list_state.select(Some(next_usize));
        self.refresh_log_tail();
    }

    /// Re-read the log tail when the selected session changes. Cheap: only
    /// touches disk on actual change, not on every redraw.
    fn refresh_log_tail(&mut self) {
        // Pull both pieces out under one immutable borrow, then drop it
        // before mutating self — otherwise the borrow checker rightly
        // rejects re-assigning `last_selected_id` while `selected()` is
        // still alive.
        let id_and_logs_dir = self.selected().map(|session| {
            let id = session.id.to_string();
            let dir = self
                .root
                .join(".fleet/sessions")
                .join(session.id.as_str())
                .join("logs");
            (id, dir)
        });
        let Some((current_id, logs_dir)) = id_and_logs_dir else {
            self.log_tail.clear();
            self.last_selected_id = None;
            return;
        };
        if Some(&current_id) == self.last_selected_id.as_ref() {
            return;
        }
        self.last_selected_id = Some(current_id);
        self.log_tail = read_latest_log_tail(&logs_dir, LOG_TAIL_LINES);
    }

    fn handle_key(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        // Shift+K is the kill affordance — match before the plain-k arm
        // so the modifier discriminates. Disabled in non-Sessions views
        // (those don't have a selection to kill).
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('K'))
            && self.view == View::Sessions
        {
            self.mark_selected_failed(store);
            return Action::None;
        }
        // Shift+A toggles autonomous mode from any view. Building the
        // tracker is lazy: deferred to the first toggle so a `gh
        // auth` failure doesn't trip up users who never use this
        // mode.
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('A'))
        {
            self.toggle_autonomous();
            return Action::None;
        }
        // Dispatch by view. Each view owns its own keybindings; common
        // ones (`q` quit, `Esc` close) are handled per-view so an `Esc`
        // out of a modal doesn't also quit the app.
        match self.view {
            View::Sessions => self.handle_key_sessions(key, store),
            View::Doctor => self.handle_key_doctor(key, store),
            View::Spawn => self.handle_key_spawn(key),
        }
    }

    fn handle_key_sessions(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('d') => {
                // Enter doctor view. Re-probe so the pane is always
                // current rather than showing stale state.
                self.doctor = Some(DoctorSnapshot::probe(self.root.clone()));
                self.view = View::Doctor;
                Action::None
            }
            KeyCode::Char('r') => {
                // Reload sessions + config so edits to
                // .fleet/config.yaml take effect (notably the
                // autonomous block's bounds + scan interval) without
                // restarting the TUI.
                self.config =
                    RepoConfig::load(self.root.join(".fleet/config.yaml")).unwrap_or_default();
                if let Err(err) = self.reload(store) {
                    self.status_line = format!(" reload failed: {err:#} ");
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Char('n') => {
                self.open_spawn_picker();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_key_doctor(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc | KeyCode::Char('d') => {
                self.view = View::Sessions;
                Action::None
            }
            KeyCode::Char('r') => {
                if let Err(err) = self.reload(store) {
                    self.status_line = format!(" reload failed: {err:#} ");
                }
                self.doctor = Some(DoctorSnapshot::probe(self.root.clone()));
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_key_spawn(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.view = View::Sessions;
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_spawn_selection(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_spawn_selection(-1);
                Action::None
            }
            KeyCode::Enter => {
                self.spawn_selected();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Refresh the workflow list and switch to the spawn picker. Empty
    /// list is fine — the renderer shows a hint instead of a picker
    /// and Enter is a no-op.
    fn open_spawn_picker(&mut self) {
        self.spawn_workflows = list_workflows_dir(&self.root);
        let select = if self.spawn_workflows.is_empty() {
            None
        } else {
            // Restore the previous selection if it still exists; default
            // to the first row otherwise.
            self.spawn_list_state
                .selected()
                .filter(|i| *i < self.spawn_workflows.len())
                .or(Some(0))
        };
        self.spawn_list_state.select(select);
        self.view = View::Spawn;
    }

    fn move_spawn_selection(&mut self, delta: isize) {
        if self.spawn_workflows.is_empty() {
            return;
        }
        let len = isize::try_from(self.spawn_workflows.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.spawn_list_state.selected().unwrap_or(0))
            .unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.spawn_list_state.select(Some(next_usize));
    }

    /// Spawn `fleet workflow run <selected>` as a detached subprocess so
    /// the TUI's event loop isn't blocked by the workflow run. The
    /// child inherits the user's environment (API keys, etc.) and runs
    /// in the same cwd, writing its session row into the same
    /// `.fleet/sessions/` the TUI is browsing. After spawning, return
    /// to the Sessions view; the user presses `r` to see the new row.
    fn spawn_selected(&mut self) {
        let Some(idx) = self.spawn_list_state.selected() else {
            self.status_line = " spawn: no workflow selected ".to_string();
            self.view = View::Sessions;
            return;
        };
        let Some(name) = self.spawn_workflows.get(idx).cloned() else {
            self.status_line = " spawn: selection out of range ".to_string();
            self.view = View::Sessions;
            return;
        };
        let Ok(binary) = std::env::current_exe() else {
            self.status_line =
                " spawn failed: cannot resolve fleet binary path (current_exe) "
                    .to_string();
            self.view = View::Sessions;
            return;
        };
        let mut cmd = build_workflow_run_command(&binary, &name, None);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => {
                // Intentionally don't `wait()`: detaching is the point.
                // The child runs to Completed/AwaitingGate/Failed on
                // its own; the user sees the row on next `r` reload.
                self.status_line = format!(" spawned `{name}` — press `r` to refresh ");
            }
            Err(err) => {
                self.status_line = format!(" spawn `{name}` failed: {err:#} ");
            }
        }
        self.view = View::Sessions;
    }

    /// Toggle autonomous mode. On the first ON, build the tracker
    /// from `.fleet/config.yaml`. Tracker build failures leave the
    /// engine OFF with a status line explaining why; re-pressing
    /// `Shift+A` after an Unsupported result does not retry.
    fn toggle_autonomous(&mut self) {
        if !self.autonomous.enabled() {
            if matches!(self.tracker, TrackerState::Pending) {
                let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
                self.tracker = crate::tracker::build(self.config.tracker, invoker)
                    .map_or(TrackerState::Unsupported, |boxed| {
                        TrackerState::Built(Arc::from(boxed))
                    });
            }
            if matches!(self.tracker, TrackerState::Unsupported) {
                self.autonomous.set_status(format!(
                    "autonomous: cannot enable — tracker `{}` is not implemented yet",
                    self.config.tracker.as_str(),
                ));
                return;
            }
        }
        self.autonomous.toggle();
    }

    /// One autonomous supervisor tick. The engine debounces, so this
    /// can be called every TUI iteration without flooding the
    /// tracker. On a spawn outcome, detach a `fleet workflow run`
    /// subprocess and force a session reload so the new row shows up
    /// in the sidebar.
    fn autonomous_tick(&mut self, store: &SessionStore, now: std::time::Instant) {
        if !self.autonomous.enabled() {
            return;
        }
        // The tracker must exist if we're enabled (toggle gates on
        // that). Defensive guard for safety.
        let TrackerState::Built(tracker) = &self.tracker else {
            return;
        };
        let tracker = Arc::clone(tracker);
        let root = self.root.clone();
        let list_open = move || -> Result<Vec<crate::session::IssueContext>, String> {
            let issues = tracker
                .list_issues(&root)
                .map_err(|e| format!("{e:#}"))?;
            Ok(issues
                .into_iter()
                .filter(|i| i.status == "open")
                .map(|i| crate::session::IssueContext {
                    id: i.id,
                    human_id: i.human_id,
                    title: i.title,
                    labels: i.labels,
                })
                .collect())
        };
        let outcome =
            self.autonomous
                .step(now, &self.config.autonomous, &self.sessions, list_open);
        if let autonomous::AutonomousOutcome::Spawn(cmd) = outcome {
            self.dispatch_autonomous_spawn(&cmd, store);
        }
    }

    fn dispatch_autonomous_spawn(
        &mut self,
        cmd: &autonomous::SpawnCommand,
        store: &SessionStore,
    ) {
        let Ok(binary) = std::env::current_exe() else {
            self.autonomous.set_status(
                "autonomous: ON · spawn failed: cannot resolve fleet binary (current_exe)",
            );
            return;
        };
        let mut child = build_workflow_run_command(
            &binary,
            &cmd.workflow,
            Some(&cmd.issue.human_id),
        );
        child
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match child.spawn() {
            Ok(_child) => {
                // Reload so the new session row appears in the
                // sidebar without waiting for the user to hit `r`.
                if let Err(err) = self.reload(store) {
                    tracing::warn!(?err, "autonomous post-spawn reload failed");
                }
            }
            Err(err) => {
                self.autonomous.set_status(format!(
                    "autonomous: ON · spawn `{}` for #{} failed: {err:#}",
                    cmd.workflow, cmd.issue.human_id,
                ));
            }
        }
    }

    fn mark_selected_failed(&mut self, store: &SessionStore) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(idx) else {
            return;
        };
        if session.state.is_terminal() {
            self.status_line = format!(" {} already {:?} ", session.id, session.state);
            return;
        }
        if let Err(err) = session.transition_to(SessionState::Failed, now_ms()) {
            self.status_line = format!(" kill rejected: {err:#} ");
            return;
        }
        if let Err(err) = store.save(session) {
            self.status_line = format!(" kill save failed: {err:#} ");
            return;
        }
        self.status_line = format!(" {} → failed ", session.id);
    }
}

/// Closed enum so the event loop's dispatch stays exhaustive.
enum Action {
    None,
    Quit,
}

/// List the workflows available to launch from this repo. Returns the
/// basenames (no `.yaml` extension) of every `.yaml` file directly under
/// `<root>/.fleet/workflows/`, sorted alphabetically for stable display
/// in the spawn picker.
///
/// Missing directory returns an empty Vec — a not-yet-`init`'d repo
/// shouldn't error the picker, just leave it empty. Non-YAML files,
/// subdirectories, and hidden files are skipped.
#[must_use]
pub fn list_workflows_dir(root: &Path) -> Vec<String> {
    let dir = root.join(".fleet/workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                return None;
            }
            let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
            if stem.starts_with('.') {
                return None;
            }
            Some(stem)
        })
        .collect();
    names.sort();
    names
}

/// Build the `fleet workflow run <name> [--issue <human_id>]` command
/// the spawn picker (no issue) and autonomous mode (with issue) both
/// kick off. Pure: doesn't actually call `spawn()`; the caller does
/// that and can choose how to detach stdio. `fleet_binary` is the
/// path to the fleet executable (typically `std::env::current_exe()`)
/// so the spawned child is the same binary the TUI is running from.
#[must_use]
pub fn build_workflow_run_command(
    fleet_binary: &Path,
    workflow_name: &str,
    issue_human_id: Option<&str>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(fleet_binary);
    cmd.args(["workflow", "run", workflow_name]);
    if let Some(id) = issue_human_id {
        cmd.args(["--issue", id]);
    }
    cmd
}

/// Sort sessions newest-first (descending `created_at_ms`). Ties broken
/// by id descending so the order is deterministic across calls.
pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    });
}

/// Short word for a state. Matches the wording the CLI commands print
/// so users see the same vocabulary in both places.
pub const fn state_word(s: SessionState) -> &'static str {
    match s {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::AwaitingGate => "awaiting_gate",
        SessionState::Completed => "completed",
        SessionState::Failed => "failed",
        SessionState::Crashed => "crashed",
    }
}

/// Render a USD cost as `$0.42` (two decimals) or `-` when absent.
/// Mirrors the wording in `cli::sessions` — duplicated rather than
/// shared because the wording is a UI contract; consolidating once
/// would invite one renderer's refactor to silently drift the other.
#[must_use]
pub fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "-".to_string(), |v| format!("${v:.2}"))
}

/// Walk every session and sum the ones with cost data. Returns
/// `(total, samples)` — `samples` is how many sessions contributed
/// (so the renderer can choose between "no data" and "$X.YZ").
#[must_use]
pub fn lifetime_cost(sessions: &[Session]) -> (f64, usize) {
    let mut total = 0.0;
    let mut samples = 0usize;
    for s in sessions {
        if let Some(v) = s.total_cost_usd() {
            total += v;
            samples += 1;
        }
    }
    (total, samples)
}

/// Render the status-bar tail used by the sidebar when autonomous
/// mode isn't taking over the line. Includes the session count and,
/// when at least one session has cost data, a lifetime total. Free
/// function so the wording is asserted in tests.
#[must_use]
pub fn render_status_line(sessions: &[Session]) -> String {
    let (total, samples) = lifetime_cost(sessions);
    if samples == 0 {
        format!(" {} sessions ", sessions.len())
    } else {
        format!(
            " {} sessions · ${total:.2} total ({samples} with cost) ",
            sessions.len(),
        )
    }
}

/// Single-glyph state marker for the sidebar list.
pub const fn state_marker(s: SessionState) -> &'static str {
    match s {
        SessionState::Created => "·",
        SessionState::Running => "▶",
        SessionState::AwaitingGate => "⏸",
        SessionState::Completed => "✓",
        SessionState::Failed => "✗",
        SessionState::Crashed => "‼",
    }
}

fn state_style(s: SessionState) -> Style {
    match s {
        SessionState::Completed => Style::default().fg(Color::Green),
        SessionState::Failed | SessionState::Crashed => Style::default().fg(Color::Red),
        SessionState::Running => Style::default().fg(Color::Yellow),
        SessionState::AwaitingGate => Style::default().fg(Color::Cyan),
        SessionState::Created => Style::default(),
    }
}

/// Pure: take a slice of lines, return the last `n`. Used to tail logs
/// for the detail pane.
#[must_use]
pub fn tail_lines(input: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = input.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| (*s).to_string()).collect()
}

/// Read the most-recently-modified `.log` under `logs_dir` and return
/// its last `n` lines. Returns an empty Vec if the dir is missing or
/// has no logs — that's the steady state for a session that hasn't
/// executed yet.
fn read_latest_log_tail(logs_dir: &Path, n: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    let Some((_, path)) = newest else {
        return Vec::new();
    };
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    tail_lines(&body, n)
}

fn render(f: &mut Frame<'_>, state: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());
    match state.view {
        View::Sessions => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(outer[0]);
            render_sidebar(f, body[0], state);
            render_detail(f, body[1], state);
        }
        View::Doctor => {
            render_doctor(f, outer[0], state);
        }
        View::Spawn => {
            render_spawn(f, outer[0], state);
        }
    }
    render_status(f, outer[1], state);
}

fn render_doctor(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().title(" Doctor ").borders(Borders::ALL);
    let Some(snapshot) = state.doctor.as_ref() else {
        let body = Paragraph::new("(probing host...)").block(block);
        f.render_widget(body, area);
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let repo_value = format!(
        "{}  ({})",
        snapshot.root.display(),
        if snapshot.initialised {
            "initialised"
        } else {
            "not initialised — run `fleet init`"
        },
    );
    lines.push(kv_line("repo", &repo_value));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "runtime adapter:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(kv_line(
        "configured",
        &format!(
            "{}  (hardening: {})",
            snapshot.configured_adapter, snapshot.configured_hardening,
        ),
    ));
    match &snapshot.adapter {
        Ok((name, caps)) => {
            lines.push(kv_line("resolved", &format!("{name}  {}", caps.describe())));
        }
        Err(msg) => {
            lines.push(Line::from(vec![
                Span::styled("resolved  ", Style::default().fg(Color::DarkGray)),
                Span::styled(msg.clone(), Style::default().fg(Color::Red)),
            ]));
        }
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "agents:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if snapshot.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, env) in &snapshot.agents {
            let env_str = if env.is_empty() {
                String::new()
            } else {
                format!("  (env: {})", env.join(", "))
            };
            lines.push(Line::from(format!("  - {name}{env_str}")));
        }
    }
    lines.push(Line::from(""));

    lines.push(kv_line("tracker", &snapshot.tracker));

    let body = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_spawn(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default()
        .title(" Spawn workflow ")
        .borders(Borders::ALL);
    if state.spawn_workflows.is_empty() {
        let body = Paragraph::new(
            "(no workflows found under .fleet/workflows/)\n\n\
             Run `fleet init` to scaffold the default standard / hotfix / review-only\n\
             workflows, or drop a `<name>.yaml` into `.fleet/workflows/` by hand.",
        )
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem<'_>> = state
        .spawn_workflows
        .iter()
        .map(|name| ListItem::new(Line::from(name.clone())))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = state.spawn_list_state;
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = format!(" Sessions ({}) ", state.sessions.len());
    let block = Block::default().title(title).borders(Borders::ALL);
    if state.sessions.is_empty() {
        let body = Paragraph::new(
            "(no sessions)\n\nUse `fleet workflow run <name>` from the shell to create one.",
        )
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem<'_>> = state
        .sessions
        .iter()
        .map(|s| {
            // Cost suffix only when we have a figure — keeps fresh
            // repos visually clean ("nothing to render" beats "a long
            // row of dashes").
            let cost_suffix = s
                .total_cost_usd()
                .map_or_else(String::new, |v| format!(" ${v:.2}"));
            let label = format!(
                "{} {} {}{cost_suffix}",
                state_marker(s.state),
                s.id,
                s.workflow,
            );
            ListItem::new(Span::styled(label, state_style(s.state)))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = state.list_state;
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(session) = state.selected() else {
        let body = Paragraph::new(
            "Select a session — j/k or arrows. r refresh, Shift+K mark failed, q quit.",
        )
        .block(Block::default().title(" Detail ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    };

    let mut lines = vec![
        kv_line("workflow", &session.workflow),
        kv_line("state", state_word(session.state)),
        kv_line("node", session.current_node.as_deref().unwrap_or("-")),
        kv_line(
            "updated",
            &format!("{} ms (epoch)", session.updated_at_ms),
        ),
        kv_line("cost", &format_cost(session.total_cost_usd())),
    ];
    if !session.node_costs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "node costs:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (node, usd) in &session.node_costs {
            lines.push(Line::from(format!("  {node:<16} ${usd:.4}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "log tail:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if state.log_tail.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no logs yet)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for log_line in &state.log_tail {
            lines.push(Line::from(format!("  {log_line}")));
        }
    }
    let title = format!(" {} ", session.id);
    let body = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_status(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let help = match state.view {
        View::Sessions => {
            "[q] quit  [j/k] nav  [r] reload  [d] doctor  [Shift+K] kill  [n] spawn  [Shift+A] auto"
        }
        View::Doctor => "[q] quit  [Esc/d] back  [r] re-probe",
        View::Spawn => "[Esc/q] cancel  [j/k] nav  [Enter] spawn",
    };
    // Autonomous status takes precedence when the engine is doing
    // something interesting (enabled, or has an override message set).
    // Otherwise show the existing free-form `status_line`.
    let tail = if state.autonomous.enabled()
        || state.autonomous.status() != "autonomous: OFF"
    {
        state.autonomous.status().to_string()
    } else {
        state.status_line.clone()
    };
    let bar = format!("{help} — {tail}");
    let p = Paragraph::new(bar).style(
        Style::default()
            .bg(Color::Black)
            .fg(Color::Gray),
    );
    f.render_widget(p, area);
}

fn kv_line(key: &str, value: &str) -> Line<'static> {
    let key_owned = format!("{key:<9} ");
    Line::from(vec![
        Span::styled(key_owned, Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    fn session(id: &str, workflow: &str, state: SessionState, ts: u64) -> Session {
        let mut s = Session::new(SessionId::new(id), workflow, ts);
        if state != SessionState::Created {
            // Walk through the legal transitions to reach `state`.
            let _ = s.transition_to(SessionState::Running, ts + 1);
            if state != SessionState::Running {
                let _ = s.transition_to(state, ts + 2);
            }
        }
        s
    }

    #[test]
    fn state_word_matches_serde_renames() {
        assert_eq!(state_word(SessionState::AwaitingGate), "awaiting_gate");
        assert_eq!(state_word(SessionState::Completed), "completed");
    }

    #[test]
    fn format_cost_some_is_two_decimal_dollars() {
        assert_eq!(format_cost(Some(0.42)), "$0.42");
        assert_eq!(format_cost(Some(1.5)), "$1.50");
        assert_eq!(format_cost(Some(0.0)), "$0.00");
    }

    #[test]
    fn format_cost_none_is_dash() {
        assert_eq!(format_cost(None), "-");
    }

    #[test]
    fn lifetime_cost_skips_sessions_without_cost_data() {
        let mut s_with = session("s-1", "wf", SessionState::Running, 1);
        s_with.record_node_cost("plan", 0.10, 2);
        let s_without = session("s-2", "wf", SessionState::Running, 3);
        let mut s_zero = session("s-3", "wf", SessionState::Running, 4);
        s_zero.record_node_cost("plan", 0.0, 5);
        let (total, samples) = lifetime_cost(&[s_with, s_without, s_zero]);
        // Two contributed; one missing.
        assert_eq!(samples, 2);
        assert!((total - 0.10).abs() < 1e-9);
    }

    #[test]
    fn lifetime_cost_returns_zero_zero_for_empty() {
        let (total, samples) = lifetime_cost(&[]);
        assert!(total.abs() < 1e-9);
        assert_eq!(samples, 0);
    }

    #[test]
    fn render_status_line_no_cost_data_shows_only_session_count() {
        let sessions = vec![
            session("s-1", "wf", SessionState::Running, 1),
            session("s-2", "wf", SessionState::Completed, 2),
        ];
        assert_eq!(render_status_line(&sessions), " 2 sessions ");
    }

    #[test]
    fn render_status_line_with_cost_data_shows_total() {
        let mut s = session("s-1", "wf", SessionState::Completed, 1);
        s.record_node_cost("plan", 0.42, 2);
        assert_eq!(
            render_status_line(&[s]),
            " 1 sessions · $0.42 total (1 with cost) "
        );
    }

    #[test]
    fn state_marker_distinguishes_each_state() {
        let glyphs: Vec<&str> = [
            SessionState::Created,
            SessionState::Running,
            SessionState::AwaitingGate,
            SessionState::Completed,
            SessionState::Failed,
            SessionState::Crashed,
        ]
        .into_iter()
        .map(state_marker)
        .collect();
        let unique: std::collections::HashSet<_> = glyphs.iter().collect();
        assert_eq!(unique.len(), glyphs.len(), "glyphs must be distinct: {glyphs:?}");
    }

    #[test]
    fn sort_sessions_orders_newest_first() {
        let mut v = vec![
            session("s-old", "a", SessionState::Completed, 100),
            session("s-mid", "b", SessionState::Running, 200),
            session("s-new", "c", SessionState::Failed, 300),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].id.as_str(), "s-new");
        assert_eq!(v[1].id.as_str(), "s-mid");
        assert_eq!(v[2].id.as_str(), "s-old");
    }

    #[test]
    fn sort_sessions_breaks_ties_by_id_descending() {
        let mut v = vec![
            session("s-aaa", "x", SessionState::Created, 100),
            session("s-zzz", "x", SessionState::Created, 100),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].id.as_str(), "s-zzz");
        assert_eq!(v[1].id.as_str(), "s-aaa");
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let input = "a\nb\nc\nd\ne";
        let tail = tail_lines(input, 3);
        assert_eq!(tail, vec!["c", "d", "e"]);
    }

    #[test]
    fn tail_lines_returns_all_when_input_shorter_than_n() {
        let input = "a\nb";
        let tail = tail_lines(input, 10);
        assert_eq!(tail, vec!["a", "b"]);
    }

    #[test]
    fn tail_lines_returns_empty_for_empty_input() {
        assert!(tail_lines("", 5).is_empty());
    }

    #[test]
    fn read_latest_log_tail_returns_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let tail = read_latest_log_tail(&tmp.path().join("nonexistent"), 10);
        assert!(tail.is_empty());
    }

    #[test]
    fn read_latest_log_tail_picks_newest_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("old.log"), "old1\nold2\n").unwrap();
        // Sleep a hair so the mtimes differ. Filesystem timestamp
        // granularity on Linux is high enough that any tiny delay
        // suffices; we use 10ms to be safe across CI machines.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(logs.join("new.log"), "new1\nnew2\nnew3\n").unwrap();
        let tail = read_latest_log_tail(&logs, 10);
        assert_eq!(tail, vec!["new1", "new2", "new3"]);
    }

    #[test]
    fn read_latest_log_tail_ignores_non_log_files() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("notes.txt"), "ignore me").unwrap();
        std::fs::write(logs.join("real.log"), "kept\n").unwrap();
        let tail = read_latest_log_tail(&logs, 10);
        assert_eq!(tail, vec!["kept"]);
    }

    #[test]
    fn app_state_reload_preserves_selection_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let s_a = session("s-a", "wf", SessionState::Completed, 100);
        let s_b = session("s-b", "wf", SessionState::Running, 200);
        store.create(&s_a).unwrap();
        store.create(&s_b).unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        // After load: sorted newest-first, so s-b at index 0.
        assert_eq!(state.selected().unwrap().id.as_str(), "s-b");
        // Move to the older one (s-a, index 1).
        state.move_selection(1);
        assert_eq!(state.selected().unwrap().id.as_str(), "s-a");
        // Reload should keep selection on s-a.
        state.reload(&store).unwrap();
        assert_eq!(state.selected().unwrap().id.as_str(), "s-a");
    }

    #[test]
    fn app_state_selects_first_session_on_initial_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-only", "wf", SessionState::Running, 1))
            .unwrap();
        let state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert_eq!(state.selected().unwrap().id.as_str(), "s-only");
    }

    #[test]
    fn app_state_selection_none_when_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert!(state.selected().is_none());
    }

    #[test]
    fn app_state_move_selection_wraps_around() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-1", "wf", SessionState::Running, 100))
            .unwrap();
        store
            .create(&session("s-2", "wf", SessionState::Running, 200))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        // start at index 0 (newest); going up should wrap to last.
        state.move_selection(-1);
        assert_eq!(state.selected().unwrap().id.as_str(), "s-1");
    }

    #[test]
    fn doctor_snapshot_probe_handles_uninitialised_repo() {
        // Probing a fresh dir with no `.fleet/` returns initialised=false
        // and falls back to default RepoConfig values.
        let tmp = tempfile::tempdir().unwrap();
        let snap = DoctorSnapshot::probe(tmp.path().to_path_buf());
        assert!(!snap.initialised);
        assert_eq!(snap.configured_adapter, "auto");
        assert_eq!(snap.configured_hardening, "auto");
        assert_eq!(snap.tracker, "git-bug");
        // Default registry contains claude-code.
        assert!(snap.agents.iter().any(|(n, _)| n == "claude-code"));
    }

    #[test]
    fn doctor_snapshot_probe_reports_initialised_after_init() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".fleet")).unwrap();
        let snap = DoctorSnapshot::probe(tmp.path().to_path_buf());
        assert!(snap.initialised);
    }

    #[test]
    fn d_toggles_into_doctor_view_and_probes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert_eq!(state.view, View::Sessions);
        state.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Doctor);
        assert!(state.doctor.is_some());
    }

    #[test]
    fn d_toggles_back_to_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            &store,
        );
        state.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Sessions);
    }

    #[test]
    fn esc_in_doctor_view_returns_to_sessions_not_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Doctor);
        let action = state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &store);
        // Should NOT quit — it returns to sessions.
        assert!(matches!(action, Action::None));
        assert_eq!(state.view, View::Sessions);
    }

    #[test]
    fn handle_key_q_returns_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        let action = state
            .handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()), &store);
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn handle_key_esc_returns_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        let action = state
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &store);
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn handle_key_shift_k_marks_running_session_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-r", "wf", SessionState::Running, 100))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            &store,
        );
        // The in-memory session reflects the transition...
        assert_eq!(state.selected().unwrap().state, SessionState::Failed);
        // ...and it was persisted.
        let loaded_id = SessionId::new(state.selected().unwrap().id.as_str());
        let loaded = store.load(&loaded_id).unwrap();
        assert_eq!(loaded.state, SessionState::Failed);
    }

    #[test]
    fn handle_key_shift_k_on_terminal_session_is_a_noop_with_status_message() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-c", "wf", SessionState::Completed, 100))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            &store,
        );
        // State unchanged; status line records the rejection.
        assert_eq!(state.selected().unwrap().state, SessionState::Completed);
        assert!(state.status_line.contains("already"));
    }

    #[test]
    fn handle_key_r_reloads_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert!(state.sessions.is_empty());
        // Add a session out-of-band and reload via the keypress.
        store
            .create(&session("s-new", "wf", SessionState::Running, 1))
            .unwrap();
        state.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()), &store);
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.selected().unwrap().id.as_str(), "s-new");
    }

    // === spawn picker ===

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
    }

    #[test]
    fn list_workflows_dir_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.fleet/workflows/` at all — picker should open empty
        // rather than error.
        assert!(list_workflows_dir(tmp.path()).is_empty());
    }

    #[test]
    fn list_workflows_dir_returns_yaml_basenames_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/standard.yaml", "name: standard\n");
        write(tmp.path(), ".fleet/workflows/hotfix.yaml", "name: hotfix\n");
        write(tmp.path(), ".fleet/workflows/review-only.yaml", "name: review-only\n");
        let names = list_workflows_dir(tmp.path());
        assert_eq!(names, vec!["hotfix", "review-only", "standard"]);
    }

    #[test]
    fn list_workflows_dir_skips_non_yaml_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/keeper.yaml", "name: keeper\n");
        write(tmp.path(), ".fleet/workflows/notes.md", "stuff\n");
        write(tmp.path(), ".fleet/workflows/.dotfile.yaml", "name: hidden\n");
        std::fs::create_dir_all(tmp.path().join(".fleet/workflows/nested-dir")).unwrap();
        let names = list_workflows_dir(tmp.path());
        assert_eq!(names, vec!["keeper"]);
    }

    #[test]
    fn build_workflow_run_command_without_issue() {
        let cmd =
            build_workflow_run_command(Path::new("/usr/local/bin/fleet"), "standard", None);
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("/usr/local/bin/fleet"), "got: {dbg}");
        assert!(dbg.contains("workflow"), "got: {dbg}");
        assert!(dbg.contains("run"), "got: {dbg}");
        assert!(dbg.contains("standard"), "got: {dbg}");
        assert!(!dbg.contains("--issue"), "got: {dbg}");
    }

    #[test]
    fn build_workflow_run_command_with_issue_appends_flag() {
        let cmd = build_workflow_run_command(
            Path::new("/usr/local/bin/fleet"),
            "standard",
            Some("42"),
        );
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("--issue"), "got: {dbg}");
        assert!(dbg.contains("42"), "got: {dbg}");
    }

    #[test]
    fn n_key_in_sessions_view_opens_spawn_picker() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/standard.yaml", "name: standard\n");
        write(tmp.path(), ".fleet/workflows/hotfix.yaml", "name: hotfix\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Spawn);
        assert_eq!(state.spawn_workflows, vec!["hotfix", "standard"]);
        assert_eq!(state.spawn_list_state.selected(), Some(0));
    }

    #[test]
    fn n_key_with_no_workflows_opens_empty_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        // View transitions even when empty; the renderer shows a hint.
        // Selection stays None.
        assert_eq!(state.view, View::Spawn);
        assert!(state.spawn_workflows.is_empty());
        assert_eq!(state.spawn_list_state.selected(), None);
    }

    #[test]
    fn esc_in_spawn_view_returns_to_sessions_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/standard.yaml", "name: standard\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Spawn);
        let action = state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &store);
        // Esc cancels — it must NOT quit.
        assert!(matches!(action, Action::None));
        assert_eq!(state.view, View::Sessions);
        // No session was created.
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn jk_in_spawn_view_wraps_through_workflow_list() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/a.yaml", "name: a\n");
        write(tmp.path(), ".fleet/workflows/b.yaml", "name: b\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.spawn_list_state.selected(), Some(0));
        state.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.spawn_list_state.selected(), Some(1));
        // Wrap: j past the end returns to 0.
        state.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.spawn_list_state.selected(), Some(0));
        // k before 0 wraps to the end.
        state.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.spawn_list_state.selected(), Some(1));
    }

    #[test]
    fn shift_k_in_spawn_view_does_not_mark_session_failed() {
        // Per-view dispatch: Shift+K only applies in Sessions. If the
        // user has a session selected, hits `n` to open the picker,
        // then accidentally Shift+K, the session must NOT be killed.
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-r", "wf", SessionState::Running, 100))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Spawn);
        state.handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            &store,
        );
        // Session stays Running — the kill was filtered out.
        let loaded = store.load(&SessionId::new("s-r")).unwrap();
        assert_eq!(loaded.state, SessionState::Running);
    }

    // === autonomous mode toggle ===

    #[test]
    fn shift_a_toggles_autonomous_with_buildable_tracker() {
        // Default tracker is `git-bug`; `tracker::build` returns
        // Some(...) for it without actually invoking the binary,
        // so the toggle path runs cleanly in a hermetic test.
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert!(!state.autonomous.enabled());
        state.handle_key(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(state.autonomous.enabled(), "Shift+A should enable");
        assert!(
            state.autonomous.status().contains("ON"),
            "got: {}",
            state.autonomous.status()
        );
        // Second toggle flips back off.
        state.handle_key(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(!state.autonomous.enabled(), "Shift+A again should disable");
    }

    #[test]
    fn shift_a_refuses_to_enable_with_unimplemented_tracker() {
        // Linear's `tracker::build` returns None — toggle must
        // explain why instead of pretending autonomous mode is live.
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/config.yaml", "tracker: linear\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(
            !state.autonomous.enabled(),
            "engine must NOT enable when tracker is unimplemented"
        );
        let status = state.autonomous.status();
        assert!(
            status.contains("cannot enable") && status.contains("linear"),
            "got: {status}"
        );
    }

    #[test]
    fn shift_a_works_from_doctor_view_too() {
        // Toggle is global, not Sessions-only — modal dispatch
        // shouldn't swallow it.
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Doctor);
        state.handle_key(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(state.autonomous.enabled());
        // View is unchanged — the toggle doesn't navigate.
        assert_eq!(state.view, View::Doctor);
    }

    #[test]
    fn autonomous_tick_when_disabled_is_a_noop() {
        // Engine off → tick does nothing. Importantly: no tracker
        // shell-out, so the test runs hermetically even though we
        // never set up a fake `git-bug`.
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert!(!state.autonomous.enabled());
        // Should return immediately.
        state.autonomous_tick(&store, std::time::Instant::now());
        assert!(!state.autonomous.enabled());
    }

    #[test]
    fn r_reload_picks_up_config_changes() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/config.yaml", "autonomous:\n  max_parallel: 1\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert_eq!(state.config.autonomous.max_parallel, 1);
        // Edit the config and reload via `r`.
        write(tmp.path(), ".fleet/config.yaml", "autonomous:\n  max_parallel: 7\n");
        state.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.config.autonomous.max_parallel, 7);
    }
}
