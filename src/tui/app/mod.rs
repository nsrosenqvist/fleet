//! State + behaviour for the TUI, split per-view (DDD-style).
//!
//! `mod.rs` owns the [`AppState`] struct itself (all per-view state
//! lives behind narrowly-named field slices), the cross-cutting
//! enums ([`View`], [`Action`], [`Overlay`], …), refresh-thread
//! plumbing, and the top-level [`AppState::handle_key`] dispatch.
//! Per-view methods live in sibling modules and are added to
//! [`AppState`] via extra `impl` blocks — Rust lets `impl` blocks
//! span files within a crate, so the type stays one piece while
//! its behaviour fans out.
//!
//! - [`sessions`] — workflows/orchestrator sidebar + detail/output pane
//! - [`plans`]    — plans sidebar + plan detail + ticket detail
//! - [`spawn`]    — spawn-picker modal (Issue / Workflow tabs)
//! - [`doctor`]   — host/adapter/agents snapshot
//!
//! Internals are `pub(super)` so [`super::input`], [`super::ui`],
//! the per-view impl modules, and tests reach them — they're not
//! re-exported outside `tui`. The only externally-public items are
//! [`build_workflow_run_command`] (consumed by `crate::cli::autonomous`).

mod doctor;
mod plans;
mod sessions;
mod spawn;

// Re-exports — anything the input/event-loop side, tests, or sibling
// `tui` modules reach for through `super::app::*` lives here. Types
// and constants used inside `AppState`'s field list need to be in
// scope here too, so the struct declaration below can name them
// without a longer path.
pub(in crate::tui) use doctor::DoctorSnapshot;
pub(in crate::tui) use sessions::sort_sessions;
pub use spawn::build_workflow_run_command;
pub(in crate::tui) use spawn::{IssuesState, PendingSpawn, SpawnPickerState, SpawnTab};

// Test-only re-exports — these support the `super::tests` module's
// pre-split `use super::app::*` import. Outside tests they're either
// unused (helpers tests assert against in isolation) or surfaced via
// other paths.
#[cfg(test)]
pub(in crate::tui) use sessions::{read_latest_log_tail, tail_lines};
#[cfg(test)]
pub(in crate::tui) use spawn::{
    list_workflow_entries, list_workflows_dir, open_spawn_log, read_tail_string, spawn_log_path,
};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use super::refresh::{self, FocusedTarget, RefreshCommand, RefreshInputs, RefreshUpdate};
use crate::autonomous;
use crate::orchestrator::OrchestratorSession;
use crate::orchestrator::store::OrchestratorStore;
use crate::plans::Plan;
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo_config::RepoConfig;
use crate::session::store::SessionStore;
use crate::session::{Session, SessionId, now_ms};

/// How many lines of the most-recently-modified log file to show in the
/// detail pane.
pub(super) const LOG_TAIL_LINES: usize = 25;

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
    /// Spawn picker state — filter input, active tab, async-loaded
    /// issue list, per-tab cursors, workflow overrides. Refreshed
    /// each time the picker opens so a newly-added workflow / new
    /// ticket appears without restarting the TUI. Always populated
    /// (cheap empty state), but only rendered when `view ==
    /// View::Spawn` so other views don't pay the layout cost.
    pub(super) spawn: SpawnPickerState,
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

    /// Most recent `tmux capture-pane -e` snapshot of the orchestrator
    /// session, refreshed by the background thread in
    /// [`super::refresh`]. `None` while the session doesn't exist or
    /// the first tick hasn't completed yet — the renderer falls back
    /// to a muted placeholder.
    pub(super) orchestrator_pane: Option<String>,

    /// Per-worker `tmux capture-pane` snapshots, keyed by session id.
    /// Populated by the refresh thread for sessions launched with
    /// `--detached` (i.e. wrapped in a `fleet-<id>` tmux session); the
    /// worker-output renderer prefers this over the static log-tail
    /// when present. Evicted (set to `None` or removed) when the
    /// tmux session goes away — capture-pane errors flow back as
    /// `WorkerPane { output: None }`.
    pub(super) worker_panes: std::collections::HashMap<SessionId, String>,

    /// Shared refresh-thread inputs (panel size, worker target list,
    /// focused target). Cloned into the thread on spawn; the UI
    /// writes through this same handle to publish state.
    pub(super) refresh_inputs: RefreshInputs,

    /// Refresh-thread plumbing. `Option` so we can take ownership at
    /// shutdown without unsafe. None outside an active event loop —
    /// tests construct `AppState` directly and never start the thread.
    pub(super) refresh_cmd_tx: Option<mpsc::Sender<RefreshCommand>>,
    pub(super) refresh_update_rx: Option<mpsc::Receiver<RefreshUpdate>>,
    pub(super) refresh_handle: Option<JoinHandle<()>>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Quit,
    /// Open the per-repo orchestrator: spawn it if it doesn't exist,
    /// respawn the agent if its tmux pane has died, then attach.
    /// Triggered by Enter on the always-visible orchestrator row in
    /// the sidebar — there's only ever one, so "new" and "attach"
    /// collapse to the same operation.
    OpenOrchestrator,
    /// Suspend the TUI and open the configured tracker's terminal UI:
    /// `git-bug termui` when `tracker: git-bug`, `gh dash` when
    /// `tracker: github`. Triggered by Shift+T. The action carries
    /// the resolved binary/args so the event loop doesn't have to
    /// re-read repo config.
    OpenTrackerTui {
        exe: &'static str,
        args: &'static [&'static str],
    },
    /// Attach to a worker session's tmux pane. Triggered by Enter on
    /// a worker row whose tmux session is live (i.e. it was launched
    /// with `--detached`). Payload is the tmux session name
    /// (`fleet-<id>`) so the event loop can run the attach script
    /// without re-deriving it.
    AttachWorker {
        tmux_name: String,
    },
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
            spawn: SpawnPickerState::empty(),
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
            orchestrator_pane: None,
            worker_panes: std::collections::HashMap::new(),
            refresh_inputs: RefreshInputs::default(),
            refresh_cmd_tx: None,
            refresh_update_rx: None,
            refresh_handle: None,
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
        let doc = deps_store
            .load()
            .unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
        self.status_line = super::ui::render_status_line(&self.sessions, &self.plans);
        // Sessions may have appeared / disappeared on reload, so the
        // worker target list the refresh thread polls needs to track.
        self.publish_refresh_inputs();
        Ok(())
    }

    /// Start the background refresh thread that polls `tmux
    /// capture-pane` against the orchestrator + every active worker.
    /// Called once from [`super::terminal::run`] after
    /// `AppState::new`; tests skip this so they don't shell out to
    /// tmux.
    pub(super) fn spawn_refresh_thread(&mut self) {
        let (cmd_tx, update_rx, handle) = refresh::spawn(self.refresh_inputs.clone());
        self.refresh_cmd_tx = Some(cmd_tx);
        self.refresh_update_rx = Some(update_rx);
        self.refresh_handle = Some(handle);
        // Publish the initial worker target list + focused target so
        // the first tick lines up with the current sidebar state
        // (workers loaded by `reload`, focus set by the constructor).
        self.publish_refresh_inputs();
    }

    /// Tell the refresh thread to exit and join it. Called from the
    /// event loop on Quit / panic teardown.
    pub(super) fn shutdown_refresh_thread(&mut self) {
        if let Some(tx) = self.refresh_cmd_tx.take() {
            let _ = tx.send(RefreshCommand::Shutdown);
        }
        // Drop the receiver so the thread's send-on-shutdown path
        // doesn't block on a full channel.
        self.refresh_update_rx = None;
        if let Some(handle) = self.refresh_handle.take() {
            let _ = handle.join();
        }
    }

    /// Bump the "re-pin window size" flag on the refresh thread.
    /// Called by the event loop after the user detaches from a tmux
    /// session: the attach script flipped tmux to `window-size latest`,
    /// so the window now matches the host terminal — not our
    /// narrower output sub-pane. The next refresh tick re-pins to
    /// panel dims before capturing.
    pub(super) fn request_focused_repin(&self) {
        if let Some(tx) = self.refresh_cmd_tx.as_ref() {
            let _ = tx.send(RefreshCommand::Repin);
        }
    }

    /// Push the current worker session list + focused target down to
    /// the refresh thread. Called by `reload` (when the sidebar
    /// changes) and by focus-change handlers (Tab, j/k that swap
    /// panes).
    pub(super) fn publish_refresh_inputs(&self) {
        // Worker targets: every session whose tmux pane is alive (i.e.
        // it was launched with `--detached`). We can't `tmux
        // has-session` from the UI thread every reload without a
        // visible stall, so the refresh thread itself treats
        // capture-pane errors as "pane gone" and emits a None body.
        // Here we just publish every non-terminal session — the
        // thread filters in flight.
        let workers: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| !s.state.is_terminal())
            .map(|s| s.id.clone())
            .collect();
        if let Ok(mut g) = self.refresh_inputs.worker_targets.lock() {
            *g = workers;
        }
        if let Ok(mut g) = self.refresh_inputs.focused.write() {
            *g = self.focused_target();
        }
    }

    /// What the right-pane preview is currently showing. Drives the
    /// refresh thread's "which window do I resize?" decision.
    pub(super) fn focused_target(&self) -> FocusedTarget {
        match self.sessions_focus {
            SessionsFocus::Orchestrator => FocusedTarget::Orchestrator,
            SessionsFocus::Workflows => self
                .selected()
                .map_or(FocusedTarget::None, |s| FocusedTarget::Worker(s.id.clone())),
        }
    }

    /// Drain any refresh-thread updates that have arrived since the
    /// last tick. Mutates the per-target pane snapshots in place;
    /// the renderer reads them on the next draw.
    pub(super) fn drain_refresh_updates(&mut self) {
        let Some(rx) = self.refresh_update_rx.as_ref() else {
            return;
        };
        while let Ok(update) = rx.try_recv() {
            match update {
                RefreshUpdate::OrchestratorPane(snapshot) => {
                    self.orchestrator_pane = snapshot;
                }
                RefreshUpdate::WorkerPane { session_id, output } => match output {
                    Some(body) => {
                        self.worker_panes.insert(session_id, body);
                    }
                    None => {
                        self.worker_panes.remove(&session_id);
                    }
                },
            }
        }
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

    /// Top-level key dispatch. Kept on the type for test ergonomics —
    /// `state.handle_key(...)` is the idiom every test in
    /// [`super::tests`] uses. The real per-view logic lives in
    /// `handle_key_*` methods on `AppState` defined in sibling
    /// per-view modules.
    pub(super) fn handle_key(&mut self, key: KeyEvent, store: &SessionStore) -> Action {
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_key_overlay(key, store);
        }
        // The spawn picker is a true modal: every key must go to its
        // handler, *including* Shift-chord characters the global
        // shortcuts (Shift+A/T/K) would otherwise eat. Otherwise the
        // user typing `bug` into the filter could accidentally
        // toggle autonomous mode or launch the tracker TUI.
        if self.view == View::Spawn {
            return self.handle_key_spawn(key);
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
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('T')) {
            return self.tracker_tui_action();
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
}
