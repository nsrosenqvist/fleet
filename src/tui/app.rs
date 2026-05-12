//! TUI state.
//!
//! Pure state + the mutation helpers that exclusively touch it. The render
//! path lives in [`crate::tui::ui`]; the crossterm event loop and the
//! command executor live in [`crate::tui::terminal`]; the per-mode key
//! handlers in [`crate::tui::input`]; the background refresh thread in
//! [`crate::tui::refresh`].
//!
//! Side effects that need to leave the alternate screen (attach, edit
//! config, …) are deferred via [`Command`]: input handlers `push_command`,
//! and the event loop drains them after each draw so the input layer never
//! touches the terminal handle.

use anyhow::Result;
use ratatui::layout::Rect;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ao::SessionInfo;
use crate::lima::VmStatus;
use crate::process::{ProcessInvoker, RealProcessInvoker};

use super::refresh::{self, RefreshCommand, RefreshUpdate};

/// Maximum gap between two clicks on the same target to count as a
/// double-click. 400 ms matches the desktop default; users mixing keyboard
/// and mouse don't accidentally activate when re-clicking to re-select.
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Identifier for a mouse-clickable region. Stored in `last_click` so a
/// double-click only fires when the *same* target is clicked twice — a
/// click on row 2 followed by a click on row 5 within 400 ms is a select,
/// not an activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClickTarget {
    SidebarItem(usize),
}

/// Outcome of `resolve_click`. `Select` for the first click in a fresh
/// double-click window; `Activate` for a second click on the same target
/// within the window. Reset after an activation so triple-click doesn't
/// re-activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClickKind {
    Select,
    Activate,
}

/// Top-level view the TUI is showing right now. View switches happen on
/// global keys (`Shift+C` / `Shift+H`) and survive across refresh ticks
/// so a slow probe doesn't bounce the user back to Sessions mid-edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum View {
    #[default]
    Sessions,
    Config,
}

/// Which form field the Config view currently has focused. Order
/// matters — Tab cycles in declaration order.
///
/// `ProjectSelection` sits at the top of the project panel; the
/// trailing `Project*` variants are detail rows of whichever project
/// is currently selected by that cycle field. Cycling away from
/// `ProjectSelection` keeps the per-project fields pointed at the same
/// project so the user can keep editing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ConfigField {
    #[default]
    Agent,
    Runtime,
    Workspace,
    Port,
    ProjectSelection,
    ProjectName,
    ProjectSessionPrefix,
    ProjectPath,
    ProjectDefaultBranch,
    ProjectAgentRulesFile,
    ProjectAgent,
}

impl ConfigField {
    pub(super) const ALL: &'static [Self] = &[
        Self::Agent,
        Self::Runtime,
        Self::Workspace,
        Self::Port,
        Self::ProjectSelection,
        Self::ProjectName,
        Self::ProjectSessionPrefix,
        Self::ProjectPath,
        Self::ProjectDefaultBranch,
        Self::ProjectAgentRulesFile,
        Self::ProjectAgent,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Runtime => "runtime",
            Self::Workspace => "workspace",
            Self::Port => "port",
            Self::ProjectSelection => "project",
            Self::ProjectName => "name",
            Self::ProjectSessionPrefix => "sessionPrefix",
            Self::ProjectPath => "path",
            Self::ProjectDefaultBranch => "defaultBranch",
            Self::ProjectAgentRulesFile => "agentRulesFile",
            Self::ProjectAgent => "agent override",
        }
    }

    /// True when this field targets the currently selected project
    /// rather than the defaults block. Used by the renderer to skip
    /// per-project rows when no projects exist, and by edit handlers
    /// to look up the right mutable target.
    pub(super) fn is_project_field(self) -> bool {
        matches!(
            self,
            Self::ProjectName
                | Self::ProjectSessionPrefix
                | Self::ProjectPath
                | Self::ProjectDefaultBranch
                | Self::ProjectAgentRulesFile
                | Self::ProjectAgent,
        )
    }

    pub(super) fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub(super) fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Known values for each enum field, in cycle order. The trailing `""`
/// represents "unset — fall back to AO's internal default" which is a
/// legitimate state worth being able to reach from the form.
pub(super) const AGENT_VALUES: &[&str] = &["claude-code", "codex", "aider", ""];
pub(super) const RUNTIME_VALUES: &[&str] = &["tmux", ""];
pub(super) const WORKSPACE_VALUES: &[&str] = &["worktree", "in-place", ""];

/// Form state for the Config view. Wraps a writable draft of the
/// parsed AO config so edits don't touch `app.ao_config` until the
/// user saves; the original cached copy stays as the comparison
/// baseline that determines whether the form is "dirty."
#[derive(Debug, Clone)]
pub(super) struct ConfigForm {
    pub(super) draft: crate::ao::config::AoConfig,
    pub(super) focus: ConfigField,
    /// `Some` when the user is mid-text-edit on a string/number field.
    /// Carries the in-progress string; commit on Enter, revert on Esc.
    pub(super) editing: Option<String>,
    /// Which project's detail rows are visible right now. Sorted-key
    /// index into `draft.projects`. Survives across form-field focus
    /// changes — moving Tab away from `ProjectSelection` doesn't reset
    /// which project you were inspecting.
    pub(super) selected_project_idx: usize,
}

impl ConfigForm {
    pub(super) fn new(cfg: crate::ao::config::AoConfig) -> Self {
        Self {
            draft: cfg,
            focus: ConfigField::default(),
            editing: None,
            selected_project_idx: 0,
        }
    }

    /// Sorted project keys — drives the project-selector cycling and
    /// the "(N of M)" indicator. Sorted (`BTreeMap`) so the order is
    /// stable across reloads.
    pub(super) fn project_keys(&self) -> Vec<&str> {
        self.draft.projects.keys().map(String::as_str).collect()
    }

    /// Currently selected project's `(key, value)` pair, if any.
    /// Clamps the index in case the project list shrunk since the
    /// last frame (e.g. a save that removed an entry).
    pub(super) fn selected_project(&self) -> Option<(&str, &crate::ao::config::Project)> {
        let keys = self.project_keys();
        if keys.is_empty() {
            return None;
        }
        let idx = self.selected_project_idx.min(keys.len() - 1);
        let key = keys[idx];
        self.draft.projects.get_key_value(key).map(|(k, v)| (k.as_str(), v))
    }

    /// True when the draft has diverged from the on-disk yaml. Drives
    /// the "(modified)" indicator and gates the save flow.
    pub(super) fn is_dirty(&self, original: &crate::ao::config::AoConfig) -> bool {
        // Serialise both and compare strings. Simpler than implementing
        // a manual deep-equal across the schema and round-trips the same
        // shape we'd write to disk on save, so "no diff" is conservative.
        let a = serde_yml::to_string(&self.draft).unwrap_or_default();
        let b = serde_yml::to_string(original).unwrap_or_default();
        a != b
    }

    /// Cycle to the next known value for an enum field. `delta = 1`
    /// for "next", `-1` for "prev" so Tab + Shift-Tab don't have to
    /// reach into the cycle table themselves.
    pub(super) fn cycle_focused(&mut self, delta: i32) {
        match self.focus {
            ConfigField::Agent => {
                self.draft.defaults.agent = cycle_value(
                    AGENT_VALUES,
                    self.draft.defaults.agent.as_deref(),
                    delta,
                );
            }
            ConfigField::Runtime => {
                self.draft.defaults.runtime = cycle_value(
                    RUNTIME_VALUES,
                    self.draft.defaults.runtime.as_deref(),
                    delta,
                );
            }
            ConfigField::Workspace => {
                self.draft.defaults.workspace = cycle_value(
                    WORKSPACE_VALUES,
                    self.draft.defaults.workspace.as_deref(),
                    delta,
                );
            }
            // Text-edited fields don't respond to cycle keystrokes.
            ConfigField::Port
            | ConfigField::ProjectName
            | ConfigField::ProjectSessionPrefix
            | ConfigField::ProjectPath
            | ConfigField::ProjectDefaultBranch
            | ConfigField::ProjectAgentRulesFile
            | ConfigField::ProjectAgent => {}
            ConfigField::ProjectSelection => {
                let n = self.draft.projects.len();
                if n == 0 {
                    return;
                }
                // Unsigned modulo: shift by `delta` rounded into `[0, n)`.
                self.selected_project_idx = if delta >= 0 {
                    (self.selected_project_idx + delta.unsigned_abs() as usize) % n
                } else {
                    (self.selected_project_idx + n - (delta.unsigned_abs() as usize % n)) % n
                };
            }
        }
    }

    /// Start a text edit on the focused field, seeding the buffer
    /// with the current value. No-op on enum fields (which cycle
    /// instead) and on the project-selector field (which cycles).
    pub(super) fn begin_edit(&mut self) {
        let initial: String = match self.focus {
            ConfigField::Port => self
                .draft
                .port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            ConfigField::ProjectName => self
                .selected_project()
                .map(|(_, p)| p.name.clone())
                .unwrap_or_default(),
            ConfigField::ProjectSessionPrefix => self
                .selected_project()
                .and_then(|(_, p)| p.session_prefix.clone())
                .unwrap_or_default(),
            ConfigField::ProjectPath => self
                .selected_project()
                .map(|(_, p)| p.path.display().to_string())
                .unwrap_or_default(),
            ConfigField::ProjectDefaultBranch => self
                .selected_project()
                .and_then(|(_, p)| p.default_branch.clone())
                .unwrap_or_default(),
            ConfigField::ProjectAgentRulesFile => self
                .selected_project()
                .and_then(|(_, p)| p.agent_rules_file.clone())
                .unwrap_or_default(),
            ConfigField::ProjectAgent => self
                .selected_project()
                .and_then(|(_, p)| p.agent.clone())
                .unwrap_or_default(),
            // Enum / cycle fields don't have a text editor.
            ConfigField::Agent
            | ConfigField::Runtime
            | ConfigField::Workspace
            | ConfigField::ProjectSelection => return,
        };
        self.editing = Some(initial);
    }

    /// Commit the current text-edit buffer back into the draft. Bad
    /// input on numeric fields drops back into "(unset)" rather than
    /// rejecting the keystroke — the user can re-edit if they meant
    /// something specific. Project-field commits write into the
    /// currently selected project; if it vanished between begin/commit
    /// (rare, but possible if an external edit removed it), the buffer
    /// is silently dropped.
    pub(super) fn commit_edit(&mut self) {
        let Some(buf) = self.editing.take() else { return };
        match self.focus {
            ConfigField::Port => {
                let trimmed = buf.trim();
                self.draft.port = if trimmed.is_empty() {
                    None
                } else {
                    trimmed.parse::<u16>().ok()
                };
            }
            ConfigField::ProjectName
            | ConfigField::ProjectSessionPrefix
            | ConfigField::ProjectPath
            | ConfigField::ProjectDefaultBranch
            | ConfigField::ProjectAgentRulesFile
            | ConfigField::ProjectAgent => {
                let Some(key) = self.selected_project_key() else {
                    return;
                };
                let Some(project) = self.draft.projects.get_mut(&key) else {
                    return;
                };
                let trimmed = buf.trim();
                let opt_string = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                match self.focus {
                    ConfigField::ProjectName => {
                        // `name` is non-optional in the schema — an
                        // empty buffer reverts to the map key as a
                        // safe-ish placeholder rather than producing
                        // invalid yaml.
                        project.name = opt_string.unwrap_or_else(|| key.clone());
                    }
                    ConfigField::ProjectSessionPrefix => project.session_prefix = opt_string,
                    ConfigField::ProjectPath => {
                        if let Some(s) = opt_string {
                            project.path = std::path::PathBuf::from(s);
                        }
                    }
                    ConfigField::ProjectDefaultBranch => project.default_branch = opt_string,
                    ConfigField::ProjectAgentRulesFile => project.agent_rules_file = opt_string,
                    ConfigField::ProjectAgent => project.agent = opt_string,
                    _ => unreachable!(),
                }
            }
            _ => {}
        }
    }

    pub(super) fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Tab navigation that skips fields with no content to edit.
    /// Today that means: when `draft.projects` is empty, the project
    /// detail fields are all unreachable — Tab walks past them so the
    /// user doesn't land on a row that won't accept input.
    pub(super) fn focus_next(&mut self) {
        self.focus = self.advance(1);
    }

    pub(super) fn focus_prev(&mut self) {
        self.focus = self.advance(-1);
    }

    fn advance(&self, delta: i32) -> ConfigField {
        let mut next = if delta >= 0 {
            self.focus.next()
        } else {
            self.focus.prev()
        };
        // Bounded loop — at most `ALL.len()` skips before we've walked
        // the full cycle. Guards against an unreachable infinite loop
        // if every field somehow gets disabled.
        for _ in 0..ConfigField::ALL.len() {
            if self.field_reachable(next) {
                return next;
            }
            next = if delta >= 0 { next.next() } else { next.prev() };
        }
        self.focus
    }

    fn field_reachable(&self, field: ConfigField) -> bool {
        if field.is_project_field() || matches!(field, ConfigField::ProjectSelection) {
            !self.draft.projects.is_empty()
        } else {
            true
        }
    }

    /// Owned copy of the currently selected project's map key, if any.
    /// Returns an owned `String` rather than a reference to avoid a
    /// long borrow against `self.draft.projects` from the call sites
    /// in `commit_edit` (which then needs to mutate the same map).
    fn selected_project_key(&self) -> Option<String> {
        let keys = self.project_keys();
        if keys.is_empty() {
            return None;
        }
        let idx = self.selected_project_idx.min(keys.len() - 1);
        Some(keys[idx].to_string())
    }
}

fn cycle_value(values: &[&str], current: Option<&str>, delta: i32) -> Option<String> {
    // `values` is a small `&'static [&str]` (4 entries at most for any
    // field today) — the casts here can never overflow in practice.
    let idx = values
        .iter()
        .position(|v| *v == current.unwrap_or(""))
        .unwrap_or(0);
    let len = values.len();
    let next = if delta >= 0 {
        (idx + delta.unsigned_abs() as usize) % len
    } else {
        (idx + len - (delta.unsigned_abs() as usize % len)) % len
    };
    let val = values[next];
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Pending destructive action awaiting a y/N confirmation in the status bar.
#[derive(Debug, Clone)]
pub(super) enum Confirm {
    KillSession(String),
    StopAo,
    SaveAoConfig,
}

impl Confirm {
    pub(super) fn prompt(&self) -> String {
        match self {
            Self::KillSession(id) => format!("Kill session {id}? [y/N]"),
            Self::StopAo => "Stop AO orchestrator + dashboard? [y/N]".to_string(),
            Self::SaveAoConfig => {
                "Save agent-orchestrator.yaml? Comments will be stripped. [y/N]".to_string()
            }
        }
    }
}

/// Deferred side effect produced by an input handler.
///
/// Drained by the event loop *after* the next draw. Anything that needs the
/// alternate screen torn down (interactive subprocesses) goes through here
/// so input handlers stay pure `(&mut App, event) -> ()` functions.
#[derive(Debug, Clone)]
pub(super) enum Command {
    AttachSelected,
    EditConfig,
    TrackerPreview,
    StartAo,
    OpenWeb,
    KillSession(String),
    StopAo,
    RequestRefresh,
    SaveAoConfig,
}

pub struct App {
    pub(super) repo_root: PathBuf,
    pub(super) invoker: Arc<dyn ProcessInvoker>,

    pub(super) view: View,

    /// Parsed `agent-orchestrator.yaml`, cached so the Config view doesn't
    /// re-parse on every draw. `None` when the file is missing; an `Err`
    /// gets folded into [`Self::action_error`] at load time so the user
    /// sees the failure instead of an empty Config panel.
    pub(super) ao_config: Option<crate::ao::config::AoConfig>,

    /// Path the cached `ao_config` was read from. `None` mirrors
    /// `ao_config = None`. Save writes back to this path so an edit
    /// loaded from the central catalog doesn't accidentally land in
    /// the per-repo file (or vice-versa).
    pub(super) ao_config_path: Option<std::path::PathBuf>,

    /// Edit state for the Config view. Built on entry (or reset on `r`
    /// reload) from `ao_config`. `None` when the yaml doesn't exist —
    /// the Config view shows the missing-file panel instead of a form.
    pub(super) config_form: Option<ConfigForm>,

    pub(super) sessions: Vec<SessionInfo>,
    pub(super) selected: usize,
    /// Most recent error from the background refresh thread (probe
    /// failure, etc.). Cleared automatically the next time `Sessions`
    /// arrives successfully — refresh errors are by their nature
    /// transient, so a recovered refresh should make the row green
    /// again without user action.
    pub(super) refresh_error: Option<String>,
    /// Most recent error from a user-triggered action (Shift+S, kill,
    /// etc.). Sticky until the next user action either succeeds (which
    /// sets `last_info`) or fails (which overwrites this). Refresh
    /// ticks must NOT clear this — otherwise an error from `Shift+S`
    /// flashes for ~1.5s and then disappears when the next sessions
    /// snapshot lands, which is exactly what users described as the
    /// "flash and vanish" bug.
    pub(super) action_error: Option<String>,
    pub(super) last_info: Option<String>,
    pub(super) ao_up: bool,
    pub(super) vm_status: VmStatus,
    pub(super) confirm: Option<Confirm>,
    /// Most recent tmux pane capture per session, keyed by session id.
    /// Populated by the background refresh thread; rendered in the output
    /// panel.
    pub(super) pane_outputs: std::collections::HashMap<String, String>,

    pub(super) should_quit: bool,

    // Refresh-thread plumbing. `Option` so we can take ownership on shutdown.
    pub(super) refresh_cmd_tx: Option<mpsc::Sender<RefreshCommand>>,
    pub(super) refresh_update_rx: Option<mpsc::Receiver<RefreshUpdate>>,
    pub(super) refresh_handle: Option<JoinHandle<()>>,

    /// Per-row hit-test rects for the sessions sidebar, populated by the
    /// renderer each frame and read by the mouse handler. `RefCell` because
    /// `ui::render` takes `&App`; off-screen rows store `Rect::default()` so
    /// a click can never match a row the layout couldn't fit.
    pub(super) sidebar_item_rects: RefCell<Vec<Rect>>,

    /// Last mouse-click timestamp + target, used by `resolve_click` to
    /// detect a double-click within `DOUBLE_CLICK_WINDOW`.
    last_click: Option<(Instant, ClickTarget)>,

    pending_commands: Vec<Command>,
}

impl App {
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(repo_root: &Path) -> Result<Self> {
        // Eagerly read AO config so the Config view has something to
        // show on first switch. A parse error here is non-fatal — the
        // view renders the error in place of the kv table.
        let loaded = crate::ao::config::AoConfig::load(repo_root).ok().flatten();
        let (ao_config_path, ao_config) = loaded
            .map_or((None, None), |(p, c)| (Some(p), Some(c)));
        let config_form = ao_config.clone().map(ConfigForm::new);
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            invoker: Arc::new(RealProcessInvoker),
            view: View::default(),
            ao_config,
            ao_config_path,
            config_form,
            sessions: Vec::new(),
            selected: 0,
            refresh_error: None,
            action_error: None,
            last_info: None,
            ao_up: false,
            vm_status: VmStatus::Missing,
            confirm: None,
            pane_outputs: std::collections::HashMap::new(),
            should_quit: false,
            refresh_cmd_tx: None,
            refresh_update_rx: None,
            refresh_handle: None,
            sidebar_item_rects: RefCell::new(Vec::new()),
            last_click: None,
            pending_commands: Vec::new(),
        })
    }

    pub(super) fn spawn_refresh_thread(&mut self) {
        let (cmd_tx, update_rx, handle) =
            refresh::spawn(self.repo_root.clone(), self.invoker.clone());
        self.refresh_cmd_tx = Some(cmd_tx);
        self.refresh_update_rx = Some(update_rx);
        self.refresh_handle = Some(handle);
    }

    pub(super) fn shutdown_refresh_thread(&mut self) {
        if let Some(tx) = self.refresh_cmd_tx.take() {
            let _ = tx.send(RefreshCommand::Shutdown);
        }
        if let Some(handle) = self.refresh_handle.take() {
            let _ = handle.join();
        }
    }

    pub(super) fn drain_updates(&mut self) {
        let Some(rx) = &self.refresh_update_rx else {
            return;
        };
        while let Ok(update) = rx.try_recv() {
            match update {
                RefreshUpdate::Sessions(s) => {
                    // Drop captures for sessions that vanished.
                    let alive: std::collections::HashSet<String> =
                        s.iter().filter_map(|s| s.id.clone()).collect();
                    self.pane_outputs.retain(|k, _| alive.contains(k));
                    self.sessions = s;
                    // A successful refresh clears only the refresh-channel
                    // error. Action errors stay until the next user
                    // action — otherwise pressing Shift+S, seeing the
                    // resulting error, and watching it vanish 1.5s later
                    // makes the failure look like a glitch.
                    self.refresh_error = None;
                    if !self.sessions.is_empty() && self.selected >= self.sessions.len() {
                        self.selected = self.sessions.len() - 1;
                    }
                }
                RefreshUpdate::PaneCapture { session_id, output } => {
                    self.pane_outputs.insert(session_id, output);
                }
                RefreshUpdate::Error(e) => {
                    self.refresh_error = Some(e);
                }
                RefreshUpdate::AoUp(up) => self.ao_up = up,
                RefreshUpdate::VmUp(s) => self.vm_status = s,
            }
        }
    }

    pub(super) fn request_refresh(&self) {
        if let Some(tx) = &self.refresh_cmd_tx {
            let _ = tx.send(RefreshCommand::ForceRefresh);
        }
    }

    /// Re-read `agent-orchestrator.yaml`. Called when the user toggles
    /// into the Config view (catches edits made via `c` + $EDITOR) and
    /// when they press `r` while in that view. Read failures surface as
    /// an action error rather than crashing the cached value to `None`.
    pub(super) fn reload_ao_config(&mut self) {
        match crate::ao::config::AoConfig::load(&self.repo_root) {
            Ok(loaded) => {
                // Reload always resets the form draft — picking up
                // external edits is the whole point of `r`. Unsaved
                // form changes are intentionally lost; the user can
                // press Ctrl+S before reloading if they want to keep
                // them.
                let (path, cfg) = loaded.map_or((None, None), |(p, c)| (Some(p), Some(c)));
                self.config_form = cfg.clone().map(ConfigForm::new);
                self.ao_config = cfg;
                self.ao_config_path = path;
                self.action_error = None;
            }
            Err(e) => {
                self.flash_err(format!("read agent-orchestrator.yaml: {e:#}"));
            }
        }
    }

    pub(super) fn nav_down(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    pub(super) fn nav_up(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
        }
    }

    /// Move selection to an explicit index. Guards against stale rects —
    /// a click that hit-tested against the previous frame's rects might
    /// point at an index that no longer exists if the session list shrunk
    /// in the same draw cycle.
    pub(super) fn select_at(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.selected = idx;
        }
    }

    /// First click on `target` returns `Select`; a second click on the
    /// same target within `DOUBLE_CLICK_WINDOW` returns `Activate`.
    /// Activation resets the timer so a triple-click doesn't re-activate.
    pub(super) fn resolve_click(&mut self, target: ClickTarget) -> ClickKind {
        let now = Instant::now();
        let activate = self
            .last_click
            .is_some_and(|(t, prev)| prev == target && now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
        if activate {
            self.last_click = None;
            ClickKind::Activate
        } else {
            self.last_click = Some((now, target));
            ClickKind::Select
        }
    }

    pub(super) fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selected)
    }

    pub(super) fn selected_session_id(&self) -> Option<String> {
        self.selected_session().and_then(|s| s.id.clone())
    }

    /// Single-line success flash for the status bar; also clears any
    /// pending action error so a successful action doesn't sit next to a
    /// stale red message. Refresh errors are left alone — a successful
    /// user action doesn't make the AO probe magically recover.
    pub(super) fn flash_ok(&mut self, msg: impl Into<String>) {
        self.last_info = Some(msg.into());
        self.action_error = None;
    }

    /// Record an error from a user-triggered action. Sticky until the
    /// next action overwrites it; see the doc on `action_error`.
    pub(super) fn flash_err(&mut self, msg: impl Into<String>) {
        self.action_error = Some(msg.into());
        self.last_info = None;
    }

    /// Fold an action's `Result` into the flash slot. On error keeps the
    /// first line of the rendered chain — the status bar is one row, so
    /// multi-line `{e:#}` would either truncate poorly or wrap.
    pub(super) fn flash_result(&mut self, ok_msg: String, res: anyhow::Result<()>) {
        match res {
            Ok(()) => self.flash_ok(ok_msg),
            Err(e) => self.flash_err(
                format!("{e:#}")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
            ),
        }
    }

    pub(super) fn push_command(&mut self, cmd: Command) {
        self.pending_commands.push(cmd);
    }

    pub(super) fn drain_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.pending_commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            id: Some(id.to_string()),
            ..Default::default()
        }
    }

    fn app_with(n: usize) -> App {
        let mut app = App::new(std::path::Path::new(".")).expect("App::new");
        app.sessions = (0..n).map(|i| session(&format!("sb-{}", i + 1))).collect();
        app
    }

    #[test]
    fn initial_state_has_no_selection_marker_but_index_zero() {
        let app = App::new(std::path::Path::new(".")).expect("ok");
        assert!(app.sessions.is_empty());
        assert_eq!(app.selected, 0);
        assert!(!app.should_quit);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn nav_down_wraps_at_end() {
        let mut app = app_with(3);
        app.nav_down();
        app.nav_down();
        assert_eq!(app.selected, 2);
        app.nav_down();
        assert_eq!(app.selected, 0, "should wrap from last back to first");
    }

    #[test]
    fn nav_up_wraps_from_zero() {
        let mut app = app_with(3);
        app.nav_up();
        assert_eq!(app.selected, 2, "should wrap from 0 to last");
    }

    #[test]
    fn nav_is_a_noop_on_empty_list() {
        let mut app = app_with(0);
        app.nav_down();
        app.nav_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn drain_updates_replaces_sessions_and_clamps_selection() {
        let mut app = app_with(3);
        app.selected = 2;
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("only")]))
            .unwrap();
        app.drain_updates();
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.selected, 0, "selection should clamp to new length");
    }

    #[test]
    fn drain_updates_drops_pane_captures_for_dead_sessions() {
        let mut app = app_with(2);
        app.pane_outputs.insert("sb-1".into(), "live output".into());
        app.pane_outputs
            .insert("ghost".into(), "stale output".into());
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("sb-1")]))
            .unwrap();
        app.drain_updates();
        assert!(app.pane_outputs.contains_key("sb-1"));
        assert!(
            !app.pane_outputs.contains_key("ghost"),
            "vanished session's pane capture should be evicted"
        );
    }

    #[test]
    fn drain_updates_applies_pane_capture() {
        let mut app = app_with(1);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::PaneCapture {
            session_id: "sb-1".into(),
            output: "hello\nworld".into(),
        })
        .unwrap();
        app.drain_updates();
        assert_eq!(
            app.pane_outputs.get("sb-1").map(String::as_str),
            Some("hello\nworld")
        );
    }

    #[test]
    fn drain_updates_records_error() {
        let mut app = app_with(0);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Error("ao unreachable".into()))
            .unwrap();
        app.drain_updates();
        assert_eq!(app.refresh_error.as_deref(), Some("ao unreachable"));
    }

    #[test]
    fn action_error_survives_a_sessions_refresh() {
        // Regression: refresh ticks used to clear the single last_error
        // field, so an error from `Shift+S` would flash for ~1.5s and
        // then vanish on the next sessions snapshot. Action errors are
        // now sticky until the user takes another action.
        let mut app = app_with(0);
        app.flash_err("no secret configured");
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("sb-1")])).unwrap();
        app.drain_updates();
        assert_eq!(app.action_error.as_deref(), Some("no secret configured"));
    }

    #[test]
    fn sessions_refresh_clears_refresh_error_only() {
        // The other half of the split: refresh errors *should* clear on
        // a successful Sessions snapshot — they're transient by nature.
        let mut app = app_with(0);
        app.refresh_error = Some("stale probe failure".into());
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("sb-1")])).unwrap();
        app.drain_updates();
        assert!(app.refresh_error.is_none());
    }

    #[test]
    fn flash_ok_clears_action_error() {
        let mut app = app_with(0);
        app.flash_err("kill failed");
        app.flash_ok("killed sb-1");
        assert_eq!(app.last_info.as_deref(), Some("killed sb-1"));
        assert!(app.action_error.is_none());
    }

    #[test]
    fn drain_updates_applies_ao_and_vm_status() {
        let mut app = app_with(0);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::AoUp(true)).unwrap();
        tx.send(RefreshUpdate::VmUp(VmStatus::Running)).unwrap();
        app.drain_updates();
        assert!(app.ao_up);
        assert_eq!(app.vm_status, VmStatus::Running);
    }

    #[test]
    fn confirm_prompts_name_the_action() {
        let kill = Confirm::KillSession("sb-1".into());
        assert!(kill.prompt().contains("sb-1"));
        assert!(kill.prompt().contains("Kill"));

        let stop = Confirm::StopAo;
        assert!(stop.prompt().contains("Stop AO"));
    }

    #[test]
    fn select_at_clamps_against_empty_session_list() {
        let mut app = app_with(0);
        app.select_at(3);
        assert_eq!(app.selected, 0, "no-op on empty list");
    }

    #[test]
    fn select_at_ignores_out_of_range_index() {
        let mut app = app_with(2);
        app.selected = 1;
        app.select_at(5);
        assert_eq!(app.selected, 1, "out-of-range index must not move selection");
    }

    #[test]
    fn resolve_click_is_select_then_activate_on_same_target() {
        let mut app = app_with(2);
        let target = ClickTarget::SidebarItem(0);
        assert!(matches!(app.resolve_click(target), ClickKind::Select));
        assert!(matches!(app.resolve_click(target), ClickKind::Activate));
    }

    #[test]
    fn resolve_click_resets_after_activation() {
        let mut app = app_with(2);
        let target = ClickTarget::SidebarItem(0);
        app.resolve_click(target);
        app.resolve_click(target);
        // Third click on the same target after an activation starts a new
        // double-click window — it's a Select, not another Activate.
        assert!(matches!(app.resolve_click(target), ClickKind::Select));
    }

    #[test]
    fn resolve_click_different_target_is_select() {
        let mut app = app_with(3);
        app.resolve_click(ClickTarget::SidebarItem(0));
        assert!(matches!(
            app.resolve_click(ClickTarget::SidebarItem(1)),
            ClickKind::Select,
        ));
    }

    fn empty_ao_config() -> crate::ao::config::AoConfig {
        serde_yml::from_str("").unwrap()
    }

    #[test]
    fn config_field_cycles_forward_and_back() {
        assert_eq!(ConfigField::Agent.next(), ConfigField::Runtime);
        assert_eq!(ConfigField::Port.next(), ConfigField::ProjectSelection);
        assert_eq!(
            ConfigField::ProjectSelection.next(),
            ConfigField::ProjectName,
        );
        assert_eq!(
            ConfigField::ProjectAgent.next(),
            ConfigField::Agent,
            "cycle wraps after the last project field",
        );
        assert_eq!(ConfigField::Agent.prev(), ConfigField::ProjectAgent);
    }

    #[test]
    fn focus_next_skips_project_fields_when_none_defined() {
        // Empty project map → Tab should walk Port → Agent, skipping
        // ProjectSelection + the six per-project field variants.
        let cfg = empty_ao_config();
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::Port;
        form.focus_next();
        assert_eq!(
            form.focus,
            ConfigField::Agent,
            "no projects → skip the project section entirely",
        );
    }

    #[test]
    fn focus_next_walks_project_fields_when_present() {
        let yaml = r"
projects:
  one:
    name: one
    path: /tmp/one
";
        let cfg: crate::ao::config::AoConfig = serde_yml::from_str(yaml).expect("parse");
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::Port;
        form.focus_next();
        assert_eq!(form.focus, ConfigField::ProjectSelection);
        form.focus_next();
        assert_eq!(form.focus, ConfigField::ProjectName);
    }

    #[test]
    fn project_name_edit_writes_into_selected_project() {
        let yaml = r"
projects:
  one:
    name: one
    path: /tmp/one
";
        let cfg: crate::ao::config::AoConfig = serde_yml::from_str(yaml).expect("parse");
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::ProjectName;
        form.begin_edit();
        let buf = form.editing.as_mut().expect("editing started");
        buf.clear();
        buf.push_str("renamed");
        form.commit_edit();
        assert_eq!(form.draft.projects.get("one").unwrap().name, "renamed");
    }

    #[test]
    fn project_selection_cycles_through_project_keys() {
        // Two-project config; cycling the selection field should walk
        // sorted keys forward and wrap.
        let yaml = r"
projects:
  alpha:
    name: alpha
    path: /tmp/a
  beta:
    name: beta
    path: /tmp/b
";
        let cfg: crate::ao::config::AoConfig = serde_yml::from_str(yaml).expect("parse");
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::ProjectSelection;
        assert_eq!(form.selected_project_idx, 0);
        form.cycle_focused(1);
        assert_eq!(form.selected_project_idx, 1);
        form.cycle_focused(1);
        assert_eq!(form.selected_project_idx, 0, "wraps");
        form.cycle_focused(-1);
        assert_eq!(form.selected_project_idx, 1, "wraps backwards");
    }

    #[test]
    fn cycle_value_walks_agent_options() {
        let v = cycle_value(AGENT_VALUES, Some("claude-code"), 1);
        assert_eq!(v.as_deref(), Some("codex"));
        let v = cycle_value(AGENT_VALUES, Some("codex"), 1);
        assert_eq!(v.as_deref(), Some("aider"));
        // "" (the unset sentinel) maps to `None`, then wraps to the
        // first non-empty value.
        let v = cycle_value(AGENT_VALUES, Some("aider"), 1);
        assert!(v.is_none(), "expected (unset) after aider");
        let v = cycle_value(AGENT_VALUES, None, 1);
        assert_eq!(v.as_deref(), Some("claude-code"));
    }

    #[test]
    fn cycle_value_walks_backward() {
        let v = cycle_value(AGENT_VALUES, Some("codex"), -1);
        assert_eq!(v.as_deref(), Some("claude-code"));
        let v = cycle_value(AGENT_VALUES, Some("claude-code"), -1);
        assert!(v.is_none());
    }

    #[test]
    fn form_dirty_only_when_draft_diverges() {
        let cfg = empty_ao_config();
        let form = ConfigForm::new(cfg.clone());
        assert!(!form.is_dirty(&cfg));

        let mut form = ConfigForm::new(cfg.clone());
        form.draft.defaults.agent = Some("codex".into());
        assert!(form.is_dirty(&cfg));
    }

    #[test]
    fn cycle_focused_writes_into_draft() {
        let cfg = empty_ao_config();
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::Agent;
        // `None` (unset) is the sentinel at the end of AGENT_VALUES;
        // +1 from there wraps to the first explicit entry.
        form.cycle_focused(1);
        assert_eq!(form.draft.defaults.agent.as_deref(), Some("claude-code"));
        form.cycle_focused(1);
        assert_eq!(form.draft.defaults.agent.as_deref(), Some("codex"));
    }

    #[test]
    fn port_text_edit_commit_parses() {
        let cfg = empty_ao_config();
        let mut form = ConfigForm::new(cfg);
        form.focus = ConfigField::Port;
        form.begin_edit();
        form.editing.as_mut().unwrap().push_str("3001");
        form.commit_edit();
        assert_eq!(form.draft.port, Some(3001));
    }

    #[test]
    fn port_text_edit_cancel_does_not_mutate() {
        let cfg = empty_ao_config();
        let mut form = ConfigForm::new(cfg);
        form.draft.port = Some(3000);
        form.focus = ConfigField::Port;
        form.begin_edit();
        form.editing.as_mut().unwrap().clear();
        form.editing.as_mut().unwrap().push_str("9999");
        form.cancel_edit();
        assert_eq!(form.draft.port, Some(3000), "cancel must revert");
    }

    #[test]
    fn push_and_drain_commands_round_trip() {
        let mut app = app_with(0);
        app.push_command(Command::RequestRefresh);
        app.push_command(Command::AttachSelected);
        let drained = app.drain_commands();
        assert_eq!(drained.len(), 2);
        assert!(matches!(drained[0], Command::RequestRefresh));
        assert!(matches!(drained[1], Command::AttachSelected));
        assert!(
            app.drain_commands().is_empty(),
            "second drain should be empty"
        );
    }
}
