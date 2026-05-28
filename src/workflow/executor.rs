//! Phase-1 workflow executor.
//!
//! Drives a parsed [`Workflow`] against a [`RuntimeAdapter`], agent
//! registry, and [`SessionStore`]. Supports every `NodeKind`:
//! `Agent`, `Bash`, `Gate`, `Assert`, `Fanout` — plus `loop_back_to`
//! and `when:` predicates over upstream `outputs:`.
//!
//! Fanout siblings are excluded from the main topological order
//! (they're owned by their fanout, not standalone-scheduled) and run
//! in parallel via `std::thread::scope` when the fanout fires. Sibling
//! `outputs:` flow into the same accumulator the rest of the workflow
//! uses, so downstream `when:` predicates can gate on parallel
//! discoveries.
//!
//! Lifecycle: `Created → Running → (Failed on node failure | Completed on
//! all nodes done)`. Each node bumps `current_node` so the TUI/sidebar can
//! show progress. Per-node logs land under `.fleet/sessions/<id>/logs/`.
//!
//! Containers are ephemeral per agent node — the plan calls for fresh
//! provisioning. `ensure_image` is idempotent so repeated nodes against
//! the same devcontainer don't rebuild.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::expr::{self, OutputMap};
use super::outcome::validate_outcome;
use super::spec::{Node, NodeKind, Workflow};
use super::validate::validate;
use crate::agent::AgentRegistry;
use crate::agent::cost::parse_agent_cost_usd;
use crate::agent::registry::AgentSpec;
use crate::bridge::{BRIDGE_HOST, Bridge};
use crate::process::ProcessInvoker;
use crate::runtime::devcontainer::Devcontainer;
use crate::runtime::{ContainerSpec, ExecOpts, ImageId, MountSpec, RuntimeAdapter, host_arch_oci};
use crate::session::store::SessionStore;
use crate::session::{IssueContext, PrContext, Session, SessionId, SessionState, now_ms};
use crate::tracker::Tracker;

/// What a node's run produced *besides* a Result. Today carries any
/// parsed agent-cost figures and engine-emitted output map entries
/// (used by `tracker-create` to surface the new ticket's id); future
/// entries (resource usage, exit reason) follow the same shape.
/// Returned from [`WorkflowExecutor::run_node`] and
/// [`WorkflowExecutor::run_fanout_node`] so the run loop — which owns
/// `&mut Session` — can apply the outcome without the inner functions
/// needing mutable access. The pattern matters most for fanout, whose
/// siblings run in `std::thread::scope` with shared access to Session
/// only, or — under `FLEET_TMUX_SESSION` — in per-sibling subprocesses
/// that serialise this value through a file on disk.
///
/// `Serialize` + `Deserialize` are load-bearing for the tmux-windows
/// fanout path: each sibling subprocess writes a `FanoutOutcome::Ok`
/// JSON file the driver picks up.
#[allow(clippy::redundant_pub_crate)]
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct NodeOutcome {
    /// `(node_id, usd)` pairs. Usually 0 or 1 entry; a fanout returns
    /// one per agent sibling that produced a parseable cost line.
    pub(crate) node_costs: Vec<(String, f64)>,
    /// `((node_id, output_name), value)` tuples the executor itself
    /// produced (not from an artifact file). Distinct from
    /// `extract_outputs` — that path is for agent-produced
    /// `<node>.outputs.json` files; this is for engine-driven nodes
    /// like `tracker-create` that compute outputs in Rust.
    pub(crate) extra_outputs: Vec<((String, String), String)>,
}

impl NodeOutcome {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    fn with_cost(node_id: impl Into<String>, usd: f64) -> Self {
        Self {
            node_costs: vec![(node_id.into(), usd)],
            extra_outputs: Vec::new(),
        }
    }
}

/// Wire format the per-sibling subprocess writes to
/// `.fleet/sessions/<id>/fanout/<node-id>.outcome` so the driver
/// can collect results from sibling tmux windows. Untagged-style
/// enum (the file's either a successful outcome JSON or a failure
/// record) keeps the format human-readable for debugging.
#[allow(clippy::redundant_pub_crate)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum FanoutOutcome {
    Ok(NodeOutcome),
    Failed { message: String },
}

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
    /// Tracker handle shared with the per-agent-node bridge listener.
    /// `None` disables the bridge entirely — agent containers run
    /// without `FLEET_BRIDGE_URL` / `FLEET_BRIDGE_TOKEN` env, which
    /// in turn makes `fleet-tracker` calls inside the container fail
    /// fast. Tests default to `None`; the production CLI builds a
    /// tracker from `.fleet/config.yaml` and threads it in via
    /// [`Self::with_tracker`].
    tracker: Option<Arc<dyn Tracker>>,
    /// Code-host handle for PR-aware nodes (`pr-list`, `pr-checks`,
    /// `create-pr`, `pr-comment`). `None` disables those node kinds
    /// — they fail with a clear "no code host configured" error at
    /// `run_node` dispatch. Built from `.fleet/config.yaml`'s
    /// `code_host:` block (or auto-detected from the git remote) in
    /// `cli::workflow::build_code_host_arc`; threaded in via
    /// [`Self::with_code_host`].
    code_host: Option<Arc<dyn crate::code_host::CodeHost>>,
    /// Labels unioned into the `labels` field of every
    /// `tracker-create` node's request, so a workflow-spawned ticket
    /// carries the labels needed to pass the supervisor's
    /// `autonomous.filter_labels` gate on the next tick. Empty (the
    /// default) is the pre-feature behaviour — the agent-supplied
    /// labels pass through unchanged. Threaded in by `cli::workflow`
    /// from `RepoConfig::autonomous.filter_labels`.
    creation_label_stamp: Vec<String>,
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
    /// Directory bind-mounted as the container's `/workspace`. For
    /// git-isolated runs this is the per-session worktree path; for
    /// the non-git fallback it's the repo root. The executor itself
    /// is agnostic about which — it just bind-mounts what it's given.
    pub workspace: &'a Path,
    pub session_id: SessionId,
    /// Issue the workflow is acting on, if any. Surfaces to nodes via
    /// `FLEET_ISSUE_ID` / `FLEET_ISSUE_HUMAN_ID` / `FLEET_ISSUE_TITLE`
    /// in both agent and bash node environments.
    pub issue: Option<IssueContext>,
    /// PR the workflow is acting on, if any. Surfaces to nodes via
    /// the `FLEET_PR_*` env block. Independent of `issue:` — a session
    /// can bind to both, to either, or to neither.
    pub pr: Option<PrContext>,
    /// Per-session worktree metadata (path + branch) to stamp onto
    /// the session's `meta.json` for later replay/inspection. `None`
    /// for non-git workspaces or when the caller chose not to isolate.
    /// The CLI provisions the worktree on disk; this only carries the
    /// resulting bookkeeping into persistence.
    pub worktree: Option<WorktreeMeta<'a>>,
    /// Egress policy enforcer. The executor calls `setup` once per
    /// `execute` / `resume` / `replay` invocation, threads the
    /// returned `proxy_env` and `network_name` into every agent
    /// node's container spec, and calls `teardown` on terminal
    /// transitions. Callers that don't need enforcement pass a
    /// [`crate::egress::NoopEnforcer`] reference.
    pub egress: &'a dyn crate::egress::EgressEnforcer,
    /// Cost budgets. The executor checks these *before each agent
    /// node starts*: if either limit is already met by the
    /// session's accumulated cost or the lifetime sum across the
    /// store, the node is refused and the session transitions to
    /// `Failed`. `CostConfig::default()` (`None`/`None`) opts every
    /// session out — historical behaviour.
    pub cost: &'a crate::repo_config::CostConfig,
    /// User-interrupt flag. The CLI's SIGINT handler flips this when
    /// the user hits Ctrl+C; the executor checks it between nodes
    /// and aborts cleanly (marking the session `Failed` with a
    /// "user interrupted" reason) rather than dying mid-loop and
    /// leaving meta stuck in `Running`. `None` for tests and any
    /// callers that don't want interrupt handling — the executor
    /// behaves exactly as before in that case.
    #[allow(clippy::struct_field_names)]
    pub interrupt_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-name secret backend declarations resolved from the repo
    /// config's `secrets:` block. Threaded through so
    /// [`run_agent_node`] can resolve `env_passthrough` entries that
    /// have a backend configured (keychain / env / op). Empty when
    /// no `secrets:` block is set, in which case the resolver is a
    /// no-op and the host-env / host-file fallbacks in
    /// [`build_agent_env`] apply.
    pub secrets: &'a std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig>,
    /// Skip every node in topo order up to and including the named
    /// id. Used by the scheduler dispatcher's `run_for_pr` path: the
    /// scheduler consumes the workflow's `pr-list bind: per` root
    /// before minting per-PR sessions, so the executor must start
    /// from the *next* node when those sessions actually run.
    /// `None` (the default for hand-fired runs) preserves the
    /// pre-feature behaviour byte-for-byte.
    pub start_after_node: Option<&'a str>,
    /// Defense-in-depth `git push` policy for this run. Resolved by
    /// the caller from `runtime.git` + `policy::protected_branches`
    /// and threaded down so the shim mount + env are gated on the
    /// same already-evaluated set the worktree assertion used.
    pub git_guard: GitGuardPolicy<'a>,
}

/// Pre-resolved git-guard policy threaded into [`ExecuteRequest`].
/// `enabled` mirrors `runtime.git.enabled`; `protected` and
/// `allow_push_to` are the already-evaluated lists (see
/// `crate::policy::protected_branches`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GitGuardPolicy<'a> {
    pub enabled: bool,
    pub protected: &'a [String],
    pub allow_push_to: &'a [String],
}

/// Per-session worktree bookkeeping, threaded into [`ExecuteRequest`]
/// so the executor stamps it onto the new session's `meta.json`. Used
/// by `replay` to read the prior session's branch back as the git
/// ref for the new session's worktree.
#[derive(Debug, Clone, Copy)]
pub struct WorktreeMeta<'a> {
    /// Absolute path to the worktree directory the CLI provisioned.
    pub path: &'a Path,
    /// Branch checked out in the worktree
    /// (`fleet/session-<short-id>`).
    pub branch: &'a str,
}

/// How far the inner loop should advance through a workflow.
///
/// - [`LoopBound::Full`]: walk every node from the start index to
///   the end, honouring `loop_back_to`. This is the historical
///   behaviour used by `execute`, `resume`, and `replay`.
/// - [`LoopBound::SingleNode`]: run exactly one node body and then
///   exit (regardless of any `loop_back_to` on that node). Used by
///   `replay_only` so the user can iterate on a single node's
///   prompt without firing anything downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopBound {
    Full,
    SingleNode,
}

impl WorkflowExecutor {
    pub fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self {
            invoker,
            clock: Box::new(now_ms),
            tracker: None,
            code_host: None,
            creation_label_stamp: Vec::new(),
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

    /// Enable the per-agent-node bridge by passing a tracker handle.
    /// `None` (the default) keeps the bridge off; `Some` causes
    /// `run_agent_node` to start a bridge before each agent run and
    /// tear it down after, exposing `FLEET_BRIDGE_URL` and
    /// `FLEET_BRIDGE_TOKEN` to the agent's container.
    #[must_use]
    pub fn with_tracker(mut self, tracker: Option<Arc<dyn Tracker>>) -> Self {
        self.tracker = tracker;
        self
    }

    /// Set the code-host backend used by `pr-list` / `pr-checks` /
    /// `create-pr` / `pr-comment` nodes. `None` (the default) makes
    /// those node kinds fail loudly at run time — they need a
    /// configured code host to do anything useful.
    #[must_use]
    pub fn with_code_host(
        mut self,
        code_host: Option<Arc<dyn crate::code_host::CodeHost>>,
    ) -> Self {
        self.code_host = code_host;
        self
    }

    /// Labels merged into the `labels` field of every
    /// `tracker-create` node's request, so a workflow-spawned ticket
    /// passes the supervisor's `autonomous.filter_labels` gate on the
    /// next tick. Empty (the default) is the pre-feature behaviour —
    /// agent-supplied labels pass through unchanged. Production
    /// callers pass `config.autonomous.filter_labels.clone()`; tests
    /// pass an empty Vec unless exercising the stamp path.
    #[must_use]
    pub fn with_creation_label_stamp(mut self, stamp: Vec<String>) -> Self {
        self.creation_label_stamp = stamp;
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
        let mut session = Session::new(req.session_id.clone(), &req.workflow.name, (self.clock)());
        // Persist the caller's issue context on the session so a
        // subsequent `fleet workflow resume` after a gate doesn't
        // lose `FLEET_ISSUE_*` env on downstream nodes.
        session.issue.clone_from(&req.issue);
        // Same story for `pr`: stamp so resume after a gate keeps
        // `FLEET_PR_*` env intact.
        session.pr.clone_from(&req.pr);
        // Stamp the per-session worktree the CLI provisioned (if any)
        // before persistence — without this, a crash between create()
        // and the next save() would lose the pointer the prune command
        // needs to find the worktree later.
        if let Some(wt) = req.worktree {
            session.set_worktree(wt.path.to_path_buf(), wt.branch, (self.clock)());
        }
        req.store.create(&session)?;
        session.transition_to(SessionState::Running, (self.clock)())?;
        session.set_driver_pid(std::process::id(), (self.clock)());
        req.store.save(&session)?;

        let egress_setup = req.egress.setup(req.session_id.as_str())?;
        // `start_after_node` shifts the starting index past the named
        // node so the dispatcher's `run_for_pr` path doesn't re-run
        // the `pr-list` root the scheduler already consumed. None →
        // start at 0 (the historical behaviour).
        let start_idx = match req.start_after_node {
            Some(name) => order
                .iter()
                .position(|id| id == name)
                .map(|i| i + 1)
                .ok_or_else(|| {
                    anyhow!(
                        "--start-after-node `{name}` is not a node in workflow `{}`",
                        req.workflow.name,
                    )
                })?,
            None => 0,
        };
        let run_outcome = (|| -> Result<()> {
            self.run_loop(
                req,
                &mut session,
                &order,
                start_idx,
                &egress_setup,
                LoopBound::Full,
            )?;
            self.finalize(req, &mut session)
        })();
        if let Err(err) = req.egress.teardown(&egress_setup) {
            tracing::warn!(error = %err, "egress teardown after execute failed");
        }
        run_outcome?;
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
        // Issue context survives resume: if the caller supplied a new
        // one, it overrides (explicit caller intent wins); otherwise
        // we keep whatever the original execute() persisted.
        // `loop_counts` is similarly preserved from the loaded
        // session — resolve_loop_back mutates it in place so the
        // counter survives the gate boundary intact.
        if req.issue.is_some() {
            session.issue.clone_from(&req.issue);
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
        session.set_driver_pid(std::process::id(), (self.clock)());
        req.store.save(&session)?;
        let egress_setup = req.egress.setup(req.session_id.as_str())?;
        let run_outcome = (|| -> Result<()> {
            self.run_loop(
                req,
                &mut session,
                &order,
                start,
                &egress_setup,
                LoopBound::Full,
            )?;
            self.finalize(req, &mut session)
        })();
        if let Err(err) = req.egress.teardown(&egress_setup) {
            tracing::warn!(error = %err, "egress teardown after resume failed");
        }
        run_outcome?;
        Ok(session)
    }

    /// Replay a workflow against a prior session's artifacts. Mints a
    /// fresh session, copies `<src>/artifacts/` into the new session's
    /// `artifacts/`, and runs the workflow starting from `rerun_from`.
    ///
    /// Use case: iterate on a reviewer prompt without re-paying for the
    /// planner + implementer agents that produced its inputs.
    ///
    /// Contract: `req.workflow` must be the workflow `src_session_id`
    /// originally ran — the caller (CLI) reads `src.workflow` from
    /// disk and re-parses the current YAML. `req.session_id` is the
    /// *new* session id, minted by the caller. `rerun_from` must
    /// appear in `req.workflow`'s topological order. Fanout siblings
    /// are excluded from that order; start from the owning fanout node
    /// instead.
    ///
    /// Carries forward from the src session:
    /// - `outputs:` from upstream nodes — so a `when:`-gated
    ///   `rerun_from` (or any node downstream of it) sees the same
    ///   accumulator the original run had at that point. Without this,
    ///   any predicate gated on an upstream declaration would skip.
    /// - `issue` context (overridable by `req.issue`).
    ///
    /// Does *not* carry forward:
    /// - `loop_counts`: replay resets cycle progress for the new
    ///   session. If the user wants to re-exercise a cycle from
    ///   scratch, that's the right default; if they want the existing
    ///   progress preserved, they should use `resume` instead.
    /// - `node_costs`: the src session's cost figures are kept in the
    ///   src's `meta.json`; the new session reports only what it
    ///   actually paid for.
    pub fn replay(
        &self,
        req: &ExecuteRequest<'_>,
        src_session_id: &SessionId,
        rerun_from: &str,
    ) -> Result<Session> {
        self.replay_with_bound(req, src_session_id, rerun_from, LoopBound::Full)
    }

    /// Single-node variant of `replay`: stage the src session's
    /// artifacts and worktree state exactly as [`Self::replay`]
    /// does, then run *only* `node_id` and exit. Used for tight
    /// iteration on one node's prompt (typically the reviewer)
    /// without firing anything downstream — no `loop_back_to`
    /// cycles fire, no successor nodes run, the run completes
    /// after the chosen node's body.
    ///
    /// `when:` still gates: a false predicate skips the node,
    /// writes a skipped-log, and the run completes with no node
    /// fired — consistent with the rest of the engine. The user
    /// adjusts the predicate (or its inputs) and re-runs.
    ///
    /// Gate nodes still pause the run via `AwaitingGate`. Fanout
    /// nodes still dispatch their siblings normally — single-node
    /// is a *workflow-level* bound, not a fanout-level one.
    pub fn replay_only(
        &self,
        req: &ExecuteRequest<'_>,
        src_session_id: &SessionId,
        node_id: &str,
    ) -> Result<Session> {
        self.replay_with_bound(req, src_session_id, node_id, LoopBound::SingleNode)
    }

    fn replay_with_bound(
        &self,
        req: &ExecuteRequest<'_>,
        src_session_id: &SessionId,
        rerun_from: &str,
        bound: LoopBound,
    ) -> Result<Session> {
        validate(req.workflow).context("workflow failed static validation")?;
        let order = topological_order(req.workflow)?;
        let start = order
            .iter()
            .position(|id| id == rerun_from)
            .ok_or_else(|| {
                anyhow!(
                    "node `{rerun_from}` is not in workflow `{}`'s topological order \
                 (fanout siblings are excluded — start from the owning fanout node)",
                    req.workflow.name
                )
            })?;

        // Load the src session — fail fast on a bogus id before we
        // mint a new directory we'd then have to clean up. We also
        // hand the src's `outputs` map forward so the new run starts
        // with the same upstream context the original had at this
        // point in the schedule.
        let src = req
            .store
            .load(src_session_id)
            .with_context(|| format!("loading source session `{src_session_id}` for replay"))?;

        let mut session = Session::new(req.session_id.clone(), &req.workflow.name, (self.clock)());
        session.issue.clone_from(&req.issue);
        session.outputs.clone_from(&src.outputs);
        // Mirror execute()'s pre-create stamp so the prune command +
        // a subsequent replay-of-this-replay can find the worktree
        // even if we crash before the next save().
        if let Some(wt) = req.worktree {
            session.set_worktree(wt.path.to_path_buf(), wt.branch, (self.clock)());
        }
        req.store.create(&session)?;

        // Stage the src run's artifacts into the new session before
        // we start running. Missing src `artifacts/` is not an error
        // (workflows with no agent nodes legitimately have nothing to
        // carry); `store.create` already materialised the empty dst.
        let src_artifacts = req.store.session_dir(src_session_id).join("artifacts");
        let dst_artifacts = req.store.session_dir(&session.id).join("artifacts");
        if src_artifacts.is_dir() {
            copy_dir_contents(&src_artifacts, &dst_artifacts).with_context(|| {
                format!(
                    "copying artifacts from session `{src_session_id}` to `{}`",
                    session.id
                )
            })?;
        }

        session.transition_to(SessionState::Running, (self.clock)())?;
        session.set_driver_pid(std::process::id(), (self.clock)());
        req.store.save(&session)?;

        let egress_setup = req.egress.setup(req.session_id.as_str())?;
        let run_outcome = (|| -> Result<()> {
            self.run_loop(req, &mut session, &order, start, &egress_setup, bound)?;
            self.finalize(req, &mut session)
        })();
        if let Err(err) = req.egress.teardown(&egress_setup) {
            tracing::warn!(error = %err, "egress teardown after replay failed");
        }
        run_outcome?;
        Ok(session)
    }

    /// The shared inner loop driving `execute`, `resume`, and the two
    /// replay variants. Walks `order` from `start_idx`, runs each
    /// node, and honours `loop_back_to` and gate-pause transitions.
    /// `bound` controls whether the loop continues to the end of
    /// the workflow or stops after a single successful node body
    /// (the `replay_only` mode). Returns Ok with the session left
    /// in `Running` (caller transitions to `Completed`) or in
    /// `AwaitingGate` (caller leaves it alone). Errors propagate
    /// after marking the session `Failed`.
    #[allow(clippy::too_many_lines)]
    fn run_loop(
        &self,
        req: &ExecuteRequest<'_>,
        session: &mut Session,
        order: &[String],
        start_idx: usize,
        egress: &crate::egress::EgressSetup,
        bound: LoopBound,
    ) -> Result<()> {
        // `loop_counts` and `outputs` both live on the session so a
        // resume after a gate (or a replay carrying upstream context)
        // doesn't reset cycle progress *or* lose the `when:`
        // accumulator. Hydrate the in-memory map from the persisted
        // shape; every successful extraction below mirrors back into
        // session.outputs so a subsequent gate boundary preserves it.
        let mut outputs: OutputMap = outputs_from_persisted(&session.outputs);
        let mut i = start_idx;
        while i < order.len() {
            // Check for a user interrupt *before* starting each node.
            // The check window is the brief between-nodes interval
            // (a few ms) when the fleet driver is foreground in the
            // tmux pane; during an agent step the agent absorbs the
            // signal directly via its PTY. Without this, an accidental
            // Ctrl+C between nodes would kill the driver process and
            // leave the session stuck in `Running` until the reaper
            // sweeps it.
            if check_interrupt(req.interrupt_flag.as_ref()) {
                let err = anyhow!("user interrupted (Ctrl+C between nodes)");
                self.mark_failed(req, session, &err);
                return Err(err);
            }
            let node_id = order[i].clone();
            let node = req
                .workflow
                .node(&node_id)
                .expect("topological_order only returns ids from the workflow's nodes");

            // `when:` is evaluated *before* anything else — a when-false
            // node leaves no state change, no log, no gate, no
            // current_node bump. This is how the planner →
            // implement → review → (revise | open_pr) fork in
            // standard.yaml is steered: only one of the sibling
            // branches actually runs each pass.
            if let Some(expr_str) = node.when.as_deref() {
                match expr::evaluate(expr_str, &outputs) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.write_skipped_log(req, session, &node_id, expr_str);
                        i += 1;
                        continue;
                    }
                    Err(err) => {
                        let wrapped =
                            err.context(format!("evaluating `when:` for node `{node_id}`"));
                        self.mark_failed(req, session, &wrapped);
                        return Err(wrapped);
                    }
                }
            }

            session.set_current_node(Some(node_id.clone()), (self.clock)());
            req.store.save(session)?;

            // Print a one-line breadcrumb to fleet's stdout so the
            // tmux pane shows workflow structure as it runs. Silent
            // when not in tmux (headless / CI keeps its clean
            // single-line stdout contract). The node kind helps the
            // user understand *what* is happening — agent steps will
            // then take over the pane via attach_pty; bash steps
            // stream their own output via the inherit-stdio branch
            // in run_bash_node.
            if std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some() {
                println!(
                    "\nfleet: ▸ node `{node_id}` ({}) starting",
                    node_kind_word(&node.kind)
                );
            }

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

            // Run the node. Fanout dispatches its siblings in
            // parallel; everything else falls through to the regular
            // single-node path.
            let run_result = if let NodeKind::Fanout { siblings } = &node.kind {
                self.run_fanout_node(req, node, siblings, session, &outputs, egress)
            } else {
                self.run_node(req, node, session, &outputs, egress)
            };
            let outcome = match run_result {
                Ok(o) => o,
                Err(err) => {
                    if std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some() {
                        println!("fleet: ✗ node `{node_id}` failed: {err}");
                    }
                    self.mark_failed(req, session, &err);
                    return Err(err);
                }
            };
            if std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some() {
                println!("fleet: ✓ node `{node_id}` done");
            }

            // Fold any cost figures the node produced into the
            // session and flush. The extra save is one disk write per
            // agent node — negligible — and means a crash between this
            // node and the next still leaves the just-fired node's
            // cost visible in the TUI / `fleet sessions show`.
            if !outcome.node_costs.is_empty() {
                for (nid, usd) in outcome.node_costs {
                    session.record_node_cost(nid, usd, (self.clock)());
                }
                req.store.save(session)?;
            }

            // Fold engine-emitted outputs (e.g. `tracker-create`'s
            // `created_id`) into the OutputMap before downstream
            // `extract_outputs` runs — that way `when:` predicates on
            // the next node can branch on them. The downstream save
            // below picks them up via `outputs_to_persisted`.
            let extra_outputs_produced = !outcome.extra_outputs.is_empty();
            for (key, value) in outcome.extra_outputs {
                outputs.insert(key, value);
            }

            // After a successful run, extract any declared `outputs:`.
            // For a fanout, the outputs live on each sibling (the
            // fanout itself is just a dispatcher), so we extract per
            // sibling. For everything else it's the node itself.
            let artifacts_dir = req.store.session_dir(&session.id).join("artifacts");
            let extract_targets: Vec<&Node> = match &node.kind {
                NodeKind::Fanout { siblings } => siblings
                    .iter()
                    .filter_map(|sid| req.workflow.node(sid))
                    .collect(),
                _ => vec![node],
            };
            let mut produced_outputs = false;
            for target in extract_targets {
                if !target.outputs.is_empty() {
                    produced_outputs = true;
                }
                if let Err(err) = extract_outputs(&artifacts_dir, target, &mut outputs) {
                    self.mark_failed(req, session, &err);
                    return Err(err);
                }
                // Outcome-convention check: opt-in per node (a node
                // that declares `outputs.outcome` joins the protocol).
                // Runs after extraction so the validator sees what the
                // executor actually committed to the OutputMap, not
                // the raw artifact bytes.
                if let Err(err) = validate_outcome(target, &outputs) {
                    self.mark_failed(req, session, &err);
                    return Err(err);
                }
            }
            // Mirror the in-memory accumulator back onto the session
            // when *any* target declared outputs OR an engine-driven
            // node emitted extras (the accumulator may have new
            // entries either way) so a future gate boundary or replay
            // sees them. Skipping the save when nothing was declared
            // keeps the no-outputs workflow's disk traffic at the
            // pre-persistence level.
            if produced_outputs || extra_outputs_produced {
                session.outputs = outputs_to_persisted(&outputs);
                req.store.save(session)?;
            }

            // SingleNode bound exits after the first node body, even
            // when the node carries `loop_back_to:`. The whole point
            // of replay-only is "fire one node and stop"; a cycle
            // would defeat that. Cycle semantics remain available
            // via `replay --rerun-from`.
            if matches!(bound, LoopBound::SingleNode) {
                return Ok(());
            }

            i = match resolve_loop_back(node, order, &mut session.loop_counts) {
                Some(target) => {
                    // Persist the bumped counter so a future resume
                    // after a gate continues from the same slot
                    // rather than restarting the cycle.
                    req.store.save(session)?;
                    target
                }
                None => i + 1,
            };
        }
        Ok(())
    }

    /// Drop a one-line log file explaining why a node was skipped. The
    /// TUI / `fleet sessions logs` surfaces this so users don't have to
    /// guess why a branch never ran.
    fn write_skipped_log(
        &self,
        req: &ExecuteRequest<'_>,
        session: &Session,
        node_id: &str,
        when_expr: &str,
    ) {
        let _ = self; // method form keeps the surface symmetric with run_*_node.
        let log_path = req
            .store
            .session_dir(&session.id)
            .join("logs")
            .join(format!("{node_id}.log"));
        let body = format!("--- skipped: when `{when_expr}` evaluated false ---\n");
        if let Err(err) = std::fs::write(&log_path, body) {
            tracing::warn!(?err, log = %log_path.display(), "writing skipped log failed");
        }
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
        // The driver is parking the session for a user to resume. No
        // fleet process is "driving" it any more, so clear the pid
        // before persisting — otherwise the reaper would see a Running-
        // adjacent session with a dead pid after this process exits.
        session.clear_driver_pid((self.clock)());
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
            // Driver is exiting cleanly — release the pid stamp so a
            // subsequent reaper sweep doesn't see a stale claim. (The
            // gate path clears separately in `handle_gate`; this branch
            // covers Completed.)
            session.clear_driver_pid((self.clock)());
            req.store.save(session)?;
        }
        Ok(())
    }

    pub(crate) fn run_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        session: &Session,
        outputs: &OutputMap,
        egress: &crate::egress::EgressSetup,
    ) -> Result<NodeOutcome> {
        // Input artifact contract: declared `artifacts.in:` must exist
        // before the node runs. Detected here so a downstream node that
        // depends on an upstream-produced file fails loudly with a
        // pointer at the missing path rather than silently running on
        // empty inputs.
        let artifacts_dir = req.store.session_dir(&session.id).join("artifacts");
        verify_inputs(&artifacts_dir, node)?;

        let outcome = match &node.kind {
            NodeKind::Agent { agent, .. } => {
                self.run_agent_node(req, node, agent, session, egress)?
            }
            NodeKind::Bash { script } => {
                self.run_bash_node(req, node, script, session)?;
                NodeOutcome::empty()
            }
            NodeKind::Assert { expr } => {
                self.run_assert_node(req, node, expr, session, outputs)?;
                NodeOutcome::empty()
            }
            // tracker-create is host-side like Bash/Assert — it reads
            // an upstream agent's recommendation out of `outputs` and
            // mutates the tracker, emitting `created_id` /
            // `created_human_id` for downstream nodes. The handler
            // returns the emitted entries; the run loop folds them
            // into `outputs` after the dispatch returns so downstream
            // `when:` predicates and the persisted session state both
            // see them.
            NodeKind::TrackerCreate { from, link_parent } => {
                let emitted =
                    self.run_tracker_create_node(req, node, from, *link_parent, session, outputs)?;
                NodeOutcome {
                    node_costs: Vec::new(),
                    extra_outputs: emitted,
                }
            }
            NodeKind::PrList { filter, bind } => {
                // `bind: per` is consumed by the scheduler before any
                // executor node runs; reaching here means stage 7's
                // dispatcher didn't skip past the root. Treat as an
                // internal error rather than running it for real.
                if matches!(bind, Some(crate::workflow::spec::PrBindMode::Per)) {
                    bail!(
                        "internal: pr-list node `{}` with `bind: per` reached run_node — \
                         the scheduler dispatcher should have set --start-after-node to \
                         skip past it",
                        node.id,
                    );
                }
                let emitted = self.run_pr_list_node(req, node, filter, session)?;
                NodeOutcome {
                    node_costs: Vec::new(),
                    extra_outputs: emitted,
                }
            }
            NodeKind::PrChecks { pr } => {
                let emitted = self.run_pr_checks_node(req, node, pr.as_deref(), session, outputs)?;
                NodeOutcome {
                    node_costs: Vec::new(),
                    extra_outputs: emitted,
                }
            }
            NodeKind::CreatePr {
                title,
                body,
                base,
                head,
                draft,
            } => {
                let emitted = self.run_create_pr_node(
                    req,
                    node,
                    title,
                    body,
                    base.as_deref(),
                    head.as_deref(),
                    *draft,
                    session,
                )?;
                NodeOutcome {
                    node_costs: Vec::new(),
                    extra_outputs: emitted,
                }
            }
            NodeKind::PrComment { body, pr } => {
                self.run_pr_comment_node(req, node, body, pr.as_deref(), session, outputs)?;
                NodeOutcome::empty()
            }
            // Unsupported kinds were rejected earlier in `execute`; this
            // branch is just an exhaustiveness guard.
            other => bail!(
                "internal: node `{}` of kind {other:?} reached run_node — should have been rejected upstream",
                node.id
            ),
        };

        // Output contract: declared `artifacts.out:` must be produced.
        // We check after the inner run so a failing node surfaces its
        // own error first; output-contract violations are reported as
        // node failures with their own clear wording.
        verify_outputs(&artifacts_dir, node)?;
        Ok(outcome)
    }

    /// Start a per-agent-node bridge when the executor was built with a
    /// tracker. The returned handle's Drop tears the listener down; the
    /// caller binds it to a local so it stays alive for the run.
    /// Returns `Ok(None)` when no tracker is configured — the agent
    /// runs without `FLEET_BRIDGE_*` and `fleet-tracker` calls inside
    /// the container fail fast.
    fn start_bridge_for_agent(
        &self,
        session: &Session,
        workspace: &Path,
        node_id: &str,
    ) -> Result<Option<Bridge>> {
        self.tracker
            .as_ref()
            .map(|tracker| {
                Bridge::start(
                    session,
                    Arc::clone(tracker),
                    self.code_host.clone(),
                    workspace.to_path_buf(),
                )
                .with_context(|| format!("starting bridge for agent node `{node_id}`"))
            })
            .transpose()
    }

    // `run_agent_node` accumulates state from many sources (cost
    // budget, agent registry, prompt artifact, egress env, bridge
    // env, fleet-tracker mount, container start/exec/stop, log
    // capture, cost parse). Further extraction starts to read worse
    // than the sequential narrative; allow the line count.
    #[allow(clippy::too_many_lines)]
    fn run_agent_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        agent_name: &str,
        session: &Session,
        egress: &crate::egress::EgressSetup,
    ) -> Result<NodeOutcome> {
        // Budget guardrail: refuse to start this agent if the session
        // or lifetime cost has already hit a configured budget. We
        // compute lifetime *here* (not once per session) because
        // earlier nodes in this same session may have just rolled
        // into the totals. Failure here propagates → mark_failed.
        check_cost_budget(req.cost, session, req.store, &node.id)?;
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
        let prompt = resolve_prompt_artifact(req.workspace, prompt_file)
            .with_context(|| format!("preparing prompt for agent node `{}`", node.id))?;
        let agent_ctx = AgentContext {
            persona,
            prompt: prompt.as_ref(),
            // Read from the persisted session so resume runs see the
            // original `--issue`; `execute()` copies `req.issue` here
            // at session creation.
            issue: session.issue.as_ref(),
            // Same story for `pr`: session is the source of truth so
            // resume after a gate keeps `FLEET_PR_*` env visible.
            pr: session.pr.as_ref(),
        };
        let mut env = build_agent_env(agent, &agent_ctx);
        // Overlay any `secrets:`-configured values on top of the
        // base env. Resolves keychain / op / env-var backends per
        // entry; missing host-env names that have *no* backend
        // config fall through to whatever `build_agent_env`
        // already filled in (host env var, claude-OAuth host-file
        // fallback).
        resolve_secrets_in_env(&mut env, req.secrets, &self.invoker)
            .with_context(|| format!("resolving secrets for node `{}`", node.id))?;
        // Egress enforcement: append the proxy env so every HTTP/HTTPS
        // client the agent uses respects HTTP_PROXY / HTTPS_PROXY.
        // Order matters — putting these after the agent's own env
        // means a workflow-specific override (e.g. `env_passthrough:
        // [HTTP_PROXY]` from the user's shell) is honoured first, and
        // fleet's proxy fills in any remaining slot.
        for (k, v) in &egress.proxy_env {
            env.push((k.clone(), v.clone()));
        }

        // Per-agent-node bridge: when the executor was built with a
        // tracker, spin up a fresh bridge for this run. The handle is
        // bound to a local variable so Drop tears the listener down
        // when this function returns — whether through ?-propagation
        // or the happy path.
        let bridge = self.start_bridge_for_agent(session, req.workspace, &node.id)?;
        push_bridge_env(&mut env, bridge.as_ref());
        push_git_guard_env(&mut env, req.git_guard, session.id.as_str());

        let image = req
            .adapter
            .ensure_image(req.devcontainer)
            .with_context(|| format!("building image for node `{}`", node.id))?;
        let artifacts_dir = req.store.session_dir(&session.id).join("artifacts");
        // Bind-mount fleet-tracker into the agent container when the
        // bridge is active, a host binary is locatable, and the image
        // arch matches the host. The mount is read-only because the
        // agent shouldn't be able to rewrite its own write-authority
        // binary mid-run. `fleet_tracker_mount` handles the gating
        // (see its doc-comment for the exact skip conditions).
        let mut extra_mounts = if bridge.is_some() {
            let mut mounts = Vec::new();
            if let Some(m) = fleet_tracker_mount(req.adapter, &image) {
                mounts.push(m);
            }
            // Mount fleet-pr alongside fleet-tracker. Same gating: arch
            // match + host binary co-located with `fleet`. When the
            // sibling binary isn't present, the agent simply doesn't get
            // the PR-read CLI; that's a degraded mode rather than a hard
            // failure, since not every workflow needs PR access.
            if let Some(m) = fleet_pr_mount(req.adapter, &image) {
                mounts.push(m);
            }
            mounts
        } else {
            Vec::new()
        };
        // Defense-in-depth `git push` guard: bind-mount the fleet-git
        // shim at /usr/local/bin/git so any `git push` the agent
        // issues is screened against the protected-branch policy.
        // Independent of the bridge — guard applies even when no
        // tracker is configured. Same arch / co-location gating as
        // the tracker mounts; missing binary collapses to no mount
        // (which falls back to the implicit no-creds boundary).
        if req.git_guard.enabled {
            if let Some(m) = fleet_git_mount(req.adapter, &image) {
                extra_mounts.push(m);
            }
        }
        // When the workspace is a `git worktree`-created tree, its
        // `.git` is a gitlink file referencing host paths that the
        // workspace mount alone doesn't expose. Bind-mount the
        // worktree's gitdir + the main repo's `.git/` at their
        // host-absolute paths so git inside the container can follow
        // the gitlink and reach shared objects/refs. No-op for
        // regular (`.git` is a dir) or non-git workspaces.
        extra_mounts.extend(worktree_git_mounts(req.workspace));
        // When running as a fanout sibling subprocess, the per-
        // sibling tmux dispatcher sets `FLEET_FANOUT_SIBLING` so we
        // can disambiguate the devcontainer CLI container identity
        // via `--id-label`. Without this, sibling A's `devcontainer
        // up --remove-existing-container` would evict sibling B's
        // container (they share the workspace-derived default
        // identity). Non-fanout calls leave it `None` so behaviour
        // for ordinary `fleet workflow run` is unchanged.
        let id_label = std::env::var("FLEET_FANOUT_SIBLING")
            .ok()
            .map(|sib| format!("fleet-sibling={sib}"));
        let spec = ContainerSpec {
            image,
            workspace: req.workspace.to_path_buf(),
            artifacts: artifacts_dir,
            env: env.clone(),
            command: None,
            network: egress.network_name.clone(),
            dns: egress.dns_ip.clone(),
            extra_mounts,
            id_label,
        };
        let container_id = req
            .adapter
            .start_container(&spec)
            .with_context(|| format!("starting container for node `{}`", node.id))?;

        // Record the container as active *immediately* after start, so a
        // fleet crash between here and the matching stop leaves a
        // marker on disk that the reaper can pick up. A failure to
        // write the marker downgrades to a warning — the agent run
        // should still proceed; we just lose the leak-detection signal
        // for this one container if it crashes.
        let session_dir = req.store.session_dir(&session.id);
        if let Err(err) =
            crate::session::containers::mark_active(&session_dir, container_id.as_str())
        {
            tracing::warn!(?err, container = %container_id, "writing container marker failed");
        }

        // Branch on whether we're running inside a per-session tmux
        // pane (set by `fleet workflow run --detached`'s wrapper).
        //
        // Inside tmux: PTY-attach so the agent's interactive UI
        // (Claude Code, aider, codex) renders live in the pane —
        // exactly what the user sees when they `tmux attach`.
        // Stdout/stderr aren't captured as Strings here, so we skip
        // the agent-cost parse (the transcript.log piped by tmux
        // preserves the cost line for post-hoc forensics; the
        // sidebar's cost summary is informational only).
        //
        // Outside tmux: stick with `exec` for the captured stdio +
        // cost parsing the headless / CI flow has relied on.
        let log_path = self.node_log_path(req, session, &node.id);
        let in_tmux = std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some();
        // Per-exec env: the same env we built for ContainerSpec.env
        // *also* has to flow through the adapter's exec / attach_pty
        // call — `devcontainer up --remote-env` (the path
        // start_container uses) only sets env for lifecycle commands
        // (postCreateCommand etc), not for arbitrary later exec. So
        // without re-passing it here, the agent's env arrives empty.
        let run_outcome: Result<NodeOutcome> = if in_tmux {
            let attach_opts = ExecOpts {
                workdir: None,
                env: env.clone(),
            };
            let attach_result = req
                .adapter
                .attach_pty(&container_id, &agent.command, attach_opts);
            let log_text = match &attach_result {
                Ok(h) => format!(
                    "--- agent {agent_name} ran interactively in tmux ---\n\
                     stdio streamed to the session's tmux pane;\n\
                     see transcript.log alongside this file for the full output.\n\
                     --- exit {} ---\n",
                    h.exit_code
                ),
                Err(err) => format!("--- agent {agent_name} pty-attach failed: {err:#} ---\n"),
            };
            if let Err(err) = std::fs::write(&log_path, log_text) {
                tracing::warn!(?err, log = %log_path.display(), "writing agent log failed");
            }
            match attach_result {
                Ok(handle) if handle.exit_code == 0 => Ok(NodeOutcome::empty()),
                Ok(handle) => Err(anyhow!(
                    "agent `{agent_name}` in node `{}` exited with code {} (see transcript)",
                    node.id,
                    handle.exit_code
                )),
                Err(err) => Err(err),
            }
        } else {
            let exec_opts = ExecOpts { workdir: None, env };
            let exec_result = req.adapter.exec(&container_id, &agent.command, exec_opts);
            let log_text = match &exec_result {
                Ok(h) => format!(
                    "--- agent {agent_name} ---\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- exit {} ---\n",
                    h.stdout, h.stderr, h.exit_code
                ),
                Err(err) => format!("--- agent {agent_name} failed: {err:#} ---\n"),
            };
            if let Err(err) = std::fs::write(&log_path, log_text) {
                tracing::warn!(?err, log = %log_path.display(), "writing agent log failed");
            }
            match exec_result {
                Ok(handle) if handle.exit_code == 0 => {
                    Ok(
                        parse_agent_cost_usd(agent_name, &handle.stdout, &handle.stderr)
                            .map_or_else(
                                || {
                                    tracing::debug!(
                                        agent = agent_name,
                                        node = %node.id,
                                        "agent cost parser found no match in output"
                                    );
                                    NodeOutcome::empty()
                                },
                                |usd| NodeOutcome::with_cost(&node.id, usd),
                            ),
                    )
                }
                Ok(handle) => Err(anyhow!(
                    "agent `{agent_name}` in node `{}` exited with code {} (log at {})",
                    node.id,
                    handle.exit_code,
                    log_path.display()
                )),
                Err(err) => Err(err),
            }
        };

        // Stop is best-effort: an agent that succeeded shouldn't get its
        // success overridden by a stale-container stop error. The
        // surrounding workflow run continues even if stop hits a flake.
        if let Err(err) = req.adapter.stop(&container_id) {
            tracing::warn!(?err, container = %container_id, "stopping container failed");
        }
        // Whether stop succeeded or not, drop the active marker. A
        // dangling marker after a successful stop would re-report a
        // dead container as leaked on the next reap. The marker file
        // is metadata for *fleet's* bookkeeping; the engine's actual
        // container state is the source of truth.
        if let Err(err) =
            crate::session::containers::mark_stopped(&session_dir, container_id.as_str())
        {
            tracing::warn!(?err, container = %container_id, "clearing container marker failed");
        }

        run_outcome
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
        // environment without polluting the outer shell. The issue comes
        // from the persisted session so a `resume` after a gate keeps
        // the original `--issue` context visible to bash scripts too.
        let env_prefix = format!(
            "{}{}",
            bash_issue_env_prefix(session.issue.as_ref()),
            bash_pr_env_prefix(session.pr.as_ref()),
        );
        let cmd = format!(
            "cd {} && {env_prefix}{script}",
            shell_quote_path(req.workspace)
        );
        let log_path = self.node_log_path(req, session, &node.id);

        // Branch on tmux mode (set by `fleet workflow run --detached`).
        // In tmux: inherit stdio so the bash output streams into the
        // pane — the user attaching sees activity instead of a silent
        // pane until the workflow completes. The pane's full output
        // is preserved in `transcript.log` via the wrapper's
        // `pipe-pane`, so we can write a small pointer to the per-node
        // log file rather than losing the content entirely. Outside
        // tmux: capture (the historical headless / CI behaviour).
        if std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some() {
            let status = std::process::Command::new("sh")
                .args(["-c", &cmd])
                .status()
                .with_context(|| format!("running bash node `{}`", node.id))?;
            let exit_code = status.code().unwrap_or(-1);
            let log_text = format!(
                "--- bash node {} ran in tmux pane (exit {exit_code}) ---\n\
                 stdio streamed to the session's tmux pane; the captured\n\
                 transcript lives at `transcript.log` alongside this file.\n",
                node.id
            );
            if let Err(err) = std::fs::write(&log_path, log_text) {
                tracing::warn!(?err, log = %log_path.display(), "writing bash log failed");
            }
            if !status.success() {
                bail!(
                    "bash node `{}` exited with {exit_code} (see transcript)",
                    node.id
                );
            }
            return Ok(());
        }

        let result = self.invoker.run("sh", vec!["-c".to_string(), cmd]);
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

    /// Evaluate the assert node's `expr:` against the live outputs map.
    /// True → no-op (workflow continues). False → node failure that
    /// transitions the session to Failed; the log file records the
    /// expression that failed so the user can see *which* invariant
    /// the workflow protects.
    /// Execute a `tracker-create` node: read the upstream agent's
    /// recommendation from `outputs`, parse it as a `{title, body,
    /// labels?}` JSON object, and call `Tracker::create` to file the
    /// new ticket. When `link_parent` is true and the session has a
    /// bound ticket, also call `Tracker::link_parent` to wire the
    /// structural relationship. Returns the
    /// `((node_id, output_name), value)` pairs the run loop folds
    /// into the `OutputMap` so downstream `when:` and `extract_outputs`
    /// see the new ticket id.
    ///
    /// Cross-link comments and `.fleet/deps.json` writes land in a
    /// follow-up commit; this commit ships only the creation path.
    /// Union [`Self::creation_label_stamp`] into `supplied`,
    /// preserving the caller's order and appending any missing stamp
    /// labels in config order. Mirrors
    /// `AutonomousConfig::stamp_creation_labels` — duplicated here
    /// rather than referenced because the executor doesn't otherwise
    /// hold a `RepoConfig` and threading one through every test seam
    /// would dwarf the six lines of logic.
    fn stamp_request_labels(&self, supplied: &[String]) -> Vec<String> {
        let mut out: Vec<String> = supplied.to_vec();
        for needed in &self.creation_label_stamp {
            if !out.iter().any(|l| l == needed) {
                out.push(needed.clone());
            }
        }
        out
    }

    fn run_tracker_create_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        from: &str,
        link_parent: bool,
        session: &Session,
        outputs: &OutputMap,
    ) -> Result<Vec<((String, String), String)>> {
        let tracker = self.tracker.as_ref().ok_or_else(|| {
            anyhow!(
                "tracker-create node `{}` requires a tracker but the executor was built \
                 without one — pass `WorkflowExecutor::with_tracker(Some(...))`",
                node.id
            )
        })?;

        // Per-session cap check. Counts distinct tracker-create nodes
        // in this workflow whose `created_id` output is already
        // populated in session state. Same-node re-fires (via
        // loop_back_to) don't inflate the count because they
        // overwrite the existing entry. This is the v1 protection
        // against a runaway agent recommending dozens of follow-up
        // tickets in one session.
        let cap = req.workflow.effective_max_recommended_tickets();
        let already_fired = count_tracker_create_firings(req.workflow, session);
        if already_fired >= cap {
            bail!(
                "tracker-create node `{}`: per-session cap reached \
                 (max_recommended_tickets = {cap}; {already_fired} already filed)",
                node.id
            );
        }

        let (src_node, src_name) = parse_output_ref(from).with_context(|| {
            format!("tracker-create node `{}`: parsing `from: {from}`", node.id)
        })?;
        let raw = outputs
            .get(&(src_node.to_string(), src_name.to_string()))
            .ok_or_else(|| {
                anyhow!(
                    "tracker-create node `{}`: no upstream output `{from}` found \
                     (did the producing node fail or did its `when:` skip it?)",
                    node.id
                )
            })?;
        let request: TrackerCreateRequest = serde_json::from_str(raw).with_context(|| {
            format!(
                "tracker-create node `{}`: parsing `{from}` value as \
                 `{{ title, body, labels? }}` JSON failed (the agent must emit \
                 this shape under the source key)",
                node.id
            )
        })?;
        if request.title.trim().is_empty() {
            bail!(
                "tracker-create node `{}`: `{from}.title` is empty — the new \
                 ticket must have a non-empty title",
                node.id
            );
        }

        let log_path = self.node_log_path(req, session, &node.id);
        // Union the executor's configured stamp into the request's
        // labels so a fleet-spawned ticket carries the labels the
        // supervisor's `autonomous.filter_labels` gate needs. Empty
        // stamp = pre-feature behaviour, request labels pass through.
        let stamped_labels = self.stamp_request_labels(&request.labels);
        let created = tracker
            .create(
                req.workspace,
                &request.title,
                &request.body,
                &stamped_labels,
            )
            .with_context(|| {
                format!(
                    "tracker-create node `{}`: `Tracker::create` failed",
                    node.id
                )
            })?;
        if link_parent {
            if let Some(parent) = req.issue.as_ref() {
                self.link_tracker_create_to_parent(
                    req,
                    node,
                    tracker.as_ref(),
                    parent,
                    &created,
                    &request.title,
                )?;
            }
            // No bound issue → quietly skip the link. This is the
            // honest behaviour for a workflow run that didn't bind a
            // ticket (replay, ad-hoc smoke); the new ticket still
            // gets created, just without a parent edge.
        }

        let body = format!(
            "--- tracker-create ---\nfiled {} ({}): {}\n",
            created.human_id, created.id, request.title
        );
        if let Err(err) = std::fs::write(&log_path, body) {
            tracing::warn!(?err, log = %log_path.display(), "writing tracker-create log failed");
        }

        Ok(vec![
            (
                (node.id.clone(), "created_id".to_string()),
                created.id.clone(),
            ),
            (
                (node.id.clone(), "created_human_id".to_string()),
                created.human_id,
            ),
        ])
    }

    /// Wire a freshly-created child ticket back to its parent:
    /// structural `link_parent` call, cross-link comments on both
    /// tickets so a human reader sees the relationship, and a
    /// `blocked → blocked_on` edge in `.fleet/deps.json` for the
    /// supervisor's scheduler. Factored out of
    /// [`Self::run_tracker_create_node`] to keep that function
    /// under clippy's line cap.
    fn link_tracker_create_to_parent(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        tracker: &dyn Tracker,
        parent: &IssueContext,
        created: &crate::tracker::Issue,
        title: &str,
    ) -> Result<()> {
        tracker
            .link_parent(req.workspace, &parent.human_id, &created.human_id)
            .with_context(|| {
                format!(
                    "tracker-create node `{}`: `Tracker::link_parent` failed \
                     (parent={}, child={})",
                    node.id, parent.human_id, created.human_id
                )
            })?;
        // Cross-link comments: visible breadcrumbs on both tickets so
        // a human browsing in GitHub / git-bug sees the relationship
        // without consulting fleet's task-list rendering. The
        // structural `link_parent` above is what the scheduler reads;
        // these comments are for human readers.
        let parent_comment = format!(
            "fleet filed follow-up #{} (\"{title}\") to unblock this ticket",
            created.human_id
        );
        tracker
            .comment(req.workspace, &parent.human_id, &parent_comment)
            .with_context(|| {
                format!(
                    "tracker-create node `{}`: posting cross-link comment on parent #{} failed",
                    node.id, parent.human_id
                )
            })?;
        let child_comment = format!(
            "filed via fleet from #{} as a prerequisite",
            parent.human_id
        );
        tracker
            .comment(req.workspace, &created.human_id, &child_comment)
            .with_context(|| {
                format!(
                    "tracker-create node `{}`: posting cross-link comment on new ticket #{} failed",
                    node.id, created.human_id
                )
            })?;
        // Record the dependency edge so the supervisor's scheduler
        // (Phase 4) can skip the parent until the child closes.
        // Cycle-check first: in v1 the new ticket has no outgoing
        // edges so the check is defensive (it can only fire if a
        // future write path re-uses an existing id under
        // `tracker-create`), but the helper is cheap and the safety
        // net belongs at the mutation point, not on the caller.
        let deps_store = crate::deps::DepsStore::at(deps_path_for(req.store));
        let proposed = crate::deps::DepEdge {
            blocked: parent.human_id.clone(),
            blocked_on: created.human_id.clone(),
            reason: crate::deps::BlockedReason::Ticket,
            created_at_ms: (self.clock)(),
        };
        let existing = deps_store.load().with_context(|| {
            format!(
                "tracker-create node `{}`: loading deps for cycle-check failed",
                node.id
            )
        })?;
        if crate::deps::would_create_cycle(&existing, &proposed) {
            bail!(
                "tracker-create node `{}`: refusing to record dep edge {} -> {} \
                 because it would close a cycle (the new ticket was filed; \
                 resolve the cycle in `.fleet/deps.json` or close one side \
                 before re-running)",
                node.id,
                parent.human_id,
                created.human_id
            );
        }
        deps_store.add_edge(proposed).with_context(|| {
            format!(
                "tracker-create node `{}`: recording deps edge {} -> {} failed",
                node.id, parent.human_id, created.human_id
            )
        })?;
        // Plan injection: when the parent is an item in an active
        // fleet plan, insert the newly-filed child immediately
        // before the parent's position so the supervisor's tick
        // sees the prerequisite first. Plans not containing the
        // parent (or paused/completed/abandoned plans) are left
        // alone — the new ticket is filed regardless.
        let plan_store = crate::plans::store::PlanStore::at(plans_root_for(req.store));
        if let Some((mut plan, idx)) = plan_store
            .find_plan_containing(&parent.human_id)
            .with_context(|| {
                format!(
                    "tracker-create node `{}`: looking up parent plan for #{} failed",
                    node.id, parent.human_id
                )
            })?
        {
            plan.items.insert(
                idx,
                crate::plans::PlanItem::pending_injected(&created.human_id),
            );
            plan.updated_at_ms = (self.clock)();
            plan_store.save(&plan).with_context(|| {
                format!(
                    "tracker-create node `{}`: persisting injected item in plan {} failed",
                    node.id, plan.id
                )
            })?;
        }
        Ok(())
    }

    /// Host-side `pr-list` handler. Emits the PR list as a JSON-
    /// stringified output (`prs`) plus a scalar `count`. `bind: per`
    /// is intercepted at the `run_node` dispatch — by the time we
    /// reach this body, `bind` is `None` and the node simply
    /// produces outputs for downstream nodes inside the current
    /// session.
    fn run_pr_list_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        filter: &crate::workflow::spec::PrFilter,
        session: &Session,
    ) -> Result<Vec<((String, String), String)>> {
        let host = self.require_code_host_for(node)?;
        let prs = host
            .list_prs(req.workspace, filter)
            .with_context(|| format!("pr-list node `{}`: list_prs failed", node.id))?;
        let log_path = self.node_log_path(req, session, &node.id);
        let mut body = format!("--- pr-list ---\n{} PR(s) matched\n", prs.len());
        for p in &prs {
            use std::fmt::Write;
            let _ = writeln!(body, "  #{} {}", p.number, p.title);
        }
        if let Err(err) = std::fs::write(&log_path, body) {
            tracing::warn!(?err, log = %log_path.display(), "writing pr-list log failed");
        }
        let prs_json = serde_json::to_string(&prs).with_context(|| {
            format!("pr-list node `{}`: serialising PR list to JSON", node.id)
        })?;
        Ok(vec![
            ((node.id.clone(), "count".to_string()), prs.len().to_string()),
            ((node.id.clone(), "prs".to_string()), prs_json),
        ])
    }

    /// Host-side `pr-checks` handler. Resolves which PR to query
    /// (explicit literal / dotted-ref / bound PR context), calls
    /// `pr_checks`, emits the scalar pre-aggregates `has_failures`,
    /// `failed_count`, `pending_count` plus the full checks list as
    /// a JSON-stringified `checks` output for downstream parsing.
    fn run_pr_checks_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        pr_ref: Option<&str>,
        session: &Session,
        outputs: &OutputMap,
    ) -> Result<Vec<((String, String), String)>> {
        let host = self.require_code_host_for(node)?;
        let number = resolve_pr_number(node, pr_ref, session, outputs)?;
        let summary = host
            .pr_checks(req.workspace, number)
            .with_context(|| format!("pr-checks node `{}`: pr_checks failed", node.id))?;
        let log_path = self.node_log_path(req, session, &node.id);
        let body = format!(
            "--- pr-checks (#{number}) ---\n\
             has_failures={} failed={} pending={}\n",
            summary.has_failures, summary.failed_count, summary.pending_count,
        );
        if let Err(err) = std::fs::write(&log_path, body) {
            tracing::warn!(?err, log = %log_path.display(), "writing pr-checks log failed");
        }
        let checks_json = serde_json::to_string(&summary.checks).with_context(|| {
            format!("pr-checks node `{}`: serialising checks list to JSON", node.id)
        })?;
        Ok(vec![
            (
                (node.id.clone(), "has_failures".to_string()),
                summary.has_failures.to_string(),
            ),
            (
                (node.id.clone(), "failed_count".to_string()),
                summary.failed_count.to_string(),
            ),
            (
                (node.id.clone(), "pending_count".to_string()),
                summary.pending_count.to_string(),
            ),
            ((node.id.clone(), "checks".to_string()), checks_json),
        ])
    }

    /// Host-side `create-pr` handler. Resolves `base`/`head`
    /// defaults from the worktree's git state when the YAML omits
    /// them, calls `create_pr`, emits `number`, `url`, `head_sha`.
    /// Supersedes the legacy `gh pr create` bash escape hatch in
    /// `standard.yaml`.
    ///
    /// When no `code_host:` is configured (the common "local test
    /// repo, no remote" case), the node logs a "skipped" notice and
    /// returns `Ok(empty)` instead of failing the whole workflow.
    /// The legacy bash `open_pr` shelling `gh pr create` would have
    /// failed in this case too; the difference is that fleet now
    /// degrades gracefully so plan / implement / review work
    /// survives.
    #[allow(clippy::too_many_arguments)]
    fn run_create_pr_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        title: &str,
        body: &str,
        base: Option<&str>,
        head: Option<&str>,
        draft: bool,
        session: &Session,
    ) -> Result<Vec<((String, String), String)>> {
        let Some(host) = self.code_host.clone() else {
            tracing::warn!(
                node = %node.id,
                "create-pr skipped: no code_host configured \
                 (set `code_host: github` in .fleet/config.yaml \
                 or add an `origin` remote that auto-detects)"
            );
            let log_path = self.node_log_path(req, session, &node.id);
            let body_log = "--- create-pr skipped ---\n\
                            No code_host configured — workflow continues without opening a PR.\n\
                            Set `code_host: github` in .fleet/config.yaml (and ensure an\n\
                            authenticated `gh` + an `origin` remote) to enable.\n";
            if let Err(err) = std::fs::write(&log_path, body_log) {
                tracing::warn!(?err, log = %log_path.display(), "writing create-pr skip log failed");
            }
            return Ok(Vec::new());
        };
        let base = base
            .map(str::to_string)
            .or_else(|| crate::policy::detect_default_branch(self.invoker.as_ref(), req.workspace))
            .unwrap_or_else(|| "main".to_string());
        let head = match head {
            Some(s) => s.to_string(),
            None => detect_current_branch(self.invoker.as_ref(), req.workspace).with_context(
                || format!("create-pr node `{}`: resolving HEAD branch failed", node.id),
            )?,
        };
        let summary = host
            .create_pr(req.workspace, title, body, &base, &head, draft)
            .with_context(|| format!("create-pr node `{}`: create_pr failed", node.id))?;
        let log_path = self.node_log_path(req, session, &node.id);
        let body_log = format!(
            "--- create-pr ---\nopened #{} at {} (head_sha={})\n",
            summary.number, summary.url, summary.head_sha,
        );
        if let Err(err) = std::fs::write(&log_path, body_log) {
            tracing::warn!(?err, log = %log_path.display(), "writing create-pr log failed");
        }
        Ok(vec![
            (
                (node.id.clone(), "number".to_string()),
                summary.number.to_string(),
            ),
            ((node.id.clone(), "url".to_string()), summary.url),
            ((node.id.clone(), "head_sha".to_string()), summary.head_sha),
        ])
    }

    /// Host-side `pr-comment` handler. No outputs; runs the
    /// `pr_comment` write and writes a one-line log entry. Same
    /// no-code-host degraded mode as [`Self::run_create_pr_node`] —
    /// skip with a notice rather than failing the workflow.
    fn run_pr_comment_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        body: &str,
        pr_ref: Option<&str>,
        session: &Session,
        outputs: &OutputMap,
    ) -> Result<()> {
        let Some(host) = self.code_host.clone() else {
            tracing::warn!(
                node = %node.id,
                "pr-comment skipped: no code_host configured",
            );
            let log_path = self.node_log_path(req, session, &node.id);
            let log_body = "--- pr-comment skipped ---\n\
                            No code_host configured — comment not posted.\n";
            if let Err(err) = std::fs::write(&log_path, log_body) {
                tracing::warn!(?err, log = %log_path.display(), "writing pr-comment skip log failed");
            }
            return Ok(());
        };
        let number = resolve_pr_number(node, pr_ref, session, outputs)?;
        host.pr_comment(req.workspace, number, body)
            .with_context(|| format!("pr-comment node `{}`: pr_comment failed", node.id))?;
        let log_path = self.node_log_path(req, session, &node.id);
        let log_body = format!("--- pr-comment ---\nposted comment on PR #{number}\n");
        if let Err(err) = std::fs::write(&log_path, log_body) {
            tracing::warn!(?err, log = %log_path.display(), "writing pr-comment log failed");
        }
        Ok(())
    }

    /// Borrow the configured code host, or surface a clear error
    /// naming the node so workflow authors see why a PR-aware node
    /// refused to run.
    fn require_code_host_for(
        &self,
        node: &Node,
    ) -> Result<Arc<dyn crate::code_host::CodeHost>> {
        self.code_host
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "PR-aware node `{}` requires a code host but the executor was built \
                     without one — pass `WorkflowExecutor::with_code_host(Some(...))` \
                     or set `code_host:` in `.fleet/config.yaml`",
                    node.id,
                )
            })
    }

    fn run_assert_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        expr_str: &str,
        session: &Session,
        outputs: &OutputMap,
    ) -> Result<()> {
        let log_path = self.node_log_path(req, session, &node.id);
        match expr::evaluate(expr_str, outputs) {
            Ok(true) => {
                let body = format!("--- assert passed ---\n{expr_str}\n");
                if let Err(err) = std::fs::write(&log_path, body) {
                    tracing::warn!(?err, log = %log_path.display(), "writing assert log failed");
                }
                Ok(())
            }
            Ok(false) => {
                let body = format!("--- assert failed ---\n{expr_str}\n");
                let _ = std::fs::write(&log_path, body);
                bail!(
                    "assert node `{}` failed: `{expr_str}` evaluated false (log at {})",
                    node.id,
                    log_path.display()
                );
            }
            Err(err) => {
                let body = format!("--- assert errored ---\n{expr_str}\n{err:#}\n");
                let _ = std::fs::write(&log_path, body);
                Err(err.context(format!("evaluating `expr:` for assert node `{}`", node.id)))
            }
        }
    }

    /// Execute a fanout node's siblings in parallel. Each sibling runs
    /// through the same [`Self::run_node`] machinery agent / bash /
    /// assert / gate use, but inside its own thread. After all join,
    /// any failure aggregates into a single error naming every
    /// sibling that failed.
    ///
    /// Sibling output extraction is left to the caller (`run_loop`),
    /// which knows the artifacts dir and the outputs accumulator. We
    /// just confirm the parallel slate completed.
    ///
    /// Gates inside a fanout sibling are rejected — pausing one of
    /// N parallel branches leaves the slate in an unwell state we
    /// haven't designed for in v1. Users wanting "fan out and pause"
    /// should put the gate downstream of the fanout instead.
    fn run_fanout_node(
        &self,
        req: &ExecuteRequest<'_>,
        node: &Node,
        siblings: &[String],
        session: &Session,
        outputs: &OutputMap,
        egress: &crate::egress::EgressSetup,
    ) -> Result<NodeOutcome> {
        let sibling_nodes: Vec<&Node> = siblings
            .iter()
            .map(|sid| {
                req.workflow.node(sid).ok_or_else(|| {
                    anyhow!(
                        "internal: fanout `{}` references sibling `{sid}` that doesn't exist \
                         (validate should have caught this)",
                        node.id
                    )
                })
            })
            .collect::<Result<_>>()?;

        for sib in &sibling_nodes {
            if matches!(sib.kind, NodeKind::Gate { .. } | NodeKind::Fanout { .. }) {
                bail!(
                    "fanout `{}` sibling `{}` is kind {:?} — gate/fanout inside a fanout is not supported in v1",
                    node.id,
                    sib.id,
                    sib.kind
                );
            }
        }

        let log_path = self.node_log_path(req, session, &node.id);

        // Branch on tmux mode. Inside a `fleet workflow run
        // --detached` pane, each sibling gets its own tmux window
        // (and therefore its own PTY) — necessary because multiple
        // interactive agents can't share one stdio. Outside tmux
        // (CI / autonomous when not detached), the threadpool path
        // is what we always had: lightweight, no extra processes.
        let results: Vec<Result<NodeOutcome>> =
            if std::env::var_os(crate::session::SESSION_TMUX_ENV).is_some() {
                self.run_fanout_via_tmux_windows(req, &sibling_nodes, session)?
            } else {
                std::thread::scope(|scope| {
                    // The `collect()` between spawn and join is deliberate, not
                    // wasteful: fusing the iterators would join each thread
                    // before spawning the next, serialising the slate. Suppress
                    // the needless-collect lint locally.
                    #[allow(clippy::needless_collect)]
                    let handles: Vec<_> = sibling_nodes
                        .iter()
                        .map(|sib| {
                            scope.spawn(|| self.run_node(req, sib, session, outputs, egress))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| match h.join() {
                            Ok(r) => r,
                            Err(panic) => Err(anyhow!("sibling thread panicked: {panic:?}")),
                        })
                        .collect()
                })
            };

        let mut failed: Vec<(String, String)> = Vec::new();
        let mut aggregate = NodeOutcome::empty();
        for (sib, res) in sibling_nodes.iter().zip(results) {
            match res {
                Ok(outcome) => {
                    aggregate.node_costs.extend(outcome.node_costs);
                }
                Err(err) => {
                    failed.push((sib.id.clone(), format!("{err:#}")));
                }
            }
        }

        if failed.is_empty() {
            let body = format!(
                "--- fanout ok ({} siblings) ---\n{}\n",
                sibling_nodes.len(),
                sibling_nodes
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if let Err(err) = std::fs::write(&log_path, body) {
                tracing::warn!(?err, log = %log_path.display(), "writing fanout log failed");
            }
            Ok(aggregate)
        } else {
            let combined = failed
                .iter()
                .map(|(id, err)| format!("  - {id}: {err}"))
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "--- fanout failed: {}/{} siblings failed ---\n{combined}\n",
                failed.len(),
                sibling_nodes.len(),
            );
            let _ = std::fs::write(&log_path, body);
            bail!(
                "fanout node `{}` had {} sibling failure(s) (log at {}):\n{combined}",
                node.id,
                failed.len(),
                log_path.display(),
            );
        }
    }

    /// Tmux-mode fanout dispatch. Each sibling gets its own tmux
    /// window inside the worker session; the window's command is
    /// `fleet workflow run-sibling --session-id <id> <node>`,
    /// wrapped in a tiny bash so a non-zero exit code keeps the
    /// window alive (via `sleep 600`) for forensic attach. The
    /// driver polls each sibling's `.outcome` file and folds the
    /// parsed `FanoutOutcome` back into the `Result<NodeOutcome>`
    /// shape the surrounding aggregator expects.
    fn run_fanout_via_tmux_windows(
        &self,
        req: &ExecuteRequest<'_>,
        sibling_nodes: &[&Node],
        session: &Session,
    ) -> Result<Vec<Result<NodeOutcome>>> {
        let tmux_session = std::env::var(crate::session::SESSION_TMUX_ENV)
            .context("FLEET_TMUX_SESSION must be set in tmux-mode fanout dispatch")?;
        let fleet_bin = std::env::current_exe()
            .context("locating current fleet binary for sibling subprocesses")?;
        let session_id_str = session.id.as_str().to_string();
        let fanout_dir = req.store.session_dir(&session.id).join("fanout");
        // Clear any prior `.outcome` files for this slate of
        // siblings — re-running a fanout (replay) must not see
        // stale outcomes.
        std::fs::create_dir_all(&fanout_dir)
            .with_context(|| format!("creating fanout dir at {}", fanout_dir.display()))?;
        for sib in sibling_nodes {
            let _ = std::fs::remove_file(fanout_dir.join(format!("{}.outcome", sib.id)));
        }

        // Pre-warm the devcontainer image once before fanning out.
        // Without this, every sibling subprocess invokes
        // `devcontainer up` in parallel — and the underlying
        // `docker buildx --load` races on the final image export,
        // with all but one sibling getting `ERROR: image "…":
        // already exists`. Building serially in the driver ensures
        // subsequent siblings hit the cached layers + tag.
        // Non-fatal: a build failure here surfaces from the first
        // sibling that runs anyway; we don't want a flaky probe to
        // block the run, but we do want the happy path warmed.
        if sibling_nodes
            .iter()
            .any(|n| matches!(n.kind, NodeKind::Agent { .. }))
        {
            if let Err(err) = req.adapter.ensure_image(req.devcontainer) {
                tracing::warn!(
                    error = %err,
                    "pre-warming devcontainer image before fanout dispatch failed; \
                     siblings will retry individually"
                );
            }
        }

        // Spawn one tmux window per sibling. Each window's command
        // is a small bash wrapper:
        //   fleet workflow run-sibling … ; rc=$?; if [ $rc -ne 0 ]; then sleep 600; fi
        // Successful siblings exit quickly and their windows close
        // by default; failed siblings hang on the sleep so the
        // user can attach (`Ctrl+B <window-num>`) and inspect.
        //
        // Spawns are staggered slightly for agent siblings because
        // multiple concurrent `devcontainer up` invocations race on
        // `docker buildx --load`'s image export ("ERROR: image
        // already exists"). A small inter-sibling delay lets each
        // build finish its export before the next one starts; the
        // node bodies still overlap (parallelism is preserved for
        // the actual agent run, only the spawn ramp is serialised).
        // Bash siblings skip the stagger — they don't touch the
        // adapter / image at all.
        let needs_stagger = sibling_nodes
            .iter()
            .any(|n| matches!(n.kind, NodeKind::Agent { .. }));
        let stagger = if needs_stagger {
            std::time::Duration::from_secs(4)
        } else {
            std::time::Duration::ZERO
        };
        for (idx, sib) in sibling_nodes.iter().enumerate() {
            if idx > 0 && !stagger.is_zero() {
                std::thread::sleep(stagger);
            }
            let sib = *sib;
            let cmd_str = format!(
                "{exe} workflow run-sibling --session-id {sid} {node}; \
                 rc=$?; \
                 if [ $rc -ne 0 ]; then echo; echo \"--- sibling exited $rc; keeping window alive for inspection (Ctrl+B & to close) ---\"; sleep 600; fi",
                exe = shell_quote_value(&fleet_bin.display().to_string()),
                sid = shell_quote_value(&session_id_str),
                node = shell_quote_value(&sib.id),
            );
            let argv = vec!["bash".to_string(), "-c".to_string(), cmd_str];
            // Same env_passthrough discipline as start_container —
            // the sibling subprocess inherits the parent fleet
            // process's env (so things like ANTHROPIC_API_KEY,
            // PATH, HOME are present). Per-sibling tmux env
            // (`-e FLEET_TMUX_SESSION=…`) keeps the run-sibling
            // process aware of its own tmux context for breadcrumbs.
            let env = vec![(
                crate::session::SESSION_TMUX_ENV.to_string(),
                tmux_session.clone(),
            )];
            crate::orchestrator::tmux::new_window(
                self.invoker.as_ref(),
                &tmux_session,
                &sib.id,
                &argv,
                &env,
            )
            .with_context(|| format!("spawning tmux window for sibling `{}`", sib.id))?;
        }

        // Poll the outcome files. Sleep between checks at a cadence
        // that's snappy when siblings finish quickly but not so
        // tight it wastes CPU on long-running agents. No timeout —
        // a hung sibling is a workflow-level problem the user
        // resolves via attach + kill.
        let mut results: Vec<Option<Result<NodeOutcome>>> =
            (0..sibling_nodes.len()).map(|_| None).collect();
        let mut pending: usize = sibling_nodes.len();
        while pending > 0 {
            // Honour interrupt requests so a parent Ctrl+C between
            // nodes doesn't strand the driver in the poll loop.
            if check_interrupt(req.interrupt_flag.as_ref()) {
                bail!("user interrupted during fanout dispatch");
            }
            for (idx, sib) in sibling_nodes.iter().enumerate() {
                if results[idx].is_some() {
                    continue;
                }
                let path = fanout_dir.join(format!("{}.outcome", sib.id));
                if !path.exists() {
                    continue;
                }
                let body = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let outcome: FanoutOutcome = serde_json::from_str(&body)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let result = match outcome {
                    FanoutOutcome::Ok(o) => Ok(o),
                    FanoutOutcome::Failed { message } => Err(anyhow!(message)),
                };
                let succeeded = result.is_ok();
                results[idx] = Some(result);
                pending -= 1;
                // Close the window for a clean exit. Failed
                // windows are left alive — the trailing `sleep
                // 600` in the wrapper keeps them open even though
                // run-sibling itself has exited.
                if succeeded {
                    let _ = crate::orchestrator::tmux::kill_window(
                        self.invoker.as_ref(),
                        &tmux_session,
                        &sib.id,
                    );
                }
            }
            if pending > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        Ok(results.into_iter().map(Option::unwrap).collect())
    }

    fn node_log_path(&self, req: &ExecuteRequest<'_>, session: &Session, node_id: &str) -> PathBuf {
        let _ = self; // method form keeps the surface symmetric with run_*_node.
        req.store
            .session_dir(&session.id)
            .join("logs")
            .join(format!("{node_id}.log"))
    }

    fn mark_failed(&self, req: &ExecuteRequest<'_>, session: &mut Session, err: &anyhow::Error) {
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
        // Driver is unwinding with an error — clear the pid alongside
        // the Failed transition so a reaper sweep doesn't reconfuse a
        // terminal session with a stale Running claim.
        session.clear_driver_pid((self.clock)());
        if let Err(save_err) = req.store.save(session) {
            tracing::warn!(?save_err, session = %session.id, original = %err, "saving Failed session failed");
        }
    }
}

/// Refuse to start an agent node if a configured spend budget has
/// already been met. Pure with respect to the session store +
/// session — separated from `run_agent_node` so the predicate can be
/// unit-tested without spinning up containers.
///
/// Reads the session's accumulated cost from memory; queries the
/// store for the lifetime total. A `None` budget is "unlimited" and
/// always passes. Returns a user-facing error with the specific
/// limit + actual figure so the user knows which knob to bump.
pub fn check_cost_budget(
    cost: &crate::repo_config::CostConfig,
    session: &Session,
    store: &crate::session::store::SessionStore,
    node_id: &str,
) -> Result<()> {
    if let Some(limit) = cost.per_session_budget_usd {
        let actual = session.total_cost_usd().unwrap_or(0.0);
        if actual >= limit {
            return Err(anyhow!(
                "cost budget exceeded: per-session limit ${limit:.2} reached by session \
                 (${actual:.4} already spent); refused to start agent node `{node_id}`. \
                 Raise `cost.per_session_budget_usd` in .fleet/config.yaml, or run \
                 `fleet workflow replay --rerun-only <node>` after the budget is loosened."
            ));
        }
    }
    if let Some(limit) = cost.lifetime_budget_usd {
        let actual = store.sum_costs().context("computing lifetime cost")?;
        if actual >= limit {
            return Err(anyhow!(
                "cost budget exceeded: lifetime limit ${limit:.2} reached by repo \
                 (${actual:.4} already spent across all sessions); refused to start \
                 agent node `{node_id}`. Raise `cost.lifetime_budget_usd` in \
                 .fleet/config.yaml, or prune historical sessions to free room."
            ));
        }
    }
    Ok(())
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

/// Extract a node's declared `outputs:` from
/// `<artifacts_dir>/<node_id>.outputs.json` and stash them into `acc`
/// keyed by `(node_id, local_name)`. The file format is a flat JSON
/// object whose keys are referenced by the RHS of each `outputs:`
/// entry; scalar values (string / number / bool) are coerced to
/// strings.
///
/// Contract:
/// - `node.outputs` empty (the common case) → no-op.
/// - File missing while outputs are declared → error pointing at the
///   path so the agent author knows what to produce.
/// - File present but a referenced key is missing → error naming the
///   key + the local name.
/// - Non-scalar value → error naming the offending key.
///
/// Re-runs (via `loop_back_to`) overwrite the prior entry under the
/// same `(node_id, local_name)`, which is exactly what downstream
/// `when:` predicates want — they see the latest pass's decision.
pub fn extract_outputs(artifacts_dir: &Path, node: &Node, acc: &mut OutputMap) -> Result<()> {
    if node.outputs.is_empty() {
        return Ok(());
    }
    let path = artifacts_dir.join(format!("{}.outputs.json", node.id));
    let body = std::fs::read_to_string(&path).map_err(|e| {
        anyhow!(
            "node `{}` declares outputs but cannot read `{}`: {e} \
             (the agent is expected to write a top-level JSON object here)",
            node.id,
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        anyhow!(
            "node `{}`: parsing outputs JSON at {}: {e}",
            node.id,
            path.display()
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        anyhow!(
            "node `{}`: outputs JSON at {} must be a top-level object",
            node.id,
            path.display()
        )
    })?;
    for (local_name, source_key) in &node.outputs {
        let raw = obj.get(source_key.as_str()).ok_or_else(|| {
            anyhow!(
                "node `{}`: outputs JSON at {} is missing key `{source_key}` \
                 (declared as local output `{local_name}`)",
                node.id,
                path.display()
            )
        })?;
        let s = match raw {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            // Objects and arrays JSON-encode to a single string so
            // downstream nodes can pluck structured payloads back out
            // — the `tracker-create` node consumes
            // `recommend_ticket: { title, body, labels }` this way.
            // `when:` predicates over object outputs won't match an
            // `==` against a scalar string (the encoded form is the
            // literal JSON), which is the same behaviour any other
            // mismatched-shape comparison would have.
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                serde_json::to_string(raw).map_err(|e| {
                    anyhow!(
                        "node `{}`: serialising output key `{source_key}` failed: {e}",
                        node.id,
                    )
                })?
            }
            // Null is treated as "this optional output doesn't apply
            // to this run" and stored as the empty string. The outcome
            // convention's `recommend_ticket` is the canonical user:
            // the implementer agent emits a `{title, body, labels?}`
            // object when outcome=blocked and a follow-up would help,
            // or `null` otherwise. Downstream `when:` predicates use
            // `!= ""` to gate the tracker-create node.
            serde_json::Value::Null => String::new(),
        };
        acc.insert((node.id.clone(), local_name.clone()), s);
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
    /// Optional PR the workflow is acting on. Exposed as the
    /// `FLEET_PR_*` block when set. Independent of [`Self::issue`];
    /// both may be set simultaneously.
    pub pr: Option<&'a PrContext>,
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
///
/// `CLAUDE_CODE_OAUTH_TOKEN` gets a special path: when the agent's
/// `env_passthrough` includes it (or its legacy alias
/// `CLAUDE_OAUTH_TOKEN`) **and** the host's env doesn't supply one,
/// we fall back to reading `~/.claude/.credentials.json` and
/// extracting `claudeAiOauth.accessToken` — the same file that
/// `claude setup-token` (and the interactive OAuth flow) write. This
/// restores the pre-AO/Lima behaviour where fleet picked up the
/// host's claude credentials automatically, without re-introducing a
/// keychain / secrets module.
#[must_use]
pub fn build_agent_env(agent: &AgentSpec, ctx: &AgentContext<'_>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = agent
        .env_passthrough
        .iter()
        .map(|k| {
            let value = std::env::var(k).unwrap_or_default();
            let resolved = if value.is_empty() && is_claude_oauth_var(k) {
                host_claude_oauth_token().unwrap_or_default()
            } else {
                value
            };
            (k.clone(), resolved)
        })
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
    if let Some(pr) = ctx.pr {
        env.push(("FLEET_PR_NUMBER".to_string(), pr.number.to_string()));
        env.push(("FLEET_PR_HUMAN_ID".to_string(), pr.human_id.clone()));
        env.push(("FLEET_PR_TITLE".to_string(), pr.title.clone()));
        env.push(("FLEET_PR_HEAD_REF".to_string(), pr.head_ref.clone()));
        env.push(("FLEET_PR_HEAD_SHA".to_string(), pr.head_sha.clone()));
        env.push(("FLEET_PR_BASE_REF".to_string(), pr.base_ref.clone()));
        env.push(("FLEET_PR_URL".to_string(), pr.url.clone()));
    }
    env
}

/// `true` if `name` is one of the two env var names users may put in
/// `env_passthrough` to flow the claude OAuth token through. The
/// canonical name is `CLAUDE_CODE_OAUTH_TOKEN` (what claude itself
/// reads); `CLAUDE_OAUTH_TOKEN` is the legacy spelling fleet's
/// default registry used pre-refactor.
fn is_claude_oauth_var(name: &str) -> bool {
    name == "CLAUDE_CODE_OAUTH_TOKEN" || name == "CLAUDE_OAUTH_TOKEN"
}

/// Walk an env Vec and replace any value whose key has a
/// configured `secrets:` backend with the value the backend
/// returns. Names match case-insensitively so the env-var-style
/// `CLAUDE_CODE_OAUTH_TOKEN` resolves the snake-case
/// `claude_code_oauth_token` secret entry.
///
/// Errors propagate from configured backends — a typo in a
/// 1Password reference or a missing keychain entry should fail
/// the agent node loudly rather than silently fall through to a
/// half-resolved env. Names without a backend config are left
/// untouched; their existing host-env / host-file fallback applies.
///
/// Pure-ish: the only side effect is whatever the backend does
/// (subprocess to `op`, keychain RPC). `invoker` is the seam for
/// tests.
pub fn resolve_secrets_in_env(
    env: &mut [(String, String)],
    secrets: &std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig>,
    invoker: &Arc<dyn ProcessInvoker>,
) -> Result<()> {
    use secrecy::ExposeSecret;
    for (key, value) in env.iter_mut() {
        let lookup = key.to_ascii_lowercase();
        let Some(cfg) = secrets.get(&lookup) else {
            continue;
        };
        let backend = crate::secrets::build(cfg, Arc::clone(invoker));
        let resolved = backend.fetch().with_context(|| {
            format!(
                "resolving secret `{lookup}` via `{}` backend for env var `{key}`",
                backend.kind()
            )
        })?;
        resolved.expose_secret().clone_into(value);
    }
    Ok(())
}

/// Best-effort read of the host's claude OAuth token from the file
/// `claude setup-token` writes. Returns `None` on any failure —
/// missing file, unparseable JSON, missing key — so the env-var
/// fallback degrades silently to "no credentials" and the agent's
/// own failure path surfaces a clear error if it really needs auth.
fn host_claude_oauth_token() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home)
        .join(".claude")
        .join(".credentials.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
    parsed
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(String::from)
}

/// Locate the sibling `fleet-tracker` binary and render it as a
/// read-only bind mount at `/usr/local/bin/fleet-tracker`. Returns
/// `None` (mount is skipped, bridge HTTP endpoint stays reachable
/// either way) when:
///
/// - The host isn't Linux. macOS / other-OS fleet builds produce a
///   non-ELF `fleet-tracker` binary that the in-container Linux
///   loader can't exec; rather than surface a cryptic "exec format
///   error" inside the agent, the executor skips the mount and the
///   agent's `fleet-tracker` calls fail at the missing-binary layer
///   with a clearer error.
/// - The sibling binary isn't where `current_exe()` says it should
///   be (e.g. fleet was installed via cargo and the user only
///   installed `fleet`, not `fleet-tracker`).
/// - The container image's architecture doesn't match the host's
///   (e.g. a linux/amd64 fleet host running a linux/arm64 image).
///   The adapter's `inspect_image_arch` answer is the source of truth;
///   any of `Err`, `Ok(None)`, or `Ok(Some(arch))` with `arch !=
///   host_arch_oci()` collapse to "don't mount." Engine errors are
///   intentionally treated as "can't tell" — failing the workflow
///   over an inspect glitch when the bridge HTTP path still works
///   would be over-strict.
fn fleet_tracker_mount(adapter: &dyn RuntimeAdapter, image: &ImageId) -> Option<MountSpec> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let host_path = locate_sibling_fleet_tracker()?;
    if !host_arch_matches_image(adapter, image) {
        return None;
    }
    Some(MountSpec {
        host_path,
        container_path: PathBuf::from("/usr/local/bin/fleet-tracker"),
        read_only: true,
    })
}

/// Resolve the host path of the sibling `fleet-tracker` binary
/// (alongside `current_exe()`'s `fleet`). `None` when the path can't
/// be resolved or the candidate file doesn't exist.
fn locate_sibling_fleet_tracker() -> Option<PathBuf> {
    locate_sibling_binary("fleet-tracker")
}

/// Same as [`fleet_tracker_mount`] but for the `fleet-pr` binary —
/// agent containers see it at `/usr/local/bin/fleet-pr`. Identical
/// arch / co-location gating; missing binary collapses to "don't
/// mount" so PR-aware reads degrade to "no fleet-pr in PATH" rather
/// than failing the workflow.
fn fleet_pr_mount(adapter: &dyn RuntimeAdapter, image: &ImageId) -> Option<MountSpec> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let host_path = locate_sibling_fleet_pr()?;
    if !host_arch_matches_image(adapter, image) {
        return None;
    }
    Some(MountSpec {
        host_path,
        container_path: PathBuf::from("/usr/local/bin/fleet-pr"),
        read_only: true,
    })
}

fn locate_sibling_fleet_pr() -> Option<PathBuf> {
    locate_sibling_binary("fleet-pr")
}

/// Same as [`fleet_tracker_mount`] but for the `fleet-git` shim —
/// bind-mounted at `/usr/local/bin/git` so it shadows the real git
/// in the container's PATH. Identical arch / co-location gating; the
/// missing binary collapses to "don't mount" rather than failing the
/// workflow (defense-in-depth: a missing shim re-exposes the implicit
/// no-credentials boundary, not a hard failure).
fn fleet_git_mount(adapter: &dyn RuntimeAdapter, image: &ImageId) -> Option<MountSpec> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let host_path = locate_sibling_binary("fleet-git")?;
    if !host_arch_matches_image(adapter, image) {
        return None;
    }
    Some(MountSpec {
        host_path,
        container_path: PathBuf::from("/usr/local/bin/git"),
        read_only: true,
    })
}

/// When `workspace` is a git worktree (i.e. its `.git` is a *file*,
/// not a directory), produce bind mounts that make the gitlink
/// resolvable inside the container.
///
/// A `git worktree add` creates two paths on the host that the
/// container's mount of the worktree dir alone can't see:
///   1. The worktree's per-instance gitdir at
///      `<main_repo>/.git/worktrees/<name>/`, referenced by the
///      `.git` file's `gitdir:` line.
///   2. The main repo's `.git/`, referenced from the gitdir's
///      `commondir` file and used for shared `objects/`, `refs/`,
///      `config`, etc.
///
/// Without these, every `git` command inside the container fails
/// with "fatal: not a git repository: '`<host_path>`'", and agents
/// observed this as "git worktree was broken" and reactively
/// `rm .git && git init`'d — disconnecting the session branch from
/// the main repo and losing the `fleet/session-<id>` ref.
///
/// Both mounts are bound at the *same absolute host path* inside the
/// container so the gitlink + commondir text doesn't have to be
/// rewritten. Both are read-write because `git commit` needs to
/// update HEAD/index in the gitdir and write objects/refs in the
/// commondir.
///
/// Returns empty when `.git` is a directory (regular non-worktree
/// repo), missing (non-git workspace), or unparseable.
fn worktree_git_mounts(workspace: &Path) -> Vec<MountSpec> {
    let mut out = Vec::new();
    let gitlink_path = workspace.join(".git");
    let Ok(meta) = std::fs::metadata(&gitlink_path) else {
        return out;
    };
    if !meta.is_file() {
        // Either a regular repo (`.git` is a dir, mounted via the
        // workspace mount and self-contained) or some other unusual
        // shape — leave alone in both cases.
        return out;
    }
    let Ok(body) = std::fs::read_to_string(&gitlink_path) else {
        return out;
    };
    let Some(gitdir) = parse_gitlink_gitdir(&body) else {
        return out;
    };
    let main_git = read_commondir(&gitdir);

    out.push(MountSpec {
        host_path: gitdir.clone(),
        container_path: gitdir,
        read_only: false,
    });
    if let Some(main) = main_git {
        out.push(MountSpec {
            host_path: main.clone(),
            container_path: main,
            read_only: false,
        });
    }
    out
}

/// Parse `gitdir: <path>` out of a `.git` gitlink file body. The
/// file format is a single trailing-newline line; whitespace tolerant
/// so a future git format quirk doesn't break us.
fn parse_gitlink_gitdir(body: &str) -> Option<PathBuf> {
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

/// Resolve `<gitdir>/commondir` to an absolute path. The file usually
/// contains a relative path (e.g. `../..`) anchored at `gitdir`.
/// Falls back to `None` when missing or unparseable — caller skips
/// the main-repo mount in that case (still usable for read-only
/// inspection of the worktree's own gitdir).
fn read_commondir(gitdir: &Path) -> Option<PathBuf> {
    let commondir_file = gitdir.join("commondir");
    let raw = std::fs::read_to_string(&commondir_file).ok()?;
    let rel = raw.trim();
    if rel.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(rel);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        gitdir.join(candidate)
    };
    std::fs::canonicalize(&resolved).ok().or(Some(resolved))
}

fn locate_sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(name);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Pure predicate: does this adapter report the image's arch as a
/// match for the host's? `inspect_image_arch` returning Err or
/// Ok(None) both collapse to `false` ("can't tell, skip"). Factored
/// out so the gate logic is unit-testable without a real binary on
/// disk.
fn host_arch_matches_image(adapter: &dyn RuntimeAdapter, image: &ImageId) -> bool {
    matches!(
        adapter.inspect_image_arch(image),
        Ok(Some(ref arch)) if arch == host_arch_oci()
    )
}

/// Append `FLEET_BRIDGE_URL`, `FLEET_BRIDGE_TOKEN`, and `NO_PROXY` to
/// the agent's env when a bridge is active. `NO_PROXY` keeps the
/// agent's HTTP client from routing the bridge request through the
/// egress tinyproxy (the proxy would refuse the loopback hostname).
/// No-op when `bridge` is `None`.
pub fn push_bridge_env(env: &mut Vec<(String, String)>, bridge: Option<&Bridge>) {
    let Some(b) = bridge else {
        return;
    };
    env.push(("FLEET_BRIDGE_URL".to_string(), b.url_for_container()));
    env.push(("FLEET_BRIDGE_TOKEN".to_string(), b.token().to_string()));
    env.push(("NO_PROXY".to_string(), BRIDGE_HOST.to_string()));
}

/// Append the `FLEET_*` env vars the `fleet-git` shim reads:
/// - `FLEET_PROTECTED_BRANCHES` / `FLEET_ALLOW_PUSH_TO`: newline-
///   joined; the shim parses them back into `Vec<String>`.
/// - `FLEET_SESSION_ID`: opaque session id copied into log lines.
/// - `FLEET_GIT_LOG`: in-container path where the shim appends one
///   JSON line per invocation. We point it at `/artifacts/git.log`
///   so it lands in the bind-mounted per-session artifacts dir for
///   forensic review.
///
/// No-op when `policy.enabled == false` — disabling the guard means
/// neither the shim mount nor any of its env should appear.
pub fn push_git_guard_env(
    env: &mut Vec<(String, String)>,
    policy: GitGuardPolicy<'_>,
    session_id: &str,
) {
    if !policy.enabled {
        return;
    }
    env.push((
        "FLEET_PROTECTED_BRANCHES".to_string(),
        crate::policy::encode_for_env(policy.protected),
    ));
    env.push((
        "FLEET_ALLOW_PUSH_TO".to_string(),
        crate::policy::encode_for_env(policy.allow_push_to),
    ));
    env.push(("FLEET_SESSION_ID".to_string(), session_id.to_string()));
    env.push((
        "FLEET_GIT_LOG".to_string(),
        "/artifacts/git.log".to_string(),
    ));
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

/// Bash-side counterpart of the `FLEET_PR_*` block in
/// [`build_agent_env`]. Returns the empty string when no PR is
/// bound. Pure; the bash-node runner concatenates this onto
/// [`bash_issue_env_prefix`] so a session bound to both shows both
/// blocks. The trailing space matters for chaining without quoting.
#[must_use]
pub fn bash_pr_env_prefix(pr: Option<&PrContext>) -> String {
    let Some(p) = pr else {
        return String::new();
    };
    format!(
        "FLEET_PR_NUMBER={} FLEET_PR_HUMAN_ID={} FLEET_PR_TITLE={} \
         FLEET_PR_HEAD_REF={} FLEET_PR_HEAD_SHA={} FLEET_PR_BASE_REF={} FLEET_PR_URL={} ",
        shell_quote_value(&p.number.to_string()),
        shell_quote_value(&p.human_id),
        shell_quote_value(&p.title),
        shell_quote_value(&p.head_ref),
        shell_quote_value(&p.head_sha),
        shell_quote_value(&p.base_ref),
        shell_quote_value(&p.url),
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

/// Decide whether a node's `loop_back_to` fires, returning the index in
/// `order` to jump to. `None` means "continue forward". The per-node
/// loop counter is mutated in place; an unrecognised target (which
/// validation already rejects) is treated as "continue forward" so a
/// stray bug never traps the executor.
fn resolve_loop_back(
    node: &Node,
    order: &[String],
    loop_counts: &mut BTreeMap<String, u32>,
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

/// Set of node ids that appear as siblings in some fanout node. These
/// are "owned" by the fanout: they execute when the fanout fires, not
/// through the main topological schedule. Returns an empty set when no
/// fanout exists, which is the common case.
fn owned_fanout_siblings(wf: &Workflow) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for n in &wf.nodes {
        if let NodeKind::Fanout { siblings } = &n.kind {
            for s in siblings {
                out.insert(s.clone());
            }
        }
    }
    out
}

/// Kahn's algorithm topo sort with stable tie-breaking (alphabetical by id)
/// so logs and tests get deterministic ordering across runs. Returns ids
/// in execution order. Cycles are rejected upstream via `validate`; if one
/// slips through here we still surface a clear error.
///
/// Fanout siblings are excluded from the order — they execute as part of
/// their owning fanout's run, not as standalone scheduled nodes. Their
/// edges are also dropped from indegree/adjacency so a non-fanout node
/// that (oddly) depends on a sibling doesn't get stranded with an
/// indegree it can never satisfy.
fn topological_order(wf: &Workflow) -> Result<Vec<String>> {
    let owned = owned_fanout_siblings(wf);
    let active: Vec<&Node> = wf.nodes.iter().filter(|n| !owned.contains(&n.id)).collect();

    let mut indegree: HashMap<String, usize> = active
        .iter()
        .map(|n| {
            let deg = n.depends_on.iter().filter(|d| !owned.contains(*d)).count();
            (n.id.clone(), deg)
        })
        .collect();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for n in &active {
        for dep in &n.depends_on {
            if owned.contains(dep) {
                continue;
            }
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

    let mut order = Vec::with_capacity(active.len());
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
    if order.len() != active.len() {
        bail!(
            "workflow `{}` is not a DAG (topological sort produced {} of {} active nodes; \
             {} fanout sibling(s) excluded by ownership)",
            wf.name,
            order.len(),
            active.len(),
            owned.len(),
        );
    }
    Ok(order)
}

/// `true` when the caller-supplied interrupt flag is `Some(flag)` and
/// `flag` is currently set. Used by the run loop to abort cleanly on
/// user Ctrl+C. Pure helper so the call site reads `if check_interrupt(...)`
/// without unwrapping the Option inline.
fn check_interrupt(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> bool {
    flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
}

/// Short word describing a node's kind, for the per-node breadcrumb
/// the run loop prints when fleet is running inside a tmux pane. Pure
/// so it's trivially testable.
#[must_use]
fn node_kind_word(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent { .. } => "agent",
        NodeKind::Bash { .. } => "bash",
        NodeKind::Gate { .. } => "gate",
        NodeKind::Assert { .. } => "assert",
        NodeKind::Fanout { .. } => "fanout",
        NodeKind::TrackerCreate { .. } => "tracker-create",
        NodeKind::PrList { .. } => "pr-list",
        NodeKind::PrChecks { .. } => "pr-checks",
        NodeKind::CreatePr { .. } => "create-pr",
        NodeKind::PrComment { .. } => "pr-comment",
    }
}

/// Flatten the session's persisted nested outputs map into the
/// in-memory [`OutputMap`] the executor's expression engine consumes.
/// Pure; tests assert the round-trip with [`outputs_to_persisted`].
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn outputs_from_persisted(
    persisted: &BTreeMap<String, BTreeMap<String, String>>,
) -> OutputMap {
    let mut out = HashMap::new();
    for (node_id, names) in persisted {
        for (name, value) in names {
            out.insert((node_id.clone(), name.clone()), value.clone());
        }
    }
    out
}

/// Nest the in-memory [`OutputMap`] back into the `BTreeMap` shape
/// that rides on `Session`. Stable ordering thanks to `BTreeMap` so
/// the persisted JSON is byte-stable across runs with the same data.
fn outputs_to_persisted(outputs: &OutputMap) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut nested: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for ((node_id, name), value) in outputs {
        nested
            .entry(node_id.clone())
            .or_default()
            .insert(name.clone(), value.clone());
    }
    nested
}

/// Recursively copy regular files (and the directory structure that
/// contains them) from `src` into `dst`. `dst` is created on demand at
/// each level via `create_dir_all`; symlinks and special files are
/// skipped silently. Used by `replay` to stage a prior session's
/// `artifacts/` into a new session.
fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    let entries =
        std::fs::read_dir(src).with_context(|| format!("reading directory {}", src.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("scanning {}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("statting {}", from.display()))?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
            copy_dir_contents(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
        }
        // Symlinks/specials: skip silently. Artifacts/ holds generated
        // text files in practice; nothing else should be there.
    }
    Ok(())
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

/// JSON deserialisation target for the `recommend_ticket`-shaped value
/// the `tracker-create` node consumes. `labels` defaults to empty so
/// the agent can omit it when there's nothing to attach.
#[derive(serde::Deserialize)]
struct TrackerCreateRequest {
    title: String,
    body: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Resolve `.fleet/deps.json` from the executor's session store.
/// The session store's root is `<fleet_root>/.fleet/sessions/`, so
/// the deps file is its sibling `<fleet_root>/.fleet/deps.json`.
/// Falls back to the session-store root's join when the path has no
/// parent (e.g. some test configurations); the parent always exists
/// under the v1 layout but the defensive fallback keeps tests
/// constructing `SessionStore::at("./sessions")` from a tempdir
/// working without each having to pre-create the parent.
fn deps_path_for(store: &SessionStore) -> PathBuf {
    store
        .root()
        .parent()
        .map_or_else(|| store.root().join("deps.json"), |p| p.join("deps.json"))
}

/// Resolve the `.fleet/plans/` directory from the executor's session
/// store, same sibling-relationship as [`deps_path_for`]. Used by the
/// `tracker-create` plan-injector so the workflow engine doesn't
/// need a separate `plans:` field threaded through `ExecuteRequest`.
fn plans_root_for(store: &SessionStore) -> PathBuf {
    store
        .root()
        .parent()
        .map_or_else(|| store.root().join("plans"), |p| p.join("plans"))
}

/// Count how many distinct `tracker-create` nodes in `workflow`
/// have already filed a ticket in this session, per the persisted
/// `created_id` output. Same-node re-fires (via `loop_back_to`)
/// overwrite the existing entry rather than appending, so the
/// returned count is "number of `tracker-create` nodes that have
/// successfully run at least once" — exactly what the per-session
/// cap wants to measure.
fn count_tracker_create_firings(workflow: &Workflow, session: &Session) -> u32 {
    let count = workflow
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::TrackerCreate { .. }))
        .filter(|n| {
            session
                .outputs
                .get(&n.id)
                .is_some_and(|m| m.contains_key("created_id"))
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Parse a `<node>.<output>` reference (the shape used by
/// `tracker-create`'s `from:` field, and by `when:` predicates'
/// left-hand sides) into `(node, output_name)`. Errors loudly on
/// missing-dot, empty halves, or anything beyond a single dot —
/// nested paths are explicitly not supported in v1.
fn parse_output_ref(reference: &str) -> Result<(&str, &str)> {
    let (node, name) = reference
        .split_once('.')
        .ok_or_else(|| anyhow!("`{reference}` is not a `<node>.<output>` reference"))?;
    if node.is_empty() || name.is_empty() {
        bail!("`{reference}` is malformed: both `<node>` and `<output>` must be non-empty");
    }
    if name.contains('.') {
        bail!(
            "`{reference}` is malformed: nested paths are not supported in v1 \
             (use the second segment as a single output name)"
        );
    }
    Ok((node, name))
}

/// Resolve which PR number a `pr-checks` / `pr-comment` node should
/// act on. Accepts three forms, tried in order:
/// 1. A bare numeric literal in `pr_ref` (`pr: "42"`).
/// 2. A dotted `<node>.<output>` reference resolved against the run's
///    `outputs` map (`pr: ${probe.number}`).
/// 3. No `pr_ref` → fall back to the session's bound [`PrContext`].
///
/// Surfaces a clear "session has no bound PR" error when both the
/// explicit ref and the session fallback are absent.
fn resolve_pr_number(
    node: &Node,
    pr_ref: Option<&str>,
    session: &Session,
    outputs: &OutputMap,
) -> Result<u32> {
    if let Some(raw) = pr_ref {
        let trimmed = raw.trim();
        // Form 1: numeric literal.
        if let Ok(n) = trimmed.parse::<u32>() {
            return Ok(n);
        }
        // Form 2: dotted reference into outputs.
        let (src_node, src_name) = parse_output_ref(trimmed).with_context(|| {
            format!(
                "node `{}`: `pr: {trimmed}` is not a numeric literal or a `<node>.<output>` \
                 reference",
                node.id
            )
        })?;
        let value = outputs
            .get(&(src_node.to_string(), src_name.to_string()))
            .ok_or_else(|| {
                anyhow!(
                    "node `{}`: `pr: {trimmed}` resolves to no upstream output \
                     (did the producing node fail or skip?)",
                    node.id,
                )
            })?;
        return value.parse::<u32>().with_context(|| {
            format!(
                "node `{}`: upstream output `{trimmed}` value `{value}` is not a PR number",
                node.id,
            )
        });
    }
    session.pr.as_ref().map(|p| p.number).ok_or_else(|| {
        anyhow!(
            "node `{}`: no `pr:` reference set and session has no bound PR — \
             scope this node to a PR via `pr:` or run the workflow with `--pr <n>`",
            node.id,
        )
    })
}

/// Resolve the worktree's current branch name. Errors out rather
/// than guessing — `create-pr` must know which branch to use as
/// `head`, and silently defaulting could push to the wrong place.
fn detect_current_branch(invoker: &dyn ProcessInvoker, repo_root: &Path) -> Result<String> {
    let out = invoker
        .run(
            "git",
            vec![
                "-C".to_string(),
                repo_root.to_string_lossy().into_owned(),
                "rev-parse".to_string(),
                "--abbrev-ref".to_string(),
                "HEAD".to_string(),
            ],
        )
        .context("running `git rev-parse --abbrev-ref HEAD`")?;
    let s = out.trim();
    if s.is_empty() || s == "HEAD" {
        bail!(
            "the worktree is in a detached-HEAD state — `create-pr` cannot infer the \
             source branch; pass `head:` explicitly"
        );
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::runtime::local::LocalAdapter;
    use mockall::predicate::{always, eq};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test]
    fn check_interrupt_returns_false_when_flag_absent() {
        // No SIGINT handler installed (tests, CI runs without --detached)
        // → executor always sees `false` and never short-circuits.
        assert!(!check_interrupt(None));
    }

    #[test]
    fn check_interrupt_returns_false_when_flag_present_but_clear() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!check_interrupt(Some(&flag)));
    }

    #[test]
    fn check_interrupt_returns_true_when_flag_set() {
        // The SIGINT handler flips the bool to true; the next run-loop
        // iteration sees it via `check_interrupt` and aborts.
        let flag = Arc::new(AtomicBool::new(true));
        assert!(check_interrupt(Some(&flag)));
    }

    #[test]
    fn resolve_secrets_in_env_overlays_env_backend_value() {
        // Secret named `gh_token`, env passthrough on `GH_TOKEN` —
        // case-insensitive match resolves the snake-case secret to
        // the uppercase env var.
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert(
            "gh_token".to_string(),
            crate::secrets::SecretBackendConfig::Env {
                var: "FLEET_TEST_RESOLVE_SECRETS_OVERLAYS".to_string(),
            },
        );
        // SAFETY: env mutation is process-global; the name is unique
        // to this test so concurrent tests can't race.
        unsafe { std::env::set_var("FLEET_TEST_RESOLVE_SECRETS_OVERLAYS", "ghp_via_secret") };
        let mut env = vec![
            ("GH_TOKEN".to_string(), String::new()),
            ("OTHER".to_string(), "untouched".to_string()),
        ];
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(crate::process::RealProcessInvoker);
        resolve_secrets_in_env(&mut env, &secrets, &invoker).unwrap();
        unsafe { std::env::remove_var("FLEET_TEST_RESOLVE_SECRETS_OVERLAYS") };
        assert_eq!(env[0].1, "ghp_via_secret");
        assert_eq!(env[1].1, "untouched");
    }

    #[test]
    fn resolve_secrets_in_env_is_a_noop_when_no_backend_matches() {
        // Empty config → every env entry passes through unchanged.
        let secrets = std::collections::BTreeMap::new();
        let mut env = vec![("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "kept".to_string())];
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(crate::process::RealProcessInvoker);
        resolve_secrets_in_env(&mut env, &secrets, &invoker).unwrap();
        assert_eq!(env[0].1, "kept");
    }

    #[test]
    fn resolve_secrets_in_env_propagates_backend_failure() {
        // A configured backend that fails (e.g. env var missing)
        // must surface the error rather than silently leaving the
        // env empty — otherwise the agent runs with no auth and the
        // user sees a confusing claude-side error instead of fleet's
        // clear "could not resolve secret X" message.
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert(
            "gh_token".to_string(),
            crate::secrets::SecretBackendConfig::Env {
                var: "FLEET_TEST_RESOLVE_SECRETS_MISSING".to_string(),
            },
        );
        // SAFETY: env mutation is process-global; the name is unique.
        unsafe { std::env::remove_var("FLEET_TEST_RESOLVE_SECRETS_MISSING") };
        let mut env = vec![("GH_TOKEN".to_string(), String::new())];
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(crate::process::RealProcessInvoker);
        let err = resolve_secrets_in_env(&mut env, &secrets, &invoker).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("gh_token"), "msg: {msg}");
        assert!(msg.contains("env"), "msg: {msg}");
    }

    #[test]
    fn fanout_outcome_round_trips_ok_variant_through_json() {
        // Pins the wire format the `run-sibling` subprocess writes
        // and the parent driver reads. Drift here would silently
        // break stage-3 fanout dispatch.
        let outcome = NodeOutcome {
            node_costs: vec![("sib_a".to_string(), 0.12)],
            extra_outputs: vec![(
                ("sib_a".to_string(), "created_id".to_string()),
                "42".to_string(),
            )],
        };
        let wire = FanoutOutcome::Ok(outcome.clone());
        let json = serde_json::to_string(&wire).unwrap();
        // Tag is on the outer object.
        assert!(json.contains("\"kind\":\"ok\""), "json: {json}");
        // Round-trip.
        let parsed: FanoutOutcome = serde_json::from_str(&json).unwrap();
        match parsed {
            FanoutOutcome::Ok(p) => {
                assert_eq!(p.node_costs, outcome.node_costs);
                assert_eq!(p.extra_outputs, outcome.extra_outputs);
            }
            FanoutOutcome::Failed { .. } => panic!("expected Ok"),
        }
    }

    #[test]
    fn fanout_outcome_round_trips_failed_variant_through_json() {
        let wire = FanoutOutcome::Failed {
            message: "agent exited with code 1".to_string(),
        };
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains("\"kind\":\"failed\""), "json: {json}");
        assert!(json.contains("agent exited with code 1"), "json: {json}");
        let parsed: FanoutOutcome = serde_json::from_str(&json).unwrap();
        match parsed {
            FanoutOutcome::Failed { message } => {
                assert_eq!(message, "agent exited with code 1");
            }
            FanoutOutcome::Ok(_) => panic!("expected Failed"),
        }
    }

    fn sample_devcontainer() -> Devcontainer {
        Devcontainer::from_str_at(
            r#"{ "image": "test:1" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap()
    }

    /// Empty static secrets map for `ExecuteRequest` literals in
    /// tests. Lives in a `OnceLock` so the reference is `'static`
    /// without each test having to declare its own owning binding.
    fn empty_secrets_static()
    -> &'static std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig> {
        use std::sync::OnceLock;
        static EMPTY: OnceLock<
            std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig>,
        > = OnceLock::new();
        EMPTY.get_or_init(std::collections::BTreeMap::new)
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
            labels: Vec::new(),
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
    fn copy_dir_contents_recursively_copies_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("top.txt"), "t").unwrap();
        std::fs::create_dir_all(src.path().join("nested/deeper")).unwrap();
        std::fs::write(src.path().join("nested/inner.md"), "i").unwrap();
        std::fs::write(src.path().join("nested/deeper/leaf.log"), "l").unwrap();

        copy_dir_contents(src.path(), dst.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.path().join("top.txt")).unwrap(),
            "t"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("nested/inner.md")).unwrap(),
            "i"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("nested/deeper/leaf.log")).unwrap(),
            "l"
        );
    }

    #[test]
    fn copy_dir_contents_overwrites_existing_files_in_dst() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "new").unwrap();
        std::fs::write(dst.path().join("a.txt"), "old").unwrap();
        copy_dir_contents(src.path(), dst.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn check_cost_budget_passes_when_both_budgets_unlimited() {
        let cost = crate::repo_config::CostConfig::default(); // None, None
        let (_d, store) = build_store();
        let session = Session::new(SessionId::new("s-x"), "wf", 1);
        check_cost_budget(&cost, &session, &store, "n").unwrap();
    }

    #[test]
    fn check_cost_budget_refuses_when_session_budget_already_met() {
        let cost = crate::repo_config::CostConfig {
            per_session_budget_usd: Some(0.5),
            lifetime_budget_usd: None,
        };
        let (_d, store) = build_store();
        let mut session = Session::new(SessionId::new("s-x"), "wf", 1);
        // Prior nodes in this session pushed the cost above the limit.
        session.record_node_cost("plan", 0.30, 2);
        session.record_node_cost("review", 0.25, 3);
        let err = check_cost_budget(&cost, &session, &store, "next-node").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("per-session limit"), "got: {msg}");
        assert!(msg.contains("$0.50"));
        assert!(msg.contains("next-node"));
    }

    #[test]
    fn check_cost_budget_passes_when_session_under_budget() {
        let cost = crate::repo_config::CostConfig {
            per_session_budget_usd: Some(1.0),
            lifetime_budget_usd: None,
        };
        let (_d, store) = build_store();
        let mut session = Session::new(SessionId::new("s-x"), "wf", 1);
        session.record_node_cost("plan", 0.30, 2);
        check_cost_budget(&cost, &session, &store, "next-node").unwrap();
    }

    #[test]
    fn check_cost_budget_refuses_when_lifetime_budget_already_met() {
        let cost = crate::repo_config::CostConfig {
            per_session_budget_usd: None,
            lifetime_budget_usd: Some(1.0),
        };
        let (_d, store) = build_store();
        // Stage some prior sessions with costs that already saturate
        // the lifetime budget.
        let mut prior = Session::new(SessionId::new("s-prior"), "wf", 1);
        prior.record_node_cost("a", 0.60, 2);
        prior.record_node_cost("b", 0.50, 3);
        store.create(&prior).unwrap();
        // The new (in-memory) session is itself cost-free; the
        // lifetime sum from the store is what trips the guard.
        let session = Session::new(SessionId::new("s-new"), "wf", 4);
        let err = check_cost_budget(&cost, &session, &store, "next-node").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("lifetime limit"), "got: {msg}");
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
        let err = verify_inputs(
            tmp.path(),
            &agent_node("code", &["plan.md", "review.md"], &[]),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("node `code` requires input artifact `review.md`"),
            "got: {msg}"
        );
    }

    #[test]
    fn verify_inputs_reports_first_missing_only() {
        // Stable wording: report the first missing artifact (per the
        // declared order in the YAML) so error messages stay
        // deterministic regardless of filesystem walk order.
        let tmp = tempfile::tempdir().unwrap();
        let err = verify_inputs(
            tmp.path(),
            &agent_node("n", &["first.md", "second.md"], &[]),
        )
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
        let err = verify_outputs(tmp.path(), &agent_node("plan", &[], &["plan.md"])).unwrap_err();
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
        // into when it sees the maker script.
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("requires input artifact `missing.md`"),
            "got: {msg}"
        );
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_TEST_KEY" && v == "secret")
        );
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
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_ISSUE_ID" && v == "gh:42")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_ISSUE_HUMAN_ID" && v == "42")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_ISSUE_TITLE" && v == "Fix the parser")
        );
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
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_PERSONA" && v == "planner")
        );
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
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_PROMPT_FILE" && v == "prompts/planner.md")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "FLEET_PROMPT" && v.contains("You are the planner."))
        );
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
            pr: None,
        };
        let env = build_agent_env(&spec, &ctx);
        let keys: std::collections::HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
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
            labels: Vec::new(),
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
            labels: Vec::new(),
        };
        let prefix = bash_issue_env_prefix(Some(&ctx));
        assert!(prefix.contains("FLEET_ISSUE_TITLE='' "), "got: {prefix}");
    }

    fn sample_pr() -> PrContext {
        PrContext {
            number: 42,
            human_id: "pr:42".to_string(),
            title: "Fix CI".to_string(),
            head_ref: "feat/x".to_string(),
            head_sha: "deadbeef".to_string(),
            base_ref: "main".to_string(),
            url: "https://github.com/o/r/pull/42".to_string(),
        }
    }

    #[test]
    fn build_agent_env_appends_pr_vars_when_present() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let pr = sample_pr();
        let ctx = AgentContext {
            pr: Some(&pr),
            ..AgentContext::default()
        };
        let env = build_agent_env(&spec, &ctx);
        let has = |key: &str, val: &str| {
            env.iter().any(|(k, v)| k == key && v == val)
        };
        assert!(has("FLEET_PR_NUMBER", "42"));
        assert!(has("FLEET_PR_HUMAN_ID", "pr:42"));
        assert!(has("FLEET_PR_TITLE", "Fix CI"));
        assert!(has("FLEET_PR_HEAD_REF", "feat/x"));
        assert!(has("FLEET_PR_HEAD_SHA", "deadbeef"));
        assert!(has("FLEET_PR_BASE_REF", "main"));
        assert!(has("FLEET_PR_URL", "https://github.com/o/r/pull/42"));
    }

    #[test]
    fn build_agent_env_omits_pr_block_when_no_pr_bound() {
        let spec = AgentSpec {
            command: vec!["agent".to_string()],
            env_passthrough: Vec::new(),
        };
        let env = build_agent_env(&spec, &AgentContext::default());
        assert!(!env.iter().any(|(k, _)| k.starts_with("FLEET_PR_")));
    }

    #[test]
    fn bash_pr_env_prefix_emits_quoted_block_with_trailing_space() {
        let prefix = bash_pr_env_prefix(Some(&sample_pr()));
        assert!(prefix.contains("FLEET_PR_NUMBER='42' "));
        assert!(prefix.contains("FLEET_PR_HEAD_REF='feat/x' "));
        assert!(prefix.contains("FLEET_PR_HEAD_SHA='deadbeef' "));
        assert!(
            prefix.ends_with(' '),
            "must end with a space for splice chaining; got: {prefix}"
        );
    }

    #[test]
    fn bash_pr_env_prefix_empty_when_unbound() {
        assert_eq!(bash_pr_env_prefix(None), "");
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
            .with(
                eq("sh"),
                eq(vec!["-c".to_string(), expected_cmd.to_string()]),
            )
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert_eq!(session.current_node.as_deref(), Some("only"));
        // Log was captured.
        let log = store.session_dir(&session.id).join("logs").join("only.log");
        assert!(log.is_file(), "expected log at {}", log.display());
    }

    #[test]
    fn execute_stamps_worktree_meta_onto_session() {
        // When the CLI provisions a worktree and passes it as `worktree`,
        // the executor must stamp the path + branch onto the session's
        // meta.json so a subsequent `replay` can read the branch back.
        let yaml = "\
name: with-worktree
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
        let wt_path = std::path::PathBuf::from("/repo/.fleet/sessions/s-wt/worktree");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: &wt_path,
            session_id: SessionId::new("s-wt"),
            issue: None,
            pr: None,
            worktree: Some(WorktreeMeta {
                path: &wt_path,
                branch: "fleet/session-s-wt",
            }),
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.worktree_path.as_deref(), Some(wt_path.as_path()));
        assert_eq!(session.branch.as_deref(), Some("fleet/session-s-wt"));

        // The persisted meta.json must carry both fields too — the
        // prune command and replay both read off disk, not off the
        // in-memory session value.
        let reloaded = store.load(&session.id).unwrap();
        assert_eq!(reloaded.worktree_path.as_deref(), Some(wt_path.as_path()));
        assert_eq!(reloaded.branch.as_deref(), Some("fleet/session-s-wt"));
    }

    #[test]
    fn execute_without_worktree_leaves_session_worktree_fields_none() {
        // Backward compat: the Local adapter and non-git workspaces
        // pass `worktree: None`, and the resulting session has
        // `worktree_path = None` / `branch = None`. Old TUIs / pruners
        // that ignore these fields keep working.
        let yaml = "\
name: no-worktree
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
            session_id: SessionId::new("s-no-wt"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert!(session.worktree_path.is_none());
        assert!(session.branch.is_none());
    }

    /// Stub enforcer that hands back a fixed setup so we can verify
    /// the executor wires `proxy_env` + `network_name` through to the
    /// container start.
    struct StubEnforcer {
        env: Vec<(String, String)>,
        network: Option<String>,
        setup_calls: std::sync::Mutex<u32>,
        teardown_calls: std::sync::Mutex<u32>,
    }

    impl StubEnforcer {
        fn new(env: Vec<(String, String)>, network: Option<String>) -> Self {
            Self {
                env,
                network,
                setup_calls: std::sync::Mutex::new(0),
                teardown_calls: std::sync::Mutex::new(0),
            }
        }
    }

    impl crate::egress::EgressEnforcer for StubEnforcer {
        fn setup(&self, _session_id: &str) -> Result<crate::egress::EgressSetup> {
            *self.setup_calls.lock().unwrap() += 1;
            Ok(crate::egress::EgressSetup {
                proxy_env: self.env.clone(),
                network_name: self.network.clone(),
                proxy_container: None,
                dns_ip: None,
                dns_container: None,
            })
        }
        fn teardown(&self, _setup: &crate::egress::EgressSetup) -> Result<()> {
            *self.teardown_calls.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn agent_container_receives_egress_env_from_enforcer() {
        // The enforcer hands the executor an HTTP_PROXY pair; the
        // local adapter's exec invocation must see those env vars in
        // the agent process's environment. The LocalAdapter writes
        // env into its captured ExecHandle via the invoker's argv
        // sequencing — easier: assert against the captured calls
        // surface on the invoker.
        let yaml = "\
name: with-proxy
nodes:
  - id: only
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        // Snoop on the env args the local adapter hands to the
        // invoker. The local adapter's `start_container` + `exec`
        // both fall through to ProcessInvoker.run; capturing them
        // lets us see what env was actually set on the child.
        let envs_seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let envs_for_mock = Arc::clone(&envs_seen);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            envs_for_mock.lock().unwrap().push(args);
            Ok(String::new())
        });
        let adapter = crate::runtime::local::LocalAdapter::new(Arc::new(mock));

        let executor = executor_returning("");
        let enforcer = StubEnforcer::new(
            vec![
                (
                    "HTTP_PROXY".to_string(),
                    "http://fleet-proxy:8888".to_string(),
                ),
                (
                    "HTTPS_PROXY".to_string(),
                    "http://fleet-proxy:8888".to_string(),
                ),
            ],
            Some("fleet-net".to_string()),
        );
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-egress-env"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &enforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert_eq!(*enforcer.setup_calls.lock().unwrap(), 1);
        assert_eq!(*enforcer.teardown_calls.lock().unwrap(), 1);
        // At least one invoker call sees the proxy env (via env= on
        // the LocalAdapter's child invocation; the local adapter
        // funnels env in argv as KEY=VALUE prefix calls).
        let calls = envs_seen.lock().unwrap().clone();
        let flat: String = calls
            .iter()
            .flat_map(|argv| argv.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        // Local adapter shell-quotes env values; match on a fuzzy
        // pattern so test isn't coupled to the exact escape style.
        assert!(
            flat.contains("HTTP_PROXY=") && flat.contains("fleet-proxy:8888"),
            "HTTP_PROXY not propagated; calls:\n{flat}"
        );
        assert!(
            flat.contains("HTTPS_PROXY="),
            "HTTPS_PROXY not propagated; calls:\n{flat}"
        );
    }

    #[test]
    fn executor_calls_enforcer_setup_and_teardown_around_run() {
        // Even when the enforcer is a no-op variant, the executor
        // must call setup before run and teardown after. Without
        // this guarantee, a real enforcer would never see the
        // session boundary it needs to allocate/reclaim resources.
        let yaml = "\
name: trivial
nodes:
  - id: only
    type: bash
    script: 'echo only'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let enforcer = StubEnforcer::new(vec![], None);
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-lifecycle"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &enforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();
        assert_eq!(*enforcer.setup_calls.lock().unwrap(), 1);
        assert_eq!(*enforcer.teardown_calls.lock().unwrap(), 1);
    }

    #[test]
    fn agent_node_records_parsed_cost_on_session() {
        // The local adapter wraps the invoker's stdout into the exec
        // handle verbatim, so we stage stdout that contains a Claude
        // Code-style cost line and assert it lands on Session.
        let yaml = "\
name: trivial
nodes:
  - id: only
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("working...\nTotal cost (USD): $0.42\n");
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
            session_id: SessionId::new("s-cost"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert_eq!(session.node_costs.get("only"), Some(&0.42));
        assert_eq!(session.total_cost_usd(), Some(0.42));
        // Cost survives the round-trip to disk.
        let loaded = store.load(&SessionId::new("s-cost")).unwrap();
        assert_eq!(loaded.node_costs.get("only"), Some(&0.42));
    }

    #[test]
    fn agent_node_clears_container_marker_after_clean_stop() {
        // After a successful agent run the marker MUST be gone — a
        // dangling marker would make the reaper report a healthy
        // container as leaked.
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
            session_id: SessionId::new("s-clean-marker"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        let active =
            crate::session::containers::list_active(&store.session_dir(&session.id)).unwrap();
        assert!(
            active.is_empty(),
            "marker must be cleared after a successful agent run: {active:?}"
        );
    }

    #[test]
    fn agent_node_without_parseable_cost_leaves_node_costs_empty() {
        // Agent ran fine but printed nothing the parser recognises.
        // Session must NOT show a $0.00 entry — that would
        // misrepresent "didn't parse" as "cost zero".
        let yaml = "\
name: trivial
nodes:
  - id: only
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("nothing to see here\n");
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
            session_id: SessionId::new("s-noparse"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert!(session.node_costs.is_empty());
        assert_eq!(session.total_cost_usd(), None);
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
                eq(vec![
                    "-c".to_string(),
                    "cd '/repo' && echo hello".to_string(),
                ]),
            )
            .returning(|_, _| Ok("hello\n".to_string()));
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-bash"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
    fn driver_pid_is_set_during_run_and_cleared_on_completion() {
        // The reaper relies on driver_pid being non-None for a Running
        // session and None for terminal states. Verify the lifecycle by
        // sampling meta.json from disk during a bash node, then again
        // after execute() returns.
        let yaml = "\
name: trivial-bash
nodes:
  - id: only
    type: bash
    script: 'true'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-pid");

        let snapshot: Arc<std::sync::Mutex<Option<Session>>> =
            Arc::new(std::sync::Mutex::new(None));
        let snap_writer = Arc::clone(&snapshot);
        let store_for_mock = store.clone();
        let sid_for_mock = session_id.clone();
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            // Load the persisted session at the moment the bash node
            // fires. By construction this is mid-run, so driver_pid
            // must be Some(this process's pid).
            let s = store_for_mock.load(&sid_for_mock).unwrap();
            *snap_writer.lock().unwrap() = Some(s);
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let final_session = executor.execute(&req).unwrap();

        let mid = snapshot
            .lock()
            .unwrap()
            .clone()
            .expect("mid-run snapshot missing");
        assert_eq!(mid.state, SessionState::Running);
        assert_eq!(
            mid.driver_pid,
            Some(std::process::id()),
            "driver_pid must be stamped on disk while the session is running"
        );

        assert_eq!(final_session.state, SessionState::Completed);
        assert!(
            final_session.driver_pid.is_none(),
            "driver_pid must clear on Completed"
        );
        // …and the on-disk session agrees with the in-memory one.
        let loaded = store.load(&session_id).unwrap();
        assert!(loaded.driver_pid.is_none());
    }

    #[test]
    fn driver_pid_cleared_when_workflow_pauses_at_gate() {
        // AwaitingGate is *not* a state the reaper should touch. By
        // clearing driver_pid when the executor parks the session, the
        // reaper sees `driver_pid == None && state == AwaitingGate` and
        // knows to leave it alone.
        let yaml = "\
name: with-gate
nodes:
  - id: g
    type: gate
    summary: 'pause'
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
            session_id: SessionId::new("s-gate-pid"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::AwaitingGate);
        assert!(session.driver_pid.is_none());
        let loaded = store.load(&session.id).unwrap();
        assert!(loaded.driver_pid.is_none());
    }

    #[test]
    fn driver_pid_cleared_when_workflow_fails() {
        // Failed is terminal; the reaper must see no stale pid claim.
        let yaml = "\
name: bash-fails
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
            session_id: SessionId::new("s-fail-pid"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let _ = executor.execute(&req).unwrap_err();
        let loaded = store.load(&SessionId::new("s-fail-pid")).unwrap();
        assert_eq!(loaded.state, SessionState::Failed);
        assert!(loaded.driver_pid.is_none());
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
    fn resume_preserves_worktree_fields_on_loaded_session() {
        // The CLI points workspace at session.worktree_path on resume;
        // the executor must not clobber the persisted fields. Here we
        // exercise the full execute → gate → resume cycle with a
        // worktree stamped on the initial run and assert it survives.
        let yaml = "\
name: gate-with-wt
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
        let wt = std::path::PathBuf::from("/repo/.fleet/sessions/s-resume-wt/worktree");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: &wt,
            session_id: SessionId::new("s-resume-wt"),
            issue: None,
            pr: None,
            worktree: Some(WorktreeMeta {
                path: &wt,
                branch: "fleet/session-s-resume-wt",
            }),
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let paused = executor.execute(&req).unwrap();
        assert_eq!(paused.state, SessionState::AwaitingGate);
        assert_eq!(paused.worktree_path.as_deref(), Some(wt.as_path()));

        // Resume — CLI would pass `worktree: None` here (the fields
        // are loaded from disk). The persisted worktree must survive.
        let resume_req = ExecuteRequest {
            worktree: None,
            ..req
        };
        let resumed = executor.resume(&resume_req).unwrap();
        assert_eq!(resumed.state, SessionState::Completed);
        assert_eq!(
            resumed.worktree_path.as_deref(),
            Some(wt.as_path()),
            "resume must preserve the persisted worktree path"
        );
        assert_eq!(resumed.branch.as_deref(), Some("fleet/session-s-resume-wt"),);
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.resume(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("loading session `s-ghost`"),
            "got: {err:#}"
        );
    }

    // === replay ===

    /// Stage a "completed src run" on disk by directly creating a
    /// session row in the store and writing a fake artifact. Lets the
    /// replay tests skip the cost of actually running a workflow to
    /// produce the source state.
    fn stage_src_session(
        store: &SessionStore,
        id: &str,
        workflow: &str,
        artifacts: &[(&str, &str)],
    ) -> SessionId {
        let sid = SessionId::new(id);
        let session = Session::new(sid.clone(), workflow, 1);
        store.create(&session).unwrap();
        let artifacts_dir = store.session_dir(&sid).join("artifacts");
        for (name, body) in artifacts {
            std::fs::write(artifacts_dir.join(name), body).unwrap();
        }
        sid
    }

    #[test]
    fn replay_copies_artifacts_and_runs_only_the_rerun_from_node() {
        // Two-node workflow: `plan` then `review`. Replay starts from
        // `review`, so the bash invoker should fire exactly once — for
        // the rerun node — and `review`'s log should exist on the new
        // session while `plan`'s log should not (its bash never ran on
        // the new session).
        let yaml = "\
name: replay-basic
nodes:
  - id: plan
    type: bash
    script: 'echo plan'
  - id: review
    depends_on: [plan]
    type: bash
    script: 'echo review'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        // Stage a fake src session with a pre-existing `plan.md`.
        let src_id = stage_src_session(
            &store,
            "s-src",
            "replay-basic",
            &[("plan.md", "from the src run")],
        );

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            calls_for_mock.lock().unwrap().push(s);
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
            session_id: SessionId::new("s-replay"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let replayed = executor.replay(&req, &src_id, "review").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
        assert_eq!(replayed.current_node.as_deref(), Some("review"));
        // The plan node was skipped (start_idx = 1), so the only bash
        // call should be the review one.
        let bashed = calls.lock().unwrap().clone();
        let review_count = bashed.iter().filter(|s| s.contains("echo review")).count();
        let plan_count = bashed.iter().filter(|s| s.contains("echo plan")).count();
        assert_eq!(review_count, 1, "review ran once; got: {bashed:?}");
        assert_eq!(plan_count, 0, "plan was skipped; got: {bashed:?}");
        // Src artifact carried over to the new session.
        let copied = store.session_dir(&replayed.id).join("artifacts/plan.md");
        assert!(copied.is_file(), "src artifact should be staged");
        let body = std::fs::read_to_string(&copied).unwrap();
        assert_eq!(body, "from the src run");
        // review's log exists on the new session.
        assert!(
            store
                .session_dir(&replayed.id)
                .join("logs/review.log")
                .exists()
        );
    }

    #[test]
    fn replay_only_fires_exactly_one_node_then_exits() {
        // Three-node workflow: plan → mid → end. Replay-only on `mid`
        // should fire ONLY the mid node — plan is upstream of the
        // rerun-from index (skipped, no log), end is downstream (would
        // run under --rerun-from, must NOT run under --rerun-only).
        let yaml = "\
name: replay-only-three
nodes:
  - id: plan
    type: bash
    script: 'echo plan'
  - id: mid
    depends_on: [plan]
    type: bash
    script: 'echo mid'
  - id: end
    depends_on: [mid]
    type: bash
    script: 'echo end'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src-only", "replay-only-three", &[]);

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            calls_for_mock.lock().unwrap().push(s);
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
            session_id: SessionId::new("s-only"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let replayed = executor.replay_only(&req, &src_id, "mid").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
        assert_eq!(replayed.current_node.as_deref(), Some("mid"));

        let scripts = calls.lock().unwrap().clone();
        let plan_count = scripts.iter().filter(|s| s.contains("echo plan")).count();
        let mid_count = scripts.iter().filter(|s| s.contains("echo mid")).count();
        let end_count = scripts.iter().filter(|s| s.contains("echo end")).count();
        assert_eq!(plan_count, 0, "upstream node must be skipped");
        assert_eq!(mid_count, 1, "the chosen node must fire exactly once");
        assert_eq!(
            end_count, 0,
            "downstream nodes must NOT fire under --rerun-only"
        );

        // Log file for `end` should not exist on the new session.
        let end_log = store.session_dir(&replayed.id).join("logs/end.log");
        assert!(!end_log.exists(), "end's log must not be created");
    }

    #[test]
    fn replay_only_ignores_loop_back_on_chosen_node() {
        // A node with loop_back_to under regular `replay` would cycle.
        // Under --rerun-only the cycle must NOT trigger — the run
        // completes after the single node body.
        let yaml = "\
name: replay-only-loop
nodes:
  - id: review
    type: bash
    script: 'echo review'
  - id: revise
    depends_on: [review]
    type: bash
    script: 'echo revise'
    loop_back_to: review
    max_loops: 5
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src-loop", "replay-only-loop", &[]);

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            calls_for_mock.lock().unwrap().push(s);
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
            session_id: SessionId::new("s-loop"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let replayed = executor.replay_only(&req, &src_id, "revise").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
        let scripts = calls.lock().unwrap().clone();
        let revise_count = scripts.iter().filter(|s| s.contains("echo revise")).count();
        let review_count = scripts.iter().filter(|s| s.contains("echo review")).count();
        assert_eq!(revise_count, 1, "revise fires once, then exit");
        assert_eq!(
            review_count, 0,
            "loop_back_to::review must NOT trigger under --rerun-only"
        );
        assert!(
            replayed
                .loop_counts
                .get("revise")
                .copied()
                .unwrap_or_default()
                == 0,
            "loop counters must not advance under --rerun-only",
        );
    }

    #[test]
    fn replay_only_rejects_unknown_node() {
        // Same validation path as replay() — a bogus node id surfaces
        // the same topological-order error.
        let yaml = "\
name: only-bad
nodes:
  - id: only
    type: bash
    script: 'echo only'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src-bad", "only-bad", &[]);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Ok(String::new()));
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-bad"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor
            .replay_only(&req, &src_id, "nonexistent")
            .unwrap_err();
        assert!(format!("{err:#}").contains("nonexistent"));
    }

    #[test]
    fn replay_stamps_worktree_meta_onto_new_session() {
        // Mirror execute()'s stamp: when the CLI passes a worktree
        // (basing off the src session's branch tip), the executor
        // must persist the new session's path + branch onto its own
        // meta.json. This is what makes replay-of-replay work.
        let yaml = "\
name: replay-worktree
nodes:
  - id: plan
    type: bash
    script: 'echo plan'
  - id: review
    depends_on: [plan]
    type: bash
    script: 'echo review'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src-wt", "replay-worktree", &[]);

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Ok(String::new()));
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());
        let new_wt = std::path::PathBuf::from("/repo/.fleet/sessions/s-replay-wt/worktree");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: &new_wt,
            session_id: SessionId::new("s-replay-wt"),
            issue: None,
            pr: None,
            worktree: Some(WorktreeMeta {
                path: &new_wt,
                branch: "fleet/session-s-replay-wt",
            }),
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let replayed = executor.replay(&req, &src_id, "review").unwrap();
        assert_eq!(replayed.worktree_path.as_deref(), Some(new_wt.as_path()));
        assert_eq!(
            replayed.branch.as_deref(),
            Some("fleet/session-s-replay-wt")
        );
        // Persistence too — the prune command reads off disk.
        let reloaded = store.load(&replayed.id).unwrap();
        assert_eq!(reloaded.worktree_path.as_deref(), Some(new_wt.as_path()));
    }

    #[test]
    fn replay_rejects_unknown_rerun_from() {
        let yaml = "\
name: replay-bad-target
nodes:
  - id: only
    type: bash
    script: 'echo only'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src", "replay-bad-target", &[]);

        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-replay-bad"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let err = executor.replay(&req, &src_id, "nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("`nope`"), "got: {msg}");
        assert!(
            msg.contains("not in workflow `replay-bad-target`"),
            "got: {msg}"
        );
        // Failed before minting; no new session directory exists.
        assert!(!store.session_dir(&SessionId::new("s-replay-bad")).exists());
    }

    #[test]
    fn replay_rejects_missing_src_session() {
        let yaml = "\
name: replay-no-src
nodes:
  - id: only
    type: bash
    script: 'echo only'
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
            session_id: SessionId::new("s-replay-no-src"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor
            .replay(&req, &SessionId::new("s-does-not-exist"), "only")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("loading source session `s-does-not-exist`"),
            "got: {msg}"
        );
        // Failed before minting; no new session directory exists.
        assert!(
            !store
                .session_dir(&SessionId::new("s-replay-no-src"))
                .exists()
        );
    }

    #[test]
    fn replay_rerun_from_first_node_replays_full_workflow() {
        // rerun_from == first node is the "expensive fresh run with
        // pre-staged artifacts" mode. The whole workflow should run
        // (both bash calls) and any staged src artifact still gets
        // copied so the new session sees the same starting state.
        let yaml = "\
name: replay-from-top
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: b
    depends_on: [a]
    type: bash
    script: 'echo b'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(
            &store,
            "s-src",
            "replay-from-top",
            &[("staged.txt", "carried")],
        );

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            calls_for_mock.lock().unwrap().push(s);
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
            session_id: SessionId::new("s-replay-top"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        let replayed = executor.replay(&req, &src_id, "a").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
        let bashed = calls.lock().unwrap().clone();
        let a_count = bashed.iter().filter(|s| s.contains("echo a")).count();
        let b_count = bashed.iter().filter(|s| s.contains("echo b")).count();
        assert_eq!(a_count, 1, "a ran once; got: {bashed:?}");
        assert_eq!(b_count, 1, "b ran once; got: {bashed:?}");
        let staged = store.session_dir(&replayed.id).join("artifacts/staged.txt");
        assert!(staged.is_file(), "staged artifact carried over");
    }

    #[test]
    fn replay_handles_missing_src_artifacts_dir() {
        // If a src session never produced an artifacts/ dir (e.g. it
        // crashed before any agent node), replay should still mint
        // the new session and run cleanly — there's just nothing to
        // copy.
        let yaml = "\
name: replay-empty
nodes:
  - id: only
    type: bash
    script: 'echo only'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let src_id = stage_src_session(&store, "s-src", "replay-empty", &[]);
        // Remove the artifacts/ subdir to simulate a crashed src run.
        std::fs::remove_dir_all(store.session_dir(&src_id).join("artifacts")).unwrap();

        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-replay-empty"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let replayed = executor.replay(&req, &src_id, "only").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
    }

    // === outputs persistence ===

    #[test]
    fn outputs_to_and_from_persisted_round_trip() {
        let mut flat: OutputMap = HashMap::new();
        flat.insert(
            ("plan".to_string(), "decision".to_string()),
            "yes".to_string(),
        );
        flat.insert(("plan".to_string(), "score".to_string()), "5".to_string());
        flat.insert(
            ("review".to_string(), "decision".to_string()),
            "no".to_string(),
        );
        let nested = outputs_to_persisted(&flat);
        assert_eq!(
            nested
                .get("plan")
                .unwrap()
                .get("decision")
                .map(String::as_str),
            Some("yes")
        );
        let back = outputs_from_persisted(&nested);
        assert_eq!(back, flat);
    }

    #[test]
    fn outputs_from_persisted_handles_empty() {
        let empty: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        assert!(outputs_from_persisted(&empty).is_empty());
    }

    #[test]
    fn outputs_survive_resume_across_a_gate() {
        // The key correctness check: a workflow that gates between an
        // output-producing node and a `when:`-gated consumer must see
        // the upstream decision after resume. Pre-persistence this
        // test would have failed — the consumer would have skipped.
        let yaml = "\
name: outputs-across-gate
nodes:
  - id: decide
    type: bash
    script: 'true'
    outputs:
      decision: pick
  - id: g
    depends_on: [decide]
    type: gate
    summary: 'pause for human'
  - id: act
    depends_on: [g]
    when: 'decide.decision == \"go\"'
    type: bash
    script: 'true'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-gate-outputs"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };

        // The `decide` bash node "produces" its declared outputs as a
        // side effect of being invoked: the mocked invoker writes the
        // JSON file into the session's artifacts dir before returning,
        // modelling the real shape where a node drops its outputs
        // file alongside whatever else it does.
        let store_ref = store.clone();
        let target_id = req.session_id.clone();
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            let path = store_ref
                .session_dir(&target_id)
                .join("artifacts/decide.outputs.json");
            if !path.exists() {
                std::fs::write(&path, br#"{"pick": "go"}"#).unwrap();
            }
            Ok(String::new())
        });
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());

        // First execute pauses at the gate; the persisted outputs
        // map must already carry `decide.decision` before the gate.
        let paused = executor.execute(&req).unwrap();
        assert_eq!(paused.state, SessionState::AwaitingGate);
        assert_eq!(
            paused
                .outputs
                .get("decide")
                .and_then(|m| m.get("decision"))
                .map(String::as_str),
            Some("go"),
            "outputs must be persisted onto the session before the gate"
        );

        // Resume: the `act` node's `when:` must evaluate against the
        // persisted upstream decision. Pre-persistence the predicate
        // would have seen the default and skipped; with persistence
        // the node fires and the workflow completes.
        let resumed = executor.resume(&req).unwrap();
        assert_eq!(resumed.state, SessionState::Completed);
        assert_eq!(resumed.current_node.as_deref(), Some("act"));
        assert!(store.session_dir(&resumed.id).join("logs/act.log").exists());
    }

    #[test]
    fn outputs_carry_forward_into_replay_from_src() {
        // Replay starting at a `when:`-gated node must see the src
        // session's persisted outputs. Stage a src session by hand
        // with a populated `outputs` map and a matching artifacts
        // file (so `verify_inputs` is happy), then replay against
        // it.
        let yaml = "\
name: replay-outputs
nodes:
  - id: decide
    type: bash
    script: 'true'
    outputs:
      decision: pick
  - id: act
    depends_on: [decide]
    when: 'decide.decision == \"go\"'
    type: bash
    script: 'true'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");

        // Stage the src session with persisted outputs as if it
        // had completed the `decide` node.
        let src_id = SessionId::new("s-src-with-outputs");
        let mut src = Session::new(src_id.clone(), "replay-outputs", 1);
        src.outputs.insert(
            "decide".to_string(),
            std::iter::once(("decision".to_string(), "go".to_string())).collect(),
        );
        store.create(&src).unwrap();

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-replay-with-outputs"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let replayed = executor.replay(&req, &src_id, "act").unwrap();
        assert_eq!(replayed.state, SessionState::Completed);
        // act fired — its log exists. Pre-persistence the when:
        // predicate would have skipped this node.
        assert!(
            store
                .session_dir(&replayed.id)
                .join("logs/act.log")
                .exists()
        );
        // The carried-forward outputs are present on the new session.
        assert_eq!(
            replayed
                .outputs
                .get("decide")
                .and_then(|m| m.get("decision"))
                .map(String::as_str),
            Some("go"),
        );
    }

    // === fanout ===

    #[test]
    fn topological_order_excludes_fanout_siblings() {
        // The fanout itself stays in the schedule; its siblings are
        // owned by the fanout and don't appear in the main order.
        let yaml = "\
name: with-fanout
nodes:
  - id: lint
    type: bash
    script: 'lint'
  - id: typecheck
    type: bash
    script: 'tc'
  - id: parallel-checks
    type: fanout
    siblings: [lint, typecheck]
  - id: merge
    depends_on: [parallel-checks]
    type: bash
    script: 'merge'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let order = topological_order(&wf).unwrap();
        assert_eq!(
            order,
            vec!["parallel-checks".to_string(), "merge".to_string()],
            "siblings should be excluded; fanout + downstream remain"
        );
    }

    #[test]
    fn fanout_runs_all_siblings_and_workflow_completes() {
        // Each sibling fires exactly once; the fanout node itself runs
        // last (in topo order) and gets a `--- fanout ok ---` log.
        let yaml = "\
name: two-siblings
nodes:
  - id: lint
    type: bash
    script: 'lint'
  - id: typecheck
    type: bash
    script: 'tc'
  - id: checks
    type: fanout
    siblings: [lint, typecheck]
";
        let (session, log) = run_with_call_counter(yaml, "s-fanout-ok");
        assert_eq!(session.state, SessionState::Completed);
        let scripts = log.lock().unwrap().clone();
        // Order between lint/tc isn't deterministic (parallel) but
        // each should appear exactly once.
        let lint_count = scripts.iter().filter(|s| s.contains("lint")).count();
        let tc_count = scripts.iter().filter(|s| s.contains("tc")).count();
        assert_eq!(lint_count, 1, "lint ran once; got scripts: {scripts:?}");
        assert_eq!(tc_count, 1, "typecheck ran once; got scripts: {scripts:?}");
    }

    #[test]
    fn fanout_with_one_failing_sibling_fails_the_workflow() {
        // Mock invoker that errors when it sees "boom" in the script.
        let yaml = "\
name: one-bad
nodes:
  - id: ok
    type: bash
    script: 'echo ok'
  - id: boom
    type: bash
    script: 'echo boom'
  - id: checks
    type: fanout
    siblings: [ok, boom]
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("echo boom") {
                Err(anyhow!("boom: synthetic failure"))
            } else {
                Ok(String::new())
            }
        });
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-fanout-fail"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fanout node `checks` had 1 sibling failure"),
            "got: {msg}"
        );
        assert!(msg.contains("boom"), "got: {msg}");
        // Session is Failed; the fanout node's log captures the failure.
        let loaded = store.load(&SessionId::new("s-fanout-fail")).unwrap();
        assert_eq!(loaded.state, SessionState::Failed);
        let fanout_log =
            std::fs::read_to_string(store.session_dir(&loaded.id).join("logs/checks.log")).unwrap();
        assert!(
            fanout_log.contains("--- fanout failed:"),
            "got: {fanout_log}"
        );
        assert!(fanout_log.contains("boom:"), "got: {fanout_log}");
    }

    #[test]
    fn fanout_sibling_outputs_flow_into_downstream_when() {
        // A sibling writes an outputs.json; a downstream node gates on
        // `<sibling>.<key>`. Exercises the post-fanout extract path.
        let yaml = "\
name: outputs-from-fanout
nodes:
  - id: discover
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: also
    type: bash
    script: 'no-op'
  - id: checks
    type: fanout
    siblings: [discover, also]
  - id: open_pr
    depends_on: [checks]
    when: 'discover.decision == \"approve\"'
    type: bash
    script: 'echo open'
  - id: revise
    depends_on: [checks]
    when: 'discover.decision == \"changes_requested\"'
    type: bash
    script: 'echo revise'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-fan-when");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(
                    af.join("discover.outputs.json"),
                    r#"{"decision":"approve"}"#,
                )
                .unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // open_pr ran (no skip marker); revise was skipped.
        let open_log =
            std::fs::read_to_string(store.session_dir(&session_id).join("logs/open_pr.log"))
                .unwrap();
        assert!(!open_log.contains("--- skipped"), "got: {open_log}");
        let revise_log =
            std::fs::read_to_string(store.session_dir(&session_id).join("logs/revise.log"))
                .unwrap();
        assert!(revise_log.contains("--- skipped:"), "got: {revise_log}");
    }

    #[test]
    fn fanout_rejects_gate_sibling() {
        let yaml = "\
name: bad-fanout
nodes:
  - id: a
    type: bash
    script: 'echo a'
  - id: g
    type: gate
    summary: 'why is this here'
  - id: checks
    type: fanout
    siblings: [a, g]
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
            session_id: SessionId::new("s-fan-gate"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("gate/fanout inside a fanout is not supported"),
            "got: {err:#}"
        );
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
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
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
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
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
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

    // === outputs extraction + `when:` predicate evaluation ===

    /// Build a node with the same shape as [`agent_node`] but with an
    /// `outputs:` map declared. The test driver writes the
    /// corresponding JSON file into the artifacts dir.
    fn agent_node_with_outputs(id: &str, outputs: &[(&str, &str)]) -> Node {
        let mut node = agent_node(id, &[], &[]);
        node.outputs = outputs
            .iter()
            .map(|(local, source)| ((*local).to_string(), (*source).to_string()))
            .collect();
        node
    }

    #[test]
    fn extract_outputs_no_op_when_node_declares_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node("review", &[], &[]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        assert!(acc.is_empty());
    }

    #[test]
    fn extract_outputs_reads_flat_keys_into_acc() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("review.outputs.json"),
            r#"{"decision":"approve","summary":"lgtm"}"#,
        )
        .unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node =
            agent_node_with_outputs("review", &[("decision", "decision"), ("note", "summary")]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        assert_eq!(
            acc.get(&("review".to_string(), "decision".to_string())),
            Some(&"approve".to_string())
        );
        assert_eq!(
            acc.get(&("review".to_string(), "note".to_string())),
            Some(&"lgtm".to_string())
        );
    }

    #[test]
    fn extract_outputs_coerces_scalar_types_to_strings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("n.outputs.json"),
            r#"{"flag":true,"count":3}"#,
        )
        .unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("n", &[("flag", "flag"), ("count", "count")]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        assert_eq!(
            acc.get(&("n".to_string(), "flag".to_string())),
            Some(&"true".to_string())
        );
        assert_eq!(
            acc.get(&("n".to_string(), "count".to_string())),
            Some(&"3".to_string())
        );
    }

    #[test]
    fn extract_outputs_errors_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("review", &[("decision", "decision")]);
        let err = extract_outputs(tmp.path(), &node, &mut acc).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("declares outputs but cannot read"),
            "got: {msg}"
        );
        assert!(msg.contains("review.outputs.json"), "got: {msg}");
    }

    #[test]
    fn extract_outputs_errors_when_referenced_key_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("review.outputs.json"),
            r#"{"summary":"lgtm"}"#,
        )
        .unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("review", &[("decision", "decision")]);
        let err = extract_outputs(tmp.path(), &node, &mut acc).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing key `decision`"), "got: {msg}");
    }

    #[test]
    fn extract_outputs_errors_when_top_level_is_not_an_object() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("n.outputs.json"), r#"["a","b"]"#).unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("n", &[("x", "x")]);
        let err = extract_outputs(tmp.path(), &node, &mut acc).unwrap_err();
        assert!(format!("{err:#}").contains("top-level object"));
    }

    #[test]
    fn extract_outputs_json_encodes_object_values() {
        // Objects under a declared output key are JSON-encoded into a
        // single string so downstream nodes (the `tracker-create`
        // node consuming `recommend_ticket`) can plug the structured
        // payload back out. Pre-Phase-2 fleet would have rejected
        // this as non-scalar; the relaxation is intentional.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("n.outputs.json"),
            r#"{"x":{"title":"hi","body":"there"}}"#,
        )
        .unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("n", &[("x", "x")]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        let v = acc.get(&("n".to_string(), "x".to_string())).unwrap();
        // serde_json::to_string preserves object-key order for the
        // same input shape; assert on the round-trip rather than the
        // exact string so we don't depend on serde's iteration order.
        let round: serde_json::Value = serde_json::from_str(v).unwrap();
        assert_eq!(round["title"], "hi");
        assert_eq!(round["body"], "there");
    }

    #[test]
    fn extract_outputs_json_encodes_array_values() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("n.outputs.json"),
            r#"{"labels":["a","b","c"]}"#,
        )
        .unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("n", &[("labels", "labels")]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        let v = acc.get(&("n".to_string(), "labels".to_string())).unwrap();
        assert_eq!(v, r#"["a","b","c"]"#);
    }

    #[test]
    fn extract_outputs_maps_null_values_to_empty_string() {
        // Null is the convention for "this optional output doesn't
        // apply to this run" — the orchestrator's `recommend_ticket`
        // is the canonical example. The OutputMap entry is the empty
        // string so a downstream `when:` can gate on `!= ""`.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("n.outputs.json"), r#"{"x":null}"#).unwrap();
        let mut acc: OutputMap = HashMap::new();
        let node = agent_node_with_outputs("n", &[("x", "x")]);
        extract_outputs(tmp.path(), &node, &mut acc).unwrap();
        let v = acc.get(&("n".to_string(), "x".to_string())).unwrap();
        assert_eq!(v, "");
    }

    #[test]
    fn when_false_skips_node_and_records_skip_log() {
        // First a bash node writes a review.outputs.json (the simulated
        // reviewer's verdict), then the executor reaches the
        // `when:-approve` branch, which must run, and the
        // `when:-changes_requested` branch, which must skip.
        let yaml = "\
name: fork
nodes:
  - id: review
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == \"approve\"'
    type: bash
    script: 'echo open_pr'
  - id: revise
    depends_on: [review]
    when: 'review.decision == \"changes_requested\"'
    type: bash
    script: 'echo revise'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-fork-approve");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            // The review script side-effects the outputs file; later
            // scripts are no-ops.
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(af.join("review.outputs.json"), r#"{"decision":"approve"}"#)
                    .unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);

        // open_pr ran (its log is the bash stdout, empty here).
        let open_pr_log = store.session_dir(&session_id).join("logs/open_pr.log");
        assert!(open_pr_log.is_file());
        let open_pr_body = std::fs::read_to_string(&open_pr_log).unwrap();
        assert!(
            !open_pr_body.contains("--- skipped"),
            "open_pr should have run; got log: {open_pr_body}"
        );

        // revise skipped — log present with skip marker.
        let revise_log = store.session_dir(&session_id).join("logs/revise.log");
        let revise_body = std::fs::read_to_string(&revise_log).unwrap();
        assert!(
            revise_body.contains("--- skipped:") && revise_body.contains("review.decision"),
            "expected skip marker, got: {revise_body}"
        );
    }

    #[test]
    fn when_true_runs_node_and_when_false_sibling_is_skipped_other_decision() {
        // Symmetric to the approve case: decision=changes_requested →
        // revise runs, open_pr skipped.
        let yaml = "\
name: fork
nodes:
  - id: review
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == \"approve\"'
    type: bash
    script: 'echo open_pr'
  - id: revise
    depends_on: [review]
    when: 'review.decision == \"changes_requested\"'
    type: bash
    script: 'echo revise'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-fork-changes");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(
                    af.join("review.outputs.json"),
                    r#"{"decision":"changes_requested"}"#,
                )
                .unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);

        let open_pr_log = store.session_dir(&session_id).join("logs/open_pr.log");
        let body = std::fs::read_to_string(&open_pr_log).unwrap();
        assert!(body.contains("--- skipped:"), "got: {body}");

        let revise_log = store.session_dir(&session_id).join("logs/revise.log");
        let revise_body = std::fs::read_to_string(&revise_log).unwrap();
        assert!(
            !revise_body.contains("--- skipped"),
            "revise should have run, got: {revise_body}"
        );
    }

    #[test]
    fn when_evaluation_error_fails_the_session_with_clear_pointer() {
        // `review` declares no outputs, so the downstream `when:`
        // reference is an unknown-output error. The whole session
        // ends Failed with an actionable pointer.
        let yaml = "\
name: bad-fork
nodes:
  - id: review
    type: bash
    script: 'echo r'
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == \"approve\"'
    type: bash
    script: 'echo p'
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
            session_id: SessionId::new("s-bad-fork"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("evaluating `when:` for node `open_pr`"),
            "got: {msg}"
        );
        assert!(
            msg.contains("unknown output `review.decision`"),
            "got: {msg}"
        );
        assert_eq!(
            store.load(&SessionId::new("s-bad-fork")).unwrap().state,
            SessionState::Failed
        );
    }

    #[test]
    fn when_skip_does_not_fire_loop_back_to() {
        // A skipped node with `loop_back_to` must not loop — the skip
        // is final. Otherwise a gated revise step would still re-enter
        // its target despite being filtered out.
        let yaml = "\
name: skip-loop
nodes:
  - id: setup
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: revise
    depends_on: [setup]
    when: 'setup.decision == \"changes_requested\"'
    type: bash
    script: 'echo revise'
    loop_back_to: setup
    max_loops: 5
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-skip-loop");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        let setup_calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let setup_calls2 = Arc::clone(&setup_calls);
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                setup_calls2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::fs::write(af.join("setup.outputs.json"), r#"{"decision":"approve"}"#).unwrap();
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // setup ran exactly once — the would-be loop never fired
        // because revise was filtered out by its `when:`.
        assert_eq!(
            setup_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "skipped revise must not loop back to setup"
        );
    }

    #[test]
    fn outputs_refresh_across_loop_back_iterations() {
        // setup writes outputs, then a revise node loops back. On the
        // second pass the outputs file changes; the third-pass `when:`
        // sees the fresh decision and exits. Verifies the executor
        // re-runs `extract_outputs` after each pass instead of caching
        // the first one.
        let yaml = "\
name: refresh
nodes:
  - id: setup
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: revise
    depends_on: [setup]
    when: 'setup.decision == \"changes_requested\"'
    type: bash
    script: 'echo revise'
    loop_back_to: setup
    max_loops: 3
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-refresh");
        let af = store.session_dir(&session_id).join("artifacts");
        // First two setup runs: changes_requested (drives a loop). Third
        // run: approve. Fourth run shouldn't happen.
        let setup_calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let setup_calls2 = Arc::clone(&setup_calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                let n = setup_calls2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let decision = if n < 2 {
                    "changes_requested"
                } else {
                    "approve"
                };
                std::fs::write(
                    af.join("setup.outputs.json"),
                    format!(r#"{{"decision":"{decision}"}}"#),
                )
                .unwrap();
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // setup ran 3 times: pass1 (changes_requested) → loop → pass2
        // (changes_requested) → loop → pass3 (approve) → revise
        // skipped → done.
        assert_eq!(
            setup_calls.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "expected three setup runs across two revision cycles"
        );
    }

    // === assert nodes ===

    #[test]
    fn assert_node_with_true_expression_passes_and_workflow_completes() {
        // setup writes a decision=approve outputs file; the assert node
        // then verifies it. True → no-op, workflow runs to Completed.
        let yaml = "\
name: with-assert
nodes:
  - id: setup
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: check
    depends_on: [setup]
    type: assert
    expr: 'setup.decision == \"approve\"'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-assert-ok");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(af.join("setup.outputs.json"), r#"{"decision":"approve"}"#).unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // The assert node leaves a "passed" log so the trail is auditable.
        let log =
            std::fs::read_to_string(store.session_dir(&session_id).join("logs/check.log")).unwrap();
        assert!(log.contains("--- assert passed ---"), "got: {log}");
    }

    #[test]
    fn assert_node_with_false_expression_fails_session() {
        let yaml = "\
name: with-failing-assert
nodes:
  - id: setup
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: check
    depends_on: [setup]
    type: assert
    expr: 'setup.decision == \"approve\"'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-assert-fail");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(
                    af.join("setup.outputs.json"),
                    r#"{"decision":"changes_requested"}"#,
                )
                .unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("assert node `check` failed") && msg.contains("setup.decision"),
            "got: {msg}"
        );
        // Session ends Failed and the log captures the failing expression.
        assert_eq!(store.load(&session_id).unwrap().state, SessionState::Failed);
        let log =
            std::fs::read_to_string(store.session_dir(&session_id).join("logs/check.log")).unwrap();
        assert!(log.contains("--- assert failed ---"), "got: {log}");
    }

    #[test]
    fn assert_node_with_unknown_output_errors_with_pointer() {
        // No upstream node declares a `decision` output. The assert
        // node's reference to `setup.decision` must fail loudly rather
        // than silently passing or quietly returning false.
        let yaml = "\
name: bad-assert
nodes:
  - id: setup
    type: bash
    script: 'echo setup'
  - id: check
    depends_on: [setup]
    type: assert
    expr: 'setup.decision == \"approve\"'
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
            session_id: SessionId::new("s-assert-unknown"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("evaluating `expr:` for assert node `check`"),
            "got: {msg}"
        );
        assert!(
            msg.contains("unknown output `setup.decision`"),
            "got: {msg}"
        );
        assert_eq!(
            store
                .load(&SessionId::new("s-assert-unknown"))
                .unwrap()
                .state,
            SessionState::Failed
        );
    }

    // === resume restoration: issue + loop_counts persist ===

    #[test]
    fn loop_counts_persist_into_session_meta_after_gate_pause() {
        // a → b (loops back to a, max 1) → g (gate) → z. Pre-fix:
        // the in-memory `loop_counts` HashMap was rebuilt empty in
        // `run_loop` on resume, so a workflow that paused *after*
        // a loop_back could re-enter the cycle. Post-fix: counts
        // live on the persisted session, so this test verifies the
        // counter ends up in `session.loop_counts`.
        let yaml = "\
name: loop-then-gate
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
  - id: g
    depends_on: [b]
    type: gate
    summary: 'pause after the loop'
  - id: z
    depends_on: [g]
    type: bash
    script: 'echo z'
";
        let (session, _log) = run_with_call_counter(yaml, "s-loop-gate");
        assert_eq!(session.state, SessionState::AwaitingGate);
        assert_eq!(
            session.loop_counts.get("b"),
            Some(&1),
            "execute should have fired the loop once and persisted the counter"
        );
    }

    #[test]
    fn resume_does_not_re_enter_already_exhausted_loop() {
        // Drives the persistence end-to-end: a workflow that hits the
        // gate after its loop budget is spent must not re-enter the
        // loop on resume. Counts the calls so an off-by-one resume
        // bug would surface.
        let yaml = "\
name: loop-then-gate
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
  - id: g
    depends_on: [b]
    type: gate
    summary: 'pause after the loop'
  - id: z
    depends_on: [g]
    type: bash
    script: 'echo z'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            calls2.lock().unwrap().push(s);
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
            session_id: SessionId::new("s-loop-resume"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let paused = executor.execute(&req).unwrap();
        assert_eq!(paused.state, SessionState::AwaitingGate);
        assert_eq!(paused.loop_counts.get("b"), Some(&1));
        let pre_resume_calls = calls.lock().unwrap().len();
        // a, b, a (post-loop), b, then gate → 4 bash invocations.
        assert_eq!(pre_resume_calls, 4, "expected a,b,a,b before gate");

        let resumed = executor.resume(&req).unwrap();
        assert_eq!(resumed.state, SessionState::Completed);
        // After resume only `z` should fire — neither a nor b again.
        let post_resume_total = calls.lock().unwrap().len();
        assert_eq!(
            post_resume_total - pre_resume_calls,
            1,
            "resume must only run `z`, not re-enter the spent loop"
        );
        // Counter survives untouched; loop_back didn't re-fire.
        assert_eq!(resumed.loop_counts.get("b"), Some(&1));
    }

    #[test]
    fn issue_context_persists_across_resume_when_caller_omits_it() {
        // execute() carries an explicit --issue; resume() supplies no
        // issue (the CLI today doesn't re-resolve). Post-fix, the
        // resumed bash node must still see FLEET_ISSUE_* in its env
        // prefix because the executor reads from session.issue.
        let yaml = "\
name: issue-across-gate
nodes:
  - id: setup
    type: bash
    script: 'echo setup'
  - id: g
    depends_on: [setup]
    type: gate
    summary: 'pause'
  - id: ship
    depends_on: [g]
    type: bash
    script: 'echo ship'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            // Capture the full sh -c script so we can inspect for the
            // FLEET_ISSUE_* env prefix.
            let s = args.last().cloned().unwrap_or_default();
            calls2.lock().unwrap().push(s);
            Ok(String::new())
        });
        let executor = WorkflowExecutor::new(Arc::new(mock)).with_clock(counter_clock());

        // First run with an issue.
        let exec_req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-issue-resume"),
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let paused = executor.execute(&exec_req).unwrap();
        assert_eq!(paused.state, SessionState::AwaitingGate);
        assert_eq!(paused.issue, Some(sample_issue()));
        // Pre-gate `setup` script must have seen the issue env.
        let pre_gate_script = calls.lock().unwrap()[0].clone();
        assert!(
            pre_gate_script.contains("FLEET_ISSUE_ID='gh:42'"),
            "pre-gate setup script missing issue env: {pre_gate_script}"
        );

        // Resume *without* re-supplying the issue (mirrors the CLI today).
        let resume_req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-issue-resume"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let resumed = executor.resume(&resume_req).unwrap();
        assert_eq!(resumed.state, SessionState::Completed);
        // The persisted issue should still be there.
        assert_eq!(resumed.issue, Some(sample_issue()));
        // Post-gate `ship` script must see the issue env too —
        // that's the user-visible effect of the persistence fix.
        let ship_script = calls
            .lock()
            .unwrap()
            .last()
            .expect("ship must have run")
            .clone();
        assert!(
            ship_script.contains("FLEET_ISSUE_ID='gh:42'"),
            "post-resume ship script missing issue env: {ship_script}"
        );
    }

    #[test]
    fn issue_context_on_resume_overrides_persisted_when_supplied() {
        // Symmetric: if the caller *does* supply a fresh issue on
        // resume, it wins. Lets a future CLI add `--issue` to
        // `workflow resume` for retargeting.
        let yaml = "\
name: override
nodes:
  - id: setup
    type: bash
    script: 'echo setup'
  - id: g
    depends_on: [setup]
    type: gate
    summary: 'pause'
  - id: ship
    depends_on: [g]
    type: bash
    script: 'echo ship'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let executor = executor_returning("");
        let original = sample_issue();
        let exec_req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-override"),
            issue: Some(original.clone()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&exec_req).unwrap();

        let replacement = IssueContext {
            id: "gh:99".to_string(),
            human_id: "99".to_string(),
            title: "Hotfix the parser".to_string(),
            labels: Vec::new(),
        };
        let resume_req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id: SessionId::new("s-override"),
            issue: Some(replacement.clone()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let resumed = executor.resume(&resume_req).unwrap();
        assert_eq!(resumed.issue, Some(replacement));
        assert_ne!(resumed.issue, Some(original));
    }

    #[test]
    fn assert_node_combined_with_when_can_be_skipped() {
        // An assert node also honours `when:`. If gated off, it neither
        // passes nor fails — it's simply skipped. Useful for guarding
        // an invariant that only applies on one branch of a fork.
        let yaml = "\
name: gated-assert
nodes:
  - id: setup
    type: bash
    script: 'write outputs'
    outputs: { decision: decision }
  - id: check
    depends_on: [setup]
    when: 'setup.decision == \"approve\"'
    type: assert
    expr: 'setup.decision == \"approve\"'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();

        let session_id = SessionId::new("s-assert-skip");
        let af = store.session_dir(&session_id).join("artifacts");
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, args| {
            let s = args.last().cloned().unwrap_or_default();
            if s.contains("write outputs") {
                std::fs::write(
                    af.join("setup.outputs.json"),
                    r#"{"decision":"changes_requested"}"#,
                )
                .unwrap();
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
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        // when:-false skipped the assert; workflow Completes despite
        // the assert expression being false.
        assert_eq!(session.state, SessionState::Completed);
        let log =
            std::fs::read_to_string(store.session_dir(&session_id).join("logs/check.log")).unwrap();
        assert!(log.contains("--- skipped:"), "got: {log}");
    }

    /// Minimal `RuntimeAdapter` whose only real method is
    /// `inspect_image_arch`. The other trait methods panic with a
    /// clear message if anyone accidentally calls them — these
    /// tests only exercise the arch gate.
    struct ArchOnlyAdapter {
        arch: Result<Option<String>, ()>,
    }

    impl ArchOnlyAdapter {
        fn returning(arch: Option<&str>) -> Self {
            Self {
                arch: Ok(arch.map(str::to_string)),
            }
        }

        fn erroring() -> Self {
            Self { arch: Err(()) }
        }
    }

    impl crate::runtime::RuntimeAdapter for ArchOnlyAdapter {
        fn name(&self) -> &'static str {
            "arch-only"
        }
        fn capabilities(&self) -> crate::runtime::Capabilities {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn ensure_image(
            &self,
            _devcontainer: &crate::runtime::Devcontainer,
        ) -> Result<crate::runtime::ImageId> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn inspect_image_arch(&self, _image: &crate::runtime::ImageId) -> Result<Option<String>> {
            match &self.arch {
                Ok(a) => Ok(a.clone()),
                Err(()) => Err(anyhow!("simulated engine error")),
            }
        }
        fn start_container(
            &self,
            _spec: &crate::runtime::ContainerSpec,
        ) -> Result<crate::runtime::ContainerId> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn exec(
            &self,
            _container: &crate::runtime::ContainerId,
            _argv: &[String],
            _opts: crate::runtime::ExecOpts,
        ) -> Result<crate::runtime::ExecHandle> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn attach_pty(
            &self,
            _container: &crate::runtime::ContainerId,
            _argv: &[String],
            _opts: crate::runtime::ExecOpts,
        ) -> Result<crate::runtime::PtyHandle> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn stop(&self, _container: &crate::runtime::ContainerId) -> Result<()> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
        fn inspect(
            &self,
            _container: &crate::runtime::ContainerId,
        ) -> Result<crate::runtime::ContainerState> {
            unimplemented!("arch-only adapter is for fleet_tracker_mount tests")
        }
    }

    #[test]
    fn host_arch_matches_image_returns_true_for_matching_arch() {
        let adapter = ArchOnlyAdapter::returning(Some(host_arch_oci()));
        assert!(host_arch_matches_image(&adapter, &ImageId::new("img:1")));
    }

    #[test]
    fn host_arch_matches_image_returns_false_for_mismatch() {
        // Pick the *other* arch from the two we natively translate, so
        // this test stays meaningful on either amd64 or arm64 hosts.
        let other = if host_arch_oci() == "amd64" {
            "arm64"
        } else {
            "amd64"
        };
        let adapter = ArchOnlyAdapter::returning(Some(other));
        assert!(!host_arch_matches_image(&adapter, &ImageId::new("img:1")));
    }

    #[test]
    fn host_arch_matches_image_returns_false_when_adapter_reports_none() {
        let adapter = ArchOnlyAdapter::returning(None);
        assert!(!host_arch_matches_image(&adapter, &ImageId::new("img:1")));
    }

    #[test]
    fn host_arch_matches_image_returns_false_when_adapter_errors() {
        // Engine errors collapse to "can't tell, skip" — failing the
        // workflow over an inspect glitch when the bridge HTTP path
        // still works would be over-strict.
        let adapter = ArchOnlyAdapter::erroring();
        assert!(!host_arch_matches_image(&adapter, &ImageId::new("img:1")));
    }

    #[test]
    fn fleet_tracker_mount_returns_none_on_arch_mismatch_even_when_binary_present() {
        // On any host, an adapter reporting a clearly-foreign arch
        // ("future-arch-9000") must result in no mount, regardless of
        // whether the sibling binary exists. This is the cross-arch
        // bug we're closing: previously we'd mount the wrong-arch
        // binary and the agent would get "exec format error."
        let adapter = ArchOnlyAdapter::returning(Some("future-arch-9000"));
        assert_eq!(fleet_tracker_mount(&adapter, &ImageId::new("img:1")), None);
    }

    // ---- worktree gitlink mounts --------------------------------------

    #[test]
    fn worktree_git_mounts_is_empty_for_non_git_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.git` at all — not a git workspace.
        assert!(worktree_git_mounts(tmp.path()).is_empty());
    }

    #[test]
    fn worktree_git_mounts_is_empty_for_regular_repo_with_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // `.git` is a directory — a regular non-worktree repo; the
        // workspace mount alone covers it, no extra mounts needed.
        std::fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();
        assert!(worktree_git_mounts(tmp.path()).is_empty());
    }

    #[test]
    fn worktree_git_mounts_emits_gitdir_and_commondir_for_worktree() {
        // Synthesise the shape `git worktree add` produces:
        //   <main>/.git/                         ← main repo's gitdir
        //   <main>/.git/worktrees/wt12/          ← per-worktree gitdir
        //   <main>/.git/worktrees/wt12/commondir contains "../.."
        //   <wt>/.git is a *file* with `gitdir: <abs path to wt12>`
        let tmp = tempfile::tempdir().unwrap();
        let main_git = tmp.path().join("main/.git");
        let wt_gitdir = main_git.join("worktrees/wt12");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::write(wt_gitdir.join("commondir"), "../..").unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();

        let mounts = worktree_git_mounts(&wt);
        assert_eq!(mounts.len(), 2, "got {mounts:?}");
        // First entry is the per-worktree gitdir.
        let canon_wt_gitdir = std::fs::canonicalize(&wt_gitdir).unwrap();
        assert_eq!(mounts[0].host_path, wt_gitdir);
        assert_eq!(mounts[0].container_path, wt_gitdir);
        assert!(!mounts[0].read_only);
        // Second entry is the main repo's gitdir, resolved via commondir.
        let canon_main_git = std::fs::canonicalize(&main_git).unwrap();
        assert_eq!(mounts[1].host_path, canon_main_git);
        assert_eq!(mounts[1].container_path, canon_main_git);
        assert!(!mounts[1].read_only);
        // Suppress unused-variable warning while we're at it.
        let _ = canon_wt_gitdir;
    }

    #[test]
    fn worktree_git_mounts_skips_main_when_commondir_missing() {
        // Edge case: worktree gitlink valid but no commondir file —
        // mount the gitdir alone so the gitlink resolves to *something*,
        // skip the main repo mount (git may degrade but at least the
        // gitlink isn't dangling).
        let tmp = tempfile::tempdir().unwrap();
        let wt_gitdir = tmp.path().join("orphan/.git/worktrees/wt99");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();

        let mounts = worktree_git_mounts(&wt);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host_path, wt_gitdir);
    }

    #[test]
    fn parse_gitlink_gitdir_handles_trailing_newline_and_whitespace() {
        assert_eq!(
            parse_gitlink_gitdir("gitdir: /foo/bar\n"),
            Some(PathBuf::from("/foo/bar")),
        );
        assert_eq!(
            parse_gitlink_gitdir("gitdir:   /weird/  \n"),
            Some(PathBuf::from("/weird/")),
        );
        assert_eq!(parse_gitlink_gitdir("# random comment\n"), None);
        assert_eq!(parse_gitlink_gitdir("gitdir:\n"), None);
    }

    // ---- tracker-create -----------------------------------------------

    /// Captures every tracker write to one log + returns canned results.
    /// Distinct from the bridge module's `MockTracker` so the executor
    /// tests stay decoupled from the bridge's internal test fixtures.
    struct MockCreateTracker {
        calls: std::sync::Mutex<Vec<String>>,
        next_create: std::sync::Mutex<Option<crate::tracker::Issue>>,
        fail_create: std::sync::Mutex<bool>,
        fail_link: std::sync::Mutex<bool>,
    }

    impl MockCreateTracker {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_create: std::sync::Mutex::new(None),
                fail_create: std::sync::Mutex::new(false),
                fail_link: std::sync::Mutex::new(false),
            }
        }

        fn with_next_create(self, issue: crate::tracker::Issue) -> Self {
            *self.next_create.lock().unwrap() = Some(issue);
            self
        }

        fn fail_create(self) -> Self {
            *self.fail_create.lock().unwrap() = true;
            self
        }

        fn fail_link(self) -> Self {
            *self.fail_link.lock().unwrap() = true;
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl crate::tracker::Tracker for MockCreateTracker {
        fn name(&self) -> &'static str {
            "mock-create"
        }

        fn list_issues(&self, _: &Path) -> Result<Vec<crate::tracker::Issue>> {
            Ok(Vec::new())
        }

        fn create(
            &self,
            _: &Path,
            title: &str,
            body: &str,
            labels: &[String],
        ) -> Result<crate::tracker::Issue> {
            self.calls.lock().unwrap().push(format!(
                "create(title={title:?}, body={body:?}, labels={labels:?})"
            ));
            if *self.fail_create.lock().unwrap() {
                return Err(anyhow!("forced tracker create failure"));
            }
            self.next_create
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no next_create configured"))
        }

        fn link_parent(&self, _: &Path, parent: &str, child: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("link_parent(parent={parent}, child={child})"));
            if *self.fail_link.lock().unwrap() {
                Err(anyhow!("forced link_parent failure"))
            } else {
                Ok(())
            }
        }

        fn comment(&self, _: &Path, id: &str, body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("comment(id={id}, body={body:?})"));
            Ok(())
        }
    }

    fn sample_created_issue(human: &str) -> crate::tracker::Issue {
        crate::tracker::Issue {
            id: format!("gh:{human}"),
            human_id: human.to_string(),
            title: "Created by tracker-create".to_string(),
            status: "open".to_string(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn parse_output_ref_splits_node_and_name() {
        let (n, o) = parse_output_ref("implement.recommend_ticket").unwrap();
        assert_eq!(n, "implement");
        assert_eq!(o, "recommend_ticket");
    }

    #[test]
    fn parse_output_ref_errors_on_missing_dot() {
        let err = parse_output_ref("implement").unwrap_err();
        assert!(format!("{err}").contains("not a `<node>.<output>`"));
    }

    #[test]
    fn parse_output_ref_errors_on_empty_halves() {
        assert!(parse_output_ref(".name").is_err());
        assert!(parse_output_ref("node.").is_err());
    }

    #[test]
    fn parse_output_ref_rejects_nested_paths() {
        let err = parse_output_ref("a.b.c").unwrap_err();
        assert!(format!("{err}").contains("nested paths are not supported"));
    }

    fn pr_node_with_id(id: &str) -> Node {
        Node {
            id: id.to_string(),
            depends_on: vec![],
            when: None,
            kind: NodeKind::PrChecks { pr: None },
            artifacts: crate::workflow::spec::ArtifactsSpec::default(),
            outputs: std::collections::BTreeMap::new(),
            loop_back_to: None,
            max_loops: None,
        }
    }

    fn session_with_pr(number: u32) -> Session {
        let mut s = Session::new(SessionId::new("s-test"), "wf", 0);
        s.pr = Some(PrContext {
            number,
            human_id: format!("pr:{number}"),
            title: "x".into(),
            head_ref: "feat/x".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            url: "https://github.com/o/r/pull/x".into(),
        });
        s
    }

    #[test]
    fn resolve_pr_number_uses_numeric_literal() {
        let node = pr_node_with_id("probe");
        let session = Session::new(SessionId::new("s"), "wf", 0);
        let outputs = OutputMap::new();
        let n = resolve_pr_number(&node, Some("42"), &session, &outputs).unwrap();
        assert_eq!(n, 42);
    }

    #[test]
    fn resolve_pr_number_uses_dotted_output_ref() {
        let node = pr_node_with_id("probe");
        let session = Session::new(SessionId::new("s"), "wf", 0);
        let mut outputs = OutputMap::new();
        outputs.insert(("pick".to_string(), "number".to_string()), "7".to_string());
        let n = resolve_pr_number(&node, Some("pick.number"), &session, &outputs).unwrap();
        assert_eq!(n, 7);
    }

    #[test]
    fn resolve_pr_number_falls_back_to_session_pr_when_no_ref() {
        let node = pr_node_with_id("probe");
        let session = session_with_pr(13);
        let outputs = OutputMap::new();
        let n = resolve_pr_number(&node, None, &session, &outputs).unwrap();
        assert_eq!(n, 13);
    }

    #[test]
    fn resolve_pr_number_errors_when_no_ref_and_no_session_pr() {
        let node = pr_node_with_id("probe");
        let session = Session::new(SessionId::new("s"), "wf", 0);
        let outputs = OutputMap::new();
        let err = resolve_pr_number(&node, None, &session, &outputs).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("session has no bound PR"), "got: {msg}");
        assert!(msg.contains("probe"), "got: {msg}");
    }

    #[test]
    fn resolve_pr_number_errors_when_upstream_output_missing() {
        let node = pr_node_with_id("probe");
        let session = Session::new(SessionId::new("s"), "wf", 0);
        let outputs = OutputMap::new();
        let err = resolve_pr_number(&node, Some("pick.number"), &session, &outputs).unwrap_err();
        assert!(
            format!("{err:#}").contains("no upstream output"),
            "got: {err:#}"
        );
    }

    #[test]
    fn resolve_pr_number_errors_when_upstream_value_not_a_number() {
        let node = pr_node_with_id("probe");
        let session = Session::new(SessionId::new("s"), "wf", 0);
        let mut outputs = OutputMap::new();
        outputs.insert(("pick".to_string(), "number".to_string()), "abc".to_string());
        let err = resolve_pr_number(&node, Some("pick.number"), &session, &outputs).unwrap_err();
        assert!(
            format!("{err:#}").contains("is not a PR number"),
            "got: {err:#}"
        );
    }

    #[test]
    fn detect_current_branch_returns_short_name() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run().returning(|_, _| Ok("feat/x".to_string()));
        let v = detect_current_branch(&inv, Path::new("/repo")).unwrap();
        assert_eq!(v, "feat/x");
    }

    #[test]
    fn execute_with_start_after_node_skips_predecessors() {
        // 3-node linear bash workflow with start_after_node="write_a"
        // → only b and c run. We check by looking at the per-node logs
        // under `.fleet/sessions/<id>/logs/`.
        let yaml = "\
name: skipper
nodes:
  - id: write_a
    type: bash
    script: 'true'
  - id: write_b
    depends_on: [write_a]
    type: bash
    script: 'true'
  - id: write_c
    depends_on: [write_b]
    type: bash
    script: 'true'
";
        let wf = Workflow::from_str_at(yaml, "/x.yaml").unwrap();
        let session_id = SessionId::new("s-skipafter");
        let adapter = local_adapter_with_stdout("");
        let agents = crate::agent::AgentRegistry::default();
        let devcontainer = sample_devcontainer();
        let (dir, store) = build_store();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &devcontainer,
            workspace: dir.path(),
            session_id: session_id.clone(),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig::default(),
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: Some("write_a"),
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        let logs = store.session_dir(&session_id).join("logs");
        assert!(
            !logs.join("write_a.log").exists(),
            "write_a log should not exist; got logs dir = {logs:?}",
        );
        assert!(logs.join("write_b.log").exists());
        assert!(logs.join("write_c.log").exists());
    }

    #[test]
    fn execute_with_start_after_node_unknown_id_errors() {
        let yaml = "name: x\nnodes:\n  - id: a\n    type: bash\n    script: 'true'\n";
        let wf = Workflow::from_str_at(yaml, "/x.yaml").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = crate::agent::AgentRegistry::default();
        let devcontainer = sample_devcontainer();
        let (dir, store) = build_store();
        let executor = executor_returning("");
        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &devcontainer,
            workspace: dir.path(),
            session_id: SessionId::new("s-bad"),
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig::default(),
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: Some("nonexistent"),
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a node in workflow"),
            "got: {err:#}"
        );
    }

    #[test]
    fn detect_current_branch_errors_on_detached_head() {
        let mut inv = MockProcessInvoker::new();
        inv.expect_run().returning(|_, _| Ok("HEAD".to_string()));
        let err = detect_current_branch(&inv, Path::new("/repo")).unwrap_err();
        assert!(
            format!("{err:#}").contains("detached-HEAD"),
            "got: {err:#}"
        );
    }

    #[test]
    fn tracker_create_node_calls_create_and_emits_outputs_end_to_end() {
        // Workflow: an upstream bash node writes the `recommend_ticket`
        // payload as if it were an agent's output, then the
        // tracker-create node fires and the test asserts the
        // MockCreateTracker saw the right call + the OutputMap on
        // disk picked up `created_id` / `created_human_id`.
        let yaml = "\
name: tc-happy
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-1");
        let af = store.session_dir(&session_id).join("artifacts");

        // The bash node writes `implement.outputs.json` with the
        // structured recommend_ticket payload.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"Missing JSON parser","body":"need it for the planner","labels":["needs-impl"]}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("87")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();

        // Tracker.create saw the parsed object verbatim, link_parent
        // was *not* called (the workflow opted out).
        let calls = tracker.calls();
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one tracker call: {calls:?}"
        );
        let create_call = &calls[0];
        assert!(create_call.contains("title=\"Missing JSON parser\""));
        assert!(create_call.contains("body=\"need it for the planner\""));
        assert!(create_call.contains("labels=[\"needs-impl\"]"));

        // OutputMap entries persisted onto the session.
        assert_eq!(session.state, SessionState::Completed);
        let file_dep_outputs = session
            .outputs
            .get("file-dep")
            .expect("file-dep emitted outputs");
        assert_eq!(
            file_dep_outputs.get("created_id"),
            Some(&"gh:87".to_string())
        );
        assert_eq!(
            file_dep_outputs.get("created_human_id"),
            Some(&"87".to_string())
        );
    }

    #[test]
    fn tracker_create_node_calls_link_parent_when_issue_bound() {
        // Same shape as above but link_parent is true (the YAML
        // default) and we provide an issue. The tracker should see
        // both `create` and `link_parent`.
        let yaml = "\
name: tc-link
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-link");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("99")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();
        let calls = tracker.calls();
        // Expect: create + link_parent + comment(parent) + comment(child).
        assert_eq!(
            calls.len(),
            4,
            "expected create + link_parent + 2 cross-link comments: {calls:?}"
        );
        assert!(calls[0].starts_with("create("));
        assert!(calls[1].starts_with("link_parent("));
        // Parent is the bound issue's human_id (`42`), child is the
        // freshly minted `99`.
        assert!(calls[1].contains("parent=42"));
        assert!(calls[1].contains("child=99"));
        // Cross-link comments — order is (i) parent gets "fleet
        // filed follow-up #99" then (ii) child gets "filed via fleet
        // from #42".
        assert!(calls[2].starts_with("comment(id=42, "));
        assert!(calls[2].contains("follow-up #99"));
        assert!(calls[3].starts_with("comment(id=99, "));
        assert!(calls[3].contains("from #42"));
    }

    #[test]
    fn tracker_create_node_records_deps_edge_when_link_parent_fires() {
        // Same setup as the link_parent test but assert on the
        // on-disk deps.json. Edge shape:
        //   blocked = parent.human_id
        //   blocked_on = created.human_id
        //   reason = Ticket
        let yaml = "\
name: tc-deps
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        // Use a tempdir-backed store so the deps file lands somewhere
        // we can read back.
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-deps");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("99")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();

        // Deps file lives at <session-store-parent>/deps.json. Same
        // resolution the executor uses.
        let deps_path = deps_path_for(&store);
        let body = std::fs::read_to_string(&deps_path).expect("deps.json should have been written");
        let doc: crate::deps::DepsDoc = serde_json::from_str(&body).unwrap();
        assert_eq!(doc.edges.len(), 1);
        assert_eq!(doc.edges[0].blocked, "42");
        assert_eq!(doc.edges[0].blocked_on, "99");
        assert_eq!(doc.edges[0].reason, crate::deps::BlockedReason::Ticket);
    }

    #[test]
    fn tracker_create_node_injects_new_ticket_before_parent_in_active_plan() {
        // Setup: an active plan owns the bound parent (#42) at
        // position 1. After tracker-create fires, the plan should
        // have the freshly-filed child injected at position 1 (just
        // before the parent), with `injected: true`.
        let yaml = "\
name: tc-inject
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-inject");
        let af = store.session_dir(&session_id).join("artifacts");

        // Pre-seed the plan: 3 items, parent #42 in the middle.
        let plans_root = plans_root_for(&store);
        let plan_store = crate::plans::store::PlanStore::at(&plans_root);
        let plan = crate::plans::Plan::new(
            crate::plans::PlanId::new("plan-preexisting"),
            "Parser refactor",
            vec!["40".to_string(), "42".to_string(), "44".to_string()],
            1_700_000_000_000,
        );
        plan_store.save(&plan).unwrap();

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("99")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();

        let loaded = plan_store
            .load(&crate::plans::PlanId::new("plan-preexisting"))
            .unwrap();
        assert_eq!(loaded.items.len(), 4);
        // Order: 40, 99 (injected), 42, 44.
        assert_eq!(loaded.items[0].ticket_id, "40");
        assert_eq!(loaded.items[1].ticket_id, "99");
        assert!(loaded.items[1].injected);
        assert_eq!(loaded.items[1].state, crate::plans::PlanItemState::Pending);
        assert_eq!(loaded.items[2].ticket_id, "42");
        assert_eq!(loaded.items[3].ticket_id, "44");
    }

    #[test]
    fn tracker_create_node_does_not_inject_when_parent_plan_is_paused() {
        // A paused plan must not be mutated by the workflow engine;
        // `find_plan_containing` already filters to active plans,
        // and this test pins the contract down end-to-end.
        let yaml = "\
name: tc-noinject-paused
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-paused");
        let af = store.session_dir(&session_id).join("artifacts");

        let plans_root = plans_root_for(&store);
        let plan_store = crate::plans::store::PlanStore::at(&plans_root);
        let mut plan = crate::plans::Plan::new(
            crate::plans::PlanId::new("plan-paused"),
            "Paused refactor",
            vec!["42".to_string(), "44".to_string()],
            1_700_000_000_000,
        );
        plan.state = crate::plans::PlanState::Paused;
        plan_store.save(&plan).unwrap();

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("99")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();

        // Plan unchanged: still 2 items, no injected child.
        let loaded = plan_store
            .load(&crate::plans::PlanId::new("plan-paused"))
            .unwrap();
        assert_eq!(loaded.items.len(), 2);
        assert!(!loaded.items.iter().any(|i| i.ticket_id == "99"));
    }

    #[test]
    fn tracker_create_node_skips_plan_injection_when_no_plan_owns_parent() {
        // No plan exists at all → tracker-create still files the
        // ticket + records deps, but the plan-injection branch is a
        // clean no-op (no error, nothing to inject into).
        let yaml = "\
name: tc-noplan
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-noplan");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("99")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();
        // Plans directory may not even exist; tracker-create
        // shouldn't have created one for no reason.
        let plans_dir = plans_root_for(&store);
        if plans_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&plans_dir).unwrap().collect();
            assert!(
                entries.is_empty(),
                "plans dir should have no plan files: {entries:?}"
            );
        }
    }

    #[test]
    fn tracker_create_node_writes_no_deps_file_when_no_issue_bound() {
        // Without a bound parent, the deps edge isn't recorded — the
        // edge has nothing to anchor `blocked` to. The deps.json
        // file should remain absent.
        let yaml = "\
name: tc-nodeps
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path().join("sessions"));
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-nodeps");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("11")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();
        let deps_path = deps_path_for(&store);
        assert!(
            !deps_path.exists(),
            "deps.json should not be written without a bound parent"
        );
    }

    #[test]
    fn tracker_create_node_skips_link_parent_when_no_issue_bound() {
        let yaml = "\
name: tc-no-issue
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-noissue");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("11")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None, // no bound ticket
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        executor.execute(&req).unwrap();
        let calls = tracker.calls();
        assert_eq!(
            calls.len(),
            1,
            "link_parent must not fire without a bound issue: {calls:?}"
        );
        assert!(calls[0].starts_with("create("));
    }

    #[test]
    fn tracker_create_node_fails_when_executor_has_no_tracker() {
        let yaml = "\
name: tc-no-tracker
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-notracker");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        // Executor built *without* a tracker.
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
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("requires a tracker"));
    }

    #[test]
    fn tracker_create_node_fails_when_recommend_ticket_is_malformed_json() {
        let yaml = "\
name: tc-bad-json
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-badjson");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            // Recommend_ticket is a *string* — JSON-encoded as
            // `"\"not-an-object\""`. The handler should bail when
            // deserializing into `{title, body, labels?}`.
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":"not-an-object"}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker = Arc::new(MockCreateTracker::new());
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("title"), "got: {msg}");
        // Tracker.create never gets called.
        assert!(
            tracker.calls().is_empty(),
            "tracker should not be invoked on parse failure"
        );
    }

    #[test]
    fn tracker_create_node_fails_on_empty_title() {
        let yaml = "\
name: tc-empty-title
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-empty");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker = Arc::new(MockCreateTracker::new());
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("title"));
        // Tracker.create not called because title-check runs first.
        assert!(tracker.calls().is_empty());
    }

    #[test]
    fn tracker_create_node_propagates_tracker_create_failures() {
        let yaml = "\
name: tc-create-fail
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-cf");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker = Arc::new(MockCreateTracker::new().fail_create());
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("forced tracker create failure"));
    }

    #[test]
    fn tracker_create_node_enforces_max_recommended_tickets_cap() {
        // Two tracker-create nodes in a workflow with max=1: the
        // first succeeds, the second fails with a cap message.
        // Mock tracker has exactly one `next_create` configured to
        // make sure it isn't called twice.
        let yaml = "\
name: tc-cap
max_recommended_tickets: 1
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: first
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
  - id: second
    depends_on: [first]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-cap");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("77")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("per-session cap"), "got: {msg}");
        assert!(msg.contains("max_recommended_tickets = 1"), "got: {msg}");

        // First node fired exactly once; second blocked at the cap.
        let calls = tracker.calls();
        assert_eq!(
            calls.len(),
            1,
            "second tracker-create must not have called create(): {calls:?}"
        );
    }

    #[test]
    fn tracker_create_node_allows_firings_up_to_default_cap() {
        // No explicit cap: default is 3. Three tracker-create nodes
        // all fire successfully.
        let yaml = "\
name: tc-cap-default
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: t1
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
  - id: t2
    depends_on: [t1]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
  - id: t3
    depends_on: [t2]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-cap-d");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        // Sequence each create's response so the assertions on
        // returned ids stay distinct.
        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("100")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let session = executor.execute(&req).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        // Three create() calls — one per tracker-create node.
        assert_eq!(tracker.calls().len(), 3);
    }

    #[test]
    fn count_tracker_create_firings_returns_zero_when_no_outputs_persisted() {
        let yaml = "\
name: zero
nodes:
  - id: t
    type: tracker-create
    from: a.b
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let session = Session::new(SessionId::new("s"), "zero", 0);
        assert_eq!(count_tracker_create_firings(&wf, &session), 0);
    }

    #[test]
    fn count_tracker_create_firings_skips_non_tracker_create_nodes() {
        // Even if a non-tracker-create node happens to emit a
        // `created_id` key in its outputs, the helper must not count
        // it — only TrackerCreate variants matter.
        let yaml = "\
name: mixed
nodes:
  - id: bash
    type: bash
    script: 'echo'
    outputs: { created_id: created_id }
  - id: t
    type: tracker-create
    from: bash.created_id
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let mut session = Session::new(SessionId::new("s"), "mixed", 0);
        let mut bash_outputs = BTreeMap::new();
        bash_outputs.insert("created_id".to_string(), "1".to_string());
        session.outputs.insert("bash".to_string(), bash_outputs);
        // bash emitted `created_id` but isn't a tracker-create.
        assert_eq!(count_tracker_create_firings(&wf, &session), 0);
    }

    #[test]
    fn tracker_create_node_propagates_link_parent_failures() {
        // `link_parent` ran after `create` — its failure must surface
        // (the ticket has been filed but the structural link is now
        // missing, which is exactly the situation we want loud, not
        // swallowed).
        let yaml = "\
name: tc-link-fail
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-lf");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b"}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker = Arc::new(
            MockCreateTracker::new()
                .with_next_create(sample_created_issue("55"))
                .fail_link(),
        );
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ));

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: Some(sample_issue()),
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let err = executor.execute(&req).unwrap_err();
        assert!(format!("{err:#}").contains("forced link_parent failure"));
    }

    #[test]
    fn stamp_request_labels_unions_preserving_supplied_order() {
        // Direct unit test on the private helper. Mirrors the
        // semantics of `AutonomousConfig::stamp_creation_labels`:
        // supplied labels keep their position, missing stamp labels
        // are appended in stamp-config order.
        let invoker = Arc::new(MockProcessInvoker::new());
        let exec = WorkflowExecutor::new(invoker)
            .with_creation_label_stamp(vec!["fleet".to_string(), "agent".to_string()]);
        assert_eq!(
            exec.stamp_request_labels(&["p1".to_string()]),
            vec!["p1".to_string(), "fleet".to_string(), "agent".to_string()]
        );
        assert_eq!(
            exec.stamp_request_labels(&[]),
            vec!["fleet".to_string(), "agent".to_string()]
        );
        // Already-supplied stamp label is not duplicated.
        assert_eq!(
            exec.stamp_request_labels(&["fleet".to_string(), "p1".to_string()]),
            vec!["fleet".to_string(), "p1".to_string(), "agent".to_string()]
        );
    }

    #[test]
    fn stamp_request_labels_empty_stamp_returns_supplied_unchanged() {
        // Pre-feature regression net: an executor built without
        // `with_creation_label_stamp` (or with an empty stamp) must
        // forward the agent's labels byte-for-byte.
        let invoker = Arc::new(MockProcessInvoker::new());
        let exec = WorkflowExecutor::new(invoker);
        assert!(exec.stamp_request_labels(&[]).is_empty());
        assert_eq!(
            exec.stamp_request_labels(&["p1".to_string(), "docs".to_string()]),
            vec!["p1".to_string(), "docs".to_string()]
        );
    }

    #[test]
    fn tracker_create_node_stamps_configured_filter_labels_onto_request() {
        // End-to-end wiring check: the executor's
        // `creation_label_stamp` reaches the tracker's `create` call.
        // Agent emits `labels: ["needs-impl"]`; executor is built
        // with stamp `["fleet","agent"]`; tracker must see all three
        // in supplied-first / stamp-order.
        let yaml = "\
name: tc-stamp
nodes:
  - id: implement
    type: bash
    script: 'write outputs'
    outputs: { recommend_ticket: recommend_ticket }
  - id: file-dep
    depends_on: [implement]
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        let adapter = local_adapter_with_stdout("");
        let agents = AgentRegistry::default();
        let (_d, store) = build_store();
        let dc = sample_devcontainer();
        let session_id = SessionId::new("s-tc-stamp");
        let af = store.session_dir(&session_id).join("artifacts");

        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(move |_, _| {
            std::fs::create_dir_all(&af).unwrap();
            std::fs::write(
                af.join("implement.outputs.json"),
                r#"{"recommend_ticket":{"title":"t","body":"b","labels":["needs-impl"]}}"#,
            )
            .unwrap();
            Ok(String::new())
        });

        let tracker =
            Arc::new(MockCreateTracker::new().with_next_create(sample_created_issue("42")));
        let executor = WorkflowExecutor::new(Arc::new(mock))
            .with_clock(counter_clock())
            .with_tracker(Some(
                Arc::clone(&tracker) as Arc<dyn crate::tracker::Tracker>
            ))
            .with_creation_label_stamp(vec!["fleet".to_string(), "agent".to_string()]);

        let req = ExecuteRequest {
            workflow: &wf,
            adapter: &adapter,
            agents: &agents,
            store: &store,
            devcontainer: &dc,
            workspace: Path::new("/repo"),
            session_id,
            issue: None,
            pr: None,
            worktree: None,
            cost: &crate::repo_config::CostConfig {
                per_session_budget_usd: None,
                lifetime_budget_usd: None,
            },
            egress: &crate::egress::NoopEnforcer,
            interrupt_flag: None,
            secrets: empty_secrets_static(),
            start_after_node: None,
            git_guard: GitGuardPolicy::default(),
        };
        let _session = executor.execute(&req).unwrap();

        let calls = tracker.calls();
        assert_eq!(calls.len(), 1, "expected one create call: {calls:?}");
        // Debug formatting of Vec<String> renders as
        // `["needs-impl", "fleet", "agent"]` — supplied first, then
        // stamp labels in config order.
        assert!(
            calls[0].contains(r#"labels=["needs-impl", "fleet", "agent"]"#),
            "stamp not applied as expected: {}",
            calls[0]
        );
    }
}
