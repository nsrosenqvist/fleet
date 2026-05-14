//! Phase-1 workflow executor.
//!
//! Drives a parsed [`Workflow`] against a [`RuntimeAdapter`] + agent registry
//! + [`SessionStore`]. Scope today: linear topological traversal of
//!   [`NodeKind::Agent`] and [`NodeKind::Bash`] nodes. Gate, assert, fanout,
//!   `loop_back_to`, and parallel siblings are parsed and validated upstream
//!   but rejected here with a clear "Phase 1 executor does not support …"
//!   message — they land with the workflow-engine commit in Phase 2.
//!
//! Lifecycle: `Created → Running → (Failed on node failure | Completed on
//! all nodes done)`. Each node bumps `current_node` so the TUI/sidebar can
//! show progress. Per-node logs land under `.fleet/sessions/<id>/logs/`.
//!
//! Containers are ephemeral per agent node — the plan calls for fresh
//! provisioning. `ensure_image` is idempotent so repeated nodes against
//! the same devcontainer don't rebuild.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::spec::{Node, NodeKind, Workflow};
use super::validate::validate;
use crate::agent::AgentRegistry;
use crate::agent::registry::AgentSpec;
use crate::process::ProcessInvoker;
use crate::runtime::devcontainer::Devcontainer;
use crate::runtime::{ContainerSpec, ExecOpts, RuntimeAdapter};
use crate::session::{Session, SessionId, SessionState, now_ms};
use crate::session::store::SessionStore;

/// Trait-bound closure type for the clock. Boxed so a single executor can
/// be reused across runs with different test clocks.
type ClockFn = Box<dyn Fn() -> u64 + Send + Sync>;

/// Executor configuration. Constructed once per fleet process; re-used per
/// workflow run via [`Self::execute`].
pub struct WorkflowExecutor {
    /// Used for [`NodeKind::Bash`] nodes (which run on the host, not in a
    /// container) and for any host-side shell-outs the executor itself
    /// needs. Agent execution flows through the adapter, which carries its
    /// own invoker.
    invoker: Arc<dyn ProcessInvoker>,
    clock: ClockFn,
}

/// What [`WorkflowExecutor::execute`] needs to run one workflow. Bundled
/// into a struct so the function signature stays stable as Phase 2 adds
/// fields (e.g. issue id, tracker handle, autonomous-mode bounds).
pub struct ExecuteRequest<'a> {
    pub workflow: &'a Workflow,
    pub adapter: &'a dyn RuntimeAdapter,
    pub agents: &'a AgentRegistry,
    pub store: &'a SessionStore,
    pub devcontainer: &'a Devcontainer,
    /// Repo root that gets bind-mounted as the container's workspace.
    pub workspace: &'a Path,
    pub session_id: SessionId,
}

impl WorkflowExecutor {
    pub fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self {
            invoker,
            clock: Box::new(now_ms),
        }
    }

    /// Test override for the clock. Production callers ignore this; tests
    /// pass a counter or fixed timestamp so assertions stay deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Run the workflow end-to-end. Mints the session row on disk, drives
    /// it through `Running` to a terminal state, returns the final
    /// snapshot. Node-level errors transition the session to `Failed` and
    /// propagate; the on-disk state always reflects the last successful
    /// transition.
    pub fn execute(&self, req: &ExecuteRequest<'_>) -> Result<Session> {
        validate(req.workflow).context("workflow failed static validation")?;
        let order = topological_order(req.workflow)?;
        reject_unsupported_node_kinds(req.workflow)?;

        let mut session = Session::new(
            req.session_id.clone(),
            &req.workflow.name,
            (self.clock)(),
        );
        req.store.create(&session)?;
        session.transition_to(SessionState::Running, (self.clock)())?;
        req.store.save(&session)?;

        for node_id in order {
            let node = req.workflow.node(&node_id).expect(
                "topological_order only returns ids from the workflow's nodes",
            );
            session.set_current_node(Some(node_id.clone()), (self.clock)());
            req.store.save(&session)?;
            if let Err(err) = self.run_node(req, node, &session) {
                self.mark_failed(req, &mut session, &err);
                return Err(err);
            }
        }

        // Last-node id stays as `current_node` for diagnostic value — it
        // gives the TUI a reasonable "finished at" pointer.
        session.transition_to(SessionState::Completed, (self.clock)())?;
        req.store.save(&session)?;
        Ok(session)
    }

    fn run_node(&self, req: &ExecuteRequest<'_>, node: &Node, session: &Session) -> Result<()> {
        match &node.kind {
            NodeKind::Agent { agent, .. } => self.run_agent_node(req, node, agent, session),
            NodeKind::Bash { script } => self.run_bash_node(req, node, script, session),
            // Unsupported kinds were rejected earlier in `execute`; this
            // branch is just an exhaustiveness guard.
            other => bail!(
                "internal: node `{}` of kind {other:?} reached run_node — should have been rejected upstream",
                node.id
            ),
        }
    }

    fn run_agent_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        agent_name: &str,
        session: &Session,
    ) -> Result<()> {
        let agent = req.agents.get(agent_name).ok_or_else(|| {
            anyhow!(
                "workflow node `{}` references unknown agent `{agent_name}` (not in agents.registry)",
                node.id
            )
        })?;
        let env = passthrough_env(agent);

        let image = req
            .adapter
            .ensure_image(req.devcontainer)
            .with_context(|| format!("building image for node `{}`", node.id))?;
        let artifacts_dir = req.store.session_dir(&session.id).join("artifacts");
        let spec = ContainerSpec {
            image,
            workspace: req.workspace.to_path_buf(),
            artifacts: artifacts_dir,
            env,
            command: None,
        };
        let container_id = req
            .adapter
            .start_container(&spec)
            .with_context(|| format!("starting container for node `{}`", node.id))?;

        let exec_result = req.adapter.exec(&container_id, &agent.command, ExecOpts::default());
        // Log capture first — happens even on adapter-level error so the
        // user can read what happened after a failure.
        let log_path = self.node_log_path(req, session, &node.id);
        let log_text = match &exec_result {
            Ok(h) => format!(
                "--- agent {agent_name} ---\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- exit {} ---\n",
                h.stdout, h.stderr, h.exit_code
            ),
            Err(err) => format!("--- agent {agent_name} failed: {err:#} ---\n"),
        };
        // Log write failures don't mask the more interesting agent failure;
        // surface them through tracing but continue with the agent result.
        if let Err(err) = std::fs::write(&log_path, log_text) {
            tracing::warn!(?err, log = %log_path.display(), "writing agent log failed");
        }

        // Stop is best-effort: an agent that succeeded shouldn't get its
        // success overridden by a stale-container stop error. The
        // surrounding workflow run continues even if stop hits a flake.
        if let Err(err) = req.adapter.stop(&container_id) {
            tracing::warn!(?err, container = %container_id, "stopping container failed");
        }

        let handle = exec_result?;
        if handle.exit_code != 0 {
            bail!(
                "agent `{agent_name}` in node `{}` exited with code {} (log at {})",
                node.id,
                handle.exit_code,
                log_path.display()
            );
        }
        Ok(())
    }

    fn run_bash_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        script: &str,
        session: &Session,
    ) -> Result<()> {
        // Host-side execution per the plan. Agent-isolation does not apply.
        // The script's cwd is the workspace so `gh pr create` etc. find the
        // repo's git config; we shell into it with `sh -c "cd <ws> && …"`,
        // mirroring `local::LocalAdapter::exec`'s wrapping.
        let cmd = format!(
            "cd {} && {script}",
            shell_quote_path(req.workspace)
        );
        let result = self.invoker.run("sh", vec!["-c".to_string(), cmd]);
        let log_path = self.node_log_path(req, session, &node.id);
        match result {
            Ok(stdout) => {
                if let Err(err) = std::fs::write(&log_path, &stdout) {
                    tracing::warn!(?err, log = %log_path.display(), "writing bash log failed");
                }
                Ok(())
            }
            Err(err) => {
                let body = format!("--- bash failed ---\n{err:#}\n");
                let _ = std::fs::write(&log_path, body);
                bail!(
                    "bash node `{}` failed: {err:#} (log at {})",
                    node.id,
                    log_path.display()
                );
            }
        }
    }

    fn node_log_path(
        &self,
        req: &ExecuteRequest<'_>,
        session: &Session,
        node_id: &str,
    ) -> PathBuf {
        let _ = self; // method form keeps the surface symmetric with run_*_node.
        req.store
            .session_dir(&session.id)
            .join("logs")
            .join(format!("{node_id}.log"))
    }

    fn mark_failed(
        &self,
        req: &ExecuteRequest<'_>,
        session: &mut Session,
        err: &anyhow::Error,
    ) {
        // Best-effort: even when persistence fails, the original node
        // error is what the user cares about. Surface a save error through
        // tracing rather than overshadowing the real failure.
        if let Err(transition_err) = session.transition_to(SessionState::Failed, (self.clock)()) {
            tracing::warn!(
                ?transition_err,
                session = %session.id,
                "transitioning to Failed after node error failed"
            );
        }
        if let Err(save_err) = req.store.save(session) {
            tracing::warn!(?save_err, session = %session.id, original = %err, "saving Failed session failed");
        }
    }
}

fn passthrough_env(agent: &AgentSpec) -> Vec<(String, String)> {
    agent
        .env_passthrough
        .iter()
        .map(|k| (k.clone(), std::env::var(k).unwrap_or_default()))
        .collect()
}

/// Phase 1 supports agent + bash. Reject the rest with a single, specific
/// error so the user doesn't watch a workflow start running and then fail
/// midway through. Detect at the top of `execute` so the session row
/// never gets created for unsupported workflows.
fn reject_unsupported_node_kinds(wf: &Workflow) -> Result<()> {
    for n in &wf.nodes {
        match &n.kind {
            NodeKind::Agent { .. } | NodeKind::Bash { .. } => {}
            NodeKind::Gate { .. } => bail!(
                "Phase 1 executor does not support gate nodes (workflow `{}`, node `{}`); \
                 human-gate handling lands with the Phase 2 workflow engine",
                wf.name,
                n.id
            ),
            NodeKind::Assert { .. } => bail!(
                "Phase 1 executor does not support assert nodes (workflow `{}`, node `{}`); \
                 expression evaluation lands in Phase 2",
                wf.name,
                n.id
            ),
            NodeKind::Fanout { .. } => bail!(
                "Phase 1 executor does not support fanout nodes (workflow `{}`, node `{}`); \
                 parallel sibling execution lands in Phase 2",
                wf.name,
                n.id
            ),
        }
        if n.loop_back_to.is_some() {
            bail!(
                "Phase 1 executor does not support loop_back_to (workflow `{}`, node `{}`); \
                 bounded revision cycles land in Phase 2",
                wf.name,
                n.id
            );
        }
    }
    Ok(())
}

/// Kahn's algorithm topo sort with stable tie-breaking (alphabetical by id)
/// so logs and tests get deterministic ordering across runs. Returns ids
/// in execution order. Cycles are rejected upstream via `validate`; if one
/// slips through here we still surface a clear error.
fn topological_order(wf: &Workflow) -> Result<Vec<String>> {
    let mut indegree: HashMap<String, usize> = wf
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.depends_on.len()))
        .collect();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for n in &wf.nodes {
        for dep in &n.depends_on {
            adjacency.entry(dep.clone()).or_default().push(n.id.clone());
        }
    }

    let mut ready: Vec<String> = indegree
        .iter()
        .filter(|&(_, deg)| *deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    ready.sort();
    let mut queue: VecDeque<String> = ready.into();

    let mut order = Vec::with_capacity(wf.nodes.len());
    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        let mut next: Vec<String> = adjacency.remove(&id).unwrap_or_default();
        next.sort();
        for dependent in next {
            if let Some(deg) = indegree.get_mut(&dependent) {
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dependent);
                }
            }
        }
    }
    if order.len() != wf.nodes.len() {
        bail!(
            "workflow `{}` is not a DAG (topological sort produced {} of {} nodes)",
            wf.name,
            order.len(),
            wf.nodes.len()
        );
    }
    Ok(order)
}

/// POSIX-shell single-quote escaping. Same shape as
/// `local::shell_escape` — duplicated rather than re-exported because the
/// local adapter's version is private and small enough that DRY here would
/// cost more than it saves.
fn shell_quote_path(p: &Path) -> String {
    let s = p.display().to_string();
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::runtime::local::LocalAdapter;
    use mockall::predicate::{always, eq};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn sample_devcontainer() -> Devcontainer {
        Devcontainer::from_str_at(
            r#"{ "image": "test:1" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap()
    }

    /// Deterministic clock: returns 1, 2, 3, … on successive calls.
    fn counter_clock() -> impl Fn() -> u64 + Send + Sync + 'static {
        let counter = Arc::new(AtomicU64::new(0));
        move || counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Build a `WorkflowExecutor` whose invoker returns a fixed stdout for
    /// every call. Useful for one-shot agent/bash success paths.
    fn executor_returning(stdout: &'static str) -> WorkflowExecutor {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |_, _| Ok(stdout.to_string()));
        WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock())
    }

    /// Make a `WorkflowExecutor` whose first invoker call errors, and any
    /// subsequent calls succeed (for the bash-node-fails test).
    fn executor_failing_once() -> WorkflowExecutor {
        let mut mock = MockProcessInvoker::new();
        let calls = AtomicU64::new(0);
        mock.expect_run().returning(move |_, _| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                Err(anyhow!("script bombed"))
            } else {
                Ok(String::new())
            }
        });
        WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock())
    }

    fn local_adapter_with_stdout(stdout: &'static str) -> LocalAdapter {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |_, _| Ok(stdout.to_string()));
        LocalAdapter::new(Arc::new(mock))
    }

    fn build_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        (dir, store)
    }

    #[test]
    fn one_agent_node_workflow_completes_against_local_adapter() {
        let yaml = "\
name: trivial
nodes:
  - id: only
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("ok\n");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-trivial"),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert_eq!(session.current_node.as_deref(), Some("only"));
        // Log was captured.
        let log = store.session_dir(&session.id).join("logs").join("only.log");
        assert!(log.is_file(), "expected log at {}", log.display());
    }

    #[test]
    fn agent_node_failure_marks_session_failed_and_propagates_error() {
        // LocalAdapter::exec returns a handle with exit_code=1 when the
        // invoker errors. The executor must see that and mark Failed.
        let yaml = "\
name: failing
nodes:
  - id: boom
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Err(anyhow!("nope")));
        let adapter = LocalAdapter::new(Arc::new(mock));
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-fail"),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exited with code 1"), "got: {msg}");
        // Session ends in Failed (even though execute returned Err).
        let loaded = store.load(&SessionId::new("s-fail")).unwrap();
        assert_eq!(loaded.state, SessionState::Failed);
    }

    #[test]
    fn unknown_agent_name_errors_and_session_ends_failed() {
        let yaml = "\
name: x
nodes:
  - id: oops
    agent: ghost
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-ghost"),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("unknown agent `ghost`"));
        assert_eq!(
            store.load(&SessionId::new("s-ghost")).unwrap().state,
            SessionState::Failed
        );
    }

    #[test]
    fn bash_node_runs_via_invoker_and_completes() {
        let yaml = "\
name: bash-only
nodes:
  - id: hello
    type: bash
    script: 'echo hello'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("sh"),
                eq(vec!["-c".to_string(), "cd '/repo' && echo hello".to_string()]),
            )
            .returning(|_, _| Ok("hello\n".to_string()));
        let executor =
            WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-bash"),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        let log = store
            .session_dir(&session.id)
            .join("logs")
            .join("hello.log");
        let log_text = std::fs::read_to_string(log).unwrap();
        assert_eq!(log_text, "hello\n");
    }

    #[test]
    fn bash_node_failure_marks_session_failed() {
        let yaml = "\
name: bash-fail
nodes:
  - id: bad
    type: bash
    script: 'false'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_failing_once();
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-bashfail"),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("bash node `bad` failed"));
        assert_eq!(
            store.load(&SessionId::new("s-bashfail")).unwrap().state,
            SessionState::Failed
        );
    }

    #[test]
    fn gate_node_is_rejected_before_session_creation() {
        let yaml = "\
name: with-gate
nodes:
  - id: g
    type: gate
    summary: 'human'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-gate"),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("does not support gate nodes"));
        // Session row must NOT have been created — the failure happened
        // before persistence kicked in.
        assert!(store.load(&SessionId::new("s-gate")).is_err());
    }

    #[test]
    fn fanout_node_is_rejected() {
        let yaml = "\
name: with-fanout
nodes:
  - id: f
    type: fanout
    siblings: [a]
  - id: a
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-fan"),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("does not support fanout nodes"));
    }

    #[test]
    fn loop_back_to_is_rejected() {
        let yaml = "\
name: with-loop
nodes:
  - id: review
    agent: claude-code
  - id: revise
    depends_on: [review]
    agent: claude-code
    loop_back_to: review
    max_loops: 1
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-loop"),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("does not support loop_back_to"));
    }

    #[test]
    fn topological_order_handles_diamond() {
        let yaml = "\
name: dia
nodes:
  - id: a
    agent: claude-code
  - id: b1
    depends_on: [a]
    agent: claude-code
  - id: b2
    depends_on: [a]
    agent: claude-code
  - id: c
    depends_on: [b1, b2]
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        // `a` runs first; `c` runs last; b1/b2 in alphabetical order.
        assert_eq!(order, vec!["a", "b1", "b2", "c"]);
    }

    #[test]
    fn topological_order_is_deterministic_for_independent_roots() {
        let yaml = "\
name: roots
nodes:
  - id: zzz
    agent: claude-code
  - id: aaa
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        assert_eq!(order, vec!["aaa", "zzz"]);
    }

    #[test]
    fn multi_node_diamond_workflow_runs_all_nodes_in_order() {
        let yaml = "\
name: dia
nodes:
  - id: plan
    agent: claude-code
  - id: code
    depends_on: [plan]
    agent: claude-code
  - id: test
    depends_on: [plan]
    agent: claude-code
  - id: review
    depends_on: [code, test]
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("ok\n");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-dia"),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // Last node id sticks in current_node.
        assert_eq!(session.current_node.as_deref(), Some("review"));
        // All four log files exist.
        for n in &["plan", "code", "test", "review"] {
            let log = store
                .session_dir(&session.id)
                .join("logs")
                .join(format!("{n}.log"));
            assert!(log.is_file(), "missing log: {}", log.display());
        }
    }
}
