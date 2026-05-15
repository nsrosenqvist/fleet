//! `fleet sessions …` — v2-era session inspection commands.
//!
//! Three entry points:
//! - `list` — enumerate `.fleet/sessions/`, sorted by id (chronological
//!   in practice thanks to `ClockIdSource`'s millisecond prefix).
//! - `show <id>` — pretty-print a session's meta + log file list.
//! - `logs <id> [--node <name>]` — print captured logs. With `--node`,
//!   prints just that one node's log; without, dumps every log file
//!   under the session preceded by a header so they're greppable.
//!
//! Render helpers are pure functions over `Session` + paths so tests can
//! assert on output text without a real filesystem. The `run_*` wrappers
//! do the I/O.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

use crate::repo;
use crate::session::reaper::{self, RealPidProbe, ReapReport};
use crate::session::store::SessionStore;
use crate::session::{Session, SessionId, SessionState, now_ms};

/// CLI entry point for `fleet sessions list`. Sorted by id.
pub fn run_list() -> Result<i32> {
    let store = open_store()?;
    let mut ids = store
        .list()
        .with_context(|| format!("reading {}", store.root().display()))?;
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    let mut rows: Vec<SessionRow> = Vec::with_capacity(ids.len());
    for id in ids {
        match store.load(&id) {
            Ok(session) => rows.push(SessionRow::from_session(&session)),
            // A directory without a meta.json (e.g. crashed mid-create)
            // shouldn't break the list — surface a synthetic "?" row.
            Err(err) => rows.push(SessionRow::unreadable(&id, &err)),
        }
    }
    print!("{}", render_session_list(store.root(), &rows));
    Ok(0)
}

/// CLI entry point for `fleet sessions show <id>`.
pub fn run_show(id: &str) -> Result<i32> {
    let store = open_store()?;
    let session_id = SessionId::new(id);
    let session = store
        .load(&session_id)
        .with_context(|| format!("loading session `{id}`"))?;
    let logs = list_log_files(&store, &session_id)?;
    print!("{}", render_session_show(&session, &store, &logs));
    Ok(0)
}

/// CLI entry point for `fleet sessions logs <id> [--node <name>]`.
pub fn run_logs(id: &str, node: Option<&str>) -> Result<i32> {
    let store = open_store()?;
    let session_id = SessionId::new(id);
    // Ensure the session exists; gives a friendlier error than "no such
    // file" when only the log path is missing.
    let _session = store
        .load(&session_id)
        .with_context(|| format!("loading session `{id}`"))?;
    let logs_dir = store.session_dir(&session_id).join("logs");

    if let Some(name) = node {
        let path = logs_dir.join(format!("{name}.log"));
        if !path.is_file() {
            bail!("no log for node `{name}` in session `{id}` (expected at {})", path.display());
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        print!("{body}");
        return Ok(0);
    }

    // No filter: concatenate every log file under the session, sorted by
    // name, each prefixed with a "=== <name> ===" header so the output
    // remains greppable.
    let logs = list_log_files(&store, &session_id)?;
    if logs.is_empty() {
        eprintln!("(no logs yet for session `{id}`)");
        return Ok(0);
    }
    for log in logs {
        let header = log
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unnamed>");
        println!("=== {header} ===");
        let body = std::fs::read_to_string(&log)
            .with_context(|| format!("reading {}", log.display()))?;
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
    }
    Ok(0)
}

/// CLI entry point for `fleet sessions reap`. Returns 0 regardless of
/// how many sessions were reaped — reaping is recovery, not a failure
/// signal. Exit non-zero only when the sweep itself errors (e.g. the
/// store root is unreadable).
pub fn run_reap() -> Result<i32> {
    let store = open_store()?;
    let probe = RealPidProbe;
    let report = reaper::reap(&store, &probe, now_ms())
        .with_context(|| format!("reaping sessions under {}", store.root().display()))?;
    print!("{}", render_reap_report(&report));
    Ok(0)
}

/// Pure renderer for the reap CLI output. Surfacing wording in a free
/// function keeps it test-asserted without I/O.
#[must_use]
pub fn render_reap_report(report: &ReapReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "scanned {} session(s); reaped {}",
        report.scanned,
        report.reaped.len()
    );
    for r in &report.reaped {
        let _ = writeln!(out, "  - {} -> crashed ({})", r.id, r.reason.summary());
    }
    out
}

/// Open the `SessionStore` for the current repo's fleet root.
fn open_store() -> Result<SessionStore> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    Ok(SessionStore::for_repo(&root))
}

/// Discover `.log` files under a session's logs directory. Returns
/// absolute paths sorted alphabetically; missing dir → empty.
fn list_log_files(store: &SessionStore, id: &SessionId) -> Result<Vec<PathBuf>> {
    let dir = store.session_dir(id).join("logs");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(anyhow!(err).context(format!("reading {}", dir.display()))),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("log") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// One row in the list output. Owned representation so the pure renderer
/// can be tested without instantiating real sessions.
///
/// `cost` is intentionally `Option<f64>`: `None` means "no agent ran or
/// no cost line could be parsed", `Some(0.0)` means "agent reported
/// zero." UI preserves the distinction by rendering `-` vs `$0.00`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub state: String,
    pub workflow: String,
    pub current_node: Option<String>,
    pub cost: Option<f64>,
    pub error: Option<String>,
}

impl SessionRow {
    fn from_session(s: &Session) -> Self {
        Self {
            id: s.id.to_string(),
            state: state_word(s.state).to_string(),
            workflow: s.workflow.clone(),
            current_node: s.current_node.clone(),
            cost: s.total_cost_usd(),
            error: None,
        }
    }

    fn unreadable(id: &SessionId, err: &anyhow::Error) -> Self {
        Self {
            id: id.to_string(),
            state: "?".to_string(),
            workflow: "?".to_string(),
            current_node: None,
            cost: None,
            error: Some(format!("{err:#}")),
        }
    }
}

/// Render an `Option<f64>` USD value with the contract the rest of
/// fleet's CLI uses: `Some(v)` → `$0.42`, `None` → `-`. Free helper
/// so every render site picks the same wording.
#[must_use]
pub fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "-".to_string(), |v| format!("${v:.2}"))
}

/// Sum a slice of optional costs into `(total, samples)`. `total` is
/// the sum of every `Some(v)`; `samples` is how many rows contributed.
/// Used by the list renderer to surface a "lifetime total" line.
#[must_use]
pub fn aggregate_cost(rows: &[SessionRow]) -> (f64, usize) {
    let mut total = 0.0;
    let mut samples = 0usize;
    for row in rows {
        if let Some(v) = row.cost {
            total += v;
            samples += 1;
        }
    }
    (total, samples)
}

/// Pure renderer for `fleet sessions list`. Stable wording.
#[must_use]
pub fn render_session_list(root: &Path, rows: &[SessionRow]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "fleet sessions in {}", root.display());
    if rows.is_empty() {
        out.push_str("  (none yet)\n");
        return out;
    }
    for row in rows {
        let node = row.current_node.as_deref().unwrap_or("-");
        let cost = format_cost(row.cost);
        let _ = writeln!(
            out,
            "  {state:<10} {id}  workflow={workflow}  node={node}  cost={cost}",
            state = row.state,
            id = row.id,
            workflow = row.workflow,
        );
        if let Some(err) = &row.error {
            let _ = writeln!(out, "    ! {err}");
        }
    }
    // Trailing total — only shown when at least one row contributed.
    // Suppressed for "every session reported no cost data" so we
    // don't add an unhelpful "$0.00 across 0 session(s)" line.
    let (total, samples) = aggregate_cost(rows);
    if samples > 0 {
        let _ = writeln!(
            out,
            "  total: ${total:.2} across {samples} session(s) with cost data"
        );
    }
    out
}

/// Pure renderer for `fleet sessions show <id>`. Tests assert on the
/// exact wording so the surface stays scriptable.
#[must_use]
pub fn render_session_show(session: &Session, store: &SessionStore, logs: &[PathBuf]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "session: {}", session.id);
    let _ = writeln!(out, "  workflow:     {}", session.workflow);
    let _ = writeln!(out, "  state:        {}", state_word(session.state));
    let _ = writeln!(
        out,
        "  current node: {}",
        session.current_node.as_deref().unwrap_or("-")
    );
    let _ = writeln!(out, "  created (ms): {}", session.created_at_ms);
    let _ = writeln!(out, "  updated (ms): {}", session.updated_at_ms);
    let _ = writeln!(
        out,
        "  directory:    {}",
        store.session_dir(&session.id).display()
    );
    let _ = writeln!(out, "  total cost:   {}", format_cost(session.total_cost_usd()));
    out.push_str("  node costs:\n");
    if session.node_costs.is_empty() {
        out.push_str("    (no agent cost lines parsed yet)\n");
    } else {
        for (node, usd) in &session.node_costs {
            let _ = writeln!(out, "    - {node:<16} ${usd:.4}");
        }
    }
    out.push_str("  logs:\n");
    if logs.is_empty() {
        out.push_str("    (none yet)\n");
    } else {
        for path in logs {
            let _ = writeln!(out, "    - {}", path.display());
        }
    }
    out
}

/// State → short word. Mirrors [`crate::cli::workflow`]'s helper so both
/// command paths render the same wording. Re-implemented here rather
/// than shared because the wording is a UI contract — duplication is
/// cheap and prevents one renderer's refactor from drifting the other.
const fn state_word(s: SessionState) -> &'static str {
    match s {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::AwaitingGate => "awaiting_gate",
        SessionState::Completed => "completed",
        SessionState::Failed => "failed",
        SessionState::Crashed => "crashed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    fn row(id: &str, state: &str, workflow: &str, node: Option<&str>) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            state: state.to_string(),
            workflow: workflow.to_string(),
            current_node: node.map(str::to_string),
            cost: None,
            error: None,
        }
    }

    fn row_with_cost(id: &str, cost: f64) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            state: "completed".to_string(),
            workflow: "standard".to_string(),
            current_node: None,
            cost: Some(cost),
            error: None,
        }
    }

    #[test]
    fn render_session_list_empty_says_so() {
        let r = render_session_list(Path::new("/repo/.fleet/sessions"), &[]);
        assert!(r.contains("fleet sessions in /repo/.fleet/sessions"));
        assert!(r.contains("(none yet)"));
    }

    #[test]
    fn render_session_list_shows_each_row() {
        let rows = vec![
            row("s-1", "completed", "standard", Some("review")),
            row("s-2", "failed", "hotfix", Some("plan")),
        ];
        let r = render_session_list(Path::new("/r"), &rows);
        assert!(r.contains("completed  s-1  workflow=standard  node=review"));
        assert!(r.contains("failed     s-2  workflow=hotfix  node=plan"));
    }

    #[test]
    fn render_session_list_renders_unreadable_rows_with_error_marker() {
        let row = SessionRow {
            id: "s-broken".into(),
            state: "?".into(),
            workflow: "?".into(),
            current_node: None,
            cost: None,
            error: Some("meta.json missing".into()),
        };
        let r = render_session_list(Path::new("/r"), &[row]);
        assert!(r.contains("?          s-broken"));
        assert!(r.contains("! meta.json missing"));
    }

    #[test]
    fn render_session_list_shows_cost_column() {
        let r = render_session_list(Path::new("/r"), &[row_with_cost("s-1", 0.42)]);
        assert!(r.contains("cost=$0.42"), "got: {r}");
    }

    #[test]
    fn render_session_list_shows_dash_for_missing_cost() {
        let r = render_session_list(
            Path::new("/r"),
            &[row("s-1", "running", "standard", Some("plan"))],
        );
        assert!(r.contains("cost=-"), "got: {r}");
    }

    #[test]
    fn render_session_list_appends_total_when_any_row_has_cost() {
        let rows = vec![
            row_with_cost("s-1", 0.10),
            row_with_cost("s-2", 0.30),
            row("s-3", "running", "standard", Some("plan")), // no cost
        ];
        let r = render_session_list(Path::new("/r"), &rows);
        assert!(
            r.contains("total: $0.40 across 2 session(s) with cost data"),
            "got: {r}"
        );
    }

    #[test]
    fn render_session_list_omits_total_when_no_row_has_cost() {
        // A repo where no agents have run yet should not see a noisy
        // "$0.00 across 0 session(s)" line. Bigger usability than
        // perfect coverage.
        let rows = vec![row("s-1", "running", "standard", Some("plan"))];
        let r = render_session_list(Path::new("/r"), &rows);
        assert!(!r.contains("total:"), "got: {r}");
    }

    #[test]
    fn aggregate_cost_sums_only_some_values() {
        let rows = vec![
            row_with_cost("a", 0.10),
            row("b", "running", "standard", None),
            row_with_cost("c", 0.30),
        ];
        let (total, samples) = aggregate_cost(&rows);
        assert!((total - 0.40).abs() < 1e-9);
        assert_eq!(samples, 2);
    }

    #[test]
    fn aggregate_cost_returns_zero_zero_for_empty() {
        let (total, samples) = aggregate_cost(&[]);
        assert!(total.abs() < 1e-9);
        assert_eq!(samples, 0);
    }

    #[test]
    fn format_cost_some_renders_two_decimals_with_dollar() {
        assert_eq!(format_cost(Some(0.42)), "$0.42");
        assert_eq!(format_cost(Some(0.0)), "$0.00");
        assert_eq!(format_cost(Some(1.5)), "$1.50");
    }

    #[test]
    fn format_cost_none_renders_dash() {
        assert_eq!(format_cost(None), "-");
    }

    #[test]
    fn render_session_list_shows_dash_for_missing_current_node() {
        let r = render_session_list(
            Path::new("/r"),
            &[row("s-1", "created", "standard", None)],
        );
        assert!(r.contains("node=-"));
    }

    #[test]
    fn render_session_show_lists_logs_when_present() {
        let store = SessionStore::at("/r/.fleet/sessions");
        let session = Session::new(SessionId::new("s-x"), "standard", 1);
        let logs = vec![
            PathBuf::from("/r/.fleet/sessions/s-x/logs/plan.log"),
            PathBuf::from("/r/.fleet/sessions/s-x/logs/review.log"),
        ];
        let r = render_session_show(&session, &store, &logs);
        assert!(r.contains("session: s-x"));
        assert!(r.contains("workflow:     standard"));
        assert!(r.contains("state:        created"));
        assert!(r.contains("current node: -"));
        assert!(r.contains("- /r/.fleet/sessions/s-x/logs/plan.log"));
        assert!(r.contains("- /r/.fleet/sessions/s-x/logs/review.log"));
    }

    #[test]
    fn render_session_show_says_no_logs_when_empty() {
        let store = SessionStore::at("/r/.fleet/sessions");
        let session = Session::new(SessionId::new("s-x"), "standard", 1);
        let r = render_session_show(&session, &store, &[]);
        assert!(r.contains("logs:\n    (none yet)\n"));
    }

    #[test]
    fn render_session_show_says_no_cost_when_node_costs_empty() {
        let store = SessionStore::at("/r/.fleet/sessions");
        let session = Session::new(SessionId::new("s-x"), "standard", 1);
        let r = render_session_show(&session, &store, &[]);
        assert!(r.contains("total cost:   -"), "got: {r}");
        assert!(
            r.contains("node costs:\n    (no agent cost lines parsed yet)"),
            "got: {r}"
        );
    }

    #[test]
    fn render_session_show_lists_per_node_costs_and_total() {
        let store = SessionStore::at("/r/.fleet/sessions");
        let mut session = Session::new(SessionId::new("s-x"), "standard", 1);
        session.record_node_cost("plan", 0.10, 2);
        session.record_node_cost("review", 0.32, 3);
        let r = render_session_show(&session, &store, &[]);
        assert!(r.contains("total cost:   $0.42"), "got: {r}");
        // BTreeMap iteration is sorted by key.
        assert!(r.contains("- plan             $0.1000"), "got: {r}");
        assert!(r.contains("- review           $0.3200"), "got: {r}");
    }

    #[test]
    fn list_log_files_returns_empty_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().to_path_buf());
        let id = SessionId::new("s-missing");
        assert!(list_log_files(&store, &id).unwrap().is_empty());
    }

    #[test]
    fn list_log_files_returns_sorted_log_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().to_path_buf());
        let id = SessionId::new("s-1");
        let logs_dir = store.session_dir(&id).join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(logs_dir.join("review.log"), b"r").unwrap();
        std::fs::write(logs_dir.join("plan.log"), b"p").unwrap();
        // Non-log files ignored.
        std::fs::write(logs_dir.join("notes.txt"), b"n").unwrap();
        let logs = list_log_files(&store, &id).unwrap();
        assert_eq!(logs.len(), 2);
        // Sorted alphabetically.
        assert!(logs[0].ends_with("plan.log"));
        assert!(logs[1].ends_with("review.log"));
    }

    #[test]
    fn session_row_from_session_carries_state_word() {
        let mut s = Session::new(SessionId::new("s-1"), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        s.transition_to(SessionState::Completed, 3).unwrap();
        let row = SessionRow::from_session(&s);
        assert_eq!(row.state, "completed");
        assert_eq!(row.workflow, "standard");
    }

    #[test]
    fn session_row_from_session_carries_cost_when_recorded() {
        let mut s = Session::new(SessionId::new("s-1"), "standard", 1);
        s.record_node_cost("plan", 0.20, 2);
        let row = SessionRow::from_session(&s);
        assert_eq!(row.cost, Some(0.20));
    }

    #[test]
    fn session_row_from_session_has_no_cost_when_node_costs_empty() {
        let s = Session::new(SessionId::new("s-1"), "standard", 1);
        let row = SessionRow::from_session(&s);
        assert_eq!(row.cost, None);
    }

    #[test]
    fn render_reap_report_says_zero_when_nothing_reaped() {
        let r = render_reap_report(&ReapReport {
            scanned: 3,
            reaped: vec![],
        });
        assert!(r.contains("scanned 3 session(s); reaped 0"));
        // No per-session lines.
        assert!(!r.contains(" -> crashed"));
    }

    #[test]
    fn render_reap_report_lists_each_reaped_with_reason() {
        use crate::session::reaper::{ReapReason, ReapedSession};
        let r = render_reap_report(&ReapReport {
            scanned: 4,
            reaped: vec![
                ReapedSession {
                    id: SessionId::new("s-a"),
                    reason: ReapReason::NoDriverPid,
                },
                ReapedSession {
                    id: SessionId::new("s-b"),
                    reason: ReapReason::DeadDriver { pid: 1234 },
                },
            ],
        });
        assert!(r.contains("scanned 4 session(s); reaped 2"));
        assert!(r.contains("- s-a -> crashed (no driver_pid recorded)"));
        assert!(r.contains("- s-b -> crashed (driver pid 1234 is dead)"));
    }
}
