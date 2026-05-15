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

/// Issue the workflow is acting on, when one was supplied via the
/// tracker. `None` means "no issue context" (the workflow ran without
/// `--issue`). Both forms are first-class: not every workflow needs a
/// tracker issue (e.g. `fleet workflow run lint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueContext {
    /// Opaque tracker id (`"gh:42"`, full git-bug hash).
    pub id: String,
    /// User-facing id (`"42"`, short git-bug hash).
    pub human_id: String,
    pub title: String,
}

/// What [`WorkflowExecutor::execute`] needs to run one workflow. Bundled
/// into a struct so the function signature stays stable as Phase 2 adds
/// fields (e.g. autonomous-mode bounds, tracker callbacks).
pub struct ExecuteRequest<'a> {
    pub workflow: &'a Workflow,
    pub adapter: &'a dyn RuntimeAdapter,
    pub agents: &'a AgentRegistry,
    pub store: &'a SessionStore,
    pub devcontainer: &'a Devcontainer,
    /// Repo root that gets bind-mounted as the container's workspace.
    pub workspace: &'a Path,
    pub session_id: SessionId,
    /// Issue the workflow is acting on, if any. Surfaces to nodes via
    /// `FLEET_ISSUE_ID` / `FLEET_ISSUE_HUMAN_ID` / `FLEET_ISSUE_TITLE`
    /// in both agent and bash node environments.
    pub issue: Option<IssueContext>,
}

impl WorkflowExecutor {
    pub fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self {
            invoker,
            clock: Box::new(now_ms),
        }
    }

    /// Test override for the clock. Production callers ignore this;
    /// tests pass a counter or fixed timestamp so assertions stay
    /// deterministic.
    #[must_use]
    #[allow(dead_code)]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Run the workflow end-to-end. Mints the session row on disk, drives
    /// it through `Running` to a terminal state (or to `AwaitingGate`,
    /// for workflows that pause on a gate), and returns the final
    /// snapshot. Node-level errors transition the session to `Failed`
    /// and propagate.
    ///
    /// `loop_back_to` is honoured: after a node with `loop_back_to: X`
    /// succeeds, the executor jumps back to `X` and replays the slice
    /// `[X ..= current]` in topological order. The cycle repeats up to
    /// `max_loops` times (default 1 when omitted). The per-loop counter
    /// is held in memory only — `resume`-after-gate restarts the
    /// counter, which is acceptable in v1.
    ///
    /// Gate nodes transition the session to `AwaitingGate` and return.
    /// Continue from the next node via [`Self::resume`].
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

        self.run_loop(req, &mut session, &order, 0)?;
        self.finalize(req, &mut session)?;
        Ok(session)
    }

    /// Resume a workflow paused at a gate. Loads the persisted session,
    /// verifies it's in `AwaitingGate`, and continues from the node
    /// *after* the gate. The workflow YAML is re-parsed by the caller
    /// (held by `req.workflow`); the topological order is recomputed
    /// from scratch so a workflow edited between pause and resume is
    /// honoured.
    pub fn resume(&self, req: &ExecuteRequest<'_>) -> Result<Session> {
        validate(req.workflow).context("workflow failed static validation")?;
        let order = topological_order(req.workflow)?;
        reject_unsupported_node_kinds(req.workflow)?;

        let mut session = req
            .store
            .load(&req.session_id)
            .with_context(|| format!("loading session `{}` for resume", req.session_id))?;
        if session.state != SessionState::AwaitingGate {
            bail!(
                "cannot resume session `{}`: state is {:?}, expected awaiting_gate",
                session.id,
                session.state
            );
        }
        // Resume picks up *after* the gate. Find the gate node's
        // position; an unknown current_node (shouldn't happen but
        // guard) means we restart from index 0.
        let start = session
            .current_node
            .as_ref()
            .and_then(|node_id| order.iter().position(|id| id == node_id))
            .map_or(0, |i| i + 1);
        session.transition_to(SessionState::Running, (self.clock)())?;
        req.store.save(&session)?;
        self.run_loop(req, &mut session, &order, start)?;
        self.finalize(req, &mut session)?;
        Ok(session)
    }

    /// The shared inner loop driving both `execute` and `resume`. Walks
    /// `order` from `start_idx`, runs each node, and honours
    /// `loop_back_to` and gate-pause transitions. Returns Ok with the
    /// session left in `Running` (caller transitions to `Completed`) or
    /// in `AwaitingGate` (caller leaves it alone). Errors propagate
    /// after marking the session `Failed`.
    fn run_loop(
        &self,
        req: &ExecuteRequest<'_>,
        session: &mut Session,
        order: &[String],
        start_idx: usize,
    ) -> Result<()> {
        let mut loop_counts: HashMap<String, u32> = HashMap::new();
        let mut i = start_idx;
        while i < order.len() {
            let node_id = order[i].clone();
            let node = req
                .workflow
                .node(&node_id)
                .expect("topological_order only returns ids from the workflow's nodes");
            session.set_current_node(Some(node_id.clone()), (self.clock)());
            req.store.save(session)?;

            // Gate: pause the workflow and let the user resume later.
            // We persist the gate's summary as the node's "log" so the
            // TUI / `fleet sessions logs` surfaces it.
            if let NodeKind::Gate { summary } = &node.kind {
                if let Err(err) = self.handle_gate(req, session, &node_id, summary) {
                    self.mark_failed(req, session, &err);
                    return Err(err);
                }
                return Ok(());
            }

            if let Err(err) = self.run_node(req, node, session) {
                self.mark_failed(req, session, &err);
                return Err(err);
            }
            i = resolve_loop_back(node, order, &mut loop_counts).map_or(i + 1, |t| t);
        }
        Ok(())
    }

    /// Drive a gate-node visit: transition the session to `AwaitingGate`,
    /// persist a gate-summary log so the user sees why the workflow
    /// paused. Pure save semantics; no adapter interaction.
    fn handle_gate(
        &self,
        req: &ExecuteRequest<'_>,
        session: &mut Session,
        node_id: &str,
        summary: &str,
    ) -> Result<()> {
        session.transition_to(SessionState::AwaitingGate, (self.clock)())?;
        req.store.save(session)?;
        let log_path = self.node_log_path(req, session, node_id);
        let body = format!("--- gate ---\n{summary}\n");
        if let Err(err) = std::fs::write(&log_path, body) {
            tracing::warn!(?err, log = %log_path.display(), "writing gate log failed");
        }
        Ok(())
    }

    /// Transition the session to `Completed` when the loop ended on a
    /// natural finish (state still `Running`). If it ended in
    /// `AwaitingGate` we leave it as-is so the resume path can pick up.
    fn finalize(&self, req: &ExecuteRequest<'_>, session: &mut Session) -> Result<()> {
        if session.state == SessionState::Running {
            session.transition_to(SessionState::Completed, (self.clock)())?;
            req.store.save(session)?;
        }
        Ok(())
    }

    fn run_node(&self, req: &ExecuteRequest<'_>, node: &Node, session: &Session) -> Result<()> {
        // Input artifact contract: declared `artifacts.in:` must exist
        // before the node runs. Detected here so a downstream node that
        // depends on an upstream-produced file fails loudly with a
        // pointer at the missing path rather than silently running on
        // empty inputs.
        let artifacts_dir = req.store.session_dir(&session.id).join("artifacts");
        verify_inputs(&artifacts_dir, node)?;

        let inner = match &node.kind {
            NodeKind::Agent { agent, .. } => self.run_agent_node(req, node, agent, session),
            NodeKind::Bash { script } => self.run_bash_node(req, node, script, session),
            // Unsupported kinds were rejected earlier in `execute`; this
            // branch is just an exhaustiveness guard.
            other => bail!(
                "internal: node `{}` of kind {other:?} reached run_node — should have been rejected upstream",
                node.id
            ),
        };
        inner?;

        // Output contract: declared `artifacts.out:` must be produced.
        // We check after the inner run so a failing node surfaces its
        // own error first; output-contract violations are reported as
        // node failures with their own clear wording.
        verify_outputs(&artifacts_dir, node)?;
        Ok(())
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
        // Pull persona + prompt_file out of the node. We just matched
        // NodeKind::Agent in run_node, but the borrow there has already
        // gone away — re-match here. The plain `_` arm is unreachable
        // because run_node dispatches on kind before calling us.
        let (persona, prompt_file) = match &node.kind {
            NodeKind::Agent {
                persona,
                prompt_file,
                ..
            } => (persona.as_deref(), prompt_file.as_ref()),
            _ => unreachable!("run_agent_node only reached via NodeKind::Agent"),
        };
        let prompt = resolve_prompt_artifact(req.workspace, prompt_file).with_context(|| {
            format!("preparing prompt for agent node `{}`", node.id)
        })?;
        let agent_ctx = AgentContext {
            persona,
            prompt: prompt.as_ref(),
            issue: req.issue.as_ref(),
        };
        let env = build_agent_env(agent, &agent_ctx);

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
        // mirroring `local::LocalAdapter::exec`'s wrapping. Issue env vars
        // are spliced in *after* the cd so they live in the script's
        // environment without polluting the outer shell.
        let env_prefix = bash_issue_env_prefix(req.issue.as_ref());
        let cmd = format!(
            "cd {} && {env_prefix}{script}",
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

/// Confirm that every path declared in `node.artifacts.in` exists under
/// `artifacts_dir`. Returns an error naming the first missing path so
/// users get an actionable pointer instead of a silent run on empty
/// inputs. Empty `in:` list (the common case) is a no-op.
pub fn verify_inputs(artifacts_dir: &Path, node: &Node) -> Result<()> {
    for rel in &node.artifacts.r#in {
        let path = artifacts_dir.join(rel);
        if !path.exists() {
            bail!(
                "node `{}` requires input artifact `{rel}` but it is missing from {}",
                node.id,
                artifacts_dir.display()
            );
        }
    }
    Ok(())
}

/// Confirm that every path declared in `node.artifacts.out` was produced
/// under `artifacts_dir`. Empty `out:` list is a no-op. A missing output
/// is reported as the node failing — the inner run already returned Ok
/// at this point so the user sees the contract-violation wording.
pub fn verify_outputs(artifacts_dir: &Path, node: &Node) -> Result<()> {
    for rel in &node.artifacts.out {
        let path = artifacts_dir.join(rel);
        if !path.exists() {
            bail!(
                "node `{}` was expected to produce output artifact `{rel}` but it is missing from {}",
                node.id,
                artifacts_dir.display()
            );
        }
    }
    Ok(())
}

/// Per-node context that flows into the agent's environment alongside
/// its base `env_passthrough` whitelist. Held by reference fields so the
/// caller can reuse the underlying strings; tests construct one inline
/// via [`Default`] and tweak the fields they care about.
#[derive(Debug, Default, Clone, Copy)]
pub struct AgentContext<'a> {
    /// Optional persona name. Exposed as `FLEET_PERSONA` when set.
    pub persona: Option<&'a str>,
    /// Optional prompt file (source path + body). Exposed as
    /// `FLEET_PROMPT_FILE` (path as authored) + `FLEET_PROMPT` (file
    /// contents) when set. Env-var delivery is the lowest common
    /// denominator across agent backends.
    pub prompt: Option<&'a PromptArtifact>,
    /// Optional issue the workflow is acting on. Exposed as the
    /// `FLEET_ISSUE_*` trio when set.
    pub issue: Option<&'a IssueContext>,
}

/// Resolved prompt file: the user-authored source path (for
/// `FLEET_PROMPT_FILE`) and the on-disk body (for `FLEET_PROMPT`).
/// Built by [`resolve_prompt_artifact`] before
/// [`WorkflowExecutor::run_agent_node`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArtifact {
    pub source: String,
    pub body: String,
}

/// Build the env Vec the agent's container receives: the agent's
/// `env_passthrough` whitelist (resolved against the host's environment)
/// plus any persona / prompt / issue context. Pure; the testable seam
/// for `run_agent_node`.
#[must_use]
pub fn build_agent_env(
    agent: &AgentSpec,
    ctx: &AgentContext<'_>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = agent
        .env_passthrough
        .iter()
        .map(|k| (k.clone(), std::env::var(k).unwrap_or_default()))
        .collect();
    if let Some(persona) = ctx.persona {
        env.push(("FLEET_PERSONA".to_string(), persona.to_string()));
    }
    if let Some(prompt) = ctx.prompt {
        env.push(("FLEET_PROMPT_FILE".to_string(), prompt.source.clone()));
        env.push(("FLEET_PROMPT".to_string(), prompt.body.clone()));
    }
    if let Some(issue) = ctx.issue {
        env.push(("FLEET_ISSUE_ID".to_string(), issue.id.clone()));
        env.push(("FLEET_ISSUE_HUMAN_ID".to_string(), issue.human_id.clone()));
        env.push(("FLEET_ISSUE_TITLE".to_string(), issue.title.clone()));
    }
    env
}

/// Read the agent node's `prompt_file:` (if any) into a [`PromptArtifact`].
/// Relative paths resolve against `workspace`. Missing-file is surfaced as
/// a Result error — the workflow author declared the file, so we don't
/// want the agent to silently run without it.
pub fn resolve_prompt_artifact(
    workspace: &Path,
    prompt_file: Option<&PathBuf>,
) -> Result<Option<PromptArtifact>> {
    let Some(pf) = prompt_file else {
        return Ok(None);
    };
    let absolute = if pf.is_absolute() {
        pf.clone()
    } else {
        workspace.join(pf)
    };
    let body = std::fs::read_to_string(&absolute)
        .with_context(|| format!("reading prompt_file at {}", absolute.display()))?;
    Ok(Some(PromptArtifact {
        source: pf.display().to_string(),
        body,
    }))
}

/// Build the env-var prefix that bash node scripts get injected with:
/// `FLEET_ISSUE_ID='…' FLEET_ISSUE_HUMAN_ID='…' FLEET_ISSUE_TITLE='…' `
/// (with trailing space). Returns the empty string when no issue
/// context is present so the unwrapped path is byte-identical to what
/// it looked like before this feature landed. Pure; testable seam.
#[must_use]
pub fn bash_issue_env_prefix(issue: Option<&IssueContext>) -> String {
    let Some(ctx) = issue else {
        return String::new();
    };
    format!(
        "FLEET_ISSUE_ID={} FLEET_ISSUE_HUMAN_ID={} FLEET_ISSUE_TITLE={} ",
        shell_quote_value(&ctx.id),
        shell_quote_value(&ctx.human_id),
        shell_quote_value(&ctx.title),
    )
}

/// POSIX-shell single-quote escape for an arbitrary string. Same
/// scheme as [`shell_quote_path`] but takes `&str` because issue
/// fields are strings, not paths.
fn shell_quote_value(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Phase 2 supports agent + bash + `loop_back_to`. Gate, assert, fanout
/// land in subsequent commits — until then they're rejected at the top
/// of `execute` so the session row never gets created for unsupported
/// workflows.
fn reject_unsupported_node_kinds(wf: &Workflow) -> Result<()> {
    for n in &wf.nodes {
        match &n.kind {
            NodeKind::Agent { .. } | NodeKind::Bash { .. } | NodeKind::Gate { .. } => {}
            NodeKind::Assert { .. } => bail!(
                "executor does not yet support assert nodes (workflow `{}`, node `{}`); \
                 expression evaluation lands in a later commit",
                wf.name,
                n.id
            ),
            NodeKind::Fanout { .. } => bail!(
                "executor does not yet support fanout nodes (workflow `{}`, node `{}`); \
                 parallel sibling execution lands in a later commit",
                wf.name,
                n.id
            ),
        }
    }
    Ok(())
}

/// Decide whether a node's `loop_back_to` fires, returning the index in
/// `order` to jump to. `None` means "continue forward". The per-node
/// loop counter is mutated in place; an unrecognised target (which
/// validation already rejects) is treated as "continue forward" so a
/// stray bug never traps the executor.
fn resolve_loop_back(
    node: &Node,
    order: &[String],
    loop_counts: &mut HashMap<String, u32>,
) -> Option<usize> {
    let target_id = node.loop_back_to.as_ref()?;
    let max = node.max_loops.unwrap_or(1);
    let count = loop_counts.entry(node.id.clone()).or_insert(0);
    if *count >= max {
        return None;
    }
    let target_idx = order.iter().position(|id| id == target_id)?;
    *count += 1;
    Some(target_idx)
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

    fn sample_issue() -> IssueContext {
        IssueContext {
            id: "gh:42".to_string(),
            human_id: "42".to_string(),
            title: "Fix the parser".to_string(),
        }
    }

    fn agent_node(id: &str, inputs: &[&str], outputs: &[&str]) -> Node {
        use crate::workflow::spec::{ArtifactsSpec, NodeKind};
        Node {
            id: id.to_string(),
            depends_on: Vec::new(),
            when: None,
            kind: NodeKind::Agent {
                agent: "x".to_string(),
                persona: None,
                prompt_file: None,
            },
            artifacts: ArtifactsSpec {
                r#in: inputs.iter().map(|s| (*s).to_string()).collect(),
                out: outputs.iter().map(|s| (*s).to_string()).collect(),
            },
            outputs: std::collections::BTreeMap::new(),
            loop_back_to: None,
            max_loops: None,
        }
    }

    #[test]
    fn verify_inputs_passes_when_no_inputs_declared() {
        let tmp = tempfile::tempdir().unwrap();
        verify_inputs(tmp.path(), &agent_node("n", &[], &[])).unwrap();
    }

    #[test]
    fn verify_inputs_passes_when_all_inputs_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plan.md"), "p").unwrap();
        std::fs::write(tmp.path().join("notes.md"), "n").unwrap();
        verify_inputs(tmp.path(), &agent_node("n", &["plan.md", "notes.md"], &[])).unwrap();
    }

    #[test]
    fn verify_inputs_fails_with_specific_path_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plan.md"), "p").unwrap();
        let err = verify_inputs(tmp.path(), &agent_node("code", &["plan.md", "review.md"], &[]))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("node `code` requires input artifact `review.md`"), "got: {msg}");
    }

    #[test]
    fn verify_inputs_reports_first_missing_only() {
        // Stable wording: report the first missing artifact (per the
        // declared order in the YAML) so error messages stay
        // deterministic regardless of filesystem walk order.
        let tmp = tempfile::tempdir().unwrap();
        let err = verify_inputs(tmp.path(), &agent_node("n", &["first.md", "second.md"], &[]))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("first.md"), "got: {msg}");
        assert!(!msg.contains("second.md"), "got: {msg}");
    }

    #[test]
    fn verify_outputs_passes_when_no_outputs_declared() {
        let tmp = tempfile::tempdir().unwrap();
        verify_outputs(tmp.path(), &agent_node("n", &[], &[])).unwrap();
    }

    #[test]
    fn verify_outputs_passes_when_all_outputs_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("out.md"), "o").unwrap();
        verify_outputs(tmp.path(), &agent_node("n", &[], &["out.md"])).unwrap();
    }

    #[test]
    fn verify_outputs_fails_with_specific_path_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = verify_outputs(tmp.path(), &agent_node("plan", &[], &["plan.md"]))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("node `plan` was expected to produce output artifact `plan.md`"),
            "got: {msg}"
        );
    }

    #[test]
    fn bash_node_with_satisfied_input_contract_completes() {
        let yaml = "\
name: chain
nodes:
  - id: maker
    type: bash
    script: 'echo plan > $ARTIFACTS/plan.md'
  - id: consumer
    depends_on: [maker]
    type: bash
    script: 'cat $ARTIFACTS/plan.md > /dev/null'
    artifacts: { in: [plan.md] }
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        // Stub invoker that writes the artifact for `maker` and is a
        // no-op for `consumer`. ARTIFACTS is the bind-mounted dir; for
        // bash nodes the executor passes the host-side path directly.
        let session_id = SessionId::new("s-chain");
        // Path the executor will create via store.create(); compute it
        // upfront so the mock closure has a stable target to side-effect
        // into when it sees the maker script. The dir itself is created
        // by the executor (don't pre-create — store.create rejects
        // pre-existing dirs).
        let artifacts_dir = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            // First call: maker. We side-effect the artifact since the
            // bash script's `$ARTIFACTS` isn't actually substituted in
            // this synthetic test (no real bash exec via the mock).
            // Recognise maker by the literal "plan > $ARTIFACTS" in the
            // wrapped script.
            let script = args.iter().find(|a| a.contains("$ARTIFACTS")).cloned();
            if script.is_some() {
                std::fs::write(artifacts_dir.join("plan.md"), "plan\n").unwrap();
            }
            Ok(String::new())
        });
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
    }

    #[test]
    fn missing_input_artifact_fails_the_node_before_run() {
        let yaml = "\
name: needs-input
nodes:
  - id: consumer
    type: bash
    script: 'echo'
    artifacts: { in: [missing.md] }
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
            session_id: SessionId::new("s-missin"),
            issue: None,
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("requires input artifact `missing.md`"), "got: {msg}");
        // Session ends Failed.
        assert_eq!(
            store.load(&SessionId::new("s-missin")).unwrap().state,
            SessionState::Failed
        );
    }

    #[test]
    fn missing_output_artifact_fails_the_node_after_run() {
        let yaml = "\
name: must-produce
nodes:
  - id: liar
    type: bash
    script: 'echo not-writing-anything'
    artifacts: { out: [plan.md] }
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
            session_id: SessionId::new("s-liar"),
            issue: None,
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("expected to produce output artifact `plan.md`"),
            "got: {msg}"
        );
    }

    #[test]
    fn build_agent_env_returns_passthrough_only_when_context_empty() {
        // Empty passthrough + empty context → empty env.
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        assert!(build_agent_env(&spec, &AgentContext::default()).is_empty());
    }

    #[test]
    fn build_agent_env_resolves_passthrough_against_host_env() {
        // SAFETY: setting test-scoped env vars on the current process is
        // fine; tests in this file are not run in parallel against the
        // same variable.
        unsafe { std::env::set_var("FLEET_TEST_KEY", "secret") };
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: vec!["FLEET_TEST_KEY".to_string()],
        };
        let env = build_agent_env(&spec, &AgentContext::default());
        assert!(env.iter().any(|(k, v)| k == "FLEET_TEST_KEY" && v == "secret"));
        unsafe { std::env::remove_var("FLEET_TEST_KEY") };
    }

    #[test]
    fn build_agent_env_appends_issue_vars_when_present() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let issue = sample_issue();
        let ctx = AgentContext {
            issue: Some(&issue),
            ..AgentContext::default()
        };
        let env = build_agent_env(&spec, &ctx);
        assert!(env
            .iter()
            .any(|(k, v)| k == "FLEET_ISSUE_ID" && v == "gh:42"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "FLEET_ISSUE_HUMAN_ID" && v == "42"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "FLEET_ISSUE_TITLE" && v == "Fix the parser"));
    }

    #[test]
    fn build_agent_env_appends_persona_when_present() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let ctx = AgentContext {
            persona: Some("planner"),
            ..AgentContext::default()
        };
        let env = build_agent_env(&spec, &ctx);
        assert!(env.iter().any(|(k, v)| k == "FLEET_PERSONA" && v == "planner"));
    }

    #[test]
    fn build_agent_env_appends_prompt_artifact_when_present() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let prompt = PromptArtifact {
            source: "prompts/planner.md".to_string(),
            body: "You are the planner.\nThink step by step.".to_string(),
        };
        let ctx = AgentContext {
            prompt: Some(&prompt),
            ..AgentContext::default()
        };
        let env = build_agent_env(&spec, &ctx);
        assert!(env
            .iter()
            .any(|(k, v)| k == "FLEET_PROMPT_FILE" && v == "prompts/planner.md"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "FLEET_PROMPT" && v.contains("You are the planner.")));
    }

    #[test]
    fn build_agent_env_combines_all_context_fields() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let issue = sample_issue();
        let prompt = PromptArtifact {
            source: "p.md".to_string(),
            body: "body".to_string(),
        };
        let ctx = AgentContext {
            persona: Some("reviewer"),
            prompt: Some(&prompt),
            issue: Some(&issue),
        };
        let env = build_agent_env(&spec, &ctx);
        let keys: std::collections::HashSet<&str> =
            env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains("FLEET_PERSONA"));
        assert!(keys.contains("FLEET_PROMPT_FILE"));
        assert!(keys.contains("FLEET_PROMPT"));
        assert!(keys.contains("FLEET_ISSUE_ID"));
        assert!(keys.contains("FLEET_ISSUE_HUMAN_ID"));
        assert!(keys.contains("FLEET_ISSUE_TITLE"));
    }

    #[test]
    fn resolve_prompt_artifact_returns_none_when_no_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_prompt_artifact(tmp.path(), None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolve_prompt_artifact_reads_relative_path_under_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prompts")).unwrap();
        std::fs::write(tmp.path().join("prompts/planner.md"), "be the planner").unwrap();
        let pf = PathBuf::from("prompts/planner.md");
        let result = resolve_prompt_artifact(tmp.path(), Some(&pf))
            .unwrap()
            .unwrap();
        assert_eq!(result.source, "prompts/planner.md");
        assert_eq!(result.body, "be the planner");
    }

    #[test]
    fn resolve_prompt_artifact_propagates_absolute_path_intact() {
        // Absolute paths bypass workspace joining; useful for shared
        // prompt directories under $HOME or system locations.
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("global.md");
        std::fs::write(&abs, "global").unwrap();
        let result = resolve_prompt_artifact(Path::new("/some/other/workspace"), Some(&abs))
            .unwrap()
            .unwrap();
        assert_eq!(result.body, "global");
        assert!(result.source.ends_with("global.md"));
    }

    #[test]
    fn resolve_prompt_artifact_surfaces_missing_file_with_path_context() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = PathBuf::from("nope.md");
        let err = resolve_prompt_artifact(tmp.path(), Some(&pf)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope.md"), "msg = {msg}");
    }

    #[test]
    fn bash_issue_env_prefix_is_empty_when_no_issue() {
        assert_eq!(bash_issue_env_prefix(None), "");
    }

    #[test]
    fn bash_issue_env_prefix_emits_quoted_assignments_with_trailing_space() {
        let prefix = bash_issue_env_prefix(Some(&sample_issue()));
        assert_eq!(
            prefix,
            "FLEET_ISSUE_ID='gh:42' FLEET_ISSUE_HUMAN_ID='42' FLEET_ISSUE_TITLE='Fix the parser' "
        );
    }

    #[test]
    fn bash_issue_env_prefix_quotes_embedded_single_quotes() {
        let ctx = IssueContext {
            id: "gh:1".to_string(),
            human_id: "1".to_string(),
            title: "Doesn't render".to_string(),
        };
        let prefix = bash_issue_env_prefix(Some(&ctx));
        // `'` inside single-quoted string becomes `'\''` (close-quote,
        // escaped quote, re-open-quote) — POSIX-portable form.
        assert!(prefix.contains("'Doesn'\\''t render'"), "got: {prefix}");
    }

    #[test]
    fn bash_issue_env_prefix_handles_empty_title() {
        let ctx = IssueContext {
            id: "gh:1".to_string(),
            human_id: "1".to_string(),
            title: String::new(),
        };
        let prefix = bash_issue_env_prefix(Some(&ctx));
        assert!(prefix.contains("FLEET_ISSUE_TITLE='' "), "got: {prefix}");
    }

    #[test]
    fn bash_node_script_carries_issue_env_when_issue_present() {
        let yaml = "\
name: with-issue
nodes:
  - id: print
    type: bash
    script: 'echo $FLEET_ISSUE_HUMAN_ID'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let expected_cmd = "cd '/repo' && FLEET_ISSUE_ID='gh:42' FLEET_ISSUE_HUMAN_ID='42' FLEET_ISSUE_TITLE='Fix the parser' echo $FLEET_ISSUE_HUMAN_ID";

        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("sh"), eq(vec!["-c".to_string(), expected_cmd.to_string()]))
            .returning(|_, _| Ok("42\n".to_string()));
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-iss"),
            issue: Some(sample_issue()),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
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
            issue: None,
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
            issue: None,
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
            issue: None,
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
            issue: None,
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
            issue: None,
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("bash node `bad` failed"));
        assert_eq!(
            store.load(&SessionId::new("s-bashfail")).unwrap().state,
            SessionState::Failed
        );
    }

    #[test]
    fn gate_node_pauses_workflow_in_awaiting_gate_state() {
        let yaml = "\
name: with-gate
nodes:
  - id: setup
    type: bash
    script: 'echo setup'
  - id: g
    depends_on: [setup]
    type: gate
    summary: 'PR ready for human review'
  - id: cleanup
    depends_on: [g]
    type: bash
    script: 'echo cleanup'
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
            issue: None,
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::AwaitingGate);
        assert_eq!(session.current_node.as_deref(), Some("g"));
        // The gate's summary lands in the gate's log file so the TUI /
        // `fleet sessions logs` can show why the workflow paused.
        let log = store.session_dir(&session.id).join("logs/g.log");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("PR ready for human review"), "got: {body}");
        // The cleanup node must NOT have run yet.
        let cleanup_log = store.session_dir(&session.id).join("logs/cleanup.log");
        assert!(!cleanup_log.exists());
    }

    #[test]
    fn resume_continues_workflow_past_the_gate() {
        let yaml = "\
name: with-gate
nodes:
  - id: setup
    type: bash
    script: 'echo setup'
  - id: g
    depends_on: [setup]
    type: gate
    summary: 'human gate'
  - id: cleanup
    depends_on: [g]
    type: bash
    script: 'echo cleanup'
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
            session_id: SessionId::new("s-resume"),
            issue: None,
        };
        // First execute pauses at the gate.
        let paused = executor.execute(&req).unwrap();
        assert_eq!(paused.state, SessionState::AwaitingGate);

        // Resume picks up after the gate and runs cleanup, then
        // transitions to Completed.
        let resumed = executor.resume(&req).unwrap();
        assert_eq!(resumed.state, SessionState::Completed);
        assert_eq!(resumed.current_node.as_deref(), Some("cleanup"));
        // cleanup's log file now exists.
        let cleanup_log = store.session_dir(&resumed.id).join("logs/cleanup.log");
        assert!(cleanup_log.exists());
    }

    #[test]
    fn resume_rejects_non_paused_session() {
        let yaml = "\
name: simple
nodes:
  - id: x
    type: bash
    script: 'echo x'
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
            session_id: SessionId::new("s-done"),
            issue: None,
        };
        // First run completes (no gate).
        let done = executor.execute(&req).unwrap();
        assert_eq!(done.state, SessionState::Completed);
        // Resume should refuse — session is already terminal.
        let err = executor.resume(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("expected awaiting_gate"),
            "got: {err:#}"
        );
    }

    #[test]
    fn resume_rejects_when_session_directory_is_missing() {
        let yaml = "\
name: simple
nodes:
  - id: x
    type: bash
    script: 'echo x'
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
            issue: None,
        };
        let err = executor.resume(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("loading session `s-ghost`"),
            "got: {err:#}"
        );
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
            issue: None,
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("does not yet support fanout nodes"));
    }

    /// Helper: run `yaml` against an invoker that counts how many times
    /// each bash script fires. Returns (final session, per-script counts).
    /// All scripts are no-ops (Ok("")) so loop semantics are exercised
    /// without artifact-contract concerns.
    fn run_with_call_counter(
        yaml: &str,
        session_id: &str,
    ) -> (Session, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let script_log: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_clone = std::sync::Arc::clone(&script_log);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            // Extract the user-script (last positional arg after `cd ... &&`).
            let script = args.last().cloned().unwrap_or_default();
            log_clone.lock().unwrap().push(script);
            Ok(String::new())
        });
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new(session_id),
            issue: None,
        };
        let session = executor.execute(&req).unwrap();
        (session, script_log)
    }

    #[test]
    fn loop_back_to_replays_target_slice_for_max_loops_iterations() {
        // a → b with b.loop_back_to=a, max_loops=2 produces three full
        // a-then-b passes (1 original + 2 revision cycles). Script log
        // expected: a1 b1 a2 b2 a3 b3.
        let yaml = "\
name: rev
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
    loop_back_to: a
    max_loops: 2
";
        let (session, log) = run_with_call_counter(yaml, "s-rev");
        assert_eq!(session.state, SessionState::Completed);
        let scripts: Vec<String> = log.lock().unwrap().clone();
        // Each entry contains `cd '/repo' && <script>` — match on the
        // tail.
        let tags: Vec<&str> = scripts
            .iter()
            .map(|s| if s.contains("echo a") { "a" } else { "b" })
            .collect();
        assert_eq!(tags, vec!["a", "b", "a", "b", "a", "b"]);
    }

    #[test]
    fn loop_back_to_with_max_loops_default_runs_one_revision_pass() {
        // No `max_loops` field → defaults to 1, so loops once.
        let yaml = "\
name: rev
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
    loop_back_to: a
";
        let (session, log) = run_with_call_counter(yaml, "s-rev-default");
        assert_eq!(session.state, SessionState::Completed);
        let scripts: Vec<String> = log.lock().unwrap().clone();
        let tags: Vec<&str> = scripts
            .iter()
            .map(|s| if s.contains("echo a") { "a" } else { "b" })
            .collect();
        // 2 passes: original + 1 revision.
        assert_eq!(tags, vec!["a", "b", "a", "b"]);
    }

    #[test]
    fn loop_back_to_with_max_loops_zero_does_not_loop() {
        // Explicit max_loops: 0 skips the cycle entirely — useful for
        // workflows that conditionally disable revisions via config
        // edits without dropping the loop_back_to field.
        let yaml = "\
name: rev
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
    loop_back_to: a
    max_loops: 0
";
        let (session, log) = run_with_call_counter(yaml, "s-rev-zero");
        assert_eq!(session.state, SessionState::Completed);
        let scripts: Vec<String> = log.lock().unwrap().clone();
        assert_eq!(scripts.len(), 2, "expected 2 runs (a,b), got {scripts:?}");
    }

    #[test]
    fn loop_back_to_replays_intermediates_too() {
        // Three-node chain a → b → c with c.loop_back_to=a. Replays the
        // full [a, b, c] slice each cycle.
        let yaml = "\
name: rev3
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
  - id: c
    depends_on: [b]
    type: bash
    script: 'echo c'
    loop_back_to: a
    max_loops: 1
";
        let (_, log) = run_with_call_counter(yaml, "s-rev3");
        let scripts: Vec<String> = log.lock().unwrap().clone();
        let tags: Vec<&str> = scripts
            .iter()
            .map(|s| {
                if s.contains("echo a") {
                    "a"
                } else if s.contains("echo b") {
                    "b"
                } else {
                    "c"
                }
            })
            .collect();
        assert_eq!(tags, vec!["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn loop_back_to_does_not_stack_across_independent_loops() {
        // Two independent loops in the same workflow: a→b cycles once,
        // c→d cycles once. The b-counter must not bleed into the d-loop.
        let yaml = "\
name: two-loops
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
    loop_back_to: a
    max_loops: 1
  - id: c
    depends_on: [b]
    type: bash
    script: 'echo c'
  - id: d
    depends_on: [c]
    type: bash
    script: 'echo d'
    loop_back_to: c
    max_loops: 1
";
        let (_, log) = run_with_call_counter(yaml, "s-two-loops");
        let scripts: Vec<String> = log.lock().unwrap().clone();
        let tags: Vec<&str> = scripts
            .iter()
            .map(|s| {
                if s.contains("echo a") {
                    "a"
                } else if s.contains("echo b") {
                    "b"
                } else if s.contains("echo c") {
                    "c"
                } else {
                    "d"
                }
            })
            .collect();
        // a,b,a,b (first loop done) → c,d,c,d (second loop done).
        assert_eq!(tags, vec!["a", "b", "a", "b", "c", "d", "c", "d"]);
    }

    #[test]
    fn resolve_loop_back_returns_target_index_when_under_max() {
        let yaml = "\
name: rev
nodes:
  - id: a
    type: bash
    script: 'x'
  - id: b
    depends_on: [a]
    type: bash
    script: 'y'
    loop_back_to: a
    max_loops: 2
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        let b = wf.node("b").unwrap();
        let mut counts: HashMap<String, u32> = HashMap::new();
        let idx = resolve_loop_back(b, &order, &mut counts).unwrap();
        assert_eq!(order[idx], "a");
        assert_eq!(counts["b"], 1);
    }

    #[test]
    fn resolve_loop_back_returns_none_at_max() {
        let yaml = "\
name: rev
nodes:
  - id: a
    type: bash
    script: 'x'
  - id: b
    depends_on: [a]
    type: bash
    script: 'y'
    loop_back_to: a
    max_loops: 1
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        let b = wf.node("b").unwrap();
        let mut counts: HashMap<String, u32> = HashMap::new();
        counts.insert("b".to_string(), 1); // already looped once
        assert!(resolve_loop_back(b, &order, &mut counts).is_none());
    }

    #[test]
    fn resolve_loop_back_returns_none_when_no_loop_back_to() {
        let yaml = "\
name: lin
nodes:
  - id: a
    type: bash
    script: 'x'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        let a = wf.node("a").unwrap();
        let mut counts: HashMap<String, u32> = HashMap::new();
        assert!(resolve_loop_back(a, &order, &mut counts).is_none());
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
            issue: None,
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
