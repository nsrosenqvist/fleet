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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::ao::SessionInfo;
use crate::lima::VmStatus;
use crate::process::{ProcessInvoker, RealProcessInvoker};

use super::refresh::{self, RefreshCommand, RefreshUpdate};

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

    pub(super) sessions: Vec<SessionInfo>,
    pub(super) selected: usize,
    pub(super) last_error: Option<String>,
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

    pending_commands: Vec<Command>,
}

impl App {
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(repo_root: &Path) -> Result<Self> {
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            invoker: Arc::new(RealProcessInvoker),
            sessions: Vec::new(),
            selected: 0,
            last_error: None,
            last_info: None,
            ao_up: false,
            vm_status: VmStatus::Missing,
            confirm: None,
            pane_outputs: std::collections::HashMap::new(),
            should_quit: false,
            refresh_cmd_tx: None,
            refresh_update_rx: None,
            refresh_handle: None,
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
                    self.last_error = None;
                    if !self.sessions.is_empty() && self.selected >= self.sessions.len() {
                        self.selected = self.sessions.len() - 1;
                    }
                }
                RefreshUpdate::PaneCapture { session_id, output } => {
                    self.pane_outputs.insert(session_id, output);
                }
                RefreshUpdate::Error(e) => {
                    self.last_error = Some(e);
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

    pub(super) fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selected)
    }

    pub(super) fn selected_session_id(&self) -> Option<String> {
        self.selected_session().and_then(|s| s.id.clone())
    }

    /// Single-line success flash for the status bar; also clears any pending
    /// error so a successful action doesn't sit next to a stale red message.
    pub(super) fn flash_ok(&mut self, msg: impl Into<String>) {
        self.last_info = Some(msg.into());
        self.last_error = None;
    }

    pub(super) fn flash_err(&mut self, msg: impl Into<String>) {
        self.last_error = Some(msg.into());
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
        assert_eq!(app.last_error.as_deref(), Some("ao unreachable"));
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
