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
//! - `n` — print a hint pointing at `fleet workflow run` (full spawn
//!   picker is Phase 2 work)
//!
//! The TUI is a *browser*, not a workflow driver — it does not run
//! containers itself. Spawning is `fleet workflow run`, attaching is
//! `fleet runtime attach`. Keeps the TUI's surface area small enough
//! that the implementation fits in one file.
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

use crate::repo;
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

    let mut stdout = io::stdout();
    enable_raw_mode().context("enabling terminal raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("constructing ratatui terminal")?;

    // Run the loop, but always tear down terminal state before
    // propagating its result — otherwise an early return leaves the
    // user staring at a useless raw-mode terminal.
    let loop_result = event_loop(&mut terminal, &root, &store);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    loop_result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    root: &Path,
    store: &SessionStore,
) -> Result<i32> {
    let mut state = AppState::new(root.to_path_buf(), store)?;
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
}

impl AppState {
    fn new(root: PathBuf, store: &SessionStore) -> Result<Self> {
        let mut state = Self {
            root,
            sessions: Vec::new(),
            list_state: ListState::default(),
            log_tail: Vec::new(),
            last_selected_id: None,
            status_line: " ready ".to_string(),
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
        self.status_line = format!(" {} sessions ", self.sessions.len());
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
        // Shift+K is the Phase-1 kill affordance — match before the
        // plain-k arm so the modifier discriminates.
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('K')) {
            self.mark_selected_failed(store);
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('r') => {
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
                self.status_line =
                    " spawn: use `fleet workflow run <name> [--issue <id>]` from the shell "
                        .to_string();
                Action::None
            }
            _ => Action::None,
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
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(outer[0]);
    render_sidebar(f, body[0], state);
    render_detail(f, body[1], state);
    render_status(f, outer[1], state);
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
            let label = format!(
                "{} {} {}",
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
        Line::from(""),
        Line::from(Span::styled(
            "log tail:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
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
    let help = "[q] quit  [j/k] nav  [r] reload  [Shift+K] kill  [n] spawn-hint";
    let bar = format!("{help} —{}", state.status_line);
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
}
