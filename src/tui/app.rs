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
use std::sync::mpsc;
use std::thread::JoinHandle;

use super::refresh::{self, FocusedTarget, RefreshCommand, RefreshInputs, RefreshUpdate};
use crate::autonomous;
use crate::orchestrator::OrchestratorSession;
use crate::orchestrator::store::OrchestratorStore;
use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanState};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::Capabilities;
use crate::runtime::detect::probe;
use crate::runtime::factory::build_adapter;
use crate::session::store::SessionStore;
use crate::session::{Session, SessionId, SessionState, now_ms};

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

/// Receiver for the background tracker-fetch thread. Aliased so the
/// nested generic doesn't recur in every function signature that
/// touches the picker's async state.
pub(super) type TrackerFetchRx =
    std::sync::mpsc::Receiver<Result<Vec<crate::tracker::Issue>, String>>;

/// Which tab of the spawn picker is active. `Issue` is the default
/// when the tracker is reachable; the picker snaps to `Workflow`
/// automatically when the tracker fetch fails or the plugin is
/// unimplemented, so the user is never stuck on a dead tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpawnTab {
    Issue,
    Workflow,
}

/// Lifecycle of the async tracker fetch driving the Issue tab. The
/// picker renders a distinct placeholder for each non-loaded state so
/// the user knows whether to wait, retry, or fall back to manual id
/// entry / the Workflow tab.
#[derive(Debug, Clone)]
pub(super) enum IssuesState {
    /// Background thread hasn't reported back yet. The picker shows a
    /// "loading issues from tracker…" line; warm-cache rows from
    /// `tickets_by_id` are layered on top so first-open isn't blank.
    Loading,
    /// Tracker returned a (possibly empty) issue list. The picker
    /// filters this slice live as the user types.
    Loaded(Vec<crate::tracker::Issue>),
    /// Tracker errored. Carries the user-facing string so the picker
    /// surfaces it inline instead of forcing the user to look up logs.
    Error(String),
    /// Plugin isn't implemented (Linear, Jira). Carries the configured
    /// plugin name for the inline message.
    Unsupported { plugin: String },
}

/// One row in the workflow list loaded at picker-open. `issueless`
/// is parsed from the workflow YAML's `trigger.issueless` flag so
/// the Workflow tab can filter to "no-ticket-needed" workflows
/// without re-parsing on every keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEntry {
    pub name: String,
    pub issueless: bool,
}

/// Spawn-picker state. Held continuously on `AppState` (empty when
/// the picker isn't open) so re-opens restore the user's last filter
/// + cursor — easier to grade on a glance than re-typing from scratch.
pub(super) struct SpawnPickerState {
    /// Which tab is currently focused. Default `Issue` on open.
    pub(super) tab: SpawnTab,
    /// Filter buffer for the active tab. Issue tab matches on
    /// `human_id` + title; Workflow tab matches on name + description.
    /// Cleared on every open so a previous-session filter doesn't
    /// hide rows the user expects to see.
    pub(super) filter: String,
    /// Tracker fetch state for the Issue tab.
    pub(super) issues: IssuesState,
    /// Cursor inside the filtered issue list.
    pub(super) issue_idx: usize,
    /// Include closed issues. Hidden by default — open repos with
    /// long histories drown the picker otherwise. Toggled with `o`.
    pub(super) show_closed: bool,
    /// Every workflow under `<root>/.fleet/workflows/`, with its
    /// `trigger.issueless` flag preparsed. Used by the Workflow tab
    /// (filters to issueless=true) AND by the Issue tab's `w` override
    /// cycle (every workflow is a valid override target — pairing it
    /// with an issue makes the agent's task concrete).
    pub(super) workflows: Vec<WorkflowEntry>,
    /// Cursor inside the filtered workflow list.
    pub(super) workflow_idx: usize,
    /// Per-issue workflow override, keyed by `human_id`. When set,
    /// trumps `resolve_workflow_for(issue.labels)` for that row.
    /// Cleared on close so a one-off override doesn't leak into the
    /// next session.
    pub(super) workflow_override: std::collections::HashMap<String, String>,
    /// Result channel from the background tracker-fetch thread. `None`
    /// once the state has transitioned out of [`IssuesState::Loading`]
    /// so the drain loop doesn't keep polling a dead receiver.
    pub(super) rx: Option<TrackerFetchRx>,
}

impl SpawnPickerState {
    /// Empty initial state used in [`AppState::new`]. The picker
    /// becomes meaningful only after [`AppState::open_spawn_picker`]
    /// populates the lists and kicks off the fetch.
    pub(super) fn empty() -> Self {
        Self {
            tab: SpawnTab::Issue,
            filter: String::new(),
            issues: IssuesState::Loading,
            issue_idx: 0,
            show_closed: false,
            workflows: Vec::new(),
            workflow_idx: 0,
            workflow_override: std::collections::HashMap::new(),
            rx: None,
        }
    }

    /// Issues that pass the current filter + closed-toggle. The
    /// renderer pulls this fresh on each frame — issues are
    /// short-lived enough (single tracker fetch worth) that the
    /// allocation is cheaper than a stale-cache reconciliation
    /// dance.
    pub(super) fn filtered_issues(&self) -> Vec<&crate::tracker::Issue> {
        let IssuesState::Loaded(all) = &self.issues else {
            return Vec::new();
        };
        all.iter()
            .filter(|i| self.show_closed || i.status != "closed")
            .filter(|i| i.matches(&self.filter))
            .collect()
    }

    /// Issueless workflows that pass the current filter. The
    /// Workflow tab only shows workflows that opt in via
    /// `trigger.issueless: true` — workflows that *need* an issue
    /// (the common case) have to be fired from the Issue tab so the
    /// agent has a concrete task to bind to. Substring match on
    /// the basename.
    pub(super) fn filtered_workflows(&self) -> Vec<&WorkflowEntry> {
        let needle = self.filter.to_lowercase();
        self.workflows
            .iter()
            .filter(|w| w.issueless)
            .filter(|w| needle.is_empty() || w.name.to_lowercase().contains(&needle))
            .collect()
    }

    /// Every workflow basename in the picker, regardless of
    /// `issueless`. Drives the Issue tab's `w` override cycle — when
    /// the user binds a workflow to an issue, every workflow is a
    /// valid override target (issueless is about "can run alone",
    /// not "can run with an issue").
    pub(super) fn all_workflow_names(&self) -> Vec<String> {
        self.workflows.iter().map(|w| w.name.clone()).collect()
    }
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

    /// Resolve Shift+T to a tracker-TUI launch. Returns
    /// [`Action::OpenTrackerTui`] for trackers that ship a terminal
    /// UI (`git-bug`, `github`), or sets a status-line hint and
    /// returns [`Action::None`] for the placeholder trackers
    /// (`linear`, `jira`) so the keypress isn't silently dropped.
    fn tracker_tui_action(&mut self) -> Action {
        use crate::repo_config::Tracker;
        match self.config.tracker {
            Tracker::GitBug => Action::OpenTrackerTui {
                exe: "git-bug",
                args: &["termui"],
            },
            Tracker::Github => Action::OpenTrackerTui {
                exe: "gh",
                args: &["dash"],
            },
            Tracker::Linear | Tracker::Jira => {
                self.status_line = format!(
                    " tracker {} has no local TUI ",
                    self.config.tracker.as_str()
                );
                Action::None
            }
        }
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
                self.enter_worker_attach()
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

    /// Enter on a worker row: attach if the worker has a live tmux
    /// pane (refresh thread is publishing captures for it), otherwise
    /// flash a status-line hint so the keypress isn't silently
    /// dropped. Foreground/CI-spawned workers don't have a tmux
    /// pane and can't be attached to.
    fn enter_worker_attach(&mut self) -> Action {
        let Some(session) = self.selected() else {
            return Action::None;
        };
        if session.state.is_terminal() {
            self.status_line = format!(
                " {} is {} — attach is only for live sessions ",
                session.id,
                super::ui::state_word(session.state)
            );
            return Action::None;
        }
        // `worker_panes` is populated by the refresh thread when
        // capture-pane succeeds; "is there an entry?" is our proxy
        // for "is the tmux pane alive?". This avoids a synchronous
        // `tmux has-session` round-trip on the input thread.
        if !self.worker_panes.contains_key(&session.id) {
            self.status_line = format!(
                " {} has no live tmux pane — was it spawned with --detached? ",
                session.id
            );
            return Action::None;
        }
        Action::AttachWorker {
            tmux_name: crate::session::worker_tmux_name(&session.id),
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
        // Focus changed → tell the refresh thread which tmux window
        // to pin to the panel dims on its next tick.
        self.publish_refresh_inputs();
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
        // The focused session may have changed — re-publish so the
        // refresh thread retargets its resize call.
        self.publish_refresh_inputs();
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
                self.status_line = format!(
                    " plan `{}` → {} ",
                    plan.id,
                    super::ui::plan_state_word(target)
                );
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
        // Esc always cancels. `q` deliberately *isn't* a cancel here —
        // the filter input accepts arbitrary letters, and a user
        // typing "queue" in the filter shouldn't accidentally close
        // the picker.
        if matches!(key.code, KeyCode::Esc) {
            self.view = View::Sessions;
            return Action::None;
        }
        if matches!(key.code, KeyCode::Enter) {
            self.spawn_picker_submit();
            return Action::None;
        }
        if matches!(key.code, KeyCode::Tab) {
            self.spawn.tab = match self.spawn.tab {
                SpawnTab::Issue => SpawnTab::Workflow,
                SpawnTab::Workflow => SpawnTab::Issue,
            };
            self.clamp_spawn_cursor();
            return Action::None;
        }
        if matches!(key.code, KeyCode::Down) {
            self.move_spawn_cursor(1);
            return Action::None;
        }
        if matches!(key.code, KeyCode::Up) {
            self.move_spawn_cursor(-1);
            return Action::None;
        }
        if matches!(key.code, KeyCode::Backspace) {
            self.spawn.filter.pop();
            self.clamp_spawn_cursor();
            return Action::None;
        }
        // Word-erase (^W) and clear (^U) — small QoL on a long filter.
        if matches!(key.code, KeyCode::Char('w')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            while matches!(self.spawn.filter.chars().last(), Some(c) if c.is_ascii_whitespace()) {
                self.spawn.filter.pop();
            }
            while matches!(self.spawn.filter.chars().last(), Some(c) if !c.is_ascii_whitespace()) {
                self.spawn.filter.pop();
            }
            self.clamp_spawn_cursor();
            return Action::None;
        }
        if matches!(key.code, KeyCode::Char('u')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.spawn.filter.clear();
            self.clamp_spawn_cursor();
            return Action::None;
        }
        // Plain-letter shortcuts only fire when nothing's typed in the
        // filter buffer — otherwise `o` would be a toggle instead of a
        // letter the user wanted to type. Empty buffer is a strong
        // signal that the user is navigating, not searching.
        let no_modifier = !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::SHIFT);
        if self.spawn.filter.is_empty() && no_modifier {
            match key.code {
                KeyCode::Char('j') => {
                    self.move_spawn_cursor(1);
                    return Action::None;
                }
                KeyCode::Char('k') => {
                    self.move_spawn_cursor(-1);
                    return Action::None;
                }
                KeyCode::Char('o') if self.spawn.tab == SpawnTab::Issue => {
                    self.spawn.show_closed = !self.spawn.show_closed;
                    self.clamp_spawn_cursor();
                    return Action::None;
                }
                KeyCode::Char('w') if self.spawn.tab == SpawnTab::Issue => {
                    self.cycle_workflow_override();
                    return Action::None;
                }
                _ => {}
            }
        }
        // Any other printable character → filter input.
        if let KeyCode::Char(c) = key.code {
            // Only accept non-control characters; chord-style combos
            // (Ctrl+letter) other than the ones handled above are
            // ignored rather than appended.
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                self.spawn.filter.push(c);
                self.clamp_spawn_cursor();
            }
        }
        Action::None
    }

    /// Move the active tab's cursor by `delta`, wrapping around the
    /// filtered list. No-op on an empty list so the cursor stays at 0
    /// instead of underflowing.
    fn move_spawn_cursor(&mut self, delta: isize) {
        let (current, len) = match self.spawn.tab {
            SpawnTab::Issue => (self.spawn.issue_idx, self.spawn.filtered_issues().len()),
            SpawnTab::Workflow => (
                self.spawn.workflow_idx,
                self.spawn.filtered_workflows().len(),
            ),
        };
        if len == 0 {
            return;
        }
        let len_i = isize::try_from(len).unwrap_or(isize::MAX);
        let cur_i = isize::try_from(current).unwrap_or(0);
        let next = (cur_i + delta).rem_euclid(len_i);
        let next_usize = usize::try_from(next).unwrap_or(0);
        match self.spawn.tab {
            SpawnTab::Issue => self.spawn.issue_idx = next_usize,
            SpawnTab::Workflow => self.spawn.workflow_idx = next_usize,
        }
    }

    /// Cycle the focused issue row's workflow override through the
    /// available `.fleet/workflows/*.yaml` names — followed by a
    /// `None` slot that drops the override entirely (route by
    /// labels again). Lets the user override without leaving the
    /// picker; no nested modal needed.
    fn cycle_workflow_override(&mut self) {
        let filtered = self.spawn.filtered_issues();
        let Some(issue) = filtered.get(self.spawn.issue_idx) else {
            return;
        };
        let id = issue.human_id.clone();
        let current = self.spawn.workflow_override.get(&id).cloned();
        let workflows = self.spawn.all_workflow_names();
        if workflows.is_empty() {
            return;
        }
        let next = current.as_deref().map_or_else(
            || Some(workflows[0].clone()),
            |name| match workflows.iter().position(|w| w == name) {
                Some(i) if i + 1 < workflows.len() => Some(workflows[i + 1].clone()),
                // Past the last workflow → drop the override.
                _ => None,
            },
        );
        match next {
            Some(w) => {
                self.spawn.workflow_override.insert(id, w);
            }
            None => {
                self.spawn.workflow_override.remove(&id);
            }
        }
    }

    /// Submit the picker. Issue tab fires
    /// `fleet workflow run <resolved> --issue <human_id>`; Workflow
    /// tab fires `fleet workflow run <name>` (no issue). Issue-tab
    /// free-text fallback: when the filter matches no row, treat the
    /// buffer as a manual ticket id and fire it through the
    /// configured autonomous default workflow.
    fn spawn_picker_submit(&mut self) {
        match self.spawn.tab {
            SpawnTab::Issue => self.submit_issue_tab(),
            SpawnTab::Workflow => self.submit_workflow_tab(),
        }
    }

    fn submit_issue_tab(&mut self) {
        let filtered = self.spawn.filtered_issues();
        if let Some(issue) = filtered.get(self.spawn.issue_idx) {
            let workflow = self.resolved_workflow_for(issue);
            let id = issue.human_id.clone();
            self.spawn_selected(&workflow, Some(&id));
            return;
        }
        // No filtered row — fall back to the raw filter as a manual
        // ticket id, routed through the default workflow. This is the
        // pre-tracker UX from the old picker.
        let raw = self.spawn.filter.trim().to_string();
        if raw.is_empty() {
            self.status_line = " spawn: no issue selected ".to_string();
            self.view = View::Sessions;
            return;
        }
        let workflow = self.config.autonomous.workflow.clone();
        self.spawn_selected(&workflow, Some(&raw));
    }

    fn submit_workflow_tab(&mut self) {
        let filtered = self.spawn.filtered_workflows();
        let Some(entry) = filtered.get(self.spawn.workflow_idx) else {
            self.status_line = " spawn: no workflow selected ".to_string();
            self.view = View::Sessions;
            return;
        };
        let name = entry.name.clone();
        self.spawn_selected(&name, None);
    }

    /// Reset the picker, populate the workflow list synchronously,
    /// kick the tracker fetch onto a background thread (warm-starting
    /// from `tickets_by_id` so first-open isn't empty), and switch to
    /// the Spawn view.
    ///
    /// Snaps to the Workflow tab when the tracker plugin isn't
    /// implemented — otherwise the user's first Enter would land on
    /// an unhelpful "tracker unsupported" placeholder.
    fn open_spawn_picker(&mut self) {
        let workflows = list_workflow_entries(&self.root);
        // Warm cache from the Plans-view bulk fetch so the user sees
        // something even while the fresh fetch is in flight.
        let warm: Vec<crate::tracker::Issue> =
            self.tickets_by_id.values().cloned().collect::<Vec<_>>();
        let (issues, rx) = self.kick_tracker_fetch(warm);
        let any_issueless = workflows.iter().any(|w| w.issueless);
        let tab = if matches!(issues, IssuesState::Unsupported { .. }) && any_issueless {
            // Unsupported tracker — Issue tab is a dead end. Land on
            // the Workflow tab IFF the user has actually opted at
            // least one workflow into issueless mode; otherwise the
            // Workflow tab is empty too and the Issue-tab "type an id
            // manually" placeholder is the more useful default.
            SpawnTab::Workflow
        } else {
            SpawnTab::Issue
        };
        self.spawn = SpawnPickerState {
            tab,
            filter: String::new(),
            issues,
            issue_idx: 0,
            show_closed: false,
            workflows,
            workflow_idx: 0,
            workflow_override: std::collections::HashMap::new(),
            rx,
        };
        self.view = View::Spawn;
    }

    /// Fire a background thread that calls `tracker.list_issues` and
    /// returns the result via mpsc. Returns the initial picker state
    /// (`Loading` with a warm-cache snapshot when one's available;
    /// `Unsupported` when the configured tracker plugin isn't
    /// implemented).
    fn kick_tracker_fetch(
        &mut self,
        warm: Vec<crate::tracker::Issue>,
    ) -> (IssuesState, Option<TrackerFetchRx>) {
        let Some(tracker) = self.ensure_tracker() else {
            return (
                IssuesState::Unsupported {
                    plugin: self.config.tracker.as_str().to_string(),
                },
                None,
            );
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let root = self.root.clone();
        let tracker = Arc::clone(&tracker);
        std::thread::spawn(move || {
            let result = tracker.list_issues(&root).map_err(|e| format!("{e:#}"));
            // Receiver may have been dropped (picker closed before the
            // fetch finished); send-failure here is fine.
            let _ = tx.send(result);
        });
        // Warm-start: render a sorted snapshot of cached tickets so
        // the picker isn't blank during the fetch. The full Loaded
        // transition happens in `drain_spawn_fetch` once the thread
        // reports back.
        let initial = if warm.is_empty() {
            IssuesState::Loading
        } else {
            let mut warm = warm;
            crate::tracker::sort_open_first(&mut warm);
            IssuesState::Loaded(warm)
        };
        (initial, Some(rx))
    }

    /// Non-blocking poll of the spawn-picker's tracker-fetch channel.
    /// Called from the event-loop tick so the picker transitions
    /// `Loading` → `Loaded` / `Error` without the user having to
    /// press a key. No-op when the picker is closed or already
    /// resolved.
    pub(super) fn drain_spawn_fetch(&mut self) {
        if self.view != View::Spawn {
            return;
        }
        let Some(rx) = self.spawn.rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(issues)) => {
                self.spawn.issues = IssuesState::Loaded(issues);
                self.spawn.issue_idx = 0;
                self.spawn.rx = None;
            }
            Ok(Err(msg)) => {
                // If we already have warm-cache rows showing, keep
                // them — better to show stale issues than dump the
                // user into an error placeholder when there's
                // *something* to act on. Otherwise surface the error.
                if matches!(self.spawn.issues, IssuesState::Loading) {
                    self.spawn.issues = IssuesState::Error(msg);
                }
                self.spawn.rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if matches!(self.spawn.issues, IssuesState::Loading) {
                    self.spawn.issues = IssuesState::Error(
                        "tracker fetch thread exited without sending a result".to_string(),
                    );
                }
                self.spawn.rx = None;
            }
        }
    }

    /// Clamp the active tab's cursor into the filtered-list range.
    /// Called after every filter / `show_closed` / tab change so the
    /// cursor never points past the end of what's actually visible.
    fn clamp_spawn_cursor(&mut self) {
        match self.spawn.tab {
            SpawnTab::Issue => {
                let len = self.spawn.filtered_issues().len();
                self.spawn.issue_idx = if len == 0 {
                    0
                } else {
                    self.spawn.issue_idx.min(len - 1)
                };
            }
            SpawnTab::Workflow => {
                let len = self.spawn.filtered_workflows().len();
                self.spawn.workflow_idx = if len == 0 {
                    0
                } else {
                    self.spawn.workflow_idx.min(len - 1)
                };
            }
        }
    }

    /// Resolve the workflow that would fire for `issue` — the per-row
    /// override (set by `w`) takes precedence, falling back to
    /// `autonomous.resolve_workflow_for(labels)`. Free function-ish
    /// (only reads `self`) so the renderer + submit handler agree on
    /// the same answer.
    pub(super) fn resolved_workflow_for(&self, issue: &crate::tracker::Issue) -> String {
        if let Some(over) = self.spawn.workflow_override.get(&issue.human_id) {
            return over.clone();
        }
        self.config
            .autonomous
            .resolve_workflow_for(&issue.labels)
            .to_string()
    }

    /// Fire `fleet workflow run <workflow> [--issue <id>]` as a
    /// detached subprocess. Common path for both spawn-picker tabs
    /// (Issue passes `Some(human_id)`, Workflow passes `None`) and a
    /// future programmatic caller. The detach + stderr-to-log +
    /// pending_spawn-watcher dance is unchanged from the
    /// workflow-only era; only the command shape grew.
    fn spawn_selected(&mut self, workflow: &str, issue: Option<&str>) {
        let Ok(binary) = std::env::current_exe() else {
            self.status_line =
                " spawn failed: cannot resolve fleet binary path (current_exe) ".to_string();
            self.view = View::Sessions;
            return;
        };

        let started_at_ms = now_ms();
        let log_path = spawn_log_path(&self.root, workflow, started_at_ms);
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

        // Detached: wrap the run in `tmux new-session -d -s
        // fleet-<id>` so the TUI's refresh thread can capture-pane it
        // and Enter can attach for a live view.
        let mut cmd = build_workflow_run_command(&binary, workflow, issue, true);
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

        // Human-readable label includes the bound issue when present
        // so the status line / failure overlay say what was spawned.
        let label = issue.map_or_else(|| workflow.to_string(), |id| format!("{workflow} #{id}"));

        match cmd.spawn() {
            Ok(child) => {
                self.status_line = format!(" spawned `{label}` — press `r` to refresh ");
                self.pending_spawn = Some(PendingSpawn {
                    name: label,
                    child,
                    log_path,
                    started_at_ms,
                });
            }
            Err(err) => {
                self.status_line = format!(" spawn `{label}` failed: {err:#} ");
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
                            status.code().map_or_else(
                                || "(signal)".to_string(),
                                |c| format!("with code {c}")
                            ),
                            pending.log_path.display(),
                            tail.trim_end(),
                        ),
                    };
                    self.status_line = format!(" spawn `{}` failed — press any key ", pending.name);
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

    fn dispatch_autonomous_spawn(&mut self, cmd: &autonomous::SpawnCommand, store: &SessionStore) {
        let Ok(binary) = std::env::current_exe() else {
            self.autonomous.set_status(
                "autonomous: ON · spawn failed: cannot resolve fleet binary (current_exe)",
            );
            return;
        };
        // Detached: TUI-driven autonomous spawns get the same tmux
        // wrapping as a manual spawn so users can attach to watch
        // what the engine picked.
        let mut child =
            build_workflow_run_command(&binary, &cmd.workflow, Some(&cmd.issue.human_id), true);
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

/// Workflow basename + `trigger.issueless` flag for every YAML under
/// `<root>/.fleet/workflows/`. Sorted alphabetically. Parsing
/// failures degrade gracefully to `issueless: false` rather than
/// hiding the workflow entirely — the user can still see it in the
/// override cycle, and the malformed YAML surfaces when they try to
/// run it.
///
/// Parsing once per picker-open is fine: typical repos have 3-10
/// workflow files in the low-KiB range. Loading them lets the picker
/// honour the schema's `issueless` opt-in without re-reading on
/// every keystroke.
#[must_use]
pub fn list_workflow_entries(root: &Path) -> Vec<WorkflowEntry> {
    let names = list_workflows_dir(root);
    let dir = root.join(".fleet/workflows");
    names
        .into_iter()
        .map(|name| {
            let path = dir.join(format!("{name}.yaml"));
            let issueless = match crate::workflow::spec::Workflow::from_path(&path) {
                Ok(wf) => wf.trigger.issueless,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "spawn picker: workflow YAML failed to parse — \
                         treating as non-issueless for the Workflow tab",
                    );
                    false
                }
            };
            WorkflowEntry { name, issueless }
        })
        .collect()
}

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

/// Build the `fleet workflow run <name> [--issue <human_id>]
/// [--detached]` command. `fleet_binary` is the path to the fleet
/// executable (typically `std::env::current_exe()`).
///
/// `detached = true` switches the spawn to the tmux wrapper path
/// (one tmux session per worker, attachable for live view). Used by
/// the TUI spawn picker and autonomous mode so workers get the same
/// preview / attach machinery as the orchestrator. CI-style callers
/// (`fleet workflow run` from a shell) pass `false` and keep the
/// inline blocking semantics they've always had.
#[must_use]
pub fn build_workflow_run_command(
    fleet_binary: &Path,
    workflow_name: &str,
    issue_human_id: Option<&str>,
    detached: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(fleet_binary);
    cmd.args(["workflow", "run", workflow_name]);
    if let Some(id) = issue_human_id {
        cmd.args(["--issue", id]);
    }
    if detached {
        cmd.arg("--detached");
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
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating spawn-log directory at {}", parent.display()))?;
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
