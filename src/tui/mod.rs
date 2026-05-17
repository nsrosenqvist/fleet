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

use anyhow::{Context, Result, bail};
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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;

use crate::autonomous;
use crate::brainstorm::store::BrainstormStore;
use crate::brainstorm::{BrainstormId, BrainstormSession, BrainstormState};
use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanItemState, PlanState};
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
    /// Active transient overlay (confirm dialog or error message).
    /// Rendered on top of whatever `view` is currently drawing.
    overlay: Overlay,
    /// Loaded plans, refreshed on entry to the Plans view. Empty
    /// until the user presses `p` (or after a `reload` from inside
    /// the Plans view). Sorted by id (lexicographic ≈ chronological
    /// thanks to the ms-prefixed id format).
    plans: Vec<Plan>,
    /// Cursor for the Plans view's sidebar. Independent of
    /// `list_state` so the user's session-row selection survives a
    /// trip into the Plans view.
    plans_list_state: ListState,
    /// Which pane of the Plans view has the cursor. Toggled by
    /// `Tab`. Default: sidebar — the natural entry point when the
    /// user presses `p`.
    plans_focus: PlansFocus,
    /// Cursor inside the currently-selected plan's items list,
    /// active when `plans_focus == Items`. Cleared (`select(None)`)
    /// whenever the sidebar selection changes so a stale item cursor
    /// doesn't carry over to a plan with fewer items.
    plans_items_state: ListState,
    /// Brainstorm sessions on disk, refreshed alongside workflow
    /// sessions. Displayed as a second section in the Sessions
    /// sidebar. Empty when the `.fleet/planning/` dir doesn't
    /// exist yet (the common case for repos that haven't used
    /// `fleet brainstorm`).
    brainstorms: Vec<BrainstormSession>,
    /// Which sidebar sub-list (Workflows / Brainstorms) is
    /// focused — `Tab` toggles. Drives where `j/k` move and what
    /// `Enter` does in the Sessions view.
    sessions_focus: SessionsFocus,
    /// Cursor inside the Brainstorms sub-list. Independent of
    /// `list_state` (which tracks Workflows) so the user's
    /// workflow-row selection survives a trip into the Brainstorms
    /// section.
    brainstorms_list_state: ListState,
    /// Ticket ids currently sitting on a cycle in the deps graph.
    /// Recomputed each `reload` from `.fleet/deps.json`. Drives
    /// the `⚠` markers on session and plan rows so the user
    /// notices a tangle the supervisor can't resolve on its own.
    /// Empty when the deps file is missing or acyclic.
    cycle_nodes: std::collections::HashSet<String>,
    /// Full deps document, cached alongside `cycle_nodes` from
    /// the same `reload`. Used by the Plans ticket-detail pane
    /// to render "Blocked on:" / "Blocks:" rows without an extra
    /// per-render disk read. Missing file → empty document.
    deps_doc: crate::deps::DepsDoc,
    /// Tracker-issue-by-human-id cache. One bulk
    /// `tracker.list_issues` call populates this whenever the
    /// Plans view is entered or reloaded, so the plan items list
    /// and the deps section can show titles next to each id
    /// without N per-render `tracker.read` calls. Empty when no
    /// tracker is configured or the list call has failed.
    tickets_by_id: std::collections::HashMap<String, crate::tracker::Issue>,
    /// Cache of tracker-fetched issue details for the Plans
    /// view's third pane, keyed by ticket id. `Err` caches the
    /// user-facing failure string so we don't re-hit the tracker
    /// on every `j`/`k` keystroke when a ticket is missing /
    /// unreadable. Cleared on `r` reload so the user has a way
    /// to force a refetch after editing the tracker out-of-band.
    focused_issue_cache:
        std::collections::HashMap<String, Result<crate::tracker::IssueDetail, String>>,
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
/// workflow picker for kicking off a new `fleet workflow run`;
/// `Plans` shows the plans-management surface (sibling to Sessions —
/// entered via `p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Sessions,
    Doctor,
    Spawn,
    Plans,
}

/// Which pane of the Plans view has the cursor. Drives where `j/k`
/// move and which `u`/`Shift+C`/`Shift+P` operate on — the sidebar
/// targets the whole plan, the items target one row in the detail
/// pane. `Tab` toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlansFocus {
    Sidebar,
    Items,
}

/// Which sub-list of the Sessions sidebar has the cursor. Workflows
/// is the default — that's the existing surface; Brainstorms is
/// reached by `Tab` and unlocks the `Enter`-to-attach affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionsFocus {
    Workflows,
    Brainstorms,
}

/// Transient overlay rendered on top of the current view.
/// Mutually-exclusive with itself but layered above the view's
/// normal render. The spawn picker is its own [`View`] for historical
/// reasons — the rest of the modal-style flows route through here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Overlay {
    None,
    /// Yes/no confirm dialog. `y` runs the action, `n`/`Esc` cancels.
    Confirm {
        prompt: String,
        action: ConfirmAction,
    },
    /// Error message — any key dismisses. Used when an action failed
    /// hard (kill rejected, spawn refused) and a flash on the status
    /// line would be too easy to miss.
    Error {
        message: String,
    },
}

/// What a confirm overlay executes when the user accepts. New entries
/// land here as the TUI grows additional destructive actions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmAction {
    /// Mark the currently-selected session as `Failed` (the "kill"
    /// affordance bound to `Shift+K`).
    KillSelected,
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
            overlay: Overlay::None,
            plans: Vec::new(),
            plans_list_state: ListState::default(),
            plans_focus: PlansFocus::Sidebar,
            plans_items_state: ListState::default(),
            brainstorms: Vec::new(),
            sessions_focus: SessionsFocus::Workflows,
            brainstorms_list_state: ListState::default(),
            cycle_nodes: std::collections::HashSet::new(),
            deps_doc: crate::deps::DepsDoc::empty(),
            tickets_by_id: std::collections::HashMap::new(),
            focused_issue_cache: std::collections::HashMap::new(),
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
        let prev = self
            .list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.id.to_string());
        self.sessions = sessions;
        let new_index = prev
            .and_then(|id| self.sessions.iter().position(|s| s.id.to_string() == id))
            .or(if self.sessions.is_empty() {
                None
            } else {
                Some(0)
            });
        self.list_state.select(new_index);
        self.refresh_log_tail();
        // Reconcile plan-item state with the latest session states
        // *before* we load plans for display, so the rendering pass
        // sees post-reconcile data. Reconcile errors degrade
        // gracefully into a status-line message so a corrupt plan
        // file doesn't block the reload.
        let plan_store = crate::plans::store::PlanStore::for_repo(&self.root);
        if let Err(err) =
            crate::autonomous::reconcile_plans_from_sessions(&self.sessions, &plan_store, now_ms())
        {
            self.status_line = format!(" reconcile failed: {err:#} ");
        }
        // Refresh plans alongside sessions so the sidebar
        // annotations + status-line plan count stay current. Plans
        // are cheap to load (one file per plan, typically a handful)
        // and the alternative (lazy load on Plans-view entry only)
        // would leave the Sessions view annotating against stale
        // data.
        self.refresh_plans();
        // Brainstorms are similarly cheap and live in the same
        // sidebar — keep the listing fresh.
        self.refresh_brainstorms();
        // Reload deps once and derive cycle membership from it —
        // both pieces of state are downstream of `.fleet/deps.json`.
        // Missing file → empty doc, which also yields empty cycles.
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        let doc = deps_store.load().unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
        self.status_line = render_status_line(&self.sessions, &self.plans);
        Ok(())
    }

    /// Reap stale brainstorms (tmux pane gone but meta says
    /// Active/Detached), then load the per-session metas from
    /// disk so the sidebar reflects current state. Per-session
    /// load failures degrade gracefully — the session is dropped
    /// from the list and the rest continue.
    fn refresh_brainstorms(&mut self) {
        let store = BrainstormStore::for_repo(&self.root);
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        if let Err(err) = crate::brainstorm::reaper::reap(&store, invoker.as_ref(), now_ms()) {
            // Reap failure shouldn't blank the sidebar — surface
            // briefly and proceed to list.
            self.status_line = format!(" brainstorms: reap failed: {err:#} ");
        }
        let ids = match store.list() {
            Ok(ids) => ids,
            Err(err) => {
                self.status_line = format!(" brainstorms: list failed: {err:#} ");
                self.brainstorms.clear();
                return;
            }
        };
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(s) = store.load(&id) {
                sessions.push(s);
            }
        }
        self.brainstorms = sessions;
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
        let current = isize::try_from(self.list_state.selected().unwrap_or(0)).unwrap_or(0);
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
        // Overlays intercept input first. A confirm dialog or error
        // overlay owns the keymap until dismissed — otherwise an
        // accidental `q` while a Confirm is up would quit the app
        // mid-decision.
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_key_overlay(key, store);
        }
        // Shift+K is the kill affordance — match before the plain-k arm
        // so the modifier discriminates. Disabled in non-Sessions views
        // (those don't have a selection to kill).
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('K'))
            && self.view == View::Sessions
        {
            self.prompt_kill_selected();
            return Action::None;
        }
        // Shift+A toggles autonomous mode from any view. Building the
        // tracker is lazy: deferred to the first toggle so a `gh
        // auth` failure doesn't trip up users who never use this
        // mode.
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('A')) {
            self.toggle_autonomous();
            return Action::None;
        }
        // Shift+B from any view spawns a fresh brainstorm session
        // and attaches to it. The actual suspend-terminal /
        // exec-subprocess / resume dance happens at the event-loop
        // level — we just signal it via the Action.
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('B')) {
            return Action::NewBrainstorm;
        }
        // Dispatch by view. Each view owns its own keybindings; common
        // ones (`q` quit, `Esc` close) are handled per-view so an `Esc`
        // out of a modal doesn't also quit the app.
        match self.view {
            View::Sessions => self.handle_key_sessions(key, store),
            View::Doctor => self.handle_key_doctor(key, store),
            View::Spawn => self.handle_key_spawn(key),
            View::Plans => self.handle_key_plans(key),
        }
    }

    /// Overlay keymap: `y` runs a confirm action; `n`/`Esc`/any other
    /// key dismisses the overlay without acting. Error overlays
    /// dismiss on any key.
    fn handle_key_overlay(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        match self.overlay.clone() {
            Overlay::Confirm { action, .. } => {
                self.overlay = Overlay::None;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.run_confirm_action(&action, store);
                } else {
                    // n, Esc, q — all cancel. Match user mental model:
                    // anything-other-than-y means "no."
                    self.status_line = " kill cancelled ".to_string();
                }
            }
            Overlay::Error { .. } => {
                // Any key dismisses; the message has been read.
                self.overlay = Overlay::None;
            }
            Overlay::None => unreachable!("handle_key_overlay called with Overlay::None"),
        }
        Action::None
    }

    /// Queue a kill-selected confirm dialog. The actual mark-failed
    /// runs from [`Self::run_confirm_action`] once the user accepts.
    /// Short-circuits on already-terminal sessions — confirming a
    /// kill on a Completed session would be theatre.
    fn prompt_kill_selected(&mut self) {
        let Some(session) = self.selected() else {
            self.status_line = " kill: no session selected ".to_string();
            return;
        };
        if session.state.is_terminal() {
            self.status_line = format!(" {} already {:?} ", session.id, session.state);
            return;
        }
        self.overlay = Overlay::Confirm {
            prompt: format!(
                "Mark session {} ({}) as failed?\n\nThis is non-recoverable.",
                session.id, session.workflow,
            ),
            action: ConfirmAction::KillSelected,
        };
    }

    fn run_confirm_action(&mut self, action: &ConfirmAction, store: &SessionStore) {
        match action {
            ConfirmAction::KillSelected => self.mark_selected_failed(store),
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
            KeyCode::Tab => {
                self.toggle_sessions_focus();
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.sessions_focus {
                    SessionsFocus::Workflows => self.move_selection(1),
                    SessionsFocus::Brainstorms => self.move_brainstorms_selection(1),
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.sessions_focus {
                    SessionsFocus::Workflows => self.move_selection(-1),
                    SessionsFocus::Brainstorms => self.move_brainstorms_selection(-1),
                }
                Action::None
            }
            KeyCode::Enter => {
                // Enter is only bound when Brainstorms has focus —
                // attach to the selected brainstorm. In Workflows
                // focus the key is reserved for a future
                // attach-workflow flow.
                if self.sessions_focus == SessionsFocus::Brainstorms {
                    if let Some(id) = self.selected_brainstorm_id() {
                        return Action::AttachBrainstorm(id);
                    }
                    self.status_line = " no brainstorm selected ".to_string();
                }
                Action::None
            }
            KeyCode::Char('p') => {
                self.open_plans_view();
                Action::None
            }
            KeyCode::Char('n') => {
                self.open_spawn_picker();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Tab between the Workflows and Brainstorms sub-lists in the
    /// Sessions sidebar. Switching to Brainstorms with no row
    /// selected lands the cursor at row 0 when there's anything to
    /// land on; empty sections keep the focus flag but the cursor
    /// stays cleared.
    fn toggle_sessions_focus(&mut self) {
        self.sessions_focus = match self.sessions_focus {
            SessionsFocus::Workflows => SessionsFocus::Brainstorms,
            SessionsFocus::Brainstorms => SessionsFocus::Workflows,
        };
        if self.sessions_focus == SessionsFocus::Brainstorms
            && !self.brainstorms.is_empty()
            && self.brainstorms_list_state.selected().is_none()
        {
            self.brainstorms_list_state.select(Some(0));
        }
    }

    /// Move the Brainstorms sub-list cursor. Same wrap-around idiom
    /// as `move_selection`. No-op when there are no brainstorms.
    fn move_brainstorms_selection(&mut self, delta: isize) {
        if self.brainstorms.is_empty() {
            return;
        }
        let len = isize::try_from(self.brainstorms.len()).unwrap_or(isize::MAX);
        let current =
            isize::try_from(self.brainstorms_list_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.brainstorms_list_state.select(Some(next_usize));
    }

    /// Selected brainstorm session id, if a row is selected and
    /// in-range. Used by `Enter` to surface the attach action.
    fn selected_brainstorm_id(&self) -> Option<BrainstormId> {
        self.brainstorms_list_state
            .selected()
            .and_then(|i| self.brainstorms.get(i))
            .map(|b| b.id.clone())
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

    fn handle_key_plans(&mut self, key: KeyEvent) -> Action {
        // Shift+P toggles the selected plan between Active and
        // Paused. Shift+C marks it Completed. Both target the
        // sidebar-selected plan regardless of which pane has focus —
        // they're plan-level operations.
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('P')) {
            self.toggle_selected_plan_pause();
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('C')) {
            self.complete_selected_plan();
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc | KeyCode::Char('p') => {
                self.view = View::Sessions;
                Action::None
            }
            KeyCode::Tab => {
                self.toggle_plans_focus();
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.plans_focus {
                    PlansFocus::Sidebar => self.move_plans_selection(1),
                    PlansFocus::Items => self.move_plans_item_selection(1),
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.plans_focus {
                    PlansFocus::Sidebar => self.move_plans_selection(-1),
                    PlansFocus::Items => self.move_plans_item_selection(-1),
                }
                Action::None
            }
            KeyCode::Char('u') => {
                self.unblock_selected_plan_item();
                Action::None
            }
            KeyCode::Char('r') => {
                // Reload also drops the ticket-detail cache + re-
                // fetches the bulk title map so a user editing
                // the tracker out-of-band has an explicit way to
                // force a re-read.
                self.focused_issue_cache.clear();
                self.refresh_plans();
                self.refresh_ticket_titles();
                self.refresh_focused_issue();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Tab between sidebar (whole-plan selection) and items (per-row
    /// cursor inside the selected plan). When switching to Items, if
    /// the current plan has any rows and the item cursor is unset,
    /// land on row 0; switching to an empty plan keeps the focus
    /// flag updated but the cursor stays cleared.
    fn toggle_plans_focus(&mut self) {
        self.plans_focus = match self.plans_focus {
            PlansFocus::Sidebar => PlansFocus::Items,
            PlansFocus::Items => PlansFocus::Sidebar,
        };
        if self.plans_focus == PlansFocus::Items {
            let n_items = self.selected_plan().map_or(0, |p| p.items.len());
            if n_items > 0 && self.plans_items_state.selected().is_none() {
                self.plans_items_state.select(Some(0));
            }
            // Pre-fetch the newly-focused item's ticket so the
            // ticket-detail pane has content on the next draw.
            self.refresh_focused_issue();
        }
    }

    /// Move the item cursor inside the currently-selected plan. Wraps
    /// at the ends (same idiom as `move_plans_selection`). No-op if
    /// no plan is selected or the plan has no items.
    fn move_plans_item_selection(&mut self, delta: isize) {
        let Some(plan) = self.selected_plan() else {
            return;
        };
        if plan.items.is_empty() {
            return;
        }
        let len = isize::try_from(plan.items.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.plans_items_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.plans_items_state.select(Some(next_usize));
        // Cache check is O(1); a tracker.read only fires the first
        // time a given ticket id is navigated to in this session.
        self.refresh_focused_issue();
    }

    /// Shift+P on the Plans view: flip Active↔Paused, leave anything
    /// else (Completed/Abandoned) alone with a status-line note.
    /// In-place mutation + atomic save; on save failure the
    /// in-memory copy is reverted so the TUI doesn't drift from disk.
    fn toggle_selected_plan_pause(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.status_line = " no plan selected ".to_string();
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        let target = match plan.state {
            PlanState::Active => PlanState::Paused,
            PlanState::Paused => PlanState::Active,
            PlanState::Completed | PlanState::Abandoned => {
                self.status_line = format!(
                    " plan `{}` is {} — pause/resume only applies to active plans ",
                    plan.id,
                    plan_state_word(plan.state),
                );
                return;
            }
        };
        let previous = plan.state;
        plan.state = target;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        match store.save(plan) {
            Ok(()) => {
                self.status_line = format!(" plan `{}` → {} ", plan.id, plan_state_word(target));
            }
            Err(err) => {
                plan.state = previous;
                self.status_line = format!(" plan save failed: {err:#} ");
            }
        }
    }

    /// Shift+C on the Plans view: mark the selected plan Completed.
    /// No-op on already-completed plans (with a status note); the
    /// CLI surface allows the equivalent transition unconditionally,
    /// but in the TUI a visual "already done" signal is more useful
    /// than silently re-stamping `updated_at_ms`.
    fn complete_selected_plan(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.status_line = " no plan selected ".to_string();
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        if plan.state == PlanState::Completed {
            self.status_line = format!(" plan `{}` already completed ", plan.id);
            return;
        }
        let previous = plan.state;
        plan.state = PlanState::Completed;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        match store.save(plan) {
            Ok(()) => {
                self.status_line = format!(" plan `{}` → completed ", plan.id);
            }
            Err(err) => {
                plan.state = previous;
                self.status_line = format!(" plan save failed: {err:#} ");
            }
        }
    }

    /// `u` on the Plans view: drop every deps edge keyed by the
    /// currently-selected item's ticket. Mirrors `fleet sessions
    /// unblock` semantics but bound to the item's session id when
    /// one's attached. Items without a bound session report a
    /// status-line message instead — there's nothing to unblock
    /// without knowing which ticket to clear edges for.
    fn unblock_selected_plan_item(&mut self) {
        if self.plans_focus != PlansFocus::Items {
            self.status_line = " press Tab to focus items before unblocking ".to_string();
            return;
        }
        let Some(plan) = self.selected_plan() else {
            return;
        };
        let Some(item_idx) = self.plans_items_state.selected() else {
            self.status_line = " no item selected ".to_string();
            return;
        };
        let Some(item) = plan.items.get(item_idx) else {
            return;
        };
        let ticket = item.ticket_id.clone();
        // Operate on deps directly — the session is optional for
        // unblock-by-ticket, and we don't want to surface a
        // tracker-comment prompt in the TUI flow.
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        match deps_store.remove_edges_for_blocked(&ticket) {
            Ok(n) => {
                self.status_line =
                    format!(" ticket `{ticket}`: cleared {n} dep edge{} ", plural(n));
                // Refresh cycle_nodes so the ⚠ marker can clear.
                self.refresh_cycle_nodes_only();
            }
            Err(err) => {
                self.status_line = format!(" unblock failed: {err:#} ");
            }
        }
    }

    /// Recompute deps-derived state (`cycle_nodes` + `deps_doc`)
    /// without touching the rest of the world. Pulled out so `u`
    /// can refresh the markers + ticket-pane deps rows after
    /// removing edges without re-listing every session and plan.
    fn refresh_cycle_nodes_only(&mut self) {
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        let doc = deps_store
            .load()
            .unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
    }

    /// Switch into the Plans view, reloading from disk so the listing
    /// matches the latest `.fleet/plans/<id>.yaml` state. Empty list
    /// is fine — the renderer shows a hint instead.
    ///
    /// Also bulk-fetches tracker issues so plan items + deps rows
    /// can render with titles next to each id. One tracker call
    /// per Plans-view entry — cheaper than fanning out
    /// `tracker.read` per item.
    fn open_plans_view(&mut self) {
        self.refresh_plans();
        self.refresh_ticket_titles();
        self.view = View::Plans;
    }

    /// Bulk-fetch every tracker issue into `tickets_by_id`. Called
    /// when entering Plans view or on `r` reload. Failure surfaces
    /// in the status line but doesn't clear the existing map —
    /// stale titles beat empty titles when the tracker is briefly
    /// unreachable.
    fn refresh_ticket_titles(&mut self) {
        let Some(tracker) = self.ensure_tracker() else {
            // No tracker plugin → clear so we don't keep stale
            // data from a previous tracker config.
            self.tickets_by_id.clear();
            return;
        };
        let root = self.root.clone();
        match tracker.list_issues(&root) {
            Ok(issues) => {
                self.tickets_by_id.clear();
                for issue in issues {
                    self.tickets_by_id.insert(issue.human_id.clone(), issue);
                }
            }
            Err(err) => {
                self.status_line = format!(" tracker list failed: {err:#} ");
                // Leave stale map alone — better than blanking.
            }
        }
    }

    /// Reload plans from disk and restore the previous selection by
    /// id (or default to the first plan when the previous one was
    /// deleted). Errors from a corrupt plan file surface in the
    /// status line — we don't want one bad file to break the
    /// listing.
    fn refresh_plans(&mut self) {
        let store = PlanStore::for_repo(&self.root);
        let prev_id = self
            .plans_list_state
            .selected()
            .and_then(|i| self.plans.get(i))
            .map(|p| p.id.clone());
        let ids = match store.list() {
            Ok(ids) => ids,
            Err(err) => {
                self.status_line = format!(" plans: list failed: {err:#} ");
                self.plans.clear();
                self.plans_list_state.select(None);
                return;
            }
        };
        let mut plans = Vec::with_capacity(ids.len());
        for id in ids {
            match store.load(&id) {
                Ok(p) => plans.push(p),
                Err(err) => {
                    self.status_line = format!(" plans: load `{id}` failed: {err:#} ");
                }
            }
        }
        self.plans = plans;
        let new_index = prev_id
            .and_then(|id| self.plans.iter().position(|p| p.id == id))
            .or(if self.plans.is_empty() { None } else { Some(0) });
        self.plans_list_state.select(new_index);
    }

    fn move_plans_selection(&mut self, delta: isize) {
        if self.plans.is_empty() {
            return;
        }
        let len = isize::try_from(self.plans.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.plans_list_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.plans_list_state.select(Some(next_usize));
        // Drop the item cursor: it indexed into the previous plan's
        // items, and the new plan likely has a different length.
        // The next Tab→Items will reseat it at row 0.
        self.plans_items_state.select(None);
    }

    fn selected_plan(&self) -> Option<&Plan> {
        self.plans_list_state
            .selected()
            .and_then(|i| self.plans.get(i))
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
        let current = isize::try_from(self.spawn_list_state.selected().unwrap_or(0)).unwrap_or(0);
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
                " spawn failed: cannot resolve fleet binary path (current_exe) ".to_string();
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
        if !self.autonomous.enabled() && self.ensure_tracker().is_none() {
            self.autonomous.set_status(format!(
                "autonomous: cannot enable — tracker `{}` is not implemented yet",
                self.config.tracker.as_str(),
            ));
            return;
        }
        self.autonomous.toggle();
    }

    /// Build the configured tracker on first use and cache it.
    /// Returns `None` when the tracker plugin isn't implemented
    /// (Linear / Jira) — callers surface a user-facing message.
    ///
    /// Lazy so users who never enter the Plans view or toggle
    /// autonomous mode aren't blocked by tracker setup at TUI
    /// startup (a `gh auth` failure on every launch would be
    /// hostile).
    fn ensure_tracker(&mut self) -> Option<Arc<dyn crate::tracker::Tracker>> {
        if matches!(self.tracker, TrackerState::Pending) {
            let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
            self.tracker = crate::tracker::build(self.config.tracker, invoker)
                .map_or(TrackerState::Unsupported, |boxed| {
                    TrackerState::Built(Arc::from(boxed))
                });
        }
        match &self.tracker {
            TrackerState::Built(t) => Some(Arc::clone(t)),
            TrackerState::Unsupported | TrackerState::Pending => None,
        }
    }

    /// Fetch the focused plan item's tracker issue (if any) into
    /// `focused_issue_cache`. Skips if no item is focused, if the
    /// ticket is already cached (success or failure), or if the
    /// tracker plugin isn't implemented. Cache failures use the
    /// formatted error string so the render path can show the
    /// reason without re-hitting the tracker.
    fn refresh_focused_issue(&mut self) {
        let Some(plan_idx) = self.plans_list_state.selected() else {
            return;
        };
        let Some(plan) = self.plans.get(plan_idx) else {
            return;
        };
        let Some(item_idx) = self.plans_items_state.selected() else {
            return;
        };
        let Some(item) = plan.items.get(item_idx) else {
            return;
        };
        let ticket_id = item.ticket_id.clone();
        if self.focused_issue_cache.contains_key(&ticket_id) {
            return;
        }
        let Some(tracker) = self.ensure_tracker() else {
            self.focused_issue_cache.insert(
                ticket_id,
                Err(format!(
                    "no tracker configured (`tracker: {}` in .fleet/config.yaml is not implemented yet)",
                    self.config.tracker.as_str(),
                )),
            );
            return;
        };
        let root = self.root.clone();
        let result = tracker
            .read(&root, &ticket_id)
            .map_err(|err| format!("{err:#}"));
        self.focused_issue_cache.insert(ticket_id, result);
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
            let issues = tracker.list_issues(&root).map_err(|e| format!("{e:#}"))?;
            let open: Vec<crate::session::IssueContext> = issues
                .into_iter()
                .filter(|i| i.status == "open")
                .map(|i| crate::session::IssueContext {
                    id: i.id,
                    human_id: i.human_id,
                    title: i.title,
                    labels: i.labels,
                })
                .collect();
            // Reorder so active-plan items come first; drop deps-
            // blocked tickets. Matches the CLI tick exactly so the
            // TUI's `Shift+A` mode behaves identically to a
            // scripted `fleet autonomous run --once`. Plan/deps
            // load failures collapse to empty so a corrupt local
            // file falls back to the pre-plans behaviour.
            let plans = crate::plans::store::PlanStore::for_repo(&root)
                .list_active()
                .unwrap_or_default();
            let deps = crate::deps::DepsStore::for_repo(&root)
                .load()
                .unwrap_or_default();
            Ok(crate::autonomous::rank_candidates_by_plan(
                open, &plans, &deps,
            ))
        };
        let outcome = self
            .autonomous
            .step(now, &self.config.autonomous, &self.sessions, list_open);
        if let autonomous::AutonomousOutcome::Spawn(cmd) = outcome {
            self.dispatch_autonomous_spawn(&cmd, store);
        }
    }

    fn dispatch_autonomous_spawn(&mut self, cmd: &autonomous::SpawnCommand, store: &SessionStore) {
        let Ok(binary) = std::env::current_exe() else {
            self.autonomous.set_status(
                "autonomous: ON · spawn failed: cannot resolve fleet binary (current_exe)",
            );
            return;
        };
        let mut child =
            build_workflow_run_command(&binary, &cmd.workflow, Some(&cmd.issue.human_id));
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
            // Already-terminal isn't an error per se — the user might
            // have hit `Shift+K` on a Completed session by mistake.
            // Keep the brief status-line note rather than escalating
            // to a modal.
            self.status_line = format!(" {} already {:?} ", session.id, session.state);
            return;
        }
        if let Err(err) = session.transition_to(SessionState::Failed, now_ms()) {
            // Hard failures (state-machine rejection, disk write) get
            // an overlay so they're impossible to miss.
            self.overlay = Overlay::Error {
                message: format!("kill rejected: {err:#}"),
            };
            return;
        }
        if let Err(err) = store.save(session) {
            self.overlay = Overlay::Error {
                message: format!("kill save failed: {err:#}"),
            };
            return;
        }
        self.status_line = format!(" {} → failed ", session.id);
    }
}

/// Closed enum so the event loop's dispatch stays exhaustive.
enum Action {
    None,
    Quit,
    /// Suspend the alternate screen and `exec fleet brainstorm` so
    /// the user lands inside a fresh brainstorm tmux pane. On
    /// detach the event loop re-enters the alternate screen and
    /// resumes. Triggered by `Shift+B` from any view.
    NewBrainstorm,
    /// Suspend the alternate screen and `tmux attach-session` to
    /// the brainstorm with this id. Triggered by `Enter` on a
    /// Brainstorms-focused sidebar row.
    AttachBrainstorm(BrainstormId),
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
/// when at least one session has cost data, a lifetime total, and
/// when any plan is in `Active` state, an "N active plan(s)"
/// segment so the user sees scheduling pressure at a glance. Free
/// function so the wording is asserted in tests.
#[must_use]
pub fn render_status_line(sessions: &[Session], plans: &[Plan]) -> String {
    let (total, samples) = lifetime_cost(sessions);
    let active_plans = plans
        .iter()
        .filter(|p| p.state == PlanState::Active)
        .count();
    let mut out = format!(" {} sessions", sessions.len());
    if samples > 0 {
        use std::fmt::Write as _;
        let _ = write!(out, " · ${total:.2} total ({samples} with cost)");
    }
    if active_plans > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            " · {active_plans} active plan{}",
            if active_plans == 1 { "" } else { "s" }
        );
    }
    out.push(' ');
    out
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
    // Spawn is an *overlay* on top of the sessions view: the user
    // hasn't left their place, they've just popped a picker. Render
    // sessions underneath, then `Clear` the modal area and draw the
    // picker on top. Doctor remains a full-pane mode because the
    // user explicitly switched contexts to inspect host state.
    match state.view {
        View::Sessions | View::Spawn => {
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
        View::Plans => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(40),
                    Constraint::Percentage(35),
                ])
                .split(outer[0]);
            render_plans_sidebar(f, body[0], state);
            render_plans_detail(f, body[1], state);
            render_plans_ticket(f, body[2], state);
        }
    }
    if state.view == View::Spawn {
        let modal = centered_rect(outer[0], 60, 60);
        f.render_widget(Clear, modal);
        render_spawn(f, modal, state);
    }
    // Transient overlay (confirm / error) — drawn last so it sits on
    // top of everything else, including the spawn picker.
    match &state.overlay {
        Overlay::Confirm { prompt, .. } => {
            let modal = centered_rect(outer[0], 50, 30);
            f.render_widget(Clear, modal);
            render_confirm(f, modal, prompt);
        }
        Overlay::Error { message } => {
            let modal = centered_rect(outer[0], 50, 25);
            f.render_widget(Clear, modal);
            render_error(f, modal, message);
        }
        Overlay::None => {}
    }
    render_status(f, outer[1], state);
}

fn render_confirm(f: &mut Frame<'_>, area: Rect, prompt: &str) {
    let mut lines: Vec<Line<'static>> = prompt.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y] yes    [n / Esc] cancel",
        Style::default().fg(Color::Yellow),
    )));
    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

fn render_error(f: &mut Frame<'_>, area: Rect, message: &str) {
    let mut lines: Vec<Line<'static>> =
        message.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key to dismiss",
        Style::default().fg(Color::DarkGray),
    )));
    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Error ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

/// Centred sub-rectangle of `parent`, sized to `pct_x`% wide and
/// `pct_y`% tall (each clamped to `[10, 95]` so the modal is always
/// readable on tiny terminals and never the entire pane). Used by the
/// spawn-picker overlay; future modals (confirm dialogs, error
/// surfaces) can share it.
#[must_use]
pub fn centered_rect(parent: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let pct_x = pct_x.clamp(10, 95);
    let pct_y = pct_y.clamp(10, 95);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(parent);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
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

    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
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

fn render_plans_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = format!(" Plans ({}) ", state.plans.len());
    let block = Block::default().title(title).borders(Borders::ALL);
    if state.plans.is_empty() {
        let body = Paragraph::new(
            "(no plans yet)\n\nUse `fleet plan new \"<name>\" --tickets a,b,c` to create one.",
        )
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem<'_>> = state
        .plans
        .iter()
        .map(|p| {
            ListItem::new(Span::styled(
                plan_row_label(p, &state.cycle_nodes),
                plan_row_style(p),
            ))
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
    let mut list_state = state.plans_list_state;
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_plans_detail(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(plan) = state.selected_plan() else {
        let body = Paragraph::new("Select a plan — j/k or arrows. r refresh, Esc/p back, q quit.")
            .block(
                Block::default()
                    .title(" Plan detail ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    };
    // Item-row cursor is only meaningful when Items has focus; in
    // Sidebar focus we render without the `▸` marker so the user
    // sees the "whole plan" framing.
    let selected_item = if state.plans_focus == PlansFocus::Items {
        state.plans_items_state.selected()
    } else {
        None
    };
    let title = if state.plans_focus == PlansFocus::Items {
        format!(" {} · items ", plan.id)
    } else {
        format!(" {} ", plan.id)
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let body = Paragraph::new(plan_detail_lines(plan, selected_item, &state.tickets_by_id))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

/// Render the third Plans-view pane: the focused item's tracker
/// issue. When the items pane has no cursor (sidebar focus, no
/// plan selected, or the plan is empty) the pane shows a hint
/// instead of fetched data — the focus model is the user's signal
/// that they want detail.
fn render_plans_ticket(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().title(" Ticket ").borders(Borders::ALL);
    let lines = ticket_detail_lines(state);
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

/// Pure: build the lines for the ticket-detail pane against the
/// current `state`. Factored out so tests can assert on the
/// rendered surface without a ratatui Frame.
///
/// Five cases the pane covers:
/// - Sidebar focus → "Tab to navigate items" hint.
/// - Items focus + plan empty → "(plan has no items)".
/// - Items focus + no cache entry → "(fetching…)" (cache fills
///   on the *next* navigation; this happens for one render cycle
///   between a fresh switch and the post-toggle fetch).
/// - Items focus + cached error → the error message verbatim.
/// - Items focus + cached `IssueDetail` → title, status, labels,
///   comment count, then the body verbatim. Body is rendered with
///   wrap; ratatui handles overflow.
#[must_use]
fn ticket_detail_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.plans_focus == PlansFocus::Sidebar {
        return vec![Line::from(Span::styled(
            "Tab to focus items, then j/k to navigate. Selected item's tracker issue will appear here.",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let Some(plan) = state.selected_plan() else {
        return vec![Line::from(Span::styled(
            "(no plan selected)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let Some(item_idx) = state.plans_items_state.selected() else {
        return vec![Line::from(Span::styled(
            "(plan has no items)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let Some(item) = plan.items.get(item_idx) else {
        return vec![Line::from(Span::styled(
            "(item index out of range)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let ticket_id = &item.ticket_id;
    let mut lines = match state.focused_issue_cache.get(ticket_id) {
        None => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled(
                "(fetching…)",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        Some(Err(msg)) => vec![
            kv_line("ticket", ticket_id),
            Line::from(""),
            Line::from(Span::styled(
                format!("tracker read failed: {msg}"),
                Style::default().fg(Color::Red),
            )),
        ],
        Some(Ok(detail)) => issue_detail_lines(detail),
    };
    // Append the deps section regardless of whether the tracker
    // fetch succeeded — deps live in `.fleet/deps.json` (cached
    // on AppState), so a tracker outage shouldn't hide them.
    append_deps_lines(
        &mut lines,
        ticket_id,
        &state.deps_doc,
        &state.cycle_nodes,
        &state.tickets_by_id,
    );
    lines
}

/// Append "Blocked on:" / "Blocks:" / cycle-warning rows for the
/// given ticket, derived from the cached deps document. No-op
/// when the ticket has no edges on either side and isn't in a
/// cycle. The two sides are listed separately so it's clear at a
/// glance which way the arrow points: *blocked on* X means we're
/// waiting for X; *blocks* Y means Y is waiting for us.
///
/// `titles` is consulted to suffix each ticket-id row with the
/// issue's title (skipped for `free:<slug>` freeform tags, which
/// aren't ticket ids). Pass an empty map to suppress suffixes
/// (tests).
fn append_deps_lines(
    lines: &mut Vec<Line<'static>>,
    ticket_id: &str,
    deps: &crate::deps::DepsDoc,
    cycle_nodes: &std::collections::HashSet<String>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) {
    let blocked_on: Vec<&crate::deps::DepEdge> = deps
        .edges
        .iter()
        .filter(|e| e.blocked == ticket_id)
        .collect();
    let blocks: Vec<&crate::deps::DepEdge> = deps
        .edges
        .iter()
        .filter(|e| e.blocked_on == ticket_id)
        .collect();
    let in_cycle = cycle_nodes.contains(ticket_id);

    if blocked_on.is_empty() && blocks.is_empty() && !in_cycle {
        return;
    }

    lines.push(Line::from(""));
    if !blocked_on.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Blocked on ({}):", blocked_on.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for edge in &blocked_on {
            let kind = match edge.reason {
                crate::deps::BlockedReason::Ticket => "ticket",
                crate::deps::BlockedReason::Freeform => "freeform",
            };
            let title_suffix = format_title_suffix(&edge.blocked_on, edge.reason, titles);
            lines.push(Line::from(format!(
                "  • {}{title_suffix}  [{kind}]",
                edge.blocked_on
            )));
        }
    }
    if !blocks.is_empty() {
        if !blocked_on.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Blocks ({}):", blocks.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for edge in &blocks {
            // Left-hand side is always a ticket id (only the
            // right side carries the `free:` prefix for freeform
            // edges), so look up the title unconditionally.
            let title_suffix = format_title_suffix(&edge.blocked, crate::deps::BlockedReason::Ticket, titles);
            lines.push(Line::from(format!("  • {}{title_suffix}", edge.blocked)));
        }
    }
    if in_cycle {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "⚠  Part of a deps cycle — supervisor can't auto-resolve.",
            Style::default().fg(Color::Yellow),
        )));
    }
}

/// `" — <title>"` when `id` is a ticket reference present in
/// `titles`, empty string otherwise. Skips lookups for freeform
/// tags (`free:<slug>`) — those aren't ticket ids and have no
/// title to fetch.
#[must_use]
fn format_title_suffix(
    id: &str,
    reason: crate::deps::BlockedReason,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> String {
    if matches!(reason, crate::deps::BlockedReason::Freeform) {
        return String::new();
    }
    titles
        .get(id)
        .map(|i| format!(" — {}", i.title))
        .unwrap_or_default()
}

/// Pure: turn an `IssueDetail` into render lines. Pulled out
/// from `ticket_detail_lines` so tests can drive it directly
/// against fixture data without seeding `AppState`.
#[must_use]
fn issue_detail_lines(detail: &crate::tracker::IssueDetail) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv_line("id", &detail.issue.human_id),
        kv_line("title", &detail.issue.title),
        kv_line("status", &detail.issue.status),
    ];
    if !detail.issue.labels.is_empty() {
        lines.push(kv_line(
            "labels",
            &format!("[{}]", detail.issue.labels.join(", ")),
        ));
    }
    let comment_count = detail.comments.len();
    lines.push(kv_line(
        "comments",
        &comment_count.to_string(),
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Body:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if detail.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no body)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Preserve the body's own line breaks; ratatui Wrap will
        // handle horizontal overflow per line.
        for line in detail.body.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

/// One-line label for the plans sidebar list. Pure: pulled out for
/// testability since the ratatui frame-based renderers don't compose
/// cleanly with assert macros.
///
/// `cycle_nodes` is consulted to add a leading `⚠ ` glyph when any
/// of the plan's items is bound to a ticket sitting on a cycle.
/// Pass an empty set to skip the warning (tests that don't care
/// about deps state).
#[must_use]
fn plan_row_label(plan: &Plan, cycle_nodes: &std::collections::HashSet<String>) -> String {
    let (done, total) = plan.progress();
    let warning_prefix = if plan_in_cycle(plan, cycle_nodes) {
        "⚠ "
    } else {
        ""
    };
    format!(
        "{warning_prefix}{} {} {done}/{total} {}",
        plan_state_marker(plan.state),
        plan.id,
        plan.name,
    )
}

/// Color the plan row according to its state — active plans pop in
/// the default style, paused/completed/abandoned recede in darker
/// shades so the sidebar's "what should I look at" line of sight
/// stays on Active.
#[must_use]
fn plan_row_style(plan: &Plan) -> Style {
    match plan.state {
        PlanState::Active => Style::default().fg(Color::Green),
        PlanState::Paused => Style::default().fg(Color::Yellow),
        PlanState::Completed | PlanState::Abandoned => Style::default().fg(Color::DarkGray),
    }
}

/// One-letter prefix for the plan-row label, mirroring the
/// state-marker idiom the sessions list uses.
#[must_use]
fn plan_state_marker(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "●",
        PlanState::Paused => "⏸",
        PlanState::Completed => "✓",
        PlanState::Abandoned => "✗",
    }
}

/// Pure: render the detail-pane lines for a plan. Header rows
/// followed by one row per plan item, each carrying state + ticket
/// id + an `(injected)` marker for tracker-create-added items.
///
/// `selected_item` highlights one item row with a `▸` prefix —
/// pass `None` from the sidebar-focus rendering (or from tests
/// that don't care about the cursor).
///
/// `titles` is the bulk-fetched ticket-id → issue map (see
/// `refresh_ticket_titles`). When an item's `ticket_id` is in
/// the map, the row is suffixed with `— <title>` so the user
/// doesn't have to memorise tracker ids. Pass an empty map to
/// skip the suffix (tests).
#[must_use]
fn plan_detail_lines(
    plan: &Plan,
    selected_item: Option<usize>,
    titles: &std::collections::HashMap<String, crate::tracker::Issue>,
) -> Vec<Line<'static>> {
    let (done, total) = plan.progress();
    let mut out = vec![
        kv_line("name", &plan.name),
        kv_line("state", plan_state_word(plan.state)),
        kv_line("progress", &format!("{done}/{total}")),
        kv_line("policy", plan_failure_word(plan.on_item_failure)),
    ];
    if let Some(epic) = &plan.epic_ref {
        out.push(kv_line("epic", &format!("{}:{}", epic.tracker, epic.id)));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        format!("Items ({total})"),
        Style::default().fg(Color::DarkGray),
    )));
    use std::fmt::Write as _;
    for (idx, item) in plan.items.iter().enumerate() {
        let cursor = if selected_item == Some(idx) {
            "▸ "
        } else {
            "  "
        };
        let mut row = format!(
            "{cursor}{idx:>3}. {marker} {state:<11} {ticket}",
            marker = plan_item_marker(item.state),
            state = plan_item_word(item.state),
            ticket = item.ticket_id,
        );
        if let Some(title) = titles.get(&item.ticket_id).map(|i| i.title.as_str()) {
            let _ = write!(row, " — {title}");
        }
        if item.injected {
            row.push_str("  (injected)");
        }
        if let Some(session) = &item.session_id {
            let _ = write!(row, "  · session {session}");
        }
        out.push(Line::from(Span::styled(row, plan_item_style(item.state))));
    }
    out
}

#[must_use]
fn plan_state_word(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "active",
        PlanState::Paused => "paused",
        PlanState::Completed => "completed",
        PlanState::Abandoned => "abandoned",
    }
}

/// "" for `n == 1`, "s" otherwise. Used to keep status-line counts
/// grammatical ("cleared 1 dep edge" / "cleared 2 dep edges") without
/// pulling in a heavier i18n crate.
#[must_use]
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[must_use]
fn plan_item_word(state: PlanItemState) -> &'static str {
    match state {
        PlanItemState::Pending => "pending",
        PlanItemState::InProgress => "in-progress",
        PlanItemState::Completed => "completed",
        PlanItemState::Skipped => "skipped",
        PlanItemState::Failed => "failed",
    }
}

#[must_use]
fn plan_item_marker(state: PlanItemState) -> &'static str {
    match state {
        PlanItemState::Pending => "○",
        PlanItemState::InProgress => "◐",
        PlanItemState::Completed => "✓",
        PlanItemState::Skipped => "⊝",
        PlanItemState::Failed => "✗",
    }
}

#[must_use]
fn plan_item_style(state: PlanItemState) -> Style {
    match state {
        PlanItemState::Pending => Style::default(),
        PlanItemState::InProgress => Style::default().fg(Color::Cyan),
        PlanItemState::Completed | PlanItemState::Skipped => Style::default().fg(Color::DarkGray),
        PlanItemState::Failed => Style::default().fg(Color::Red),
    }
}

#[must_use]
fn plan_failure_word(policy: crate::plans::ItemFailurePolicy) -> &'static str {
    match policy {
        crate::plans::ItemFailurePolicy::Stop => "stop",
        crate::plans::ItemFailurePolicy::Continue => "continue",
        crate::plans::ItemFailurePolicy::RetryOnce => "retry-once",
    }
}

fn render_sidebar(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = if state.brainstorms.is_empty() {
        format!(" Sessions ({}) ", state.sessions.len())
    } else {
        format!(
            " Sessions ({}) · Brainstorms ({}) ",
            state.sessions.len(),
            state.brainstorms.len(),
        )
    };
    let outer = Block::default().title(title).borders(Borders::ALL);
    if state.sessions.is_empty() && state.brainstorms.is_empty() {
        let body = Paragraph::new(
            "(no sessions)\n\n\
             Use `fleet workflow run <name>` from the shell to create a workflow session, \
             or `fleet brainstorm` for an interactive planning session.",
        )
        .block(outer)
        .wrap(Wrap { trim: false });
        f.render_widget(body, area);
        return;
    }
    // Render the outer titled border, then split the inner area
    // into Workflows / Brainstorms regions. When one of the two
    // sections is empty, we skip its allocation and the other
    // takes the full inner space.
    f.render_widget(outer.clone(), area);
    let inner = outer.inner(area);
    let (workflow_area, brainstorm_area) = split_sidebar_areas(
        inner,
        state.sessions.len(),
        state.brainstorms.len(),
    );
    if let Some(area) = workflow_area {
        render_workflow_list(f, area, state);
    }
    if let Some(area) = brainstorm_area {
        render_brainstorm_list(f, area, state);
    }
}

/// Split the sidebar's inner area into Workflows / Brainstorms
/// sub-regions. Either side gets `None` when the corresponding list
/// is empty (and the other takes the full area). When both have
/// content, the split is proportional to the row counts plus the
/// one-line section header — that way a 10-workflow / 1-brainstorm
/// repo doesn't waste half the sidebar on a single brainstorm row.
#[must_use]
fn split_sidebar_areas(
    inner: Rect,
    n_workflows: usize,
    n_brainstorms: usize,
) -> (Option<Rect>, Option<Rect>) {
    if n_workflows == 0 && n_brainstorms == 0 {
        return (None, None);
    }
    if n_brainstorms == 0 {
        return (Some(inner), None);
    }
    if n_workflows == 0 {
        return (None, Some(inner));
    }
    // +1 in each numerator: one line for the section header. The
    // u32 cap below clamps the very-unlikely case of a repo with
    // millions of either kind of session — the ratio will be
    // wildly skewed long before we hit the cast limit.
    let workflow_share = u32::try_from(n_workflows + 1).unwrap_or(u32::MAX);
    let brainstorm_share = u32::try_from(n_brainstorms + 1).unwrap_or(u32::MAX);
    let total = workflow_share.saturating_add(brainstorm_share);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Ratio(workflow_share, total),
            Constraint::Ratio(brainstorm_share, total),
        ])
        .split(inner);
    (Some(chunks[0]), Some(chunks[1]))
}

fn render_workflow_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut items: Vec<ListItem<'_>> = Vec::with_capacity(state.sessions.len() + 1);
    items.push(ListItem::new(Span::styled(
        "── Workflows ──".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    for s in &state.sessions {
        items.push(ListItem::new(Span::styled(
            session_row_label(s, &state.plans, &state.cycle_nodes),
            state_style(s.state),
        )));
    }
    let highlight = if state.sessions_focus == SessionsFocus::Workflows {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("> ");
    // The list_state index is 0-based on the workflow rows, but the
    // rendered list has a header at row 0. Shift by one so the
    // highlight lands on the right row.
    let mut shifted = state.list_state;
    if let Some(i) = shifted.selected() {
        shifted.select(Some(i + 1));
    }
    f.render_stateful_widget(list, area, &mut shifted);
}

fn render_brainstorm_list(f: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut items: Vec<ListItem<'_>> = Vec::with_capacity(state.brainstorms.len() + 1);
    items.push(ListItem::new(Span::styled(
        "── Brainstorms ──".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    for b in &state.brainstorms {
        items.push(ListItem::new(Span::styled(
            brainstorm_row_label(b),
            brainstorm_row_style(b.state),
        )));
    }
    let highlight = if state.sessions_focus == SessionsFocus::Brainstorms {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("> ");
    let mut shifted = state.brainstorms_list_state;
    if let Some(i) = shifted.selected() {
        shifted.select(Some(i + 1));
    }
    f.render_stateful_widget(list, area, &mut shifted);
}

/// One-line label for a brainstorm row in the sidebar. Pure helper
/// so tests can assert on the exact text without a ratatui Frame.
#[must_use]
pub fn brainstorm_row_label(session: &BrainstormSession) -> String {
    format!(
        "{} {}  agent={}  ({})",
        brainstorm_state_marker(session.state),
        session.id,
        session.agent,
        brainstorm_state_word(session.state),
    )
}

#[must_use]
fn brainstorm_row_style(state: BrainstormState) -> Style {
    match state {
        BrainstormState::Active => Style::default().fg(Color::Cyan),
        BrainstormState::Detached => Style::default().fg(Color::Yellow),
        BrainstormState::Closed => Style::default().fg(Color::DarkGray),
    }
}

#[must_use]
fn brainstorm_state_marker(state: BrainstormState) -> &'static str {
    match state {
        BrainstormState::Active => "◐",
        BrainstormState::Detached => "⏸",
        BrainstormState::Closed => "✗",
    }
}

#[must_use]
fn brainstorm_state_word(state: BrainstormState) -> &'static str {
    match state {
        BrainstormState::Active => "active",
        BrainstormState::Detached => "detached",
        BrainstormState::Closed => "closed",
    }
}

/// Pure: the per-session sidebar label, with cost suffix when
/// present and a plan annotation when the session's ticket is in
/// an active plan. Factored out so tests can assert on the exact
/// surface without a ratatui Frame.
///
/// `cycle_nodes` is the set of ticket ids currently part of a
/// cycle in the deps graph — when the session's bound ticket is
/// in that set, the label gets a leading `⚠ ` so the user
/// notices. Pass an empty set to skip the warning entirely (e.g.
/// from tests that don't care about deps state).
///
/// The plan lookup walks every active plan once — fine for typical
/// fleet workloads (a handful of plans, dozens of items each). If
/// that grows, swap to a precomputed `ticket_id → plan` map kept
/// alongside `AppState.plans`.
#[must_use]
pub fn session_row_label(
    session: &Session,
    plans: &[Plan],
    cycle_nodes: &std::collections::HashSet<String>,
) -> String {
    let cost_suffix = session
        .total_cost_usd()
        .map_or_else(String::new, |v| format!(" ${v:.2}"));
    let warning_prefix = if session_in_cycle(session, cycle_nodes) {
        "⚠ "
    } else {
        ""
    };
    let mut label = format!(
        "{warning_prefix}{} {} {}{cost_suffix}",
        state_marker(session.state),
        session.id,
        session.workflow,
    );
    if let Some(annotation) = plan_annotation_for(session, plans) {
        use std::fmt::Write as _;
        let _ = write!(label, " · {annotation}");
    }
    label
}

/// `true` when the session is bound to a ticket that's part of a
/// cycle in the deps graph. Pulled out so the row-label helper and
/// the tests share the same definition.
#[must_use]
fn session_in_cycle(session: &Session, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    session
        .issue
        .as_ref()
        .is_some_and(|i| cycle_nodes.contains(&i.human_id))
}

/// `true` when any item in the plan is bound to a ticket sitting on
/// a cycle in the deps graph. Used to flag plans whose forward
/// progress depends on a tangle the supervisor can't auto-resolve.
#[must_use]
fn plan_in_cycle(plan: &Plan, cycle_nodes: &std::collections::HashSet<String>) -> bool {
    plan.items
        .iter()
        .any(|item| cycle_nodes.contains(&item.ticket_id))
}

/// `Some("<plan-name> (n/N)")` when the session's bound ticket is
/// an item in an active plan, `None` otherwise. Skips non-active
/// plans for the same reason the tracker-create injector does — a
/// paused/completed/abandoned plan shouldn't claim ownership of a
/// session row visually.
#[must_use]
fn plan_annotation_for(session: &Session, plans: &[Plan]) -> Option<String> {
    let ticket = &session.issue.as_ref()?.human_id;
    let active_plan = plans
        .iter()
        .find(|p| p.state == PlanState::Active && p.position_of(ticket).is_some())?;
    let idx = active_plan.position_of(ticket)?;
    Some(format!(
        "{} ({}/{})",
        active_plan.name,
        idx + 1,
        active_plan.items.len()
    ))
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

    let mut lines: Vec<Line<'static>> = detail_kv_pairs(session)
        .into_iter()
        .map(|(k, v)| kv_line(k, &v))
        .collect();
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
            "[q] quit  [Tab] focus  [j/k] nav  [Enter] attach  [r] reload  [d] doctor  [p] plans  [Shift+K] kill  [n] spawn  [Shift+A] auto  [Shift+B] brainstorm"
        }
        View::Doctor => "[q] quit  [Esc/d] back  [r] re-probe",
        View::Spawn => "[Esc/q] cancel  [j/k] nav  [Enter] spawn",
        View::Plans => {
            "[q] quit  [Esc/p] back  [Tab] focus  [j/k] nav  [Shift+P] pause/resume  [Shift+C] complete  [u] unblock  [r] reload"
        }
    };
    // Autonomous status takes precedence when the engine is doing
    // something interesting (enabled, or has an override message set).
    // Otherwise show the existing free-form `status_line`.
    let tail = if state.autonomous.enabled() || state.autonomous.status() != "autonomous: OFF" {
        state.autonomous.status().to_string()
    } else {
        state.status_line.clone()
    };
    let bar = format!("{help} — {tail}");
    let p = Paragraph::new(bar).style(Style::default().bg(Color::Black).fg(Color::Gray));
    f.render_widget(p, area);
}

fn kv_line(key: &str, value: &str) -> Line<'static> {
    let key_owned = format!("{key:<9} ");
    Line::from(vec![
        Span::styled(key_owned, Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

/// Pure helper: build the (label, value) pairs the detail view shows
/// at the top, in display order. Factored out for testability — the
/// ratatui `Frame`-based renderer is awkward to assert on directly.
///
/// `worktree` and `branch` rows are only emitted when the session
/// has them populated, so non-git sessions (and pre-worktree
/// meta.json) keep the previous compact display.
#[must_use]
pub fn detail_kv_pairs(session: &Session) -> Vec<(&'static str, String)> {
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("workflow", session.workflow.clone()),
        ("state", state_word(session.state).to_string()),
        (
            "node",
            session
                .current_node
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ),
    ];
    if let Some(path) = session.worktree_path.as_deref() {
        pairs.push(("worktree", path.display().to_string()));
    }
    if let Some(branch) = session.branch.as_deref() {
        pairs.push(("branch", branch.to_string()));
    }
    pairs.push(("updated", format!("{} ms (epoch)", session.updated_at_ms)));
    pairs.push(("cost", format_cost(session.total_cost_usd())));
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    /// Convenience: an empty cycle-node set for tests that don't
    /// care about the deps graph. Saves repeating the type-spelled
    /// `HashSet::new()` at every call site.
    fn no_cycles() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    /// Sibling of `no_cycles`: empty title map for tests that
    /// don't care about title suffixes in plan / deps rendering.
    fn no_titles() -> std::collections::HashMap<String, crate::tracker::Issue> {
        std::collections::HashMap::new()
    }

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
    fn detail_kv_pairs_omits_worktree_rows_when_unset() {
        // Non-git sessions never get worktree_path/branch populated;
        // the detail view stays compact, matching pre-worktree
        // behaviour exactly.
        let s = session("s-1", "standard", SessionState::Running, 1);
        let pairs = detail_kv_pairs(&s);
        let labels: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert!(!labels.contains(&"worktree"));
        assert!(!labels.contains(&"branch"));
    }

    #[test]
    fn detail_kv_pairs_includes_worktree_rows_when_set() {
        let mut s = session("s-1", "standard", SessionState::Running, 1);
        s.set_worktree(
            std::path::PathBuf::from("/repo/.fleet/sessions/s-1/worktree"),
            "fleet/session-s-1",
            2,
        );
        let pairs = detail_kv_pairs(&s);
        let by_key: std::collections::BTreeMap<&str, &str> =
            pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(
            by_key.get("worktree").copied(),
            Some("/repo/.fleet/sessions/s-1/worktree"),
        );
        assert_eq!(by_key.get("branch").copied(), Some("fleet/session-s-1"));
        // Order: worktree + branch slot between `node` and `updated`,
        // so the user sees them right after the per-run context.
        let labels: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        let wt_idx = labels.iter().position(|k| *k == "worktree").unwrap();
        let br_idx = labels.iter().position(|k| *k == "branch").unwrap();
        let up_idx = labels.iter().position(|k| *k == "updated").unwrap();
        assert!(wt_idx < br_idx);
        assert!(br_idx < up_idx);
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
        assert_eq!(render_status_line(&sessions, &[]), " 2 sessions ");
    }

    #[test]
    fn render_status_line_with_cost_data_shows_total() {
        let mut s = session("s-1", "wf", SessionState::Completed, 1);
        s.record_node_cost("plan", 0.42, 2);
        assert_eq!(
            render_status_line(&[s], &[]),
            " 1 sessions · $0.42 total (1 with cost) "
        );
    }

    #[test]
    fn render_status_line_shows_active_plan_count() {
        let s = session("s-1", "wf", SessionState::Running, 1);
        let plans = vec![sample_plan()]; // 1 active by default
        let line = render_status_line(&[s], &plans);
        assert!(line.contains("1 active plan"), "got: {line}");
        // No trailing 's' for the singular case.
        assert!(!line.contains("1 active plans"), "got: {line}");
    }

    #[test]
    fn render_status_line_pluralises_when_more_than_one_active_plan() {
        let s = session("s-1", "wf", SessionState::Running, 1);
        let mut p2 = sample_plan();
        p2.id = PlanId::new("plan-2");
        let plans = vec![sample_plan(), p2];
        let line = render_status_line(&[s], &plans);
        assert!(line.contains("2 active plans"), "got: {line}");
    }

    #[test]
    fn render_status_line_omits_plan_segment_when_no_active_plans() {
        // Paused / completed / abandoned shouldn't be counted.
        let s = session("s-1", "wf", SessionState::Running, 1);
        let mut paused = sample_plan();
        paused.state = PlanState::Paused;
        let mut done = sample_plan();
        done.id = PlanId::new("plan-2");
        done.state = PlanState::Completed;
        let line = render_status_line(&[s], &[paused, done]);
        assert!(!line.contains("active plan"), "got: {line}");
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
        assert_eq!(
            unique.len(),
            glyphs.len(),
            "glyphs must be distinct: {glyphs:?}"
        );
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
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
            &store,
        );
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn handle_key_esc_returns_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        let action = state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &store);
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn handle_key_shift_k_opens_confirm_overlay_without_immediate_kill() {
        // Shift+K is now a two-step affordance: the first press opens
        // a confirm overlay; the session is unchanged until the user
        // explicitly presses `y`. Protects against fat-fingering a
        // kill in the middle of `j/k` nav.
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
        // Overlay is up; session unchanged on disk and in memory.
        assert!(matches!(state.overlay, Overlay::Confirm { .. }));
        assert_eq!(state.selected().unwrap().state, SessionState::Running);
    }

    #[test]
    fn confirm_overlay_y_executes_the_kill() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        store
            .create(&session("s-r", "wf", SessionState::Running, 100))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        // Open the confirm.
        state.handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(matches!(state.overlay, Overlay::Confirm { .. }));
        // Confirm → kill runs.
        state.handle_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.selected().unwrap().state, SessionState::Failed);
        assert!(matches!(state.overlay, Overlay::None));
        let loaded_id = SessionId::new(state.selected().unwrap().id.as_str());
        let loaded = store.load(&loaded_id).unwrap();
        assert_eq!(loaded.state, SessionState::Failed);
    }

    #[test]
    fn confirm_overlay_n_cancels_without_kill() {
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
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.selected().unwrap().state, SessionState::Running);
        assert!(matches!(state.overlay, Overlay::None));
        assert!(state.status_line.contains("cancelled"));
    }

    #[test]
    fn confirm_overlay_esc_cancels_without_kill() {
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
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &store);
        assert_eq!(state.selected().unwrap().state, SessionState::Running);
        assert!(matches!(state.overlay, Overlay::None));
    }

    #[test]
    fn confirm_overlay_q_during_confirm_cancels_does_not_quit() {
        // Regression guard: pressing `q` while a confirm is up must
        // not quit the app — overlays own input until dismissed.
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
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
            &store,
        );
        assert!(
            matches!(action, Action::None),
            "q must not quit during confirm"
        );
        assert!(matches!(state.overlay, Overlay::None));
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
        // Already-terminal short-circuits before the confirm even
        // opens — status line gets the note; no overlay.
        assert_eq!(state.selected().unwrap().state, SessionState::Completed);
        assert!(state.status_line.contains("already"));
        assert!(matches!(state.overlay, Overlay::None));
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
        state.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
            &store,
        );
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
        write(
            tmp.path(),
            ".fleet/workflows/standard.yaml",
            "name: standard\n",
        );
        write(tmp.path(), ".fleet/workflows/hotfix.yaml", "name: hotfix\n");
        write(
            tmp.path(),
            ".fleet/workflows/review-only.yaml",
            "name: review-only\n",
        );
        let names = list_workflows_dir(tmp.path());
        assert_eq!(names, vec!["hotfix", "review-only", "standard"]);
    }

    #[test]
    fn list_workflows_dir_skips_non_yaml_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".fleet/workflows/keeper.yaml", "name: keeper\n");
        write(tmp.path(), ".fleet/workflows/notes.md", "stuff\n");
        write(
            tmp.path(),
            ".fleet/workflows/.dotfile.yaml",
            "name: hidden\n",
        );
        std::fs::create_dir_all(tmp.path().join(".fleet/workflows/nested-dir")).unwrap();
        let names = list_workflows_dir(tmp.path());
        assert_eq!(names, vec!["keeper"]);
    }

    #[test]
    fn build_workflow_run_command_without_issue() {
        let cmd = build_workflow_run_command(Path::new("/usr/local/bin/fleet"), "standard", None);
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("/usr/local/bin/fleet"), "got: {dbg}");
        assert!(dbg.contains("workflow"), "got: {dbg}");
        assert!(dbg.contains("run"), "got: {dbg}");
        assert!(dbg.contains("standard"), "got: {dbg}");
        assert!(!dbg.contains("--issue"), "got: {dbg}");
    }

    #[test]
    fn build_workflow_run_command_with_issue_appends_flag() {
        let cmd =
            build_workflow_run_command(Path::new("/usr/local/bin/fleet"), "standard", Some("42"));
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("--issue"), "got: {dbg}");
        assert!(dbg.contains("42"), "got: {dbg}");
    }

    #[test]
    fn n_key_in_sessions_view_opens_spawn_picker() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            ".fleet/workflows/standard.yaml",
            "name: standard\n",
        );
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
    fn centered_rect_returns_rect_inside_parent() {
        let parent = Rect::new(0, 0, 100, 50);
        let r = centered_rect(parent, 60, 60);
        assert!(r.x >= parent.x);
        assert!(r.y >= parent.y);
        assert!(r.x + r.width <= parent.x + parent.width);
        assert!(r.y + r.height <= parent.y + parent.height);
        // Roughly the expected proportions — exact due to clean
        // arithmetic on the example dims.
        assert_eq!(r.width, 60);
        assert_eq!(r.height, 30);
        assert_eq!(r.x, 20);
        assert_eq!(r.y, 10);
    }

    #[test]
    fn centered_rect_clamps_percentages_to_safe_range() {
        // 5% would produce a 5x2 modal at 100x50 — too tiny to render.
        // The clamp pulls both percentages up to 10%.
        let parent = Rect::new(0, 0, 100, 50);
        let small = centered_rect(parent, 5, 5);
        assert_eq!(small.width, 10);
        assert_eq!(small.height, 5);
        // 200% would underflow the layout math. Clamp at 95%.
        let huge = centered_rect(parent, 200, 200);
        // 95% of 100 = 95 exact; 95% of 50 splits ratatui's layout
        // into 2/47/1 or 1/48/1 depending on rounding — both fall
        // inside the parent and leave clear margins, which is what
        // the clamp is for. Assert the bounds, not the exact value.
        assert_eq!(huge.width, 95);
        assert!(
            huge.height >= 45 && huge.height <= 48,
            "got {}",
            huge.height
        );
        assert!(huge.y >= 1, "top margin missing; got y={}", huge.y);
        assert!(huge.y + huge.height < parent.y + parent.height);
    }

    #[test]
    fn spawn_view_renders_overlay_atop_sessions() {
        // The overlay must coexist with the sessions content: the
        // sidebar's session count appears underneath, while the
        // picker's title appears on top of it. Use a test backend
        // to capture the rendered buffer.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            ".fleet/workflows/standard.yaml",
            "name: standard\n",
        );
        write(tmp.path(), ".fleet/workflows/hotfix.yaml", "name: hotfix\n");
        let store = SessionStore::at(tmp.path().join("sessions"));
        store
            .create(&session("s-bg", "wf", SessionState::Running, 1))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.view, View::Spawn);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &state)).unwrap();
        let buf = terminal.backend().buffer();
        let dumped: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Sidebar background still rendered.
        assert!(
            dumped.contains("Sessions"),
            "background sidebar missing; got:\n{dumped}"
        );
        // Overlay's title and entries on top.
        assert!(
            dumped.contains("Spawn workflow"),
            "overlay title missing; got:\n{dumped}"
        );
        assert!(
            dumped.contains("standard"),
            "overlay entry missing; got:\n{dumped}"
        );
    }

    #[test]
    fn esc_in_spawn_view_returns_to_sessions_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            ".fleet/workflows/standard.yaml",
            "name: standard\n",
        );
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
        write(
            tmp.path(),
            ".fleet/config.yaml",
            "autonomous:\n  max_parallel: 1\n",
        );
        let store = SessionStore::at(tmp.path().join("sessions"));
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        assert_eq!(state.config.autonomous.max_parallel, 1);
        // Edit the config and reload via `r`.
        write(
            tmp.path(),
            ".fleet/config.yaml",
            "autonomous:\n  max_parallel: 7\n",
        );
        state.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
            &store,
        );
        assert_eq!(state.config.autonomous.max_parallel, 7);
    }

    // ---- plans view ---------------------------------------------------

    use crate::plans::{ItemFailurePolicy, Plan, PlanId, PlanItem, PlanItemState, PlanState};

    fn sample_plan() -> Plan {
        let mut p = Plan::new(
            PlanId::new("plan-1"),
            "Parser refactor",
            vec!["42".into(), "43".into(), "44".into()],
            1_700_000_000_000,
        );
        p.items[0].state = PlanItemState::Completed;
        p.items[1].state = PlanItemState::InProgress;
        p
    }

    #[test]
    fn plan_row_label_shows_marker_id_progress_and_name() {
        let p = sample_plan();
        let label = plan_row_label(&p, &no_cycles());
        assert!(label.starts_with('●'), "active marker expected: {label}");
        assert!(label.contains("plan-1"));
        assert!(label.contains("1/3"));
        assert!(label.contains("Parser refactor"));
    }

    #[test]
    fn plan_row_label_marker_reflects_state() {
        let mut p = sample_plan();
        p.state = PlanState::Paused;
        assert!(plan_row_label(&p, &no_cycles()).starts_with('⏸'));
        p.state = PlanState::Completed;
        assert!(plan_row_label(&p, &no_cycles()).starts_with('✓'));
        p.state = PlanState::Abandoned;
        assert!(plan_row_label(&p, &no_cycles()).starts_with('✗'));
    }

    #[test]
    fn plan_detail_lines_includes_progress_and_item_rows() {
        let p = sample_plan();
        let lines = plan_detail_lines(&p, None, &no_titles());
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        let joined = rendered.join("\n");
        assert!(joined.contains("Parser refactor"));
        assert!(joined.contains("1/3"));
        assert!(joined.contains("Items (3)"));
        // Each item is rendered with its state word.
        assert!(joined.contains("completed"));
        assert!(joined.contains("in-progress"));
        assert!(joined.contains("pending"));
        // Tickets ids appear verbatim.
        assert!(joined.contains("42"));
        assert!(joined.contains("43"));
        assert!(joined.contains("44"));
    }

    #[test]
    fn plan_detail_lines_marks_injected_items() {
        let mut p = sample_plan();
        p.items.insert(1, PlanItem::pending_injected("99"));
        let lines = plan_detail_lines(&p, None, &no_titles());
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("(injected)"), "got: {joined}");
        assert!(joined.contains("99"), "got: {joined}");
    }

    #[test]
    fn plan_detail_lines_emits_epic_ref_only_when_set() {
        let p = sample_plan();
        let lines = plan_detail_lines(&p, None, &no_titles());
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("epic"));

        let mut p_with = sample_plan();
        p_with.epic_ref = Some(crate::plans::EpicRef {
            tracker: "github".into(),
            id: "200".into(),
        });
        let lines2 = plan_detail_lines(&p_with, None, &no_titles());
        let joined2: String = lines2
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined2.contains("github:200"), "got: {joined2}");
    }

    #[test]
    fn plan_state_word_round_trip() {
        assert_eq!(plan_state_word(PlanState::Active), "active");
        assert_eq!(plan_state_word(PlanState::Paused), "paused");
        assert_eq!(plan_state_word(PlanState::Completed), "completed");
        assert_eq!(plan_state_word(PlanState::Abandoned), "abandoned");
    }

    #[test]
    fn plan_item_word_uses_kebab_case_for_in_progress() {
        // Match the on-disk YAML so users can grep both surfaces.
        assert_eq!(plan_item_word(PlanItemState::InProgress), "in-progress");
        assert_eq!(plan_item_word(PlanItemState::Pending), "pending");
    }

    #[test]
    fn plan_failure_word_renders_kebab_case() {
        assert_eq!(plan_failure_word(ItemFailurePolicy::Stop), "stop");
        assert_eq!(plan_failure_word(ItemFailurePolicy::Continue), "continue");
        assert_eq!(
            plan_failure_word(ItemFailurePolicy::RetryOnce),
            "retry-once"
        );
    }

    // ---- session row plan annotations ---------------------------------

    use crate::session::IssueContext;

    fn session_with_ticket(id: &str, workflow: &str, ticket_human_id: &str) -> Session {
        let mut s = session(id, workflow, SessionState::Running, 1);
        s.issue = Some(IssueContext {
            id: format!("gh:{ticket_human_id}"),
            human_id: ticket_human_id.to_string(),
            title: "T".into(),
            labels: Vec::new(),
        });
        s
    }

    #[test]
    fn session_row_label_omits_plan_annotation_when_session_has_no_ticket() {
        let s = session("s-1", "standard", SessionState::Running, 1);
        let plans = vec![sample_plan()];
        let label = session_row_label(&s, &plans, &no_cycles());
        assert!(!label.contains('·'), "got: {label}");
    }

    #[test]
    fn session_row_label_omits_plan_annotation_when_no_active_plan_owns_the_ticket() {
        let s = session_with_ticket("s-1", "standard", "999");
        let plans = vec![sample_plan()]; // contains 42, 43, 44 only
        let label = session_row_label(&s, &plans, &no_cycles());
        assert!(!label.contains('·'), "got: {label}");
    }

    #[test]
    fn session_row_label_appends_plan_annotation_when_active_plan_contains_ticket() {
        let s = session_with_ticket("s-1", "standard", "43");
        let plans = vec![sample_plan()];
        let label = session_row_label(&s, &plans, &no_cycles());
        // 1-based position so users see "2/3", not "1/3".
        assert!(label.contains("· Parser refactor (2/3)"), "got: {label}");
    }

    #[test]
    fn session_row_label_skips_paused_plans_for_annotation() {
        // A paused plan's tickets shouldn't display as plan-bound —
        // the scheduler doesn't claim them, and the visual should
        // reflect that.
        let s = session_with_ticket("s-1", "standard", "43");
        let mut plan = sample_plan();
        plan.state = PlanState::Paused;
        let label = session_row_label(&s, &[plan], &no_cycles());
        assert!(!label.contains("· Parser"), "got: {label}");
    }

    #[test]
    fn session_row_label_preserves_cost_suffix_before_plan_annotation() {
        let mut s = session_with_ticket("s-1", "standard", "43");
        s.record_node_cost("plan", 0.5, 2);
        let label = session_row_label(&s, &[sample_plan()], &no_cycles());
        // Cost first ("...$0.50"), then plan annotation ("· …").
        let cost_idx = label.find("$0.50").expect("cost suffix");
        let plan_idx = label.find("· Parser").expect("plan annotation");
        assert!(cost_idx < plan_idx, "got: {label}");
    }

    #[test]
    fn session_row_label_prepends_warning_when_session_ticket_is_in_cycle() {
        let s = session_with_ticket("s-1", "standard", "43");
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("43".to_string());
        let label = session_row_label(&s, &[], &cycles);
        assert!(label.starts_with("⚠ "), "got: {label}");
    }

    #[test]
    fn session_row_label_omits_warning_when_ticket_is_not_in_cycle_set() {
        let s = session_with_ticket("s-1", "standard", "43");
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("999".to_string()); // unrelated ticket
        let label = session_row_label(&s, &[], &cycles);
        assert!(!label.starts_with("⚠ "), "got: {label}");
    }

    #[test]
    fn session_row_label_omits_warning_when_session_has_no_ticket_even_with_cycles() {
        // A session without a ticket binding can't be in a cycle —
        // the cycle set is keyed by ticket id, and no key matches.
        let s = session("s-1", "standard", SessionState::Running, 1);
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("43".to_string());
        let label = session_row_label(&s, &[], &cycles);
        assert!(!label.starts_with("⚠ "), "got: {label}");
    }

    #[test]
    fn plan_row_label_prepends_warning_when_any_item_ticket_is_in_cycle() {
        // sample_plan's items reference 42, 43, 44.
        let p = sample_plan();
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("43".to_string());
        let label = plan_row_label(&p, &cycles);
        assert!(label.starts_with("⚠ "), "got: {label}");
    }

    #[test]
    fn plan_row_label_omits_warning_when_no_item_ticket_intersects_cycle_set() {
        let p = sample_plan();
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("999".to_string()); // not in the plan
        let label = plan_row_label(&p, &cycles);
        assert!(!label.starts_with("⚠ "), "got: {label}");
    }

    // ---- ticket-detail pane (Plans view 3rd column) -----------------

    fn issue_detail_fixture(human: &str, body: &str, labels: &[&str], comments: usize) -> crate::tracker::IssueDetail {
        crate::tracker::IssueDetail {
            issue: crate::tracker::Issue {
                id: format!("gh:{human}"),
                human_id: human.into(),
                title: format!("Ticket {human} title"),
                status: "open".into(),
                labels: labels.iter().map(|s| (*s).to_string()).collect(),
            },
            body: body.into(),
            comments: (0..comments)
                .map(|i| crate::tracker::Comment {
                    author: format!("user{i}"),
                    body: format!("comment {i}"),
                    created_at: "2026-01-01T00:00:00Z".into(),
                })
                .collect(),
        }
    }

    fn rendered(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn issue_detail_lines_includes_id_title_status_and_comment_count() {
        let d = issue_detail_fixture("42", "Body text", &["bug", "urgent"], 3);
        let r = rendered(&issue_detail_lines(&d));
        assert!(r.contains("42"));
        assert!(r.contains("Ticket 42 title"));
        assert!(r.contains("open"));
        assert!(r.contains("[bug, urgent]"));
        assert!(r.contains("comments"));
        assert!(r.contains('3'));
        assert!(r.contains("Body text"));
    }

    #[test]
    fn issue_detail_lines_omits_labels_row_when_none() {
        let d = issue_detail_fixture("42", "Body", &[], 0);
        let r = rendered(&issue_detail_lines(&d));
        assert!(!r.contains("labels"), "labels row should be hidden: {r}");
    }

    #[test]
    fn issue_detail_lines_shows_no_body_placeholder_when_empty() {
        let d = issue_detail_fixture("42", "   ", &[], 0);
        let r = rendered(&issue_detail_lines(&d));
        assert!(r.contains("(no body)"));
    }

    #[test]
    fn ticket_detail_lines_says_tab_when_sidebar_has_focus() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42", "43"]);
        // Default focus is Sidebar.
        assert_eq!(state.plans_focus, PlansFocus::Sidebar);
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("Tab"), "got: {r}");
        // Even with an item-cache entry seeded the sidebar-focus
        // hint wins — the user told us they're not navigating
        // items right now.
        state.focused_issue_cache.insert(
            "42".to_string(),
            Ok(issue_detail_fixture("42", "Body", &[], 0)),
        );
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("Tab"));
    }

    #[test]
    fn ticket_detail_lines_renders_cached_issue_when_items_focused() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42", "43"]);
        state.toggle_plans_focus(); // Items, cursor at 0 (ticket 42)
        // Seed the cache as if a tracker.read succeeded.
        state.focused_issue_cache.insert(
            "42".to_string(),
            Ok(issue_detail_fixture("42", "Body of 42", &["bug"], 1)),
        );
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("Body of 42"), "got: {r}");
        assert!(r.contains("[bug]"));
    }

    #[test]
    fn ticket_detail_lines_surfaces_cached_error_verbatim() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        state.toggle_plans_focus();
        state.focused_issue_cache.insert(
            "42".to_string(),
            Err("simulated git-bug failure".to_string()),
        );
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("simulated git-bug failure"), "got: {r}");
        assert!(r.contains("tracker read failed"));
    }

    #[test]
    fn append_deps_lines_is_a_noop_when_no_edges_and_no_cycle() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc::empty();
        let cycles = std::collections::HashSet::new();
        append_deps_lines(&mut lines, "42", &doc, &cycles, &no_titles());
        assert!(lines.is_empty(), "no deps + no cycle → no lines");
    }

    #[test]
    fn append_deps_lines_renders_blocked_on_section_with_kind_marker() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![
                crate::deps::DepEdge {
                    blocked: "42".into(),
                    blocked_on: "100".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 1,
                },
                crate::deps::DepEdge {
                    blocked: "42".into(),
                    blocked_on: "free:apt-mirror".into(),
                    reason: crate::deps::BlockedReason::Freeform,
                    created_at_ms: 2,
                },
            ],
        };
        let cycles = std::collections::HashSet::new();
        append_deps_lines(&mut lines, "42", &doc, &cycles, &no_titles());
        let r = rendered(&lines);
        assert!(r.contains("Blocked on (2):"), "got: {r}");
        assert!(r.contains("100  [ticket]"));
        assert!(r.contains("free:apt-mirror  [freeform]"));
    }

    #[test]
    fn append_deps_lines_renders_blocks_section_when_others_depend_on_this() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![
                crate::deps::DepEdge {
                    blocked: "99".into(),
                    blocked_on: "42".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 1,
                },
                crate::deps::DepEdge {
                    blocked: "200".into(),
                    blocked_on: "42".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 2,
                },
            ],
        };
        let cycles = std::collections::HashSet::new();
        append_deps_lines(&mut lines, "42", &doc, &cycles, &no_titles());
        let r = rendered(&lines);
        assert!(r.contains("Blocks (2):"), "got: {r}");
        assert!(r.contains("• 99"));
        assert!(r.contains("• 200"));
    }

    #[test]
    fn append_deps_lines_renders_both_sections_when_ticket_is_on_both_sides() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![
                crate::deps::DepEdge {
                    blocked: "42".into(),
                    blocked_on: "100".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 1,
                },
                crate::deps::DepEdge {
                    blocked: "99".into(),
                    blocked_on: "42".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 2,
                },
            ],
        };
        let cycles = std::collections::HashSet::new();
        append_deps_lines(&mut lines, "42", &doc, &cycles, &no_titles());
        let r = rendered(&lines);
        assert!(r.contains("Blocked on (1):"));
        assert!(r.contains("Blocks (1):"));
        // Order: blocked-on (what we wait for) before blocks (who
        // waits for us). The user is more likely to care about
        // what's holding *us* up than who we're holding up.
        let blocked_on_idx = r.find("Blocked on").unwrap();
        let blocks_idx = r.find("Blocks").unwrap();
        assert!(blocked_on_idx < blocks_idx, "ordering wrong: {r}");
    }

    #[test]
    fn append_deps_lines_emits_cycle_warning_when_ticket_is_in_cycle_set() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc::empty();
        let mut cycles = std::collections::HashSet::new();
        cycles.insert("42".to_string());
        append_deps_lines(&mut lines, "42", &doc, &cycles, &no_titles());
        let r = rendered(&lines);
        assert!(r.contains("⚠"), "warning missing: {r}");
        assert!(r.contains("cycle"), "cycle word missing: {r}");
    }

    /// Tracker issue fixture for tests that want a title in the
    /// `tickets_by_id` map without dragging the full
    /// `issue_detail_fixture` helper around.
    fn ticket(human: &str, title: &str) -> crate::tracker::Issue {
        crate::tracker::Issue {
            id: format!("gh:{human}"),
            human_id: human.into(),
            title: title.into(),
            status: "open".into(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn plan_detail_lines_appends_title_when_known() {
        let p = sample_plan();
        let mut titles = std::collections::HashMap::new();
        titles.insert("42".to_string(), ticket("42", "Parser refactor"));
        titles.insert("43".to_string(), ticket("43", "Token tweaks"));
        // 44 left without a title — the row should still render
        // without a suffix.
        let r = rendered(&plan_detail_lines(&p, None, &titles));
        assert!(r.contains("42 — Parser refactor"), "got: {r}");
        assert!(r.contains("43 — Token tweaks"));
        // 44 should appear (it's an item) but without a title
        // suffix.
        let line_44 = r.lines().find(|l| l.contains("44")).unwrap();
        assert!(!line_44.contains("—"), "unexpected title suffix: {line_44}");
    }

    #[test]
    fn append_deps_lines_appends_title_for_ticket_edges_only() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![
                // Ticket edge — should get a title suffix.
                crate::deps::DepEdge {
                    blocked: "42".into(),
                    blocked_on: "100".into(),
                    reason: crate::deps::BlockedReason::Ticket,
                    created_at_ms: 1,
                },
                // Freeform edge — no title lookup, no suffix.
                crate::deps::DepEdge {
                    blocked: "42".into(),
                    blocked_on: "free:apt-mirror".into(),
                    reason: crate::deps::BlockedReason::Freeform,
                    created_at_ms: 2,
                },
            ],
        };
        let cycles = std::collections::HashSet::new();
        let mut titles = std::collections::HashMap::new();
        titles.insert("100".to_string(), ticket("100", "Define OpenAPI schema"));
        append_deps_lines(&mut lines, "42", &doc, &cycles, &titles);
        let r = rendered(&lines);
        assert!(
            r.contains("100 — Define OpenAPI schema  [ticket]"),
            "got: {r}"
        );
        // Freeform row stays bare — `free:` prefix isn't a ticket id.
        let line_apt = r.lines().find(|l| l.contains("apt-mirror")).unwrap();
        assert!(
            !line_apt.contains("—"),
            "freeform row gained a phantom title: {line_apt}"
        );
    }

    #[test]
    fn append_deps_lines_appends_title_on_blocks_side_too() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![crate::deps::DepEdge {
                blocked: "99".into(),
                blocked_on: "42".into(),
                reason: crate::deps::BlockedReason::Ticket,
                created_at_ms: 1,
            }],
        };
        let cycles = std::collections::HashSet::new();
        let mut titles = std::collections::HashMap::new();
        titles.insert("99".to_string(), ticket("99", "Frontend client"));
        append_deps_lines(&mut lines, "42", &doc, &cycles, &titles);
        let r = rendered(&lines);
        assert!(r.contains("99 — Frontend client"), "got: {r}");
    }

    #[test]
    fn ticket_detail_lines_includes_deps_section_under_cached_issue() {
        // End-to-end: a focused item with a cached issue *and* a
        // deps edge should show both the issue header and the
        // deps section in the same render.
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        state.toggle_plans_focus();
        state.focused_issue_cache.insert(
            "42".to_string(),
            Ok(issue_detail_fixture("42", "Body of 42", &[], 0)),
        );
        state.deps_doc = crate::deps::DepsDoc {
            version: crate::deps::DEPS_SCHEMA_VERSION,
            edges: vec![crate::deps::DepEdge {
                blocked: "42".into(),
                blocked_on: "100".into(),
                reason: crate::deps::BlockedReason::Ticket,
                created_at_ms: 1,
            }],
        };
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("Body of 42"), "issue body missing: {r}");
        assert!(r.contains("Blocked on (1):"), "deps section missing: {r}");
    }

    #[test]
    fn ticket_detail_lines_shows_fetching_when_cache_is_empty_under_items_focus() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        state.toggle_plans_focus();
        // The toggle calls refresh_focused_issue, which builds the
        // tracker; in a test with no git-bug available it'll
        // populate the cache with an Unsupported placeholder.
        // Drop the cache so we can exercise the "(fetching…)"
        // path explicitly.
        state.focused_issue_cache.clear();
        let r = rendered(&ticket_detail_lines(&state));
        assert!(r.contains("(fetching"), "got: {r}");
    }

    // ---- plans view interactivity -----------------------------------

    /// Build an `AppState` with a single Active plan on disk so the
    /// Plans-view key-handler tests can drive against real `PlanStore`
    /// I/O without re-deriving the boilerplate per test.
    fn plans_state_with_one_active_plan(
        tickets: &[&str],
    ) -> (tempfile::TempDir, SessionStore, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionStore::at(tmp.path().to_path_buf());
        let plans = PlanStore::for_repo(tmp.path());
        let tickets: Vec<String> = tickets.iter().map(|s| (*s).to_string()).collect();
        let plan = Plan::new(crate::plans::PlanId::new("plan-1"), "Test plan", tickets, 1);
        plans.create(&plan).unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &sessions).unwrap();
        state.view = View::Plans;
        state.refresh_plans();
        (tmp, sessions, state)
    }

    #[test]
    fn tab_in_plans_view_toggles_focus_between_sidebar_and_items() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42", "43"]);
        assert_eq!(state.plans_focus, PlansFocus::Sidebar);
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(state.plans_focus, PlansFocus::Items);
        // First Tab→Items lands the cursor at item 0 when there are items.
        assert_eq!(state.plans_items_state.selected(), Some(0));
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(state.plans_focus, PlansFocus::Sidebar);
    }

    #[test]
    fn j_in_items_focus_moves_the_item_cursor_not_the_sidebar() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42", "43", "44"]);
        state.toggle_plans_focus(); // now Items, cursor at 0
        let sidebar_before = state.plans_list_state.selected();
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()));
        assert_eq!(state.plans_items_state.selected(), Some(1));
        assert_eq!(state.plans_list_state.selected(), sidebar_before);
    }

    #[test]
    fn item_cursor_clears_when_the_sidebar_selection_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionStore::at(tmp.path().to_path_buf());
        let plans = PlanStore::for_repo(tmp.path());
        // Two plans so the sidebar has somewhere to move to.
        plans
            .create(&Plan::new(
                crate::plans::PlanId::new("plan-a"),
                "A",
                vec!["42".into(), "43".into()],
                1,
            ))
            .unwrap();
        plans
            .create(&Plan::new(
                crate::plans::PlanId::new("plan-b"),
                "B",
                vec!["99".into()],
                2,
            ))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &sessions).unwrap();
        state.view = View::Plans;
        state.refresh_plans();
        state.toggle_plans_focus(); // Items, cursor at 0
        state.move_plans_item_selection(1); // cursor at 1
        assert_eq!(state.plans_items_state.selected(), Some(1));
        state.move_plans_selection(1); // sidebar moves
        // Stale cursor (was item 1 in plan-a, plan-b only has one
        // item) must clear so the next Tab→Items reseats from 0.
        assert_eq!(state.plans_items_state.selected(), None);
    }

    #[test]
    fn shift_p_in_plans_view_toggles_active_to_paused_and_back() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        // Active → Paused
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert_eq!(state.plans[0].state, PlanState::Paused);
        // Disk is updated too.
        let on_disk = PlanStore::for_repo(state.root.as_path())
            .load(&crate::plans::PlanId::new("plan-1"))
            .unwrap();
        assert_eq!(on_disk.state, PlanState::Paused);
        // Paused → Active
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert_eq!(state.plans[0].state, PlanState::Active);
    }

    #[test]
    fn shift_p_is_a_noop_with_status_when_plan_is_completed() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        state.plans[0].state = PlanState::Completed;
        PlanStore::for_repo(state.root.as_path())
            .save(&state.plans[0])
            .unwrap();
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        // State left alone; status line explains why.
        assert_eq!(state.plans[0].state, PlanState::Completed);
        assert!(
            state.status_line.contains("completed"),
            "got: {}",
            state.status_line
        );
    }

    #[test]
    fn shift_c_marks_selected_plan_completed_and_saves_to_disk() {
        let (_tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT));
        assert_eq!(state.plans[0].state, PlanState::Completed);
        let on_disk = PlanStore::for_repo(state.root.as_path())
            .load(&crate::plans::PlanId::new("plan-1"))
            .unwrap();
        assert_eq!(on_disk.state, PlanState::Completed);
    }

    #[test]
    fn u_in_items_focus_clears_deps_edges_keyed_by_selected_ticket() {
        let (tmp, _store, mut state) = plans_state_with_one_active_plan(&["42", "43"]);
        // Seed two deps edges; only the one keyed on "42" should
        // clear when `u` runs against item-0 (which is ticket 42).
        let deps_store = crate::deps::DepsStore::for_repo(tmp.path());
        deps_store
            .add_edge(crate::deps::DepEdge {
                blocked: "42".into(),
                blocked_on: "100".into(),
                reason: crate::deps::BlockedReason::Ticket,
                created_at_ms: 1,
            })
            .unwrap();
        deps_store
            .add_edge(crate::deps::DepEdge {
                blocked: "43".into(),
                blocked_on: "200".into(),
                reason: crate::deps::BlockedReason::Ticket,
                created_at_ms: 2,
            })
            .unwrap();
        state.toggle_plans_focus(); // Items, cursor at item 0 (ticket 42)
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::empty()));
        let doc = deps_store.load().unwrap();
        // 42→100 cleared; 43→200 survives.
        assert_eq!(doc.edges.len(), 1, "edges: {:?}", doc.edges);
        assert_eq!(doc.edges[0].blocked, "43");
    }

    #[test]
    fn u_with_sidebar_focus_emits_status_message_and_does_nothing() {
        let (tmp, _store, mut state) = plans_state_with_one_active_plan(&["42"]);
        let deps_store = crate::deps::DepsStore::for_repo(tmp.path());
        deps_store
            .add_edge(crate::deps::DepEdge {
                blocked: "42".into(),
                blocked_on: "100".into(),
                reason: crate::deps::BlockedReason::Ticket,
                created_at_ms: 1,
            })
            .unwrap();
        // Sidebar focus by default — pressing `u` should not clear
        // anything; it should nudge the user to Tab into items.
        let _ = state.handle_key_plans(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::empty()));
        let doc = deps_store.load().unwrap();
        assert_eq!(doc.edges.len(), 1);
        assert!(
            state.status_line.contains("Tab"),
            "got: {}",
            state.status_line
        );
    }

    // ---- sessions view brainstorm bindings --------------------------

    /// Seed a TempDir-rooted state with one workflow session and one
    /// brainstorm session — enough surface to exercise the Tab/j/k/
    /// Enter/Shift+B handlers against real on-disk metadata.
    fn sessions_state_with_one_of_each() -> (
        tempfile::TempDir,
        SessionStore,
        AppState,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionStore::at(tmp.path().to_path_buf());
        sessions
            .create(&session("s-1", "standard", SessionState::Running, 100))
            .unwrap();
        let bstore = BrainstormStore::for_repo(tmp.path());
        let b = BrainstormSession::new(BrainstormId::new("b-1"), "claude", 200);
        bstore.save(&b).unwrap();
        let state = AppState::new(tmp.path().to_path_buf(), &sessions).unwrap();
        (tmp, sessions, state)
    }

    #[test]
    fn tab_in_sessions_view_toggles_focus_between_workflows_and_brainstorms() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        assert_eq!(state.sessions_focus, SessionsFocus::Workflows);
        let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), &store);
        assert_eq!(state.sessions_focus, SessionsFocus::Brainstorms);
        // First Tab to Brainstorms lands the cursor at row 0.
        assert_eq!(state.brainstorms_list_state.selected(), Some(0));
        let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), &store);
        assert_eq!(state.sessions_focus, SessionsFocus::Workflows);
    }

    #[test]
    fn enter_with_brainstorms_focus_returns_attach_brainstorm_action() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        state.toggle_sessions_focus();
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &store,
        );
        match action {
            Action::AttachBrainstorm(id) => assert_eq!(id.as_str(), "b-1"),
            _ => panic!("expected AttachBrainstorm, got something else"),
        }
    }

    #[test]
    fn enter_with_workflows_focus_is_a_noop_for_now() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &store,
        );
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn shift_b_returns_new_brainstorm_from_any_view() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(matches!(action, Action::NewBrainstorm));
        // From the Plans view, too.
        state.view = View::Plans;
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(matches!(action, Action::NewBrainstorm));
    }

    #[test]
    fn j_in_brainstorms_focus_moves_brainstorm_cursor_not_workflow_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionStore::at(tmp.path().to_path_buf());
        sessions
            .create(&session("s-1", "standard", SessionState::Running, 100))
            .unwrap();
        let bstore = BrainstormStore::for_repo(tmp.path());
        bstore
            .save(&BrainstormSession::new(BrainstormId::new("b-1"), "claude", 1))
            .unwrap();
        bstore
            .save(&BrainstormSession::new(BrainstormId::new("b-2"), "claude", 2))
            .unwrap();
        let mut state = AppState::new(tmp.path().to_path_buf(), &sessions).unwrap();
        state.toggle_sessions_focus(); // Brainstorms, cursor at 0
        let workflow_before = state.list_state.selected();
        let _ = state.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()),
            &sessions,
        );
        assert_eq!(state.brainstorms_list_state.selected(), Some(1));
        assert_eq!(state.list_state.selected(), workflow_before);
    }

    // ---- brainstorm sidebar -----------------------------------------

    use crate::brainstorm::{BrainstormId, BrainstormSession, BrainstormState};

    fn brainstorm(id: &str, agent: &str, state: BrainstormState) -> BrainstormSession {
        let mut s = BrainstormSession::new(BrainstormId::new(id), agent, 1);
        s.state = state;
        s
    }

    #[test]
    fn brainstorm_row_label_carries_marker_id_agent_and_state_word() {
        let s = brainstorm("b-1", "claude", BrainstormState::Active);
        let label = brainstorm_row_label(&s);
        assert!(label.starts_with('◐'), "marker missing: {label}");
        assert!(label.contains("b-1"));
        assert!(label.contains("agent=claude"));
        assert!(label.contains("(active)"));
    }

    #[test]
    fn brainstorm_row_label_marker_reflects_state() {
        assert!(
            brainstorm_row_label(&brainstorm("b-1", "x", BrainstormState::Detached))
                .starts_with('⏸')
        );
        assert!(
            brainstorm_row_label(&brainstorm("b-1", "x", BrainstormState::Closed)).starts_with('✗')
        );
    }
}
