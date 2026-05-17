//! State, types, and behaviour for the TUI.
//!
//! Pure-ish module: no rendering, no crossterm setup. The event loop in
//! [`super::terminal`] constructs an [`AppState`] and calls into it via
//! [`super::input`]; renderers in [`super::ui`] take a `&AppState` and
//! produce frames.
//!
//! Internals are `pub(super)` so [`super::input`], [`super::ui`], and
//! the tests living in [`super::mod`] can reach them — they're not
//! re-exported outside the `tui` module. The only externally-public
//! item is [`build_workflow_run_command`], consumed by
//! `crate::cli::autonomous`.

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::autonomous;
use crate::orchestrator::store::OrchestratorStore;
use crate::orchestrator::OrchestratorSession;
use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanState};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::Capabilities;
use crate::runtime::detect::probe;
use crate::runtime::factory::build_adapter;
use crate::session::store::SessionStore;
use crate::session::{Session, SessionState, now_ms};

/// How many lines of the most-recently-modified log file to show in the
/// detail pane.
pub(super) const LOG_TAIL_LINES: usize = 25;

/// Cap on how many bytes of a spawn-log file we read for the failure
/// overlay. 4 KiB is enough to surface the typical Rust panic +
/// backtrace tail without flooding the modal.
pub(super) const SPAWN_LOG_TAIL_BYTES: u64 = 4 * 1024;

/// In-memory app state. Held by the event loop, mutated by key handlers,
/// snapshotted by the render functions.
pub(super) struct AppState {
    pub(super) root: PathBuf,
    pub(super) sessions: Vec<Session>,
    pub(super) list_state: ListState,
    /// Tail of the most-recently-touched log under the selected session.
    /// Re-read on selection change so it doesn't go stale across refreshes.
    pub(super) log_tail: Vec<String>,
    pub(super) last_selected_id: Option<String>,
    pub(super) status_line: String,
    /// Which top-level view is active. Toggled by `d`.
    pub(super) view: View,
    /// Lazily-computed doctor snapshot. `None` until the user enters
    /// the doctor view at least once; refreshed on every entry so the
    /// pane reflects current config + host probe state.
    pub(super) doctor: Option<DoctorSnapshot>,
    /// Workflow names available to launch from this repo. Refreshed
    /// each time the spawn picker opens so a newly-added workflow
    /// file appears without restarting the TUI. Empty when no
    /// `.fleet/workflows/` exists.
    pub(super) spawn_workflows: Vec<String>,
    /// Cursor for the spawn picker. Independent of `list_state` (which
    /// tracks the sessions sidebar) so reopening the picker doesn't
    /// nuke the session selection.
    pub(super) spawn_list_state: ListState,
    /// Repo config snapshot. Reloaded on `r`. Carries the
    /// `autonomous:` bounds + the tracker choice the engine needs.
    pub(super) config: RepoConfig,
    /// Autonomous supervisor. Off by default; toggled via `Shift+A`.
    pub(super) autonomous: autonomous::AutonomousEngine,
    /// Tracker built lazily on first `Shift+A` — the `gh`/`git-bug`
    /// construction happens then, not at TUI startup, so users who
    /// never use autonomous mode aren't blocked by tracker setup.
    pub(super) tracker: TrackerState,
    /// Active transient overlay (confirm dialog or error message).
    /// Rendered on top of whatever `view` is currently drawing.
    pub(super) overlay: Overlay,
    /// Loaded plans, refreshed on entry to the Plans view.
    pub(super) plans: Vec<Plan>,
    pub(super) plans_list_state: ListState,
    pub(super) plans_focus: PlansFocus,
    pub(super) plans_items_state: ListState,
    pub(super) orchestrators: Vec<OrchestratorSession>,
    pub(super) sessions_focus: SessionsFocus,
    pub(super) orchestrators_list_state: ListState,
    /// Ticket ids currently sitting on a cycle in the deps graph.
    pub(super) cycle_nodes: std::collections::HashSet<String>,
    /// Full deps document, cached alongside `cycle_nodes`.
    pub(super) deps_doc: crate::deps::DepsDoc,
    /// Tracker-issue-by-human-id cache.
    pub(super) tickets_by_id: std::collections::HashMap<String, crate::tracker::Issue>,
    /// Cache of tracker-fetched issue details for the Plans view.
    pub(super) focused_issue_cache:
        std::collections::HashMap<String, Result<crate::tracker::IssueDetail, String>>,
    /// Most-recently-spawned workflow-run subprocess, watched on each
    /// event-loop tick so an immediate failure surfaces as an error
    /// overlay instead of vanishing.
    pub(super) pending_spawn: Option<PendingSpawn>,
}

/// Handle to a workflow-run subprocess the TUI is waiting to see the
/// exit status of. `child` is `try_wait`'d each tick; `log_path` points
/// at the file the child's stderr was redirected to so the
/// failure-overlay codepath can read a tail off disk.
pub(super) struct PendingSpawn {
    pub(super) name: String,
    pub(super) child: std::process::Child,
    pub(super) log_path: PathBuf,
    pub(super) started_at_ms: u64,
}

/// Lifecycle of the lazily-built tracker.
pub(super) enum TrackerState {
    Pending,
    Built(Arc<dyn crate::tracker::Tracker>),
    Unsupported,
}

/// Top-level view enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    Sessions,
    Doctor,
    Spawn,
    Plans,
}

/// Which pane of the Plans view has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlansFocus {
    Sidebar,
    Items,
}

/// Which sub-list of the Sessions sidebar has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionsFocus {
    Workflows,
    Orchestrator,
}

/// Transient overlay rendered on top of the current view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Overlay {
    None,
    Confirm {
        prompt: String,
        action: ConfirmAction,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConfirmAction {
    KillSelected,
}

/// Closed enum so the event loop's dispatch stays exhaustive.
pub(super) enum Action {
    None,
    Quit,
    /// Open the per-repo orchestrator: spawn it if it doesn't exist,
    /// respawn the agent if its tmux pane has died, then attach.
    /// Triggered by Enter on the always-visible orchestrator row in
    /// the sidebar — there's only ever one, so "new" and "attach"
    /// collapse to the same operation.
    OpenOrchestrator,
}

/// Resolved snapshot for the doctor pane.
#[derive(Debug, Clone)]
pub struct DoctorSnapshot {
    pub root: PathBuf,
    pub initialised: bool,
    pub configured_adapter: String,
    pub configured_hardening: String,
    pub tracker: String,
    pub adapter: Result<(String, Capabilities), String>,
    pub agents: Vec<(String, Vec<String>)>,
}

impl DoctorSnapshot {
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
    pub(super) fn new(root: PathBuf, store: &SessionStore) -> Result<Self> {
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
            orchestrators: Vec::new(),
            sessions_focus: SessionsFocus::Workflows,
            orchestrators_list_state: ListState::default(),
            cycle_nodes: std::collections::HashSet::new(),
            deps_doc: crate::deps::DepsDoc::empty(),
            tickets_by_id: std::collections::HashMap::new(),
            focused_issue_cache: std::collections::HashMap::new(),
            pending_spawn: None,
        };
        state.reload(store)?;
        // Default sidebar focus: if there are no workers, land the
        // cursor on the always-visible orchestrator row instead of
        // an empty workers list. That way a brand-new repo can
        // spawn the orchestrator without first hitting Tab.
        if state.sessions.is_empty() {
            state.sessions_focus = SessionsFocus::Orchestrator;
            state.orchestrators_list_state.select(Some(0));
        }
        Ok(state)
    }

    pub(super) fn reload(&mut self, store: &SessionStore) -> Result<()> {
        let ids = store.list().context("listing sessions")?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(s) = store.load(&id) {
                sessions.push(s);
            }
        }
        sort_sessions(&mut sessions);
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
        let plan_store = crate::plans::store::PlanStore::for_repo(&self.root);
        if let Err(err) =
            crate::autonomous::reconcile_plans_from_sessions(&self.sessions, &plan_store, now_ms())
        {
            self.status_line = format!(" reconcile failed: {err:#} ");
        }
        self.refresh_plans();
        self.refresh_orchestrators();
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        let doc = deps_store.load().unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
        self.status_line = super::ui::render_status_line(&self.sessions, &self.plans);
        Ok(())
    }

    pub(super) fn refresh_orchestrators(&mut self) {
        // Single-session model: the orchestrator collection is
        // always 0 or 1 entries. Vec is kept (rather than Option)
        // so the rest of the TUI's list-rendering / ListState
        // machinery doesn't need a special case.
        let store = OrchestratorStore::for_repo(&self.root);
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        if let Err(err) = crate::orchestrator::reaper::reap(&store, invoker.as_ref(), now_ms()) {
            self.status_line = format!(" orchestrator: reap failed: {err:#} ");
        }
        self.orchestrators.clear();
        if store.exists() {
            match store.load() {
                Ok(s) => self.orchestrators.push(s),
                Err(err) => {
                    self.status_line = format!(" orchestrator: load failed: {err:#} ");
                }
            }
        }
    }

    pub(super) fn selected(&self) -> Option<&Session> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    fn refresh_log_tail(&mut self) {
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

    /// Top-level key dispatch. Kept on the type for test ergonomics —
    /// `state.handle_key(...)` is the idiom every test in
    /// [`super::tests`] already uses. The real per-view logic lives
    /// in `handle_key_*` methods below.
    pub(super) fn handle_key(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_key_overlay(key, store);
        }
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('K'))
            && self.view == View::Sessions
        {
            self.prompt_kill_selected();
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('A')) {
            self.toggle_autonomous();
            return Action::None;
        }
        // The orchestrator row is always visible in the sidebar, so
        // Tab + Enter is enough to open it; no Shift+O hotkey
        // necessary.
        match self.view {
            View::Sessions => self.handle_key_sessions(key, store),
            View::Doctor => self.handle_key_doctor(key, store),
            View::Spawn => self.handle_key_spawn(key),
            View::Plans => self.handle_key_plans(key),
        }
    }

    fn handle_key_overlay(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        match self.overlay.clone() {
            Overlay::Confirm { action, .. } => {
                self.overlay = Overlay::None;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.run_confirm_action(&action, store);
                } else {
                    self.status_line = " kill cancelled ".to_string();
                }
            }
            Overlay::Error { .. } => {
                self.overlay = Overlay::None;
            }
            Overlay::None => unreachable!("handle_key_overlay called with Overlay::None"),
        }
        Action::None
    }

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
                self.doctor = Some(DoctorSnapshot::probe(self.root.clone()));
                self.view = View::Doctor;
                Action::None
            }
            KeyCode::Char('r') => {
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
                self.sidebar_move(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sidebar_move(-1);
                Action::None
            }
            KeyCode::Enter => {
                if self.sessions_focus == SessionsFocus::Orchestrator {
                    // Single-session model: there's at most one row
                    // and "attach an existing" == "open the
                    // orchestrator", so both branches collapse to
                    // OpenOrchestrator. The synthetic "not running"
                    // row is also Enter-able, so a brand-new repo
                    // can spawn the orchestrator from here directly.
                    return Action::OpenOrchestrator;
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

    pub(super) fn toggle_sessions_focus(&mut self) {
        self.sessions_focus = match self.sessions_focus {
            SessionsFocus::Workflows => SessionsFocus::Orchestrator,
            SessionsFocus::Orchestrator => SessionsFocus::Workflows,
        };
        // Always auto-select row 0 when focus moves to the
        // Orchestrator pane — the sidebar renders a synthetic
        // not-running row when no orchestrator exists yet, so there
        // is always something to highlight (and Enter to spawn).
        if self.sessions_focus == SessionsFocus::Orchestrator
            && self.orchestrators_list_state.selected().is_none()
        {
            self.orchestrators_list_state.select(Some(0));
        }
    }

    /// Unified up/down navigation across the two stacked sidebar
    /// panes. The orchestrator pane sits on top and always has one
    /// row (live or synthetic); the workers pane is below with 0..n
    /// rows. Moving Up from the top of workers hops focus to
    /// orchestrator; Down from orchestrator hops focus to workers
    /// row 0 (or stays if there are none). Neither edge wraps —
    /// Up at the top and Down at the bottom of the whole sidebar
    /// are no-ops.
    fn sidebar_move(&mut self, delta: isize) {
        match (self.sessions_focus, delta) {
            (SessionsFocus::Orchestrator, 1) => {
                // Down from orchestrator → workers row 0 if any,
                // otherwise stay (nothing to move to).
                if !self.sessions.is_empty() {
                    self.sessions_focus = SessionsFocus::Workflows;
                    self.list_state.select(Some(0));
                    self.refresh_log_tail();
                }
            }
            (SessionsFocus::Orchestrator, _) => {
                // Up from orchestrator is the top of everything;
                // no row to move to.
            }
            (SessionsFocus::Workflows, -1) => {
                // Up: from row 0 → orchestrator (always has its
                // synthetic row). From any other row, move within
                // the workers list.
                let at_top = self.list_state.selected().is_none_or(|i| i == 0);
                if at_top {
                    self.sessions_focus = SessionsFocus::Orchestrator;
                    self.orchestrators_list_state.select(Some(0));
                } else {
                    self.move_workflow_selection(-1);
                }
            }
            (SessionsFocus::Workflows, _) => {
                // Down within workers. Clamped (no wrap) so a long
                // hold-down doesn't surprise the user by jumping
                // back to row 0.
                self.move_workflow_selection(1);
            }
        }
    }

    /// Move the workers cursor within its own pane, clamped to
    /// `[0, len-1]`. No wraparound — cross-pane navigation is the
    /// caller's job (see [`Self::sidebar_move`]).
    pub(super) fn move_workflow_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let len = isize::try_from(self.sessions.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.list_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).clamp(0, len - 1);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.list_state.select(Some(next_usize));
        self.refresh_log_tail();
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

    pub(super) fn handle_key_plans(&mut self, key: KeyEvent) -> Action {
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
                self.focused_issue_cache.clear();
                self.refresh_plans();
                self.refresh_ticket_titles();
                self.refresh_focused_issue();
                Action::None
            }
            _ => Action::None,
        }
    }

    pub(super) fn toggle_plans_focus(&mut self) {
        self.plans_focus = match self.plans_focus {
            PlansFocus::Sidebar => PlansFocus::Items,
            PlansFocus::Items => PlansFocus::Sidebar,
        };
        if self.plans_focus == PlansFocus::Items {
            let n_items = self.selected_plan().map_or(0, |p| p.items.len());
            if n_items > 0 && self.plans_items_state.selected().is_none() {
                self.plans_items_state.select(Some(0));
            }
            self.refresh_focused_issue();
        }
    }

    pub(super) fn move_plans_item_selection(&mut self, delta: isize) {
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
        self.refresh_focused_issue();
    }

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
                    super::ui::plan_state_word(plan.state),
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
                self.status_line =
                    format!(" plan `{}` → {} ", plan.id, super::ui::plan_state_word(target));
            }
            Err(err) => {
                plan.state = previous;
                self.status_line = format!(" plan save failed: {err:#} ");
            }
        }
    }

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
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        match deps_store.remove_edges_for_blocked(&ticket) {
            Ok(n) => {
                self.status_line =
                    format!(" ticket `{ticket}`: cleared {n} dep edge{} ", plural(n));
                self.refresh_cycle_nodes_only();
            }
            Err(err) => {
                self.status_line = format!(" unblock failed: {err:#} ");
            }
        }
    }

    fn refresh_cycle_nodes_only(&mut self) {
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        let doc = deps_store
            .load()
            .unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
    }

    fn open_plans_view(&mut self) {
        self.refresh_plans();
        self.refresh_ticket_titles();
        self.view = View::Plans;
    }

    fn refresh_ticket_titles(&mut self) {
        let Some(tracker) = self.ensure_tracker() else {
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
            }
        }
    }

    pub(super) fn refresh_plans(&mut self) {
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

    pub(super) fn move_plans_selection(&mut self, delta: isize) {
        if self.plans.is_empty() {
            return;
        }
        let len = isize::try_from(self.plans.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.plans_list_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.plans_list_state.select(Some(next_usize));
        self.plans_items_state.select(None);
    }

    pub(super) fn selected_plan(&self) -> Option<&Plan> {
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

    fn open_spawn_picker(&mut self) {
        self.spawn_workflows = list_workflows_dir(&self.root);
        let select = if self.spawn_workflows.is_empty() {
            None
        } else {
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

        let started_at_ms = now_ms();
        let log_path = spawn_log_path(&self.root, &name, started_at_ms);
        let log_file = match open_spawn_log(&log_path) {
            Ok(f) => Some(f),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %log_path.display(),
                    "could not open spawn-log file; child stderr will be discarded",
                );
                None
            }
        };

        let mut cmd = build_workflow_run_command(&binary, &name, None);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        match log_file.as_ref().and_then(|f| f.try_clone().ok()) {
            Some(f) => {
                cmd.stderr(std::process::Stdio::from(f));
            }
            None => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        match cmd.spawn() {
            Ok(child) => {
                self.status_line = format!(" spawned `{name}` — press `r` to refresh ");
                self.pending_spawn = Some(PendingSpawn {
                    name,
                    child,
                    log_path,
                    started_at_ms,
                });
            }
            Err(err) => {
                self.status_line = format!(" spawn `{name}` failed: {err:#} ");
            }
        }
        self.view = View::Sessions;
    }

    pub(super) fn poll_pending_spawn(&mut self) {
        let Some(mut pending) = self.pending_spawn.take() else {
            return;
        };
        match pending.child.try_wait() {
            Ok(None) => {
                self.pending_spawn = Some(pending);
            }
            Ok(Some(status)) if status.success() => {}
            Ok(Some(status)) => {
                if matches!(self.overlay, Overlay::None) {
                    let tail = read_tail_string(&pending.log_path, SPAWN_LOG_TAIL_BYTES)
                        .unwrap_or_else(|| "(no stderr captured)".to_string());
                    self.overlay = Overlay::Error {
                        message: format!(
                            "spawn `{}` exited {} — see log:\n{}\n\n{}",
                            pending.name,
                            status
                                .code()
                                .map_or_else(|| "(signal)".to_string(), |c| format!("with code {c}")),
                            pending.log_path.display(),
                            tail.trim_end(),
                        ),
                    };
                    self.status_line =
                        format!(" spawn `{}` failed — press any key ", pending.name);
                }
                let _ = pending.started_at_ms;
            }
            Err(err) => {
                if matches!(self.overlay, Overlay::None) {
                    self.overlay = Overlay::Error {
                        message: format!(
                            "spawn `{}` — could not poll subprocess: {err:#}\n\nLog: {}",
                            pending.name,
                            pending.log_path.display(),
                        ),
                    };
                }
            }
        }
    }

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

    pub(super) fn autonomous_tick(&mut self, store: &SessionStore, now: std::time::Instant) {
        if !self.autonomous.enabled() {
            return;
        }
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
        let mut child =
            build_workflow_run_command(&binary, &cmd.workflow, Some(&cmd.issue.human_id));
        child
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match child.spawn() {
            Ok(_child) => {
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

// ───────────────────────── free-function helpers ─────────────────────────

/// List the workflows available to launch from this repo. Returns the
/// basenames (no `.yaml` extension) of every `.yaml` file directly
/// under `<root>/.fleet/workflows/`, sorted alphabetically.
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

/// Build the `fleet workflow run <name> [--issue <human_id>]` command.
/// `fleet_binary` is the path to the fleet executable (typically
/// `std::env::current_exe()`).
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

/// Build the path the spawn picker captures a workflow's stderr to.
#[must_use]
pub fn spawn_log_path(root: &Path, workflow: &str, started_at_ms: u64) -> PathBuf {
    let safe = sanitize_log_segment(workflow);
    root.join(".fleet/spawn-logs")
        .join(format!("{safe}-{started_at_ms}.log"))
}

/// Replace anything that's not [A-Za-z0-9._-] with `_` so a surprising
/// workflow filename can't escape the log directory.
#[must_use]
fn sanitize_log_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `create_dir_all` the spawn-log directory and open the log file
/// for write-from-empty.
pub(super) fn open_spawn_log(log_path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating spawn-log directory at {}", parent.display())
        })?;
    }
    std::fs::File::create(log_path)
        .with_context(|| format!("creating spawn-log file at {}", log_path.display()))
}

/// Read up to the last `max_bytes` of `path` and return it as a
/// UTF-8 string (lossy).
#[must_use]
pub fn read_tail_string(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Sort sessions newest-first (descending `created_at_ms`). Ties broken
/// by id descending so the order is deterministic.
pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    });
}

/// Pure: take a slice of lines, return the last `n`.
#[must_use]
pub fn tail_lines(input: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = input.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| (*s).to_string()).collect()
}

/// Read the most-recently-modified `.log` under `logs_dir` and return
/// its last `n` lines.
pub(super) fn read_latest_log_tail(logs_dir: &Path, n: usize) -> Vec<String> {
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

/// "" for `n == 1`, "s" otherwise.
#[must_use]
pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
