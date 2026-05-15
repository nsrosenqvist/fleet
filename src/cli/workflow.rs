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
use crate::session::{ClockIdSource, IdSource, SessionState};
use crate::tracker::{Issue, build as build_tracker};
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
pub fn run_run(name: &str, issue_id: Option<&str>) -> Result<i32> {
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
    let session_id = ClockIdSource.mint();

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
    let provision = provision_worktree(invoker.as_ref(), &root, &store, &session_id)?;
    let workspace_path: &Path = provision
        .as_ref()
        .map_or(root.as_path(), |p| p.path.as_path());
    let worktree_meta = provision.as_ref().map(|p| WorktreeMeta {
        path: p.path.as_path(),
        branch: p.branch.as_str(),
    });

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(invoker);
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
        egress: enforcer.as_ref(),
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
            Ok(i32::from(session.state != SessionState::Completed))
        }
        Err(err) => Err(err),
    }
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

    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(invoker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: &root,
        session_id: id,
        // Resume doesn't re-resolve issue context — the user's original
        // `--issue` is lost across processes today. Workflows that
        // depend on `FLEET_ISSUE_*` post-resume should re-spawn instead.
        issue: None,
        // Resume re-uses the original session's worktree; commit 5
        // wires that through. For now the field is None and resume
        // continues to run against the repo root.
        worktree: None,
        egress: enforcer.as_ref(),
    };
    println!("{session_id}");
    let resumed = executor.resume(&req)?;
    eprintln!(
        "fleet workflow resume: `{session_id}` {} (last node: {})",
        state_word(resumed.state),
        resumed.current_node.as_deref().unwrap_or("none"),
    );
    Ok(i32::from(resumed.state != SessionState::Completed))
}

/// CLI entry point for `fleet workflow replay <session> --rerun-from <node>`.
/// Mints a new session whose workflow is read off the source session's
/// `meta.json`, copies the source's artifacts/ into it, and runs the
/// workflow from `rerun_from` onward.
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
pub fn run_replay(session_id: &str, rerun_from: &str) -> Result<i32> {
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
    let enforcer = build_workflow_enforcer(&config, adapter.as_ref(), Arc::clone(&invoker));
    let executor = WorkflowExecutor::new(invoker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: &root,
        session_id: new_id.clone(),
        // Issue context is not carried forward — re-spawning a fresh
        // run via `workflow run --issue` is the supported path when
        // `FLEET_ISSUE_*` matters.
        issue: None,
        // Replay provisions its own worktree off the src session's
        // branch tip in commit 4. For now the field is None and
        // replay continues to run against the repo root.
        worktree: None,
        egress: enforcer.as_ref(),
    };

    println!("{new_id}");
    let result = executor.replay(&req, &src_id, rerun_from);
    match result {
        Ok(session) => {
            eprintln!(
                "fleet workflow replay: from `{session_id}` re-running `{}` (last node: {}) — {}",
                src.workflow,
                session.current_node.as_deref().unwrap_or("none"),
                state_word(session.state),
            );
            Ok(i32::from(session.state != SessionState::Completed))
        }
        Err(err) => Err(err),
    }
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
/// `fleet/session-<short-id>` branch based off the host's current
/// `HEAD`. Returns `None` (with a warning logged) when `root` is not
/// inside a git working tree — the caller falls back to the shared
/// repo root.
///
/// Pre-creates the per-session directory because `git worktree add`
/// requires the *parent* of the target path to exist. `store.create`
/// (called later by the executor) tolerates the pre-existing dir.
fn provision_worktree(
    invoker: &dyn ProcessInvoker,
    root: &Path,
    store: &SessionStore,
    session_id: &SessionId,
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
    worktree::create_worktree(invoker, root, &wt_path, &branch, "HEAD")?;
    Ok(Some(WorktreeProvision {
        path: wt_path,
        branch,
    }))
}

/// Build the egress enforcer for a workflow run. Single helper used
/// by execute / resume / replay so the wiring stays in lockstep.
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
