//! `fleet workflow …` — v2-era workflow commands.
//!
//! Three entry points:
//! - `list` — enumerate `.fleet/workflows/*.yaml`. No adapter needed.
//! - `validate <name>` — parse and DAG-validate. No adapter needed.
//! - `run <name>` — parse, build the adapter via the factory, mint a
//!   session, and drive it through the executor end-to-end.
//!
//! Render helpers are pure functions so tests can assert on output without
//! a real filesystem; the `run_*` wrappers handle the I/O.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::egress::{self, EgressEnforcer};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::detect::probe;
use crate::runtime::devcontainer::Devcontainer;
use crate::runtime::factory::build_adapter;
use crate::session::IssueContext;
use crate::session::SessionId;
use crate::session::store::SessionStore;
use crate::session::{ClockIdSource, IdSource, Session, SessionState, now_ms};
use crate::tracker::{Issue, Tracker, build as build_tracker};
use crate::workflow::executor::{ExecuteRequest, WorkflowExecutor, WorktreeMeta};
use crate::workflow::spec::Workflow;
use crate::workflow::validate::validate;
use crate::worktree;

/// CLI entry point for `fleet workflow list`. Walks
/// `.fleet/workflows/` and prints discovered workflow YAMLs.
pub fn run_list() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let workflows = discover_workflows(&root)?;
    print!("{}", render_workflow_list(&root, &workflows));
    Ok(0)
}

/// CLI entry point for `fleet workflow validate <name>`. Loads the named
/// workflow and runs static checks; exits 0 on valid, 1 on parse/validation
/// failure with the error on stderr.
pub fn run_validate(name: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let path = workflow_path(&root, name);
    let wf = Workflow::from_path(&path).with_context(|| format!("loading workflow `{name}`"))?;
    validate(&wf).with_context(|| format!("validating workflow `{name}`"))?;
    println!(
        "fleet workflow validate: `{name}` ok ({} node{})",
        wf.nodes.len(),
        if wf.nodes.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

/// CLI entry point for `fleet workflow run <name>`. Single-shot: build the
/// adapter from .fleet/config.yaml + probe, mint a session, execute. Prints
/// the session id on stdout for scripting, and a one-line status summary
/// on stderr. Exit code: 0 on Completed, 1 on Failed. When `issue_id` is
/// `Some`, resolves the human id through the configured tracker and
/// surfaces it to nodes via `FLEET_ISSUE_*` env vars.
///
/// With `detached = true`, this is the **wrapper**: it mints the session
/// id, prints it, spawns `tmux new-session -d -s fleet-<id>` running
/// the inner (same binary, re-invoked with `--session-id <id>` so the
/// pre-minted id flows through), and returns immediately. Without
/// `--detached`, the run is inline and blocking — the contract CI /
/// autonomous mode have always relied on.
///
/// `preassigned_session_id` carries the wrapper-minted id into the
/// inner invocation. When `None` (the inline path or a first-class
/// CLI call), a fresh id is minted as before.
pub fn run_run(
    name: &str,
    issue_id: Option<&str>,
    preassigned_session_id: Option<&str>,
    detached: bool,
) -> Result<i32> {
    if detached {
        return run_run_detached(name, issue_id);
    }

    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config =
        RepoConfig::load(root.join(".fleet/config.yaml")).context("loading .fleet/config.yaml")?;

    let wf_path = workflow_path(&root, name);
    let wf = Workflow::from_path(&wf_path).with_context(|| format!("loading workflow `{name}`"))?;

    let dc_path = if config.runtime.devcontainer.is_absolute() {
        config.runtime.devcontainer.clone()
    } else {
        root.join(&config.runtime.devcontainer)
    };
    let devcontainer = Devcontainer::from_path(&dc_path).with_context(|| {
        format!(
            "loading devcontainer at {} — did you run `fleet init`?",
            dc_path.display()
        )
    })?;

    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let report = probe(invoker.as_ref());
    let adapter = build_adapter(&config.runtime, &report, Arc::clone(&invoker))?;

    let store = SessionStore::for_repo(&root);
    // Re-use the wrapper's pre-minted id when present (so the tmux
    // session name `fleet-<id>` and the wrapper's stdout `<id>` line
    // address the same on-disk session). Otherwise mint as before.
    let session_id = preassigned_session_id.map_or_else(|| ClockIdSource.mint(), SessionId::new);

    let issue = match issue_id {
        Some(id) => Some(resolve_issue(&config, &root, Arc::clone(&invoker), id)?),
        None => None,
    };

    // Provision the per-session worktree if the workspace is a git
    // repo. On non-git workspaces (the `Local` adapter, scratch dirs)
    // fall back to running directly against the repo root — without
    // isolation between parallel sessions and without replay's
    // code-state snapshot guarantee. We log loudly so users don't
    // wonder why two sessions stomp on each other's files.
    let provision = provision_worktree(invoker.as_ref(), &root, &store, &session_id, "HEAD")?;
    let workspace_path: &Path = provision
        .as_ref()
        .map_or(root.as_path(), |p| p.path.as_path());
    let worktree_meta = provision.as_ref().map(|p| WorktreeMeta {
        path: p.path.as_path(),
        branch: p.branch.as_str(),
    });

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let tracker = build_tracker_arc(&config, Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(Arc::clone(&invoker)).with_tracker(tracker);
    // SIGINT handler shared with the executor: between nodes, the
    // executor checks this flag and aborts cleanly (marking the
    // session `Failed` with a "user interrupted" reason) rather than
    // dying mid-loop. During an agent step the agent absorbs the
    // signal directly through its PTY — claude code's `cancel current
    // message` UX kicks in there. `try_set_handler` swallows the
    // harmless "already set" case for any future caller that nests
    // runs in the same process.
    let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler_flag = Arc::clone(&interrupt);
    let _ = ctrlc::try_set_handler(move || {
        handler_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: workspace_path,
        session_id: session_id.clone(),
        issue,
        worktree: worktree_meta,
        cost: &config.cost,
        egress: enforcer.as_ref(),
        interrupt_flag: Some(Arc::clone(&interrupt)),
        secrets: &config.secrets,
    };

    println!("{session_id}");
    let result = executor.execute(&req);
    match result {
        Ok(session) => {
            eprintln!(
                "fleet workflow run: `{name}` {} (last node: {})",
                state_word(session.state),
                session.current_node.as_deref().unwrap_or("none")
            );
            auto_prune_completed_session(&config, invoker.as_ref(), &root, &store, &session);
            Ok(i32::from(session.state != SessionState::Completed))
        }
        Err(err) => {
            // 130 is the conventional SIGINT exit code. Distinguish
            // it from other failures so shells / cron / CI can tell
            // "user cancelled" apart from "agent failed".
            if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("fleet workflow run: `{name}` aborted (user interrupted)");
                return Ok(130);
            }
            Err(err)
        }
    }
}

/// Wrapper path for `fleet workflow run --detached`. Pre-mints the
/// session id (so the tmux session can be named and the id can be
/// printed *before* the inner starts) and shells out to a fresh
/// `fleet workflow run … --session-id <id>` inside
/// `tmux new-session -d -s fleet-<id>`. Returns as soon as the tmux
/// session is alive; the actual workflow keeps running inside the
/// pane and the user can attach via the TUI or `fleet sessions
/// attach <id>`.
///
/// Probes for tmux up-front so a missing binary fails loudly rather
/// than after we've half-set things up. `pipe-pane` mirrors the pane
/// to `.fleet/sessions/<id>/transcript.log` for post-hoc forensics
/// — same shape as the orchestrator.
fn run_run_detached(name: &str, issue_id: Option<&str>) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);

    crate::orchestrator::tmux::probe(invoker.as_ref())?;

    let session_id = ClockIdSource.mint();
    let tmux_name = crate::session::worker_tmux_name(&session_id);

    // Materialise the session dir so the transcript pipe has a place
    // to write; the inner will populate meta.json on its first save.
    let store = SessionStore::for_repo(&root);
    let session_dir = store.session_dir(&session_id);
    std::fs::create_dir_all(&session_dir)
        .with_context(|| format!("creating session dir at {}", session_dir.display()))?;

    let exe = std::env::current_exe().context("locating current fleet binary")?;
    let mut argv: Vec<String> = vec![
        exe.display().to_string(),
        "workflow".to_string(),
        "run".to_string(),
        name.to_string(),
        "--session-id".to_string(),
        session_id.as_str().to_string(),
    ];
    if let Some(id) = issue_id {
        argv.push("--issue".to_string());
        argv.push(id.to_string());
    }
    let env = vec![(
        crate::session::SESSION_TMUX_ENV.to_string(),
        tmux_name.clone(),
    )];

    crate::orchestrator::tmux::new_session(invoker.as_ref(), &tmux_name, &argv, &env)
        .with_context(|| format!("spawning tmux session `{tmux_name}`"))?;

    // Best-effort transcript pipe — same as the orchestrator. A
    // missing/old tmux that doesn't support `pipe-pane` is a
    // diagnostic loss, not a run failure.
    let transcript_path = session_dir.join("transcript.log");
    if let Err(err) =
        crate::orchestrator::tmux::pipe_pane_to(invoker.as_ref(), &tmux_name, &transcript_path)
    {
        eprintln!("warning: tmux pipe-pane failed (no transcript will be captured): {err:#}");
    }

    println!("{session_id}");
    eprintln!(
        "fleet workflow run: `{name}` spawned detached in tmux `{tmux_name}` — \
         attach with `fleet sessions attach {session_id}` or via the TUI"
    );
    Ok(0)
}

/// CLI entry point for `fleet workflow resume <session-id>`. Loads the
/// persisted session, re-loads the workflow YAML named in its
/// `meta.json`, rebuilds the adapter, and continues execution from the
/// node *after* the gate. Exit code: 0 on Completed, 1 on Failed or
/// another `AwaitingGate` (still paused).
pub fn run_resume(session_id: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config =
        RepoConfig::load(root.join(".fleet/config.yaml")).context("loading .fleet/config.yaml")?;

    let store = SessionStore::for_repo(&root);
    let id = SessionId::new(session_id);
    // Load the session first so we know which workflow to re-parse.
    let session = store
        .load(&id)
        .with_context(|| format!("loading session `{session_id}`"))?;

    let wf_path = workflow_path(&root, &session.workflow);
    let wf = Workflow::from_path(&wf_path).with_context(|| {
        format!(
            "loading workflow `{}` for session `{session_id}`",
            session.workflow
        )
    })?;

    let dc_path = if config.runtime.devcontainer.is_absolute() {
        config.runtime.devcontainer.clone()
    } else {
        root.join(&config.runtime.devcontainer)
    };
    let devcontainer = Devcontainer::from_path(&dc_path)
        .with_context(|| format!("loading devcontainer at {}", dc_path.display()))?;

    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let report = probe(invoker.as_ref());
    let adapter = build_adapter(&config.runtime, &report, Arc::clone(&invoker))?;

    // Resume runs against the *original* session's worktree — the
    // agent must see the in-progress files it was working with
    // before the gate fired, not a fresh checkout. If the worktree
    // has been removed out of band (e.g. `fleet sessions prune`),
    // bail with a clear error rather than silently bind-mounting
    // the repo root and confusing the agent.
    let workspace_path: PathBuf = match session.worktree_path.as_deref() {
        Some(p) if p.is_dir() => p.to_path_buf(),
        Some(p) => {
            return Err(anyhow!(
                "session `{session_id}` is bound to worktree `{}` but that directory no \
                 longer exists — recreate the worktree (e.g. via `git worktree add`) or \
                 spawn a fresh run with `fleet workflow run`",
                p.display(),
            ));
        }
        // Pre-worktree-feature meta.json or a session created on a
        // non-git workspace: fall back to the shared repo root,
        // matching how it ran originally.
        None => root.clone(),
    };

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let tracker = build_tracker_arc(&config, Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(Arc::clone(&invoker)).with_tracker(tracker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: &workspace_path,
        session_id: id,
        // Resume doesn't re-resolve issue context — the user's original
        // `--issue` is lost across processes today. Workflows that
        // depend on `FLEET_ISSUE_*` post-resume should re-spawn instead.
        issue: None,
        // Worktree path + branch already live on the loaded session —
        // resume must not re-stamp (would bump updated_at_ms with no
        // change) and the executor's resume() doesn't read it.
        worktree: None,
        cost: &config.cost,
        egress: enforcer.as_ref(),
        interrupt_flag: None,
        secrets: &config.secrets,
    };
    println!("{session_id}");
    let resumed = executor.resume(&req)?;
    eprintln!(
        "fleet workflow resume: `{session_id}` {} (last node: {})",
        state_word(resumed.state),
        resumed.current_node.as_deref().unwrap_or("none"),
    );
    auto_prune_completed_session(&config, invoker.as_ref(), &root, &store, &resumed);
    Ok(i32::from(resumed.state != SessionState::Completed))
}

/// Internal CLI entry: `fleet workflow run-sibling --session-id
/// <id> <node>`. Spawned by [`crate::workflow::executor::WorkflowExecutor::run_fanout_node`]
/// as the command for each per-sibling tmux window. Runs a single
/// node body in this subprocess, then writes a `FanoutOutcome`
/// JSON record to `.fleet/sessions/<id>/fanout/<node>.outcome` so
/// the driver process can pick up the result.
///
/// Exit code: 0 when the node body returned `Ok`, 1 otherwise.
/// The driver doesn't actually read the exit code — it polls the
/// outcome file — but the code is still meaningful for users who
/// invoke `run-sibling` manually for forensics.
pub fn run_sibling(session_id: &str, node_id: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config =
        RepoConfig::load(root.join(".fleet/config.yaml")).context("loading .fleet/config.yaml")?;

    let store = SessionStore::for_repo(&root);
    let sid = SessionId::new(session_id);
    let session = store
        .load(&sid)
        .with_context(|| format!("loading session `{session_id}`"))?;

    let wf_path = workflow_path(&root, &session.workflow);
    let wf = Workflow::from_path(&wf_path).with_context(|| {
        format!(
            "loading workflow `{}` for sibling `{node_id}`",
            session.workflow
        )
    })?;
    let node = wf
        .node(node_id)
        .ok_or_else(|| anyhow!("workflow `{}` has no node `{node_id}`", session.workflow))?;

    let dc_path = if config.runtime.devcontainer.is_absolute() {
        config.runtime.devcontainer.clone()
    } else {
        root.join(&config.runtime.devcontainer)
    };
    let devcontainer = Devcontainer::from_path(&dc_path)
        .with_context(|| format!("loading devcontainer at {}", dc_path.display()))?;

    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let report = probe(invoker.as_ref());
    let adapter = build_adapter(&config.runtime, &report, Arc::clone(&invoker))?;

    // Workspace: same as the driver — the per-session worktree if
    // it exists, the repo root otherwise. Matches `resume`'s
    // fallback policy.
    let workspace_path: PathBuf = match session.worktree_path.as_deref() {
        Some(p) if p.is_dir() => p.to_path_buf(),
        _ => root.clone(),
    };

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let tracker = build_tracker_arc(&config, Arc::clone(&invoker));
    let executor = crate::workflow::executor::WorkflowExecutor::new(Arc::clone(&invoker))
        .with_tracker(tracker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: &workspace_path,
        session_id: sid,
        // Inherit the parent run's issue context so `FLEET_ISSUE_*`
        // is consistent across sibling subprocesses.
        issue: session.issue.clone(),
        // Worktree was provisioned by the parent run; siblings
        // don't re-stamp.
        worktree: None,
        cost: &config.cost,
        egress: enforcer.as_ref(),
        // Siblings don't install their own SIGINT handler — a
        // Ctrl+C in the parent driver's pane propagates through
        // the tmux session and reaches every window.
        interrupt_flag: None,
        secrets: &config.secrets,
    };

    // Hydrate the parent's outputs map so `when:` predicates and
    // `extract_outputs` references against upstream nodes resolve.
    let outputs = crate::workflow::executor::outputs_from_persisted(&session.outputs);
    // Signal to the executor that this run is a fanout sibling —
    // the agent-node path reads this env var to add an `--id-label`
    // to `devcontainer up` so concurrent siblings get distinct
    // container identities (avoids `--remove-existing-container`
    // sibling-eviction races).
    // SAFETY: env mutation is process-global, but this is a fresh
    // subprocess (the `run-sibling` invocation) so nothing else in
    // the address space is concurrent with the set_var call.
    unsafe { std::env::set_var("FLEET_FANOUT_SIBLING", node_id) };
    let egress_setup = req.egress.setup(req.session_id.as_str())?;
    let outcome_result = executor.run_node(&req, node, &session, &outputs, &egress_setup);
    if let Err(err) = req.egress.teardown(&egress_setup) {
        tracing::warn!(error = %err, "egress teardown after run-sibling failed");
    }

    let outcome_path = root
        .join(".fleet")
        .join("sessions")
        .join(session_id)
        .join("fanout")
        .join(format!("{node_id}.outcome"));
    if let Some(parent) = outcome_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating fanout outcome dir at {}", parent.display()))?;
    }
    let (wire, exit_code) = match &outcome_result {
        Ok(outcome) => (
            crate::workflow::executor::FanoutOutcome::Ok(outcome.clone()),
            0,
        ),
        Err(err) => (
            crate::workflow::executor::FanoutOutcome::Failed {
                message: format!("{err:#}"),
            },
            1,
        ),
    };
    let body =
        serde_json::to_string_pretty(&wire).with_context(|| "serialising sibling outcome")?;
    std::fs::write(&outcome_path, body)
        .with_context(|| format!("writing {}", outcome_path.display()))?;
    Ok(exit_code)
}

/// CLI entry point for
/// `fleet workflow replay <session> --rerun-from <node> | --rerun-only <node>`.
/// Mints a new session whose workflow is read off the source session's
/// `meta.json`, copies the source's artifacts/ into it, and runs the
/// workflow from the chosen node. With `--rerun-from`, continues to
/// the end of the workflow; with `--rerun-only`, fires the chosen
/// node exactly once and exits.
///
/// Caveats inherited from the executor's `replay` contract:
/// - The src session must have been run against a workflow whose YAML
///   is still resolvable under `.fleet/workflows/<name>.yaml`. Replay
///   re-parses the *current* YAML — if the workflow has been edited
///   between runs, the new run uses the new shape.
/// - `outputs:` accumulator is rebuilt empty for the rerun (same gap
///   as resume). `when:`-gated nodes downstream of an upstream
///   `outputs:` declaration see the default and may skip.
/// - The issue context is not re-resolved; new agent nodes won't see
///   `FLEET_ISSUE_*`. Re-spawn via `workflow run --issue <id>` if a
///   replay needs them.
pub fn run_replay(
    session_id: &str,
    rerun_from: Option<&str>,
    rerun_only: Option<&str>,
) -> Result<i32> {
    // Exactly-one validation. Clap's conflicts_with already rejects
    // the both-set case at parse time; this catches neither-set.
    let mode = match (rerun_from, rerun_only) {
        (Some(node), None) => ReplayMode::From(node),
        (None, Some(node)) => ReplayMode::Only(node),
        (None, None) => {
            return Err(anyhow!(
                "fleet workflow replay: one of `--rerun-from <node>` or \
                 `--rerun-only <node>` is required"
            ));
        }
        // conflicts_with should have caught this; treat defensively.
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rejects both flags"),
    };
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let config =
        RepoConfig::load(root.join(".fleet/config.yaml")).context("loading .fleet/config.yaml")?;

    let store = SessionStore::for_repo(&root);
    let src_id = SessionId::new(session_id);
    let src = store
        .load(&src_id)
        .with_context(|| format!("loading source session `{session_id}` for replay"))?;

    let wf_path = workflow_path(&root, &src.workflow);
    let wf = Workflow::from_path(&wf_path).with_context(|| {
        format!(
            "loading workflow `{}` for replay of session `{session_id}`",
            src.workflow
        )
    })?;

    let dc_path = if config.runtime.devcontainer.is_absolute() {
        config.runtime.devcontainer.clone()
    } else {
        root.join(&config.runtime.devcontainer)
    };
    let devcontainer = Devcontainer::from_path(&dc_path)
        .with_context(|| format!("loading devcontainer at {}", dc_path.display()))?;

    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let report = probe(invoker.as_ref());
    let adapter = build_adapter(&config.runtime, &report, Arc::clone(&invoker))?;

    let new_id = ClockIdSource.mint();

    // Replay provisions a fresh worktree off the src session's
    // branch tip. When src ran without a branch (non-git workspace,
    // or pre-worktree-feature meta.json), replay falls back to the
    // shared repo root — same behaviour the src session had, so
    // replay's contract isn't surprising.
    let provision = if let Some(branch) = src.branch.as_deref() {
        provision_worktree(invoker.as_ref(), &root, &store, &new_id, branch)?
    } else {
        tracing::warn!(
            src = %session_id,
            "source session has no recorded branch — replay runs against the shared \
             working tree without code-state isolation"
        );
        None
    };
    let workspace_path: &Path = provision
        .as_ref()
        .map_or(root.as_path(), |p| p.path.as_path());
    let worktree_meta = provision.as_ref().map(|p| WorktreeMeta {
        path: p.path.as_path(),
        branch: p.branch.as_str(),
    });

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let tracker = build_tracker_arc(&config, Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(Arc::clone(&invoker)).with_tracker(tracker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: workspace_path,
        session_id: new_id.clone(),
        // Issue context is not carried forward — re-spawning a fresh
        // run via `workflow run --issue` is the supported path when
        // `FLEET_ISSUE_*` matters.
        issue: None,
        worktree: worktree_meta,
        cost: &config.cost,
        egress: enforcer.as_ref(),
        interrupt_flag: None,
        secrets: &config.secrets,
    };

    println!("{new_id}");
    let result = match mode {
        ReplayMode::From(node) => executor.replay(&req, &src_id, node),
        ReplayMode::Only(node) => executor.replay_only(&req, &src_id, node),
    };
    match result {
        Ok(session) => {
            eprintln!(
                "fleet workflow replay: from `{session_id}` re-running `{}` (last node: {}) — {}",
                src.workflow,
                session.current_node.as_deref().unwrap_or("none"),
                state_word(session.state),
            );
            auto_prune_completed_session(&config, invoker.as_ref(), &root, &store, &session);
            Ok(i32::from(session.state != SessionState::Completed))
        }
        Err(err) => Err(err),
    }
}

/// Dispatch mode for `fleet workflow replay`. Decided at the CLI
/// layer, consumed where the executor method is selected.
enum ReplayMode<'a> {
    From(&'a str),
    Only(&'a str),
}

/// Resolve the on-disk path for a workflow name.
fn workflow_path(root: &Path, name: &str) -> PathBuf {
    root.join(".fleet/workflows").join(format!("{name}.yaml"))
}

/// Per-session worktree provisioned for a fresh `workflow run`. Owned
/// strings rather than borrows so the CLI can hold this across the
/// executor call without lifetime gymnastics.
pub struct WorktreeProvision {
    pub path: PathBuf,
    pub branch: String,
}

/// Create a per-session git worktree under
/// `<root>/.fleet/sessions/<id>/worktree` on a new
/// `fleet/session-<short-id>` branch based off `base` (a branch
/// name, tag, or sha). Returns `None` (with a warning logged) when
/// `root` is not inside a git working tree — the caller falls back
/// to the shared repo root.
///
/// Pre-creates the per-session directory because `git worktree add`
/// requires the *parent* of the target path to exist. `store.create`
/// (called later by the executor) tolerates the pre-existing dir.
///
/// `base` is what makes this function reusable across run and
/// replay: a fresh run bases off `HEAD`, replay bases off the src
/// session's branch tip so the new worktree starts at the same
/// commit the prior run ended at.
fn provision_worktree(
    invoker: &dyn ProcessInvoker,
    root: &Path,
    store: &SessionStore,
    session_id: &SessionId,
    base: &str,
) -> Result<Option<WorktreeProvision>> {
    if !worktree::is_git_repo(invoker, root) {
        tracing::warn!(
            workspace = %root.display(),
            "workspace is not a git repo — running session against the shared working tree \
             (no per-session isolation, replay loses its code-state snapshot guarantees)"
        );
        return Ok(None);
    }
    let session_dir = store.session_dir(session_id);
    std::fs::create_dir_all(&session_dir).with_context(|| {
        format!(
            "creating per-session directory at {} for the worktree parent",
            session_dir.display()
        )
    })?;
    let wt_path = session_dir.join("worktree");
    let branch = worktree::session_branch_name(session_id.as_str());
    worktree::create_worktree(invoker, root, &wt_path, &branch, base)?;
    Ok(Some(WorktreeProvision {
        path: wt_path,
        branch,
    }))
}

/// Apply the `cleanup.auto_prune_completed` policy: if the config
/// asks for it AND the session reached `Completed`, prune the
/// session's worktree in-line. Errors are logged but never
/// propagated — the workflow itself succeeded; a cleanup hiccup
/// shouldn't poison its exit code.
///
/// Other terminal states (`Failed`, `Crashed`) are not auto-pruned
/// regardless of the setting — they're forensic data, and the user
/// asks for `--all` explicitly when they're ready to reclaim.
fn auto_prune_completed_session(
    config: &RepoConfig,
    invoker: &dyn ProcessInvoker,
    root: &Path,
    store: &SessionStore,
    session: &Session,
) {
    if !config.cleanup.auto_prune_completed {
        return;
    }
    if session.state != SessionState::Completed {
        return;
    }
    // Auto-prune intentionally keeps the branch — config-driven
    // cleanup should be conservative. Users who want branches reaped
    // run `fleet sessions prune --with-branch` explicitly.
    match crate::cli::sessions::prune_one(invoker, root, store, &session.id, now_ms(), false) {
        Ok(crate::cli::sessions::PruneOutcome::Pruned) => {
            tracing::info!(session = %session.id, "auto-pruned worktree after completion");
        }
        Ok(crate::cli::sessions::PruneOutcome::AlreadyClean) => {}
        Err(err) => {
            tracing::warn!(
                error = %err,
                session = %session.id,
                "auto-prune-on-completed failed; worktree remains on disk"
            );
        }
    }
}

/// Build the egress enforcer for a workflow run. Single helper used
/// by execute / resume / replay so the wiring stays in lockstep.
/// Build an `Arc<dyn Tracker>` for the bridge to share with the
/// listener thread. Returns `None` when the configured tracker plugin
/// isn't implemented yet (Linear, Jira). The bridge is fine being
/// disabled — `fleet-tracker` calls inside the container will fail
/// fast with a clear error.
///
/// `Arc::from(Box<dyn Tracker>)` upcasts the box into an Arc without
/// reboxing the concrete impl; the bridge holds it and the executor
/// holds it for the duration of the run.
fn build_tracker_arc(
    config: &RepoConfig,
    invoker: Arc<dyn ProcessInvoker>,
) -> Option<Arc<dyn Tracker>> {
    build_tracker(config.tracker, invoker).map(Arc::from)
}

fn build_workflow_enforcer(
    config: &RepoConfig,
    adapter: &dyn crate::runtime::RuntimeAdapter,
    invoker: Arc<dyn ProcessInvoker>,
) -> Box<dyn EgressEnforcer> {
    let defaults: Vec<String> = fleet_default_allowlist_hosts(config);
    let defaults_refs: Vec<&str> = defaults.iter().map(String::as_str).collect();
    egress::build_enforcer(
        &config.runtime.network,
        adapter.name(),
        invoker,
        &defaults_refs,
    )
}

/// Derive the set of hosts fleet adds to the allowlist regardless of
/// `extra_hosts`. Today's list:
/// - the configured tracker's host (e.g. `api.github.com` for the
///   GitHub tracker; nothing for git-bug which is local).
/// - LLM-provider hosts implied by the agents' `env_passthrough`
///   (`ANTHROPIC_API_KEY` → `api.anthropic.com`, `OPENAI_API_KEY`
///   → `api.openai.com`).
/// - registry hosts the proxy sidecar's own image needs to be
///   pullable (`registry-1.docker.io`).
///
/// Best-effort: unknown env vars don't add anything, and the user
/// remains free to drop a custom host into `extra_hosts`. Returned
/// sorted+deduped so the resulting tinyproxy.conf is byte-stable.
fn fleet_default_allowlist_hosts(config: &RepoConfig) -> Vec<String> {
    let mut hosts: Vec<String> = vec![
        // Registry the proxy sidecar's image lives on.
        "registry-1.docker.io".to_string(),
    ];
    // Tracker host: GitHub → api.github.com. Other trackers
    // (git-bug, future Linear/Jira) only get added when the host is
    // known and stable.
    if matches!(config.tracker, crate::repo_config::Tracker::Github) {
        hosts.push("api.github.com".to_string());
        hosts.push("github.com".to_string());
    }
    // LLM provider inference via env_passthrough. Cheap heuristic:
    // most providers' SDKs read a single `<PROVIDER>_API_KEY` env.
    for (_name, spec) in config.agents.registry.iter() {
        for env in &spec.env_passthrough {
            if env.contains("ANTHROPIC") {
                hosts.push("api.anthropic.com".to_string());
            }
            if env.contains("OPENAI") {
                hosts.push("api.openai.com".to_string());
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Resolve an `--issue <id>` argument through the repo's configured
/// tracker. Exact `human_id` match wins (numbers for GitHub, short
/// hashes for git-bug); on no match, the error suggests
/// `fleet issues list`.
fn resolve_issue(
    config: &RepoConfig,
    repo_root: &Path,
    invoker: Arc<dyn ProcessInvoker>,
    issue_id: &str,
) -> Result<IssueContext> {
    let Some(tracker) = build_tracker(config.tracker, invoker) else {
        return Err(anyhow!(
            "tracker `{}` is not yet implemented — cannot resolve `--issue {issue_id}`",
            config.tracker.as_str()
        ));
    };
    let issues = tracker
        .list_issues(repo_root)
        .with_context(|| format!("listing issues via `{}`", tracker.name()))?;
    let found = pick_issue(&issues, issue_id).ok_or_else(|| {
        anyhow!(
            "issue `{issue_id}` not found in tracker `{}` — run `fleet issues list` to see available ids",
            tracker.name()
        )
    })?;
    Ok(IssueContext {
        id: found.id.clone(),
        human_id: found.human_id.clone(),
        title: found.title.clone(),
        labels: found.labels.clone(),
    })
}

/// Pure-function lookup so the resolution logic is testable without a
/// real tracker subprocess. Exact `human_id` match; returns the first
/// matching `Issue` or `None`.
#[must_use]
pub fn pick_issue<'a>(issues: &'a [Issue], requested: &str) -> Option<&'a Issue> {
    issues.iter().find(|i| i.human_id == requested)
}

/// Enumerate workflows under `<root>/.fleet/workflows/`. Returns the
/// short names (file stem) of each `.yaml`/`.yml` file. Returns an empty
/// list when the directory is missing — that's fine, just means the user
/// hasn't authored any yet.
fn discover_workflows(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(".fleet/workflows");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(anyhow!(err).context(format!("reading {}", dir.display()))),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Pure renderer for `fleet workflow list`. Tests assert on this text.
#[must_use]
pub fn render_workflow_list(root: &Path, workflows: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "fleet workflows in {}", root.display());
    if workflows.is_empty() {
        out.push_str("  (none yet — drop YAML files under .fleet/workflows/)\n");
    } else {
        for w in workflows {
            let _ = writeln!(out, "  - {w}");
        }
    }
    out
}

/// Short state word for the `run` command's summary line. Stable wording —
/// matches the serde rename of [`SessionState`] so scripting tools can
/// grep without case fiddling.
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
    use std::fs;

    #[test]
    fn render_workflow_list_with_entries() {
        let rendered =
            render_workflow_list(Path::new("/repo"), &["a".to_string(), "b".to_string()]);
        assert!(rendered.contains("fleet workflows in /repo"));
        assert!(rendered.contains("  - a"));
        assert!(rendered.contains("  - b"));
    }

    #[test]
    fn render_workflow_list_when_empty_says_so() {
        let rendered = render_workflow_list(Path::new("/repo"), &[]);
        assert!(rendered.contains("(none yet"));
    }

    #[test]
    fn discover_workflows_returns_empty_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_workflows(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn discover_workflows_returns_yaml_stems_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().join(".fleet/workflows");
        fs::create_dir_all(&wd).unwrap();
        fs::write(wd.join("standard.yaml"), b"name: standard\nnodes: []").unwrap();
        fs::write(wd.join("hotfix.yml"), b"name: hotfix\nnodes: []").unwrap();
        // Non-yaml files are ignored.
        fs::write(wd.join("README.md"), b"docs").unwrap();
        fs::create_dir_all(wd.join("subdir")).unwrap();
        let names = discover_workflows(dir.path()).unwrap();
        assert_eq!(names, vec!["hotfix".to_string(), "standard".to_string()]);
    }

    #[test]
    fn workflow_path_joins_under_dot_fleet_workflows() {
        let p = workflow_path(Path::new("/repo"), "standard");
        assert_eq!(p, PathBuf::from("/repo/.fleet/workflows/standard.yaml"));
    }

    fn make_issue(human_id: &str, title: &str) -> Issue {
        Issue {
            id: format!("gh:{human_id}"),
            human_id: human_id.to_string(),
            title: title.to_string(),
            status: "open".to_string(),
            labels: vec![],
        }
    }

    #[test]
    fn pick_issue_returns_exact_human_id_match() {
        let issues = vec![
            make_issue("1", "a"),
            make_issue("42", "b"),
            make_issue("100", "c"),
        ];
        let hit = pick_issue(&issues, "42").unwrap();
        assert_eq!(hit.title, "b");
    }

    #[test]
    fn pick_issue_returns_none_when_no_exact_match() {
        let issues = vec![make_issue("42", "x")];
        // Substring match doesn't count — `4` does not match `42`.
        assert!(pick_issue(&issues, "4").is_none());
    }

    #[test]
    fn pick_issue_returns_none_for_empty_input() {
        assert!(pick_issue(&[], "anything").is_none());
    }

    #[test]
    fn state_word_matches_serde_renames() {
        assert_eq!(state_word(SessionState::AwaitingGate), "awaiting_gate");
        assert_eq!(state_word(SessionState::Completed), "completed");
        assert_eq!(state_word(SessionState::Failed), "failed");
        assert_eq!(state_word(SessionState::Crashed), "crashed");
        assert_eq!(state_word(SessionState::Created), "created");
        assert_eq!(state_word(SessionState::Running), "running");
    }
}
