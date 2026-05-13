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


/// Pending destructive action awaiting a y/N confirmation in the status bar.
#[derive(Debug, Clone)]
pub(super) enum Confirm {
    KillSession(String),
    StopAo,
}

impl Confirm {
    pub(super) fn prompt(&self) -> String {
        match self {
            Self::KillSession(id) => format!("Kill session {id}? [y/N]"),
            Self::StopAo => "Stop AO orchestrator + dashboard? [y/N]".to_string(),
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
}

pub struct App {
    pub(super) repo_root: PathBuf,
    pub(super) invoker: Arc<dyn ProcessInvoker>,

    /// Parsed `agent-orchestrator.yaml`, cached for the breadcrumb chip
    /// and the per-cwd project filter. `None` when no file exists in
    /// either the XDG or per-repo location; users `c` to create one.
    pub(super) ao_config: Option<crate::ao::config::AoConfig>,

    /// Path the cached `ao_config` was read from. Used by `c` to know
    /// which file to hand to `$EDITOR`; `None` mirrors `ao_config =
    /// None` and falls back to creating the XDG file on first edit.
    pub(super) ao_config_path: Option<std::path::PathBuf>,

    /// Project key (the map key inside `ao_config.projects`) matching
    /// the directory fleet was launched from. `None` when no project
    /// in the catalog claims this cwd. Drives the session-list filter
    /// and the breadcrumb chip.
    pub(super) current_project_key: Option<String>,

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
        // Eagerly read AO config so the breadcrumb chip + cwd-scope
        // filter have something to work with on the first frame. A
        // parse error here is non-fatal — fleet just operates in
        // "(unscoped)" mode and the user can fix the yaml via `c`.
        let loaded = crate::ao::config::AoConfig::load(repo_root).ok().flatten();
        let (ao_config_path, ao_config) = loaded
            .map_or((None, None), |(p, c)| (Some(p), Some(c)));
        let current_project_key = ao_config
            .as_ref()
            .and_then(|c| c.project_for_cwd(repo_root).map(String::from));
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            invoker: Arc::new(RealProcessInvoker),
            ao_config,
            ao_config_path,
            current_project_key,
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
                RefreshUpdate::Sessions(all) => {
                    // Scope to the project this fleet instance is
                    // pinned to. AO is multi-tenant, but the design
                    // is "one fleet per repo / project" — surfacing
                    // sessions for other projects would just confuse
                    // the user.
                    let s: Vec<SessionInfo> = match &self.current_project_key {
                        Some(key) => all
                            .into_iter()
                            .filter(|info| info.project_id.as_deref() == Some(key.as_str()))
                            .collect(),
                        None => all,
                    };
                    // Drop pane captures whose session is no longer in
                    // our visible set — frees memory for sessions that
                    // either vanished from AO or moved out of scope.
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
    /// Re-read the AO catalog from disk. Called after the user returns
    /// from `c` + `$EDITOR` so an edited yaml takes effect immediately,
    /// and on the `r` keybind for manual refresh.
    pub(super) fn reload_ao_config(&mut self) {
        match crate::ao::config::AoConfig::load(&self.repo_root) {
            Ok(loaded) => {
                let (path, cfg) = loaded.map_or((None, None), |(p, c)| (Some(p), Some(c)));
                self.current_project_key = cfg
                    .as_ref()
                    .and_then(|c| c.project_for_cwd(&self.repo_root).map(String::from));
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
        // Tests that don't opt into the cwd-scoped filter (most of them)
        // expect the session list to flow through unfiltered. App::new
        // resolves `current_project_key` from any catalog yaml the test
        // host happens to have on disk; null it out here so tests stay
        // isolated from the dev's `~/.config/fleet/`.
        app.current_project_key = None;
        app.ao_config = None;
        app.ao_config_path = None;
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
    fn drain_updates_filters_sessions_to_current_project() {
        let mut app = app_with(0);
        app.current_project_key = Some("alpha".to_string());
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);

        let mk = |id: &str, project: &str| SessionInfo {
            id: Some(id.to_string()),
            project_id: Some(project.to_string()),
            ..Default::default()
        };
        tx.send(RefreshUpdate::Sessions(vec![
            mk("sb-1", "alpha"),
            mk("sb-2", "beta"),
            mk("sb-3", "alpha"),
        ]))
        .unwrap();
        app.drain_updates();
        assert_eq!(app.sessions.len(), 2, "only alpha's sessions remain");
        assert!(app.sessions.iter().all(|s| s.project_id.as_deref() == Some("alpha")));
    }

    #[test]
    fn drain_updates_keeps_all_sessions_when_unscoped() {
        let mut app = app_with(0);
        assert!(app.current_project_key.is_none());
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);

        let mk = |id: &str, project: &str| SessionInfo {
            id: Some(id.to_string()),
            project_id: Some(project.to_string()),
            ..Default::default()
        };
        tx.send(RefreshUpdate::Sessions(vec![
            mk("sb-1", "alpha"),
            mk("sb-2", "beta"),
        ]))
        .unwrap();
        app.drain_updates();
        assert_eq!(app.sessions.len(), 2, "unscoped fleet sees everything");
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
