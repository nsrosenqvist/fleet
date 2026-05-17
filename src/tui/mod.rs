//! `fleet ui` — ratatui session browser.
//!
//! Module layout (mirrors keel's TUI structure, adapted to fleet's scale):
//!
//! - [`app`]      — state + behaviour, no rendering, no I/O setup
//! - [`ui`]       — pure `(&AppState, &mut Frame) -> ()` renderers
//!                  plus the display-side helpers they need
//! - [`theme`]    — colour tokens + small inline-span helpers shared
//!                  across every panel; ported from the pre-AO/Lima TUI
//! - [`input`]    — keyboard dispatch seam between the event loop
//!                  and `AppState`'s mutation methods
//! - [`terminal`] — crossterm lifecycle, event loop, subprocess-suspend
//!
//! Aesthetic inspiration: keel — dark background, rounded borders,
//! section titles inline on the border, dot-separated status bar at the
//! bottom. (The visual restoration to that style is staged in follow-up
//! work; this module split is the prerequisite cleanup.)
//!
//! The only public surface outside the `tui` module is [`run`] (the
//! entry point) and [`build_workflow_run_command`] (used by
//! `cli::autonomous` to detach workflow-run subprocesses).

mod app;
mod input;
mod terminal;
mod theme;
mod ui;

pub use app::build_workflow_run_command;
pub use terminal::run;

#[cfg(test)]
mod tests {
    use super::app::*;
    use super::ui::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use std::path::{Path, PathBuf};

    use crate::orchestrator::store::OrchestratorStore;
    use crate::orchestrator::{OrchestratorSession, OrchestratorState};
    use crate::plans::store::PlanStore;
    use crate::plans::{ItemFailurePolicy, Plan, PlanId, PlanItem, PlanItemState, PlanState};
    use crate::session::store::SessionStore;
    use crate::session::SessionId;
    use crate::session::{IssueContext, Session, SessionState};

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
    fn spawn_log_path_places_files_under_dot_fleet_spawn_logs() {
        let p = spawn_log_path(Path::new("/repo"), "review-only", 1_234);
        assert_eq!(
            p,
            PathBuf::from("/repo/.fleet/spawn-logs/review-only-1234.log")
        );
    }

    #[test]
    fn spawn_log_path_sanitizes_hostile_workflow_names() {
        // A `..` or slash in the name must not escape the log dir or
        // synthesise a directory the picker didn't intend.
        let p = spawn_log_path(Path::new("/repo"), "../oops/name with spaces", 7);
        assert_eq!(
            p,
            PathBuf::from("/repo/.fleet/spawn-logs/.._oops_name_with_spaces-7.log")
        );
    }

    #[test]
    fn read_tail_string_returns_none_on_missing_or_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing file.
        assert!(read_tail_string(&tmp.path().join("absent.log"), 64).is_none());
        // Empty file — read_tail returns None so callers can fall
        // back to a "no stderr captured" message instead of an empty
        // tail block.
        let empty = tmp.path().join("empty.log");
        std::fs::write(&empty, b"").unwrap();
        assert!(read_tail_string(&empty, 64).is_none());
    }

    #[test]
    fn read_tail_string_clamps_to_max_bytes_from_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("tail.log");
        std::fs::write(&p, b"AAAAAAAA-tail").unwrap();
        // 4-byte cap → just the last 4 bytes, no panic on short file
        // with mid-byte-window UTF-8 (lossy decode).
        assert_eq!(read_tail_string(&p, 4).as_deref(), Some("tail"));
        // Large cap → whole file is returned.
        assert_eq!(
            read_tail_string(&p, 1024).as_deref(),
            Some("AAAAAAAA-tail")
        );
    }

    #[test]
    fn open_spawn_log_creates_parent_dir_and_truncates_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(".fleet/spawn-logs/standard-1.log");
        // Pre-existing content in the same path: open_spawn_log
        // should truncate so each spawn's tail isn't polluted by a
        // prior run.
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, b"stale").unwrap();
        let _file = open_spawn_log(&log).unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"");
    }

    #[test]
    fn poll_pending_spawn_surfaces_nonzero_exit_as_error_overlay() {
        // Use `/bin/false` (or `false` on PATH) as a stand-in for a
        // workflow-run subprocess that fails fast: the binary's whole
        // job is to exit 1, which is exactly the "child died before
        // it could write a session row" case the user hit. Hard-coded
        // because tests run on Linux/macOS where both are reliable
        // commodities.
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();

        let log_path = state.root.join(".fleet/spawn-logs/test.log");
        let log_file = open_spawn_log(&log_path).unwrap();
        // Pre-seed the log so read_tail_string can pull a recognisable
        // snippet — `/bin/false` itself prints nothing on stderr.
        std::fs::write(&log_path, b"fatal: invalid reference: HEAD\n").unwrap();
        let stderr = log_file.try_clone().unwrap();
        let child = std::process::Command::new("false")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(stderr))
            .spawn()
            .expect("`false` must be on PATH for this test");
        state.pending_spawn = Some(PendingSpawn {
            name: "review-only".into(),
            child,
            log_path: log_path.clone(),
            started_at_ms: 0,
        });

        // `false` is essentially instant; spin a few cycles to dodge a
        // scheduling hiccup where try_wait() races the process exit.
        for _ in 0..50 {
            state.poll_pending_spawn();
            if state.pending_spawn.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            state.pending_spawn.is_none(),
            "poll should have reaped the exited child",
        );
        match &state.overlay {
            Overlay::Error { message } => {
                assert!(message.contains("review-only"), "got: {message}");
                assert!(
                    message.contains("invalid reference: HEAD"),
                    "tail of captured log should be inlined into the overlay; got: {message}",
                );
                assert!(
                    message.contains(log_path.to_string_lossy().as_ref()),
                    "log path should be cited in the overlay; got: {message}",
                );
            }
            other => panic!("expected Error overlay, got {other:?}"),
        }
        assert!(
            state.status_line.contains("review-only"),
            "status line should name the failed workflow; got: {}",
            state.status_line,
        );
    }

    #[test]
    fn poll_pending_spawn_silently_clears_successful_runs() {
        // `/bin/true` finishes successfully — poll should drop the
        // handle without raising an overlay (the user already saw
        // the "spawned … press `r`" status line, and a row will
        // appear on the next reload).
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());
        let mut state = AppState::new(tmp.path().to_path_buf(), &store).unwrap();
        state.overlay = Overlay::None;

        let log_path = state.root.join(".fleet/spawn-logs/test-ok.log");
        let _ = open_spawn_log(&log_path).unwrap();
        let child = std::process::Command::new("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("`true` must be on PATH for this test");
        state.pending_spawn = Some(PendingSpawn {
            name: "standard".into(),
            child,
            log_path,
            started_at_ms: 0,
        });

        for _ in 0..50 {
            state.poll_pending_spawn();
            if state.pending_spawn.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(state.pending_spawn.is_none());
        assert!(matches!(state.overlay, Overlay::None));
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
            dumped.contains("workers"),
            "background sidebar missing; got:\n{dumped}"
        );
        // Overlay's title and entries on top.
        assert!(
            dumped.contains("spawn workflow"),
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

    // ---- sessions view orchestrator bindings ------------------------

    /// Seed a TempDir-rooted state with one workflow session and an
    /// orchestrator — enough surface to exercise the Tab/j/k/
    /// Enter/Shift+O handlers against real on-disk metadata.
    fn sessions_state_with_one_of_each() -> (tempfile::TempDir, SessionStore, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionStore::at(tmp.path().to_path_buf());
        sessions
            .create(&session("s-1", "standard", SessionState::Running, 100))
            .unwrap();
        let ostore = OrchestratorStore::for_repo(tmp.path());
        ostore.save(&OrchestratorSession::new("claude", 200)).unwrap();
        let state = AppState::new(tmp.path().to_path_buf(), &sessions).unwrap();
        (tmp, sessions, state)
    }

    #[test]
    fn tab_in_sessions_view_toggles_focus_between_workflows_and_orchestrator() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        assert_eq!(state.sessions_focus, SessionsFocus::Workflows);
        let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), &store);
        assert_eq!(state.sessions_focus, SessionsFocus::Orchestrator);
        // First Tab to Orchestrator lands the cursor at row 0
        // (the one orchestrator row).
        assert_eq!(state.orchestrators_list_state.selected(), Some(0));
        let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), &store);
        assert_eq!(state.sessions_focus, SessionsFocus::Workflows);
    }

    #[test]
    fn enter_with_orchestrator_focus_returns_open_orchestrator_action() {
        // Single-session model: Enter on the orchestrator row
        // collapses to OpenOrchestrator (the CLI's reuse-or-spawn
        // command does the right thing regardless of current state).
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        state.toggle_sessions_focus();
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &store,
        );
        assert!(matches!(action, Action::OpenOrchestrator));
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
    fn shift_o_returns_open_orchestrator_from_any_view() {
        let (_tmp, store, mut state) = sessions_state_with_one_of_each();
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(matches!(action, Action::OpenOrchestrator));
        // From the Plans view, too — Shift+O is a global hotkey.
        state.view = View::Plans;
        let action = state.handle_key(
            KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
            &store,
        );
        assert!(matches!(action, Action::OpenOrchestrator));
    }

    // ---- orchestrator sidebar ---------------------------------------

    fn orchestrator(agent: &str, state: OrchestratorState) -> OrchestratorSession {
        let mut s = OrchestratorSession::new(agent, 1);
        s.state = state;
        s
    }

    #[test]
    fn orchestrator_row_label_carries_marker_tmux_name_agent_and_state_word() {
        // The row label always shows the fixed `fl-orchestrator`
        // tmux name — there's no per-id variation in the
        // single-session model.
        let s = orchestrator("claude", OrchestratorState::Active);
        let label = orchestrator_row_label(&s);
        assert!(label.starts_with('◐'), "marker missing: {label}");
        assert!(label.contains("fl-orchestrator"));
        assert!(label.contains("agent=claude"));
        assert!(label.contains("(active)"));
    }

    #[test]
    fn orchestrator_row_label_marker_reflects_state() {
        assert!(
            orchestrator_row_label(&orchestrator("x", OrchestratorState::Detached))
                .starts_with('⏸')
        );
        assert!(
            orchestrator_row_label(&orchestrator("x", OrchestratorState::Closed)).starts_with('✗')
        );
    }
}
