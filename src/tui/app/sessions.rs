//! Sessions-view behaviour — Tab/j/k/Enter dispatch across the
//! workers + orchestrator sidebar, kill confirm + apply, log-tail
//! re-read on selection change, and the free helpers (`sort_sessions`,
//! `read_latest_log_tail`, `read_tail_string`, `tail_lines`) that
//! support them.

use crossterm::event::{KeyCode, KeyEvent};
use std::path::{Path, PathBuf};

use crate::repo_config::RepoConfig;
use crate::session::store::SessionStore;
use crate::session::{Session, SessionState, now_ms};

use super::{
    Action, AppState, ConfirmAction, DoctorSnapshot, LOG_TAIL_LINES, Overlay, SessionsFocus, View,
};

impl AppState {
    pub(in crate::tui) fn selected(&self) -> Option<&Session> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    pub(in crate::tui) fn refresh_log_tail(&mut self) {
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

    /// Resolve Shift+T to a tracker-TUI launch. Returns
    /// [`Action::OpenTrackerTui`] for trackers that ship a terminal
    /// UI (`git-bug`, `github`), or sets a status-line hint and
    /// returns [`Action::None`] for the placeholder trackers
    /// (`linear`, `jira`) so the keypress isn't silently dropped.
    pub(in crate::tui) fn tracker_tui_action(&mut self) -> Action {
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

    pub(in crate::tui) fn prompt_kill_selected(&mut self) {
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

    pub(in crate::tui) fn run_confirm_action(
        &mut self,
        action: &ConfirmAction,
        store: &SessionStore,
    ) {
        match action {
            ConfirmAction::KillSelected => self.mark_selected_failed(store),
        }
    }

    pub(in crate::tui) fn handle_key_sessions(
        &mut self,
        key: KeyEvent,
        store: &SessionStore,
    ) -> Action {
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
                crate::tui::ui::state_word(session.state)
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

    pub(in crate::tui) fn toggle_sessions_focus(&mut self) {
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
    pub(in crate::tui) fn move_workflow_selection(&mut self, delta: isize) {
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
pub(in crate::tui) fn read_latest_log_tail(logs_dir: &Path, n: usize) -> Vec<String> {
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
