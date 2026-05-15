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

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::detect::probe;
use crate::runtime::devcontainer::Devcontainer;
use crate::runtime::factory::build_adapter;
use crate::session::{ClockIdSource, IdSource, SessionState};
use crate::session::store::SessionStore;
use crate::session::SessionId;
use crate::tracker::{Issue, build as build_tracker};
use crate::workflow::executor::{ExecuteRequest, IssueContext, WorkflowExecutor};
use crate::workflow::spec::Workflow;
use crate::workflow::validate::validate;

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
    let wf = Workflow::from_path(&path)
        .with_context(|| format!("loading workflow `{name}`"))?;
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
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .context("loading .fleet/config.yaml")?;

    let wf_path = workflow_path(&root, name);
    let wf = Workflow::from_path(&wf_path)
        .with_context(|| format!("loading workflow `{name}`"))?;

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

    let executor = WorkflowExecutor::new(invoker);
    let req = ExecuteRequest {
        workflow: &wf,
        adapter: adapter.as_ref(),
        agents: &config.agents.registry,
        store: &store,
        devcontainer: &devcontainer,
        workspace: &root,
        session_id: session_id.clone(),
        issue,
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
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .context("loading .fleet/config.yaml")?;

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

/// Resolve the on-disk path for a workflow name.
fn workflow_path(root: &Path, name: &str) -> PathBuf {
    root.join(".fleet/workflows").join(format!("{name}.yaml"))
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
        let rendered = render_workflow_list(Path::new("/repo"), &["a".to_string(), "b".to_string()]);
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
        let issues = vec![make_issue("1", "a"), make_issue("42", "b"), make_issue("100", "c")];
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
