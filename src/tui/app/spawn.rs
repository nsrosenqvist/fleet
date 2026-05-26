//! Spawn-picker state + behaviour, plus the autonomous engine glue
//! that fires `fleet workflow run` subprocesses.
//!
//! Why "spawn + autonomous" share a file: both paths converge on
//! [`build_workflow_run_command`] and the detach/pending-spawn dance.
//! Keeping them together avoids the round-trip of plumbing
//! `dispatch_autonomous_spawn` through a sibling module purely for
//! orthogonality.

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::autonomous;
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::session::now_ms;
use crate::session::store::SessionStore;

use super::{Action, AppState, Overlay, TrackerState, View};

/// Cap on how many bytes of a spawn-log file we read for the failure
/// overlay. 4 KiB is enough to surface the typical Rust panic +
/// backtrace tail without flooding the modal.
pub(in crate::tui) const SPAWN_LOG_TAIL_BYTES: u64 = 4 * 1024;

/// Handle to a workflow-run subprocess the TUI is waiting to see the
/// exit status of. `child` is `try_wait`'d each tick; `log_path` points
/// at the file the child's stderr was redirected to so the
/// failure-overlay codepath can read a tail off disk.
pub(in crate::tui) struct PendingSpawn {
    pub(in crate::tui) name: String,
    pub(in crate::tui) child: std::process::Child,
    pub(in crate::tui) log_path: PathBuf,
    pub(in crate::tui) started_at_ms: u64,
}

/// Receiver for the background tracker-fetch thread. Aliased so the
/// nested generic doesn't recur in every function signature that
/// touches the picker's async state.
pub(in crate::tui) type TrackerFetchRx =
    std::sync::mpsc::Receiver<Result<Vec<crate::tracker::Issue>, String>>;

/// Which tab of the spawn picker is active. `Issue` is the default
/// when the tracker is reachable; the picker snaps to `Workflow`
/// automatically when the tracker fetch fails or the plugin is
/// unimplemented, so the user is never stuck on a dead tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum SpawnTab {
    Issue,
    Workflow,
}

/// Lifecycle of the async tracker fetch driving the Issue tab. The
/// picker renders a distinct placeholder for each non-loaded state so
/// the user knows whether to wait, retry, or fall back to manual id
/// entry / the Workflow tab.
#[derive(Debug, Clone)]
pub(in crate::tui) enum IssuesState {
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
pub(in crate::tui) struct SpawnPickerState {
    /// Which tab is currently focused. Default `Issue` on open.
    pub(in crate::tui) tab: SpawnTab,
    /// Filter buffer for the active tab. Issue tab matches on
    /// `human_id` + title; Workflow tab matches on name + description.
    /// Cleared on every open so a previous-session filter doesn't
    /// hide rows the user expects to see.
    pub(in crate::tui) filter: String,
    /// Tracker fetch state for the Issue tab.
    pub(in crate::tui) issues: IssuesState,
    /// Cursor inside the filtered issue list.
    pub(in crate::tui) issue_idx: usize,
    /// Include closed issues. Hidden by default — open repos with
    /// long histories drown the picker otherwise. Toggled with `o`.
    pub(in crate::tui) show_closed: bool,
    /// Every workflow under `<root>/.fleet/workflows/`, with its
    /// `trigger.issueless` flag preparsed. Used by the Workflow tab
    /// (filters to issueless=true) AND by the Issue tab's `w` override
    /// cycle (every workflow is a valid override target — pairing it
    /// with an issue makes the agent's task concrete).
    pub(in crate::tui) workflows: Vec<WorkflowEntry>,
    /// Cursor inside the filtered workflow list.
    pub(in crate::tui) workflow_idx: usize,
    /// Per-issue workflow override, keyed by `human_id`. When set,
    /// trumps `resolve_workflow_for(issue.labels)` for that row.
    /// Cleared on close so a one-off override doesn't leak into the
    /// next session.
    pub(in crate::tui) workflow_override: std::collections::HashMap<String, String>,
    /// Result channel from the background tracker-fetch thread. `None`
    /// once the state has transitioned out of [`IssuesState::Loading`]
    /// so the drain loop doesn't keep polling a dead receiver.
    pub(in crate::tui) rx: Option<TrackerFetchRx>,
}

impl SpawnPickerState {
    /// Empty initial state used in [`AppState::new`]. The picker
    /// becomes meaningful only after [`AppState::open_spawn_picker`]
    /// populates the lists and kicks off the fetch.
    pub(in crate::tui) fn empty() -> Self {
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
    pub(in crate::tui) fn filtered_issues(&self) -> Vec<&crate::tracker::Issue> {
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
    pub(in crate::tui) fn filtered_workflows(&self) -> Vec<&WorkflowEntry> {
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
    fn all_workflow_names(&self) -> Vec<String> {
        self.workflows.iter().map(|w| w.name.clone()).collect()
    }
}

impl AppState {
    pub(in crate::tui) fn handle_key_spawn(&mut self, key: KeyEvent) -> Action {
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
            self.status.flash(" spawn: no issue selected ".to_string());
            self.view = View::Sessions;
            return;
        }
        let workflow = self.config.autonomous.workflow.clone();
        self.spawn_selected(&workflow, Some(&raw));
    }

    fn submit_workflow_tab(&mut self) {
        let filtered = self.spawn.filtered_workflows();
        let Some(entry) = filtered.get(self.spawn.workflow_idx) else {
            self.status.flash(" spawn: no workflow selected ".to_string());
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
    pub(in crate::tui) fn open_spawn_picker(&mut self) {
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
    pub(in crate::tui) fn drain_spawn_fetch(&mut self) {
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
    pub(in crate::tui) fn resolved_workflow_for(&self, issue: &crate::tracker::Issue) -> String {
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
        // Same pre-flight as autonomous / scheduler: refuse to
        // spawn when the configured runtime daemon isn't answering,
        // so a stopped Docker doesn't produce a useless "started"
        // session that fails seconds later at devcontainer build.
        if let Some(Err(msg)) = self
            .doctor
            .as_ref()
            .map(|d| d.engine_reachable.clone())
        {
            self.status.flash_error(format!(" spawn refused: {msg} "));
            self.view = View::Sessions;
            return;
        }
        let binary = match resolve_fleet_binary() {
            Ok(p) => p,
            Err(err) => {
                self.status.flash_error(format!(" spawn failed: {err:#} "));
                self.view = View::Sessions;
                return;
            }
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
                self.status.flash(format!(" spawned `{label}` — press `r` to refresh "));
                self.pending_spawn = Some(PendingSpawn {
                    name: label,
                    child,
                    log_path,
                    started_at_ms,
                });
            }
            Err(err) => {
                self.status.flash_error(format!(" spawn `{label}` failed: {err:#} "));
            }
        }
        self.view = View::Sessions;
    }

    pub(in crate::tui) fn poll_pending_spawn(&mut self) {
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
                    self.status.flash_error(format!(" spawn `{}` failed — press any key ", pending.name));
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

    pub(in crate::tui) fn toggle_autonomous(&mut self) {
        if !self.autonomous.enabled() && self.ensure_tracker().is_none() {
            self.autonomous.set_status(format!(
                "autonomous: cannot enable — tracker `{}` is not implemented yet",
                self.config.tracker.as_str(),
            ));
            self.surface_autonomous_status_change();
            return;
        }
        self.autonomous.toggle();
        // toggle() flips the engine status to the bare "autonomous:
        // ON"/"OFF" toggle string; surface_autonomous_status_change
        // recognises those as steady-state and skips them, so a
        // toggle alone doesn't fire a flash. Any verbose status that
        // was on screen before the toggle has already been dismissed
        // by `input::handle_key`.
        self.surface_autonomous_status_change();
    }

    /// `Shift+L` handler. Flips the scheduler engine's enabled flag
    /// and resets the tick debounce so the very next event-loop
    /// iteration fires a fresh tick instead of waiting out a stale
    /// 10-second window.
    pub(in crate::tui) fn toggle_scheduler(&mut self) {
        self.scheduler.toggle();
        self.last_scheduler_tick = None;
    }

    pub(in crate::tui) fn ensure_tracker(&mut self) -> Option<Arc<dyn crate::tracker::Tracker>> {
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

    pub(in crate::tui) fn autonomous_tick(
        &mut self,
        store: &SessionStore,
        now: std::time::Instant,
    ) {
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
        self.surface_autonomous_status_change();
    }

    /// Compare the engine's current `status()` to the mirror; if it
    /// changed AND the new value is more than the bare on/off
    /// toggle, route it through the [`StatusBar`] so the verbose
    /// part ("spawned X for #Y", "tracker error: …") decays in 4s
    /// like every other flash. The compact "auto:●" badge in the
    /// breadcrumb still shows the engine's enabled state regardless.
    pub(in crate::tui) fn surface_autonomous_status_change(&mut self) {
        let current = self.autonomous.status();
        if current == self.last_seen_autonomous_status {
            return;
        }
        let snapshot = current.to_string();
        self.last_seen_autonomous_status.clone_from(&snapshot);
        // The bare "autonomous: ON" / "autonomous: OFF" are
        // steady-state toggle indicators, not action results — those
        // are surfaced by the breadcrumb dot, not the bottom bar, so
        // skip them.
        if snapshot == "autonomous: ON" || snapshot == "autonomous: OFF" {
            return;
        }
        // Failure-flavoured engine status (spawn refused, tracker
        // error, daemon unreachable, cannot enable) should stick
        // so the operator can read / copy the diagnostic.
        if snapshot.contains("failed")
            || snapshot.contains("error")
            || snapshot.contains("refused")
            || snapshot.contains("rejected")
            || snapshot.contains("cannot enable")
        {
            self.status.flash_error(snapshot);
        } else {
            self.status.flash(snapshot);
        }
    }

    /// Parallel to [`Self::autonomous_tick`] for the loop scheduler.
    /// Debounced to 10 seconds wall-clock — the TUI event loop runs
    /// ~10×/s, but each tick reloads workflows + sessions + state
    /// from disk, and `loop:` intervals are at least 60s so a finer
    /// cadence buys nothing. The `last_scheduler_tick` `None` ⇒
    /// "fire immediately" path lets a fresh `Shift+L` produce
    /// instant feedback.
    pub(in crate::tui) fn scheduler_tick(
        &mut self,
        store: &SessionStore,
        now: std::time::Instant,
    ) {
        if !self.scheduler.enabled() {
            return;
        }
        const TICK_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(10);
        if let Some(last) = self.last_scheduler_tick {
            if now.duration_since(last) < TICK_DEBOUNCE {
                return;
            }
        }
        self.last_scheduler_tick = Some(now);

        // Same pre-flight as the autonomous engine: if the runtime
        // daemon is unreachable, surface why instead of spawning
        // sessions that will all fail at `devcontainer build`.
        if let Some(Err(msg)) = self
            .doctor
            .as_ref()
            .map(|d| d.engine_reachable.clone())
        {
            self.scheduler.set_status(format!("scheduler: ON · {msg}"));
            return;
        }

        let workflows = match self.load_loop_workflows() {
            Ok(w) => w,
            Err(err) => {
                self.scheduler.set_status(format!(
                    "scheduler: ON · workflow load failed: {err:#}",
                ));
                return;
            }
        };
        if workflows.is_empty() {
            self.scheduler
                .set_status("scheduler: ON · no workflows with `loop:` configured");
            return;
        }

        let state_store = crate::scheduler::store::SchedulerStateStore::for_fleet_dir(
            &self.root.join(".fleet"),
        );
        let mut state = match state_store.load() {
            Ok(s) => s,
            Err(err) => {
                self.scheduler
                    .set_status(format!("scheduler: ON · state load failed: {err:#}"));
                return;
            }
        };

        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        let code_host = crate::code_host::build(self.config.code_host, invoker, &self.root);
        let root = self.root.clone();
        let list_prs = |_workflow: &str, filter: &crate::code_host::PrFilter| {
            let host = code_host
                .as_ref()
                .ok_or_else(|| "no code host configured for pr-list".to_string())?;
            host.list_prs(&root, filter).map_err(|e| format!("{e:#}"))
        };
        let outcome = self.scheduler.step(
            std::time::SystemTime::now(),
            now,
            &workflows,
            &self.sessions,
            &state,
            list_prs,
        );
        let (spawns, marks) = match outcome {
            crate::scheduler::SchedulerOutcome::Idle => return,
            crate::scheduler::SchedulerOutcome::WaitedFor(reason) => match reason {
                crate::scheduler::SkipReason::NoWorkflowDue => return,
                crate::scheduler::SkipReason::AllSuppressedOrEmpty { marks } => {
                    (Vec::new(), marks)
                }
            },
            crate::scheduler::SchedulerOutcome::Spawn { spawns, marks } => (spawns, marks),
        };
        for (name, ts) in &marks {
            state.last_run_at.insert(name.clone(), *ts);
        }
        if let Err(err) = state_store.save(&state) {
            self.scheduler
                .set_status(format!("scheduler: ON · state save failed: {err:#}"));
        }
        if !spawns.is_empty() {
            let binary = match resolve_fleet_binary() {
                Ok(p) => p,
                Err(err) => {
                    self.scheduler
                        .set_status(format!("scheduler: ON · spawn failed: {err:#}"));
                    return;
                }
            };
            for res in crate::scheduler::dispatcher::dispatch_spawns(&spawns, &binary) {
                if let Err(err) = res {
                    self.scheduler
                        .set_status(format!("scheduler: ON · spawn failed: {err:#}"));
                }
            }
            if let Err(err) = self.reload(store) {
                tracing::warn!(?err, "scheduler post-spawn reload failed");
            }
        }
    }

    /// Load every parseable workflow under `.fleet/workflows/` that
    /// declares a `loop:` interval. Per-file parse failures are
    /// logged via the status line and skipped — one bad YAML
    /// shouldn't disable the whole scheduler.
    fn load_loop_workflows(&self) -> anyhow::Result<Vec<crate::workflow::spec::Workflow>> {
        let wf_dir = self.root.join(".fleet/workflows");
        let entries = match std::fs::read_dir(&wf_dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(anyhow::anyhow!(err)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }
            match crate::workflow::spec::Workflow::from_path(&path) {
                Ok(wf) if wf.loop_interval.is_some() => out.push(wf),
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(?err, path = %path.display(), "scheduler: skipping bad workflow");
                }
            }
        }
        Ok(out)
    }

    fn dispatch_autonomous_spawn(&mut self, cmd: &autonomous::SpawnCommand, store: &SessionStore) {
        // Pre-flight the runtime. Without this, a stopped Docker
        // daemon turns into N silently-failing sessions accumulating
        // on disk because the engine has no way to know the spawn
        // can never succeed. Reuses the cached doctor snapshot; the
        // engine_reachable result is refreshed on every reload, so
        // a "user just started Docker" recovery only needs an `r`.
        if let Some(Err(msg)) = self
            .doctor
            .as_ref()
            .map(|d| d.engine_reachable.clone())
        {
            self.autonomous
                .set_status(format!("autonomous: ON · {msg}"));
            return;
        }
        let binary = match resolve_fleet_binary() {
            Ok(p) => p,
            Err(err) => {
                self.autonomous
                    .set_status(format!("autonomous: ON · spawn failed: {err:#}"));
                return;
            }
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
/// Resolve the path of the running fleet binary, defending against
/// Linux's ` (deleted)` suffix that `current_exe()` returns when the
/// in-process executable was unlinked (typical for a `cargo build`
/// rebuild while the TUI is running). Strips the suffix and verifies
/// the path still resolves; otherwise surfaces a clear "binary moved"
/// error so the status bar can flash something useful.
///
/// Used by every TUI code path that spawns `fleet workflow run …` as
/// a subprocess: spawn-picker, autonomous-mode dispatcher,
/// scheduler dispatcher.
pub fn resolve_fleet_binary() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let raw = std::env::current_exe().context("resolving current fleet binary via current_exe()")?;
    let s = raw.to_string_lossy();
    let stripped = s.strip_suffix(" (deleted)").unwrap_or(&s);
    let path = PathBuf::from(stripped.to_string());
    if !path.is_file() {
        anyhow::bail!(
            "fleet binary `{}` is gone — was the TUI's binary rebuilt or moved \
             while it was running? Relaunch fleet to pick up the new path.",
            path.display(),
        );
    }
    Ok(path)
}

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
pub fn open_spawn_log(log_path: &Path) -> Result<std::fs::File> {
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
