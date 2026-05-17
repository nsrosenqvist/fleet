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
use std::sync::Arc;

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::runtime::factory::build_stopper;
use crate::session::reaper::{self, RealPidProbe, ReapReport};
use crate::session::store::SessionStore;
use crate::session::{Session, SessionId, SessionState, now_ms};
use crate::worktree;

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
            bail!(
                "no log for node `{name}` in session `{id}` (expected at {})",
                path.display()
            );
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
        let body =
            std::fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
    }
    Ok(0)
}

/// CLI entry point for `fleet sessions attach <id>`. Resolves the
/// session's tmux pane (`fleet-<id>`), runs the shared bash attach
/// script — same styled status bar with `ctrl+b d to detach` the
/// orchestrator uses — and `exec tmux attach`s into it. Errors
/// clearly when the pane is gone (workflow exited or was never
/// detached). Doesn't require a TUI to be running — scripted users
/// can `fleet workflow run --detached … | xargs fleet sessions
/// attach`.
pub fn run_attach(id: &str) -> Result<i32> {
    let store = open_store()?;
    let session_id = SessionId::new(id);
    // Load the session record up-front to give a clearer error than
    // "tmux pane gone" when the user typo's an id. We don't need
    // anything from the record beyond confirming it exists.
    let _ = store
        .load(&session_id)
        .with_context(|| format!("loading session `{id}`"))?;

    let tmux_name = crate::session::worker_tmux_name(&session_id);
    let invoker = RealProcessInvoker;
    crate::orchestrator::tmux::probe(&invoker)?;
    if !crate::orchestrator::tmux::has_session(&invoker, &tmux_name) {
        bail!(
            "session `{id}` has no live tmux pane `{tmux_name}` — was it spawned with \
             `fleet workflow run --detached`? Foreground / pre-stage-2 runs can't be attached to."
        );
    }

    let script = crate::cli::orchestrator::build_attach_script(&tmux_name);
    let status = std::process::Command::new("bash")
        .args(["-c", &script])
        .status()
        .with_context(|| format!("running attach script for `{tmux_name}`"))?;
    Ok(status.code().unwrap_or(0))
}

/// CLI entry point for `fleet sessions reap`. Returns 0 regardless of
/// how many sessions were reaped — reaping is recovery, not a failure
/// signal. Exit non-zero only when the sweep itself errors (e.g. the
/// store root is unreadable).
pub fn run_reap() -> Result<i32> {
    let store = open_store()?;
    let probe = RealPidProbe;
    let cwd = std::env::current_dir().context("reading current directory")?;
    let stopper = build_stopper(&repo::fleet_root(&cwd));
    let report = reaper::reap(&store, &probe, stopper.as_ref(), now_ms())
        .with_context(|| format!("reaping sessions under {}", store.root().display()))?;
    print!("{}", render_reap_report(&report));
    Ok(0)
}

/// CLI entry point for `fleet sessions prune [id|--completed|--all]`.
/// Removes the per-session git worktree, clears
/// `worktree_path` on the session's meta.json, and leaves everything
/// else (logs, artifacts, the branch) intact. With
/// `with_branch == true`, also deletes the `fleet/session-<id>`
/// branch (force-deletes — unmerged work on that branch is lost).
/// Returns 0 unless at least one prune attempt failed; per-session
/// errors are surfaced in the report.
pub fn run_prune(id: Option<&str>, completed: bool, all: bool, with_branch: bool) -> Result<i32> {
    if id.is_none() && !completed && !all {
        bail!(
            "fleet sessions prune: specify a session id, --completed, or --all \
             (one of them is required)"
        );
    }
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = SessionStore::for_repo(&root);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);

    let targets = if let Some(s) = id {
        vec![SessionId::new(s)]
    } else if all {
        select_sessions_by_state(
            &store,
            &[
                SessionState::Completed,
                SessionState::Failed,
                SessionState::Crashed,
            ],
        )?
    } else {
        // completed = true (validated above).
        select_sessions_by_state(&store, &[SessionState::Completed])?
    };

    let mut report = PruneReport::default();
    let now = now_ms();
    for id in targets {
        match prune_one(invoker.as_ref(), &root, &store, &id, now, with_branch) {
            Ok(PruneOutcome::Pruned) => report.pruned.push(id),
            Ok(PruneOutcome::AlreadyClean) => report.skipped.push(id),
            Err(err) => report.failed.push((id, format!("{err:#}"))),
        }
    }
    print!("{}", render_prune_report(&report));
    Ok(i32::from(!report.failed.is_empty()))
}

/// Outcome of a single prune attempt. `AlreadyClean` is a no-op —
/// the session never had a worktree, or one has already been pruned;
/// either way nothing changes on disk.
#[derive(Debug, PartialEq, Eq)]
pub enum PruneOutcome {
    Pruned,
    AlreadyClean,
}

/// Bookkeeping for the renderer. Pure data so tests can assert on
/// the surface without spinning up real sessions.
#[derive(Debug, Default)]
pub struct PruneReport {
    pub pruned: Vec<SessionId>,
    pub skipped: Vec<SessionId>,
    pub failed: Vec<(SessionId, String)>,
}

/// Prune one session: remove its worktree (if any), clear the meta
/// pointer, leave logs/artifacts alone. When `with_branch` is true,
/// also delete the `fleet/session-<id>` git branch (force-delete;
/// unmerged work is lost).
///
/// Refuses to prune a still-`Running` session — kill or reap first.
/// All other states (including `AwaitingGate`) are fair game: the
/// user passed the id explicitly, so they meant it.
pub fn prune_one(
    invoker: &dyn ProcessInvoker,
    root: &Path,
    store: &SessionStore,
    id: &SessionId,
    now: u64,
    with_branch: bool,
) -> Result<PruneOutcome> {
    let mut session = store
        .load(id)
        .with_context(|| format!("loading session `{id}`"))?;
    if matches!(session.state, SessionState::Running) {
        bail!(
            "session `{id}` is still running — `fleet sessions reap` or wait for it \
             to finish before pruning"
        );
    }
    let Some(wt_path) = session.worktree_path.clone() else {
        // No worktree to prune — but the user may still want the
        // branch dropped. Honour `with_branch` in that case so
        // `prune --all --with-branch` reaps every fleet/session-*
        // branch even from already-pruned sessions.
        if with_branch {
            if let Some(branch) = session.branch.clone() {
                worktree::delete_branch(invoker, root, &branch, true)?;
                session.clear_branch(now);
                store.save(&session)?;
                return Ok(PruneOutcome::Pruned);
            }
        }
        return Ok(PruneOutcome::AlreadyClean);
    };
    if wt_path.is_dir() {
        worktree::remove_worktree(invoker, root, &wt_path, true)?;
    } else {
        // Worktree dir already gone from disk; drop any stale
        // administrative entries so a future `git worktree list`
        // doesn't surface the ghost. Best-effort: a failure here
        // is not fatal — the meta.json clear below is the
        // user-visible win.
        if let Err(err) = worktree::prune_worktrees(invoker, root) {
            tracing::warn!(error = %err, "pruning stale worktree metadata failed");
        }
    }
    session.clear_worktree_path(now);
    if with_branch {
        if let Some(branch) = session.branch.clone() {
            // Branch deletion failure is hard — the user asked for
            // it explicitly. Surface so they know the branch is
            // still around and they can investigate (uncommitted
            // ref? someone else's checkout? deleted manually?).
            worktree::delete_branch(invoker, root, &branch, true)?;
            session.clear_branch(now);
        }
    }
    store.save(&session)?;
    Ok(PruneOutcome::Pruned)
}

/// Walk the session store and collect ids whose state is in `states`.
/// Sessions whose meta.json fails to parse are silently skipped —
/// `fleet sessions list` surfaces those separately; prune shouldn't
/// fail on read errors elsewhere in the store.
fn select_sessions_by_state(
    store: &SessionStore,
    states: &[SessionState],
) -> Result<Vec<SessionId>> {
    let mut out = Vec::new();
    for id in store
        .list()
        .with_context(|| format!("listing sessions under {}", store.root().display()))?
    {
        if let Ok(s) = store.load(&id) {
            if states.contains(&s.state) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Pure renderer for `fleet sessions prune`. Stable wording so
/// scripts can grep `pruned: N`.
#[must_use]
pub fn render_prune_report(report: &PruneReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "fleet sessions prune:");
    let _ = writeln!(out, "  pruned: {}", report.pruned.len());
    let _ = writeln!(
        out,
        "  skipped (no worktree to remove): {}",
        report.skipped.len()
    );
    for id in &report.pruned {
        let _ = writeln!(out, "    + {id}");
    }
    for id in &report.skipped {
        let _ = writeln!(out, "    . {id}");
    }
    if !report.failed.is_empty() {
        let _ = writeln!(out, "  failed: {}", report.failed.len());
        for (id, err) in &report.failed {
            let _ = writeln!(out, "    ! {id}: {err}");
        }
    }
    out
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
        if !r.stopped_containers.is_empty() {
            let _ = writeln!(
                out,
                "      stopped {} leaked container(s): {}",
                r.stopped_containers.len(),
                r.stopped_containers.join(", "),
            );
        }
        if !r.failed_container_stops.is_empty() {
            let _ = writeln!(
                out,
                "      failed to stop {} leaked container(s); reclaim manually: {}",
                r.failed_container_stops.len(),
                r.failed_container_stops.join(", "),
            );
        }
    }
    out
}

/// Open the `SessionStore` for the current repo's fleet root.
fn open_store() -> Result<SessionStore> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    Ok(SessionStore::for_repo(&root))
}

/// `fleet sessions unblock <session-id> [--reason <text>]`. Thin
/// CLI wrapper that loads the per-repo config + stores and
/// delegates to [`unblock_session`] for the actual mutation.
///
/// Returns 0 even when the session had no edges to clear; the
/// operation is idempotent.
pub fn run_unblock(id: &str, reason: Option<&str>) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = SessionStore::for_repo(&root);
    let deps_store = crate::deps::DepsStore::for_repo(&root);
    let session_id = SessionId::new(id);

    // Resolve the tracker lazily: only --reason needs it, so a repo
    // without a configured tracker can still clear deps locally.
    let tracker: Option<Box<dyn crate::tracker::Tracker>> = if reason.is_some() {
        let config = crate::repo_config::RepoConfig::load(root.join(".fleet/config.yaml"))
            .with_context(|| format!("loading repo config under {}", root.display()))?;
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        Some(
            crate::tracker::build(config.tracker, invoker).ok_or_else(|| {
                anyhow!(
                    "session `{id}`: --reason posts a tracker comment, but no tracker \
                 is configured for this repo (.fleet/config.yaml `tracker:`)"
                )
            })?,
        )
    } else {
        None
    };

    let outcome = unblock_session(
        &store,
        &deps_store,
        tracker.as_deref(),
        &root,
        &session_id,
        reason,
    )?;
    print!("{}", render_unblock_outcome(&outcome));
    Ok(0)
}

/// Outcome of an unblock operation. Pure value object so tests can
/// assert on the report without screen-scraping the CLI output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockOutcome {
    pub session_id: String,
    pub ticket_id: String,
    pub edges_removed: usize,
    pub comment_posted: bool,
}

/// Drop every deps edge keyed by `session`'s bound ticket; when a
/// `reason` and a `tracker` are both supplied, also post a comment
/// explaining why. Pure-ish — takes the dependencies as parameters
/// instead of reaching for the cwd/config, so tests can exercise it
/// against a tempdir-rooted set of stores.
///
/// Errors when:
/// - the session isn't found, or
/// - the session has no bound issue, or
/// - `reason` is set but no tracker was provided, or
/// - the tracker call fails.
pub fn unblock_session(
    store: &SessionStore,
    deps_store: &crate::deps::DepsStore,
    tracker: Option<&dyn crate::tracker::Tracker>,
    repo_root: &Path,
    session_id: &SessionId,
    reason: Option<&str>,
) -> Result<UnblockOutcome> {
    let session = store
        .load(session_id)
        .with_context(|| format!("loading session `{session_id}`"))?;
    let issue = session.issue.as_ref().ok_or_else(|| {
        anyhow!(
            "session `{session_id}` has no bound issue — `unblock` removes deps \
             edges keyed by the session's ticket id, so there's nothing to do here"
        )
    })?;

    let removed = deps_store
        .remove_edges_for_blocked(&issue.human_id)
        .with_context(|| {
            format!(
                "removing deps edges for ticket `{}` (session `{session_id}`)",
                issue.human_id
            )
        })?;

    let comment_posted = if let Some(reason) = reason {
        let tracker = tracker.ok_or_else(|| {
            anyhow!(
                "session `{session_id}`: --reason requires a tracker, but the \
                 caller did not pass one"
            )
        })?;
        let body = format!("Unblocked manually: {reason}");
        tracker
            .comment(repo_root, &issue.human_id, &body)
            .with_context(|| format!("posting unblock comment on ticket `{}`", issue.human_id))?;
        true
    } else {
        false
    };

    Ok(UnblockOutcome {
        session_id: session_id.as_str().to_string(),
        ticket_id: issue.human_id.clone(),
        edges_removed: removed,
        comment_posted,
    })
}

/// Render the user-visible summary of an unblock outcome.
#[must_use]
pub fn render_unblock_outcome(outcome: &UnblockOutcome) -> String {
    let edges_phrase = if outcome.edges_removed == 0 {
        "no deps edges to clear".to_string()
    } else {
        format!(
            "cleared {} deps edge{}",
            outcome.edges_removed,
            if outcome.edges_removed == 1 { "" } else { "s" }
        )
    };
    let comment_phrase = if outcome.comment_posted {
        " · posted comment"
    } else {
        ""
    };
    format!(
        "session `{}` (ticket `{}`): {}{}\n",
        outcome.session_id, outcome.ticket_id, edges_phrase, comment_phrase
    )
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
    let _ = writeln!(
        out,
        "  created:      {}",
        crate::tui::format_epoch_ms(session.created_at_ms)
    );
    let _ = writeln!(
        out,
        "  updated:      {}",
        crate::tui::format_epoch_ms(session.updated_at_ms)
    );
    let _ = writeln!(
        out,
        "  directory:    {}",
        store.session_dir(&session.id).display()
    );
    let _ = writeln!(
        out,
        "  total cost:   {}",
        format_cost(session.total_cost_usd())
    );
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::session::SessionId;
    use std::path::PathBuf;

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
        let r = render_session_list(Path::new("/r"), &[row("s-1", "created", "standard", None)]);
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
    fn render_prune_report_says_pruned_and_skipped() {
        let report = PruneReport {
            pruned: vec![SessionId::new("s-a"), SessionId::new("s-b")],
            skipped: vec![SessionId::new("s-c")],
            failed: vec![],
        };
        let r = render_prune_report(&report);
        assert!(r.contains("pruned: 2"));
        assert!(r.contains("skipped (no worktree to remove): 1"));
        assert!(r.contains("+ s-a"));
        assert!(r.contains("+ s-b"));
        assert!(r.contains(". s-c"));
        assert!(!r.contains("failed:"));
    }

    #[test]
    fn render_prune_report_surfaces_failures() {
        let report = PruneReport {
            pruned: vec![],
            skipped: vec![],
            failed: vec![(SessionId::new("s-x"), "boom".to_string())],
        };
        let r = render_prune_report(&report);
        assert!(r.contains("failed: 1"));
        assert!(r.contains("! s-x: boom"));
    }

    /// Test helper: stage a completed session at the given id with a
    /// pre-existing on-disk worktree directory.
    fn stage_completed_with_worktree(
        store: &SessionStore,
        id: &str,
        branch: &str,
    ) -> (SessionId, PathBuf) {
        let sid = SessionId::new(id);
        let mut s = Session::new(sid.clone(), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        s.transition_to(SessionState::Completed, 3).unwrap();
        let wt_path = store.session_dir(&sid).join("worktree");
        std::fs::create_dir_all(&wt_path).unwrap();
        s.set_worktree(wt_path.clone(), branch, 3);
        store.create(&s).unwrap();
        (sid, wt_path)
    }

    #[test]
    fn prune_one_removes_worktree_and_clears_meta_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let (id, wt_path) = stage_completed_with_worktree(&store, "s-1", "fleet/session-s-1");

        let mut mock = MockProcessInvoker::new();
        let expected_wt = wt_path.to_string_lossy().into_owned();
        let root_str = dir.path().to_string_lossy().into_owned();
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root_str.clone(),
                            "worktree".to_string(),
                            "remove".to_string(),
                            "--force".to_string(),
                            expected_wt.clone(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));

        let outcome = prune_one(&mock, dir.path(), &store, &id, 99, false).unwrap();
        assert_eq!(outcome, PruneOutcome::Pruned);

        let reloaded = store.load(&id).unwrap();
        assert!(
            reloaded.worktree_path.is_none(),
            "worktree_path must be cleared on meta.json"
        );
        // Branch is intentionally preserved — user can `git checkout`.
        assert_eq!(reloaded.branch.as_deref(), Some("fleet/session-s-1"));
        assert_eq!(reloaded.updated_at_ms, 99);
    }

    #[test]
    fn prune_one_is_noop_when_session_has_no_worktree() {
        // Non-git workspaces leave worktree_path = None on the
        // session. Prune should report AlreadyClean and not invoke
        // git at all.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let sid = SessionId::new("s-nowt");
        let mut s = Session::new(sid.clone(), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        s.transition_to(SessionState::Completed, 3).unwrap();
        store.create(&s).unwrap();

        let mock = MockProcessInvoker::new(); // no expect_run() — must not be called.
        let outcome = prune_one(&mock, dir.path(), &store, &sid, 99, false).unwrap();
        assert_eq!(outcome, PruneOutcome::AlreadyClean);
    }

    #[test]
    fn prune_one_refuses_running_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let sid = SessionId::new("s-run");
        let mut s = Session::new(sid.clone(), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        let wt_path = store.session_dir(&sid).join("worktree");
        std::fs::create_dir_all(&wt_path).unwrap();
        s.set_worktree(wt_path, "fleet/session-s-run", 2);
        store.create(&s).unwrap();

        let mock = MockProcessInvoker::new(); // must not be called.
        let err = prune_one(&mock, dir.path(), &store, &sid, 99, false).unwrap_err();
        assert!(format!("{err:#}").contains("still running"));
    }

    #[test]
    fn prune_one_handles_missing_worktree_dir_with_prune_metadata() {
        // The worktree dir was rm -rf'd out of band. We invoke
        // `git worktree prune` to drop stale administrative entries
        // and still clear the meta pointer.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let sid = SessionId::new("s-gone");
        let mut s = Session::new(sid.clone(), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        s.transition_to(SessionState::Completed, 3).unwrap();
        // Set worktree_path to a directory that does not exist.
        s.set_worktree(
            PathBuf::from("/no/such/worktree"),
            "fleet/session-s-gone",
            3,
        );
        store.create(&s).unwrap();

        let mut mock = MockProcessInvoker::new();
        let root_str = dir.path().to_string_lossy().into_owned();
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root_str.clone(),
                            "worktree".to_string(),
                            "prune".to_string(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));

        let outcome = prune_one(&mock, dir.path(), &store, &sid, 99, false).unwrap();
        assert_eq!(outcome, PruneOutcome::Pruned);
        let reloaded = store.load(&sid).unwrap();
        assert!(reloaded.worktree_path.is_none());
    }

    #[test]
    fn prune_one_with_branch_removes_worktree_and_deletes_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let (id, wt_path) = stage_completed_with_worktree(&store, "s-wb", "fleet/session-s-wb");

        let mut mock = MockProcessInvoker::new();
        // First call: remove the worktree.
        let expected_wt = wt_path.to_string_lossy().into_owned();
        let root1 = dir.path().to_string_lossy().into_owned();
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root1.clone(),
                            "worktree".to_string(),
                            "remove".to_string(),
                            "--force".to_string(),
                            expected_wt.clone(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));
        // Second call: delete the branch with -D (force).
        let root2 = dir.path().to_string_lossy().into_owned();
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root2.clone(),
                            "branch".to_string(),
                            "-D".to_string(),
                            "fleet/session-s-wb".to_string(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));

        let outcome = prune_one(&mock, dir.path(), &store, &id, 99, true).unwrap();
        assert_eq!(outcome, PruneOutcome::Pruned);

        let reloaded = store.load(&id).unwrap();
        assert!(reloaded.worktree_path.is_none());
        assert!(
            reloaded.branch.is_none(),
            "--with-branch must clear the branch field too"
        );
    }

    #[test]
    fn prune_one_with_branch_alone_drops_branch_when_worktree_already_gone() {
        // Already-pruned session (worktree_path = None, branch =
        // Some) — `--with-branch` should still reap the branch.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let sid = SessionId::new("s-onlybranch");
        let mut s = Session::new(sid.clone(), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        s.transition_to(SessionState::Completed, 3).unwrap();
        // Manually populate just the branch — simulates a session
        // that was already pruned-without-branch previously.
        s.branch = Some("fleet/session-s-onlybranch".to_string());
        store.create(&s).unwrap();

        let mut mock = MockProcessInvoker::new();
        let root_str = dir.path().to_string_lossy().into_owned();
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root_str.clone(),
                            "branch".to_string(),
                            "-D".to_string(),
                            "fleet/session-s-onlybranch".to_string(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));

        let outcome = prune_one(&mock, dir.path(), &store, &sid, 99, true).unwrap();
        assert_eq!(outcome, PruneOutcome::Pruned);
        let reloaded = store.load(&sid).unwrap();
        assert!(reloaded.branch.is_none());
    }

    #[test]
    fn prune_one_with_branch_off_keeps_branch_intact() {
        // Regression guard: default `with_branch = false` must leave
        // the branch on git and on meta.json — matches the contract
        // shipped in commit 6.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());
        let (id, wt_path) = stage_completed_with_worktree(&store, "s-keep", "fleet/session-s-keep");

        let mut mock = MockProcessInvoker::new();
        let expected_wt = wt_path.to_string_lossy().into_owned();
        let root_str = dir.path().to_string_lossy().into_owned();
        // Only the worktree-remove call should fire; no branch
        // deletion. mockall's MockProcessInvoker fails the test on
        // an unexpected call, so the absence of an expectation here
        // is the assertion.
        mock.expect_run()
            .withf(move |prog, args| {
                prog == "git"
                    && args
                        == &[
                            "-C".to_string(),
                            root_str.clone(),
                            "worktree".to_string(),
                            "remove".to_string(),
                            "--force".to_string(),
                            expected_wt.clone(),
                        ]
            })
            .returning(|_, _| Ok(String::new()));

        prune_one(&mock, dir.path(), &store, &id, 99, false).unwrap();
        let reloaded = store.load(&id).unwrap();
        assert_eq!(reloaded.branch.as_deref(), Some("fleet/session-s-keep"));
    }

    #[test]
    fn select_sessions_by_state_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_repo(dir.path());

        let mut completed = Session::new(SessionId::new("s-c"), "x", 1);
        completed.transition_to(SessionState::Running, 2).unwrap();
        completed.transition_to(SessionState::Completed, 3).unwrap();
        store.create(&completed).unwrap();

        let mut failed = Session::new(SessionId::new("s-f"), "x", 1);
        failed.transition_to(SessionState::Running, 2).unwrap();
        failed.transition_to(SessionState::Failed, 3).unwrap();
        store.create(&failed).unwrap();

        let mut running = Session::new(SessionId::new("s-r"), "x", 1);
        running.transition_to(SessionState::Running, 2).unwrap();
        store.create(&running).unwrap();

        let only_completed = select_sessions_by_state(&store, &[SessionState::Completed]).unwrap();
        assert_eq!(only_completed, vec![SessionId::new("s-c")]);

        let terminals = select_sessions_by_state(
            &store,
            &[
                SessionState::Completed,
                SessionState::Failed,
                SessionState::Crashed,
            ],
        )
        .unwrap();
        let ids: Vec<_> = terminals.iter().map(SessionId::to_string).collect();
        assert!(ids.contains(&"s-c".to_string()));
        assert!(ids.contains(&"s-f".to_string()));
        assert!(!ids.contains(&"s-r".to_string()));
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
                    stopped_containers: vec![],
                    failed_container_stops: vec![],
                },
                ReapedSession {
                    id: SessionId::new("s-b"),
                    reason: ReapReason::DeadDriver { pid: 1234 },
                    stopped_containers: vec![],
                    failed_container_stops: vec![],
                },
            ],
        });
        assert!(r.contains("scanned 4 session(s); reaped 2"));
        assert!(r.contains("- s-a -> crashed (no driver_pid recorded)"));
        assert!(r.contains("- s-b -> crashed (driver pid 1234 is dead)"));
    }

    #[test]
    fn render_reap_report_surfaces_stopped_and_failed_containers() {
        use crate::session::reaper::{ReapReason, ReapedSession};
        let r = render_reap_report(&ReapReport {
            scanned: 1,
            reaped: vec![ReapedSession {
                id: SessionId::new("s-c"),
                reason: ReapReason::DeadDriver { pid: 99_999 },
                stopped_containers: vec!["c-aaa".to_string(), "c-bbb".to_string()],
                failed_container_stops: vec!["c-ccc".to_string()],
            }],
        });
        assert!(r.contains("stopped 2 leaked container(s): c-aaa, c-bbb"));
        assert!(r.contains("failed to stop 1 leaked container(s); reclaim manually: c-ccc"));
    }

    // ---- unblock --------------------------------------------------

    use crate::deps::{DepEdge, DepsStore};
    use crate::session::IssueContext;
    use crate::tracker::Tracker;

    /// Tiny mock tracker that records every `comment` call. Used by
    /// the unblock-with-reason test below; other Tracker methods
    /// panic so a mistaken caller blows up rather than passing
    /// silently.
    struct CommentRecordingTracker {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl CommentRecordingTracker {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Tracker for CommentRecordingTracker {
        fn name(&self) -> &'static str {
            "mock-comment-recording"
        }
        fn list_issues(&self, _: &Path) -> Result<Vec<crate::tracker::Issue>> {
            Ok(Vec::new())
        }
        fn comment(&self, _: &Path, id: &str, body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((id.to_string(), body.to_string()));
            Ok(())
        }
    }

    /// Set up: a tempdir-rooted `SessionStore` with one session
    /// bound to ticket `42`, a `DepsStore` pre-seeded with edges
    /// where 42 is blocked on 43 and 44 plus an unrelated 43→44
    /// edge.
    fn unblock_fixture() -> (tempfile::TempDir, SessionStore, DepsStore, SessionId) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let deps = DepsStore::at(dir.path().join("deps.json"));
        let session_id = SessionId::new("s-blocked");
        let mut session = Session::new(session_id.clone(), "standard", 0);
        session.issue = Some(IssueContext {
            id: "gh:42".to_string(),
            human_id: "42".to_string(),
            title: "Stuck ticket".to_string(),
            labels: Vec::new(),
        });
        store.create(&session).unwrap();
        deps.add_edge(DepEdge {
            blocked: "42".to_string(),
            blocked_on: "43".to_string(),
            reason: crate::deps::BlockedReason::Ticket,
            created_at_ms: 1,
        })
        .unwrap();
        deps.add_edge(DepEdge {
            blocked: "42".to_string(),
            blocked_on: "44".to_string(),
            reason: crate::deps::BlockedReason::Ticket,
            created_at_ms: 2,
        })
        .unwrap();
        deps.add_edge(DepEdge {
            blocked: "43".to_string(),
            blocked_on: "44".to_string(),
            reason: crate::deps::BlockedReason::Ticket,
            created_at_ms: 3,
        })
        .unwrap();
        (dir, store, deps, session_id)
    }

    #[test]
    fn unblock_session_clears_only_edges_keyed_by_the_sessions_ticket() {
        let (dir, store, deps, session_id) = unblock_fixture();
        let outcome = unblock_session(&store, &deps, None, dir.path(), &session_id, None).unwrap();
        assert_eq!(outcome.ticket_id, "42");
        assert_eq!(outcome.edges_removed, 2);
        assert!(!outcome.comment_posted);
        // 43→44 edge survives.
        let remaining = deps.load().unwrap();
        assert_eq!(remaining.edges.len(), 1);
        assert_eq!(remaining.edges[0].blocked, "43");
    }

    #[test]
    fn unblock_session_with_reason_posts_a_tracker_comment() {
        let (dir, store, deps, session_id) = unblock_fixture();
        let tracker = CommentRecordingTracker::new();
        let outcome = unblock_session(
            &store,
            &deps,
            Some(&tracker as &dyn Tracker),
            dir.path(),
            &session_id,
            Some("apt mirror recovered"),
        )
        .unwrap();
        assert!(outcome.comment_posted);
        let calls = tracker.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "42");
        assert!(
            calls[0]
                .1
                .contains("Unblocked manually: apt mirror recovered"),
            "got: {}",
            calls[0].1
        );
    }

    #[test]
    fn unblock_session_with_reason_but_no_tracker_errors() {
        let (dir, store, deps, session_id) = unblock_fixture();
        let err = unblock_session(
            &store,
            &deps,
            None,
            dir.path(),
            &session_id,
            Some("needed-to-comment"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("--reason requires a tracker"),
            "got: {err:#}"
        );
    }

    #[test]
    fn unblock_session_errors_when_session_has_no_bound_issue() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let deps = DepsStore::at(dir.path().join("deps.json"));
        let session_id = SessionId::new("s-noissue");
        // Session created without an issue — typical of an ad-hoc
        // smoke run that didn't bind a ticket.
        let session = Session::new(session_id.clone(), "standard", 0);
        store.create(&session).unwrap();
        let err = unblock_session(&store, &deps, None, dir.path(), &session_id, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no bound issue"),
            "got: {err:#}"
        );
    }

    #[test]
    fn unblock_session_is_idempotent_with_zero_edges() {
        // A session with a ticket the deps map doesn't reference at
        // all should report zero edges removed and not error.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let deps = DepsStore::at(dir.path().join("deps.json"));
        let session_id = SessionId::new("s-unrelated");
        let mut session = Session::new(session_id.clone(), "standard", 0);
        session.issue = Some(IssueContext {
            id: "gh:999".to_string(),
            human_id: "999".to_string(),
            title: "Unrelated".to_string(),
            labels: Vec::new(),
        });
        store.create(&session).unwrap();
        let outcome = unblock_session(&store, &deps, None, dir.path(), &session_id, None).unwrap();
        assert_eq!(outcome.edges_removed, 0);
        assert!(!outcome.comment_posted);
    }

    #[test]
    fn render_unblock_outcome_pluralises_correctly() {
        let one = UnblockOutcome {
            session_id: "s-1".into(),
            ticket_id: "42".into(),
            edges_removed: 1,
            comment_posted: false,
        };
        let multi = UnblockOutcome {
            edges_removed: 3,
            ..one.clone()
        };
        let none = UnblockOutcome {
            edges_removed: 0,
            ..one.clone()
        };
        assert!(render_unblock_outcome(&one).contains("cleared 1 deps edge\n"));
        assert!(render_unblock_outcome(&multi).contains("cleared 3 deps edges"));
        assert!(render_unblock_outcome(&none).contains("no deps edges to clear"));
    }

    #[test]
    fn render_unblock_outcome_notes_when_comment_posted() {
        let with_comment = UnblockOutcome {
            session_id: "s-1".into(),
            ticket_id: "42".into(),
            edges_removed: 2,
            comment_posted: true,
        };
        let out = render_unblock_outcome(&with_comment);
        assert!(out.contains("· posted comment"), "got: {out}");
    }
}
