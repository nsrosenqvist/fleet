//! Workflow DSL types and YAML deserialisation.
//!
//! A [`Workflow`] is a small DAG over [`Node`]s. Node kinds: `agent`
//! (default when `type:` is absent), `bash`, `gate`, `assert`, `fanout`.
//! The YAML form is the surface users write — these types are how fleet
//! reasons about it internally.
//!
//! Parsing is two-step: a permissive [`Raw`] form deserialises every
//! field as optional, then `From<RawNode>` collapses it into a typed
//! [`Node`] with each kind's required fields validated. This split lets
//! us emit specific error messages ("agent node `plan` missing
//! `agent:`") instead of serde's generic untagged-enum miss.
//!
//! Cycle / missing-dependency validation lives in [`super::validate`];
//! this module only parses.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_yml::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A complete workflow: name + ordered list of nodes. The order of `nodes`
/// is not the execution order — [`super::validate`] computes that from the
/// `depends_on` graph — but order *is* preserved through parsing so error
/// messages quote the YAML in the order the user wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub description: Option<String>,
    pub trigger: Trigger,
    pub nodes: Vec<Node>,
    /// Hard cap on the number of `tracker-create` nodes that may
    /// successfully file a ticket during a single session run.
    /// Protects against runaway agents recommending hundreds of
    /// follow-up tickets in a row. `None` falls back to
    /// [`DEFAULT_MAX_RECOMMENDED_TICKETS`].
    pub max_recommended_tickets: Option<u32>,
    /// Scheduled-loop interval. When `Some`, the
    /// [`crate::scheduler::SchedulerEngine`] fires this workflow once per
    /// elapsed interval. Parsed from a humantime string (`1h`, `30m`,
    /// `6h30m`). A hard floor of [`MIN_LOOP_INTERVAL`] applies at parse
    /// time to keep the scheduler from hot-spinning.
    ///
    /// Mutually exclusive with `trigger.autonomous`: that engine fires
    /// per open issue, the scheduler fires per elapsed interval — a
    /// workflow opting into both would be claimed twice. The parser
    /// rejects the combination up front.
    pub loop_interval: Option<Duration>,
    /// Policy when a scheduler tick is ready to fire but a session of
    /// this workflow is still running. `Skip` (default) defers to the
    /// next interval; `Run` spawns anyway, accepting that multiple
    /// instances may overlap (e.g. when each session has its own PR
    /// scope and overlap is harmless).
    pub on_overlap: OnOverlap,
}

/// Minimum accepted `loop:` interval. Anything shorter would risk the
/// TUI-driven scheduler tick loop hot-spinning between idempotent
/// rescans; 60s is comfortable headroom over the ~10/s tick cadence
/// without requiring an explicit debounce of its own.
pub const MIN_LOOP_INTERVAL: Duration = Duration::from_secs(60);

/// Per-workflow overlap policy. Drives the scheduler's decision when a
/// tick is ready to fire and a session of the same workflow is already
/// in `Running` / `AwaitingGate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnOverlap {
    /// Defer the tick; the scheduler waits another interval before
    /// retrying. The safe default — a stalled session can't trigger a
    /// runaway slate of sibling sessions.
    #[default]
    Skip,
    /// Spawn the new session anyway. Reasonable for workflows whose
    /// candidate enumeration is naturally disjoint (each tick mints
    /// sessions bound to *new* PRs), so overlap means "two ticks did
    /// non-overlapping work."
    Run,
}

/// How a `pr-list` node feeds the rest of the workflow.
///
/// `Some(Per)`: the *scheduler* consumes this node before the executor
/// runs. The list of PR candidates becomes one independent session per
/// PR (each with its own [`crate::session::PrContext`] and worktree on
/// the PR's head). Downstream nodes run inside each minted session.
///
/// `None`: the node runs inside the current session like any host-side
/// node, emitting a structured `{ prs, count }` output for downstream
/// `when:` / `outputs:` consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrBindMode {
    /// One session per PR. The node must be the workflow's root (no
    /// `depends_on:`) so the executor never reaches it after the
    /// scheduler consumes it.
    Per,
}

/// CI-status filter on `pr-list`. Implementations evaluate against the
/// PR's check-suite rollup. `Pending` matches checks that haven't
/// completed; `Skipped` matches checks the CI system marked as skipped
/// (distinct from missing checks entirely).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    Skipped,
}

/// PR state filter on `pr-list`. Mirrors GitHub's three lifecycle
/// values; defaults to [`PrState::Open`] for the common "look at
/// active PRs only" case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrState {
    #[default]
    Open,
    Closed,
    Merged,
}

/// Filter applied to `pr-list`. Empty / defaulted fields are treated as
/// "no filter on that dimension" — `state` defaults to `Open` because
/// that is what every practical caller wants.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrFilter {
    pub ci_status: Option<CiStatus>,
    pub state: PrState,
    pub labels: Vec<String>,
}

/// Per-session cap applied when a workflow doesn't declare one.
/// Three is the value the plan settled on — enough headroom for a
/// real blocked-handler chain (e.g. agent files a parent ticket
/// that itself depends on two prerequisites) without surrendering
/// the runaway-protection story.
pub const DEFAULT_MAX_RECOMMENDED_TICKETS: u32 = 3;

/// How the workflow can be started. Manual = a user invokes
/// `fleet workflow run …`; autonomous = autonomous-mode picks it from
/// the open-issue queue; issueless = the workflow doesn't need a
/// tracker ticket to bind to, so the TUI's spawn picker surfaces it
/// on the Workflow tab. Defaults: manual-only (autonomous + issueless
/// both off) so adding a workflow file doesn't surprise users by
/// joining the autonomous queue *or* the issueless spawn list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Trigger {
    #[serde(default = "default_true")]
    pub manual: bool,
    #[serde(default)]
    pub autonomous: bool,
    /// Workflow runs meaningfully without a tracker ticket bound to it
    /// — e.g. a refactor-candidate scan, a periodic codebase audit, a
    /// "bring up the dev environment" pass. Opt-in because most
    /// workflows reference `FLEET_ISSUE_*` env vars and would run
    /// with empty inputs otherwise. The TUI's spawn picker Workflow
    /// tab filters on this flag.
    #[serde(default)]
    pub issueless: bool,
}

impl Default for Trigger {
    fn default() -> Self {
        Self {
            manual: true,
            autonomous: false,
            issueless: false,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// One node in the workflow DAG. Common fields here; kind-specific fields
/// live inside [`NodeKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub depends_on: Vec<String>,
    /// Optional gating expression evaluated at run time against prior
    /// nodes' `outputs`. Phase 1 stores the string verbatim; an expression
    /// engine lands with the executor.
    pub when: Option<String>,
    pub kind: NodeKind,
    pub artifacts: ArtifactsSpec,
    /// `outputs: { decision: review.decision }` — maps a local name (the
    /// key) to a dotted path the executor extracts from the node's
    /// produced artifacts. Plain `String → String` map; semantics
    /// resolved at execute time.
    pub outputs: BTreeMap<String, String>,
    /// Optional revision-cycle target: this node re-enters `loop_back_to`
    /// after running, up to `max_loops` times.
    pub loop_back_to: Option<String>,
    pub max_loops: Option<u32>,
}

/// Kind-discriminated payload. The five variants map directly onto the
/// plan's DSL: agent, bash, gate, assert, fanout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// An agent spawned inside a fresh container instance. References the
    /// agent registry by name.
    Agent {
        agent: String,
        persona: Option<String>,
        prompt_file: Option<PathBuf>,
    },
    /// A shell snippet run on the *host* (not inside a container). Phase 1
    /// scope: only safe utility commands like `gh pr create`. Future work
    /// can add a `container: true` modifier to run inside the workspace.
    Bash { script: String },
    /// A human-gate: workflow pauses until a person acts. `summary` is the
    /// one-line text shown in the TUI.
    Gate { summary: String },
    /// Inline boolean predicate over outputs, evaluated at run time.
    /// Failing assert → workflow fails. Phase 1 stores the string verbatim.
    Assert { expr: String },
    /// Run a set of sibling nodes in parallel, gather results. The
    /// `siblings` list names other nodes in the same workflow.
    Fanout { siblings: Vec<String> },
    /// Create a new ticket via the configured tracker. `from` is a
    /// dotted `<node>.<output>` reference into the `OutputMap` whose
    /// value is expected to be a JSON object shaped
    /// `{ "title": String, "body": String, "labels"?: [String] }`.
    /// On success the node emits `created_id` and `created_human_id`
    /// outputs the rest of the workflow can branch on. `link_parent`
    /// (default `true`) attaches the new ticket to the session's
    /// bound ticket via [`crate::tracker::Tracker::link_parent`]
    /// when an issue context is present; cross-link comments and
    /// `.fleet/deps.json` entries land in a follow-up commit.
    TrackerCreate { from: String, link_parent: bool },
    /// Enumerate pull requests via the configured
    /// [`crate::code_host::CodeHost`]. `bind: Some(Per)` is the
    /// per-PR fanout entry point consumed by the scheduler — see
    /// [`PrBindMode`]. With `bind: None`, the node runs inside the
    /// current session and emits `{ prs, count }` as a structured
    /// output.
    PrList {
        filter: PrFilter,
        bind: Option<PrBindMode>,
    },
    /// Read CI checks for a pull request. `pr` may be `None` (use the
    /// session's bound [`crate::session::PrContext`]), an explicit
    /// numeric literal as a string, or a dotted `<node>.<output>`
    /// reference resolved against the `OutputMap` at run time. The
    /// node writes `{ has_failures, failed_count, pending_count,
    /// checks }` as outputs.
    PrChecks { pr: Option<String> },
    /// Open a pull request via the configured `CodeHost`. `base` and
    /// `head` default at execute time: `base` to the remote's default
    /// branch, `head` to the worktree's current branch. The node emits
    /// `{ number, url, head_sha }` as outputs. Host-side; supersedes
    /// the legacy `bash` invocation of `gh pr create`.
    CreatePr {
        title: String,
        body: String,
        base: Option<String>,
        head: Option<String>,
        draft: bool,
    },
    /// Post a comment on a pull request via the configured `CodeHost`.
    /// `pr` resolves like [`NodeKind::PrChecks::pr`]. No outputs.
    PrComment { body: String, pr: Option<String> },
}

/// `artifacts:` block. Both lists default to empty; bare strings are paths
/// relative to the session's artifacts directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactsSpec {
    pub r#in: Vec<String>,
    pub out: Vec<String>,
}

impl Workflow {
    /// Parse a workflow YAML string. `path_hint` is for error context.
    pub fn from_str_at(input: &str, path_hint: impl AsRef<Path>) -> Result<Self> {
        let hint = path_hint.as_ref();
        let raw: RawWorkflow = serde_yml::from_str(input)
            .with_context(|| format!("parsing workflow YAML at {}", hint.display()))?;
        raw.try_into()
            .with_context(|| format!("workflow at {}", hint.display()))
    }

    /// Load a workflow from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading workflow file: {}", path.display()))?;
        Self::from_str_at(&s, path)
    }

    /// Look up a node by id. Returns `None` if no node has that id.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Effective cap on `tracker-create` firings per session. Returns
    /// the workflow's explicit value if any, falling back to
    /// [`DEFAULT_MAX_RECOMMENDED_TICKETS`] otherwise.
    #[must_use]
    pub fn effective_max_recommended_tickets(&self) -> u32 {
        self.max_recommended_tickets
            .unwrap_or(DEFAULT_MAX_RECOMMENDED_TICKETS)
    }
}

// === Raw deserialisation layer ===

/// Permissive raw form: every kind-specific field is optional so the
/// untyped YAML lands as-is and the conversion step decides what's
/// required for each `NodeKind`.
#[derive(Deserialize, Default)]
struct RawWorkflow {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    trigger: Option<Trigger>,
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default)]
    max_recommended_tickets: Option<u32>,
    /// `loop: 1h` — humantime string parsed into [`Duration`] at
    /// conversion time. `loop` is a Rust reserved word, so we
    /// serde-rename and store under [`Workflow::loop_interval`].
    #[serde(default, rename = "loop")]
    loop_interval: Option<String>,
    #[serde(default)]
    on_overlap: Option<OnOverlap>,
}

#[derive(Deserialize, Default)]
struct RawNode {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    artifacts: Option<RawArtifacts>,
    #[serde(default)]
    outputs: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    loop_back_to: Option<String>,
    #[serde(default)]
    max_loops: Option<u32>,
    // Kind-specific fields. Each is Optional; the conversion enforces
    // presence based on `kind`.
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    prompt_file: Option<PathBuf>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    siblings: Option<Vec<String>>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    link_parent: Option<bool>,
    // PR node kinds.
    #[serde(default)]
    filter: Option<RawPrFilter>,
    #[serde(default)]
    bind: Option<PrBindMode>,
    #[serde(default)]
    pr: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    head: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawPrFilter {
    #[serde(default)]
    ci_status: Option<CiStatus>,
    #[serde(default)]
    state: Option<PrState>,
    #[serde(default)]
    labels: Vec<String>,
}

impl From<RawPrFilter> for PrFilter {
    fn from(r: RawPrFilter) -> Self {
        Self {
            ci_status: r.ci_status,
            state: r.state.unwrap_or_default(),
            labels: r.labels,
        }
    }
}

#[derive(Deserialize, Default)]
struct RawArtifacts {
    #[serde(default, rename = "in")]
    inputs: Vec<String>,
    #[serde(default)]
    out: Vec<String>,
}

impl TryFrom<RawWorkflow> for Workflow {
    type Error = anyhow::Error;

    fn try_from(raw: RawWorkflow) -> Result<Self> {
        if raw.name.trim().is_empty() {
            bail!("workflow `name:` must be a non-empty string");
        }
        let trigger = raw.trigger.unwrap_or_default();
        // Autonomous mode fires workflows against open tracker issues
        // — it has no source of "no issue here, run anyway." So an
        // `autonomous: true && issueless: true` config is a
        // contradiction the schema must reject up front; otherwise
        // the supervisor would silently skip such workflows at
        // dispatch time and the user would never know why.
        if trigger.autonomous && trigger.issueless {
            bail!(
                "workflow `{}`: `trigger.autonomous: true` and \
                 `trigger.issueless: true` are mutually exclusive — \
                 autonomous mode only runs workflows against an open issue",
                raw.name,
            );
        }
        let loop_interval = match raw.loop_interval {
            Some(s) => {
                let trimmed = s.trim();
                let d = humantime::parse_duration(trimmed).map_err(|e| {
                    anyhow!(
                        "workflow `{}`: `loop: {}` is not a valid humantime duration \
                         (try `1h`, `30m`, `6h30m`): {e}",
                        raw.name,
                        trimmed,
                    )
                })?;
                if d < MIN_LOOP_INTERVAL {
                    bail!(
                        "workflow `{}`: `loop: {}` resolves to {:?} which is below the \
                         {}s minimum (the scheduler would hot-spin)",
                        raw.name,
                        trimmed,
                        d,
                        MIN_LOOP_INTERVAL.as_secs(),
                    );
                }
                Some(d)
            }
            None => None,
        };
        if loop_interval.is_some() && trigger.autonomous {
            bail!(
                "workflow `{}`: `loop:` and `trigger.autonomous: true` are mutually \
                 exclusive — the scheduler fires per elapsed interval, autonomous \
                 mode fires per open issue; opting into both would have two engines \
                 claim the same workflow",
                raw.name,
            );
        }
        let on_overlap = raw.on_overlap.unwrap_or_default();
        let nodes: Vec<Node> = raw
            .nodes
            .into_iter()
            .map(Node::try_from)
            .collect::<Result<_>>()?;
        Ok(Self {
            name: raw.name,
            description: raw.description,
            trigger,
            nodes,
            max_recommended_tickets: raw.max_recommended_tickets,
            loop_interval,
            on_overlap,
        })
    }
}

impl TryFrom<RawNode> for Node {
    type Error = anyhow::Error;

    fn try_from(raw: RawNode) -> Result<Self> {
        if raw.id.trim().is_empty() {
            bail!("workflow node `id:` must be a non-empty string");
        }
        let kind_name = raw.kind.as_deref().unwrap_or("agent");
        let kind = match kind_name {
            "agent" => NodeKind::Agent {
                agent: raw
                    .agent
                    .ok_or_else(|| anyhow!("agent node `{}` missing `agent:`", raw.id))?,
                persona: raw.persona,
                prompt_file: raw.prompt_file,
            },
            "bash" => NodeKind::Bash {
                script: raw
                    .script
                    .ok_or_else(|| anyhow!("bash node `{}` missing `script:`", raw.id))?,
            },
            "gate" => NodeKind::Gate {
                summary: raw
                    .summary
                    .ok_or_else(|| anyhow!("gate node `{}` missing `summary:`", raw.id))?,
            },
            "assert" => NodeKind::Assert {
                expr: raw
                    .expr
                    .ok_or_else(|| anyhow!("assert node `{}` missing `expr:`", raw.id))?,
            },
            "fanout" => NodeKind::Fanout {
                siblings: raw
                    .siblings
                    .ok_or_else(|| anyhow!("fanout node `{}` missing `siblings:`", raw.id))?,
            },
            "tracker-create" => NodeKind::TrackerCreate {
                from: raw
                    .from
                    .ok_or_else(|| anyhow!("tracker-create node `{}` missing `from:`", raw.id))?,
                // Default `true` — the v1 convention links the new
                // ticket back to the bound ticket so the parent's
                // task list / labels reflect the dependency without
                // the workflow author having to spell it out.
                link_parent: raw.link_parent.unwrap_or(true),
            },
            "pr-list" => NodeKind::PrList {
                // `filter:` is optional — an absent block means
                // "default filter" (open PRs, no CI / label
                // constraint). That matches the common "list every
                // open PR" case without forcing boilerplate.
                filter: raw.filter.map(Into::into).unwrap_or_default(),
                bind: raw.bind,
            },
            "pr-checks" => NodeKind::PrChecks { pr: raw.pr },
            "create-pr" => NodeKind::CreatePr {
                title: raw
                    .title
                    .ok_or_else(|| anyhow!("create-pr node `{}` missing `title:`", raw.id))?,
                body: raw
                    .body
                    .ok_or_else(|| anyhow!("create-pr node `{}` missing `body:`", raw.id))?,
                base: raw.base,
                head: raw.head,
                draft: raw.draft.unwrap_or(false),
            },
            "pr-comment" => NodeKind::PrComment {
                body: raw
                    .body
                    .ok_or_else(|| anyhow!("pr-comment node `{}` missing `body:`", raw.id))?,
                pr: raw.pr,
            },
            other => bail!(
                "node `{}` has unknown `type: {other}` \
                 (allowed: agent, bash, gate, assert, fanout, tracker-create, \
                 pr-list, pr-checks, create-pr, pr-comment)",
                raw.id
            ),
        };
        let artifacts = raw.artifacts.map(Into::into).unwrap_or_default();
        let outputs = raw
            .outputs
            .map(stringify_outputs)
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            id: raw.id,
            depends_on: raw.depends_on,
            when: raw.when,
            kind,
            artifacts,
            outputs,
            loop_back_to: raw.loop_back_to,
            max_loops: raw.max_loops,
        })
    }
}

impl From<RawArtifacts> for ArtifactsSpec {
    fn from(raw: RawArtifacts) -> Self {
        Self {
            r#in: raw.inputs,
            out: raw.out,
        }
    }
}

/// `outputs:` values are scalars in practice but YAML allows arbitrary
/// `Value` shapes. Coerce to string here so the executor's data model
/// stays flat; complex shapes return an error pointing at the key.
fn stringify_outputs(raw: BTreeMap<String, Value>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        let s = match v {
            Value::String(s) => s,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            other => bail!("outputs.{k} must be a scalar (string/bool/number); got: {other:?}"),
        };
        out.insert(k, s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_one_agent_node_workflow() {
        let yaml = "\
name: standard
nodes:
  - id: only
    agent: claude-code
";
        let wf = Workflow::from_str_at(yaml, "/x.yaml").unwrap();
        assert_eq!(wf.name, "standard");
        assert_eq!(wf.nodes.len(), 1);
        let n = &wf.nodes[0];
        assert_eq!(n.id, "only");
        match &n.kind {
            NodeKind::Agent {
                agent,
                persona,
                prompt_file,
            } => {
                assert_eq!(agent, "claude-code");
                assert!(persona.is_none());
                assert!(prompt_file.is_none());
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn defaults_trigger_to_manual_only() {
        let yaml = "name: x\nnodes: []";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(wf.trigger.manual);
        assert!(!wf.trigger.autonomous);
    }

    #[test]
    fn parses_explicit_trigger_block() {
        let yaml = "\
name: x
trigger:
  manual: false
  autonomous: true
nodes: []
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(!wf.trigger.manual);
        assert!(wf.trigger.autonomous);
    }

    #[test]
    fn parses_bash_node_with_script() {
        let yaml = "\
name: x
nodes:
  - id: pr
    type: bash
    script: 'gh pr create'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::Bash { script } => assert_eq!(script, "gh pr create"),
            other => panic!("expected Bash, got {other:?}"),
        }
    }

    #[test]
    fn parses_gate_node() {
        let yaml = "\
name: x
nodes:
  - id: review
    type: gate
    summary: 'waiting for human'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::Gate { summary } => assert_eq!(summary, "waiting for human"),
            other => panic!("expected Gate, got {other:?}"),
        }
    }

    #[test]
    fn parses_assert_node() {
        let yaml = "\
name: x
nodes:
  - id: must
    type: assert
    expr: 'review.decision == \"approve\"'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::Assert { expr } => assert_eq!(expr, r#"review.decision == "approve""#),
            other => panic!("expected Assert, got {other:?}"),
        }
    }

    #[test]
    fn parses_fanout_node() {
        let yaml = "\
name: x
nodes:
  - id: parallel
    type: fanout
    siblings: [a, b, c]
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::Fanout { siblings } => assert_eq!(siblings, &["a", "b", "c"]),
            other => panic!("expected Fanout, got {other:?}"),
        }
    }

    #[test]
    fn parses_full_standard_workflow_shape() {
        // The example from the plan, end-to-end. Verifies the parser
        // accepts everything fleet promises to support.
        let yaml = "\
name: standard
description: Plan → Code → Review → PR
trigger:
  manual: true
  autonomous: true
nodes:
  - id: plan
    agent: claude-code
    persona: planner
    prompt_file: prompts/planner.md
    artifacts: { out: [plan.md] }
  - id: implement
    depends_on: [plan]
    agent: claude-code
    persona: implementer
    artifacts: { in: [plan.md] }
  - id: review
    depends_on: [implement]
    agent: claude-code
    persona: reviewer
    artifacts: { in: [diff], out: [review.md] }
    outputs: { decision: review.decision }
  - id: revise
    depends_on: [review]
    when: 'review.decision == \"changes_requested\"'
    agent: claude-code
    persona: implementer
    artifacts: { in: [review.md] }
    loop_back_to: review
    max_loops: 2
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == \"approve\"'
    type: bash
    script: 'gh pr create'
  - id: human_review
    depends_on: [open_pr]
    type: gate
    summary: PR ready for human review
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert_eq!(wf.nodes.len(), 6);
        let revise = wf.node("revise").unwrap();
        assert_eq!(revise.loop_back_to.as_deref(), Some("review"));
        assert_eq!(revise.max_loops, Some(2));
        let review = wf.node("review").unwrap();
        assert_eq!(
            review.outputs.get("decision"),
            Some(&"review.decision".to_string())
        );
        assert_eq!(review.artifacts.r#in, vec!["diff"]);
        assert_eq!(review.artifacts.out, vec!["review.md"]);
    }

    #[test]
    fn missing_agent_field_on_agent_node_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: plan
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("agent node `plan` missing `agent:`"),
            "got: {msg}"
        );
    }

    #[test]
    fn missing_script_field_on_bash_node_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: pr
    type: bash
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("bash node `pr` missing `script:`"));
    }

    #[test]
    fn missing_summary_on_gate_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: g
    type: gate
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("gate node `g` missing `summary:`"));
    }

    #[test]
    fn missing_expr_on_assert_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: a
    type: assert
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("assert node `a` missing `expr:`"));
    }

    #[test]
    fn missing_siblings_on_fanout_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: f
    type: fanout
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("fanout node `f` missing `siblings:`"));
    }

    #[test]
    fn unknown_node_type_errors_with_allowed_list() {
        let yaml = "\
name: x
nodes:
  - id: weird
    type: turbocharger
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown `type: turbocharger`"));
        assert!(msg.contains("allowed: agent, bash, gate, assert, fanout, tracker-create"));
    }

    #[test]
    fn parses_tracker_create_node_with_default_link_parent() {
        let yaml = "\
name: x
nodes:
  - id: file-dep
    type: tracker-create
    from: implement.recommend_ticket
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::TrackerCreate { from, link_parent } => {
                assert_eq!(from, "implement.recommend_ticket");
                // Default is true: the v1 convention links back to
                // the bound ticket without the YAML author opting in.
                assert!(*link_parent);
            }
            other => panic!("expected TrackerCreate, got {other:?}"),
        }
    }

    #[test]
    fn parses_tracker_create_node_with_explicit_link_parent_false() {
        let yaml = "\
name: x
nodes:
  - id: file-dep
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: false
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::TrackerCreate { link_parent, .. } => {
                assert!(!*link_parent);
            }
            other => panic!("expected TrackerCreate, got {other:?}"),
        }
    }

    #[test]
    fn effective_max_recommended_tickets_falls_back_to_default_when_unset() {
        let yaml = "name: x\nnodes: []";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(wf.max_recommended_tickets.is_none());
        assert_eq!(
            wf.effective_max_recommended_tickets(),
            DEFAULT_MAX_RECOMMENDED_TICKETS
        );
    }

    #[test]
    fn effective_max_recommended_tickets_honours_explicit_value() {
        let yaml = "\
name: x
max_recommended_tickets: 1
nodes: []
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert_eq!(wf.max_recommended_tickets, Some(1));
        assert_eq!(wf.effective_max_recommended_tickets(), 1);
    }

    #[test]
    fn missing_from_on_tracker_create_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: f
    type: tracker-create
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("tracker-create node `f` missing `from:`"));
    }

    #[test]
    fn empty_workflow_name_is_rejected() {
        let yaml = "name: ''\nnodes: []";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("`name:` must be a non-empty string"));
    }

    #[test]
    fn empty_node_id_is_rejected() {
        let yaml = "\
name: x
nodes:
  - id: ''
    agent: claude
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("`id:` must be a non-empty string"));
    }

    #[test]
    fn outputs_accept_scalar_shapes_but_reject_maps() {
        // Strings, bools, numbers are fine. Maps aren't — workflows are
        // flat in v1; nested outputs would force the executor's data
        // model to grow without need.
        let scalars = "\
name: x
nodes:
  - id: n
    agent: claude
    outputs:
      a: hello
      b: 7
      c: true
";
        let wf = Workflow::from_str_at(scalars, "/x").unwrap();
        let outputs = &wf.nodes[0].outputs;
        assert_eq!(outputs.get("a"), Some(&"hello".to_string()));
        assert_eq!(outputs.get("b"), Some(&"7".to_string()));
        assert_eq!(outputs.get("c"), Some(&"true".to_string()));

        let maps = "\
name: x
nodes:
  - id: n
    agent: claude
    outputs:
      a: { nested: true }
";
        let err = Workflow::from_str_at(maps, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("outputs.a must be a scalar"));
    }

    #[test]
    fn from_path_loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wf.yaml");
        std::fs::write(
            &path,
            "name: ondisk\nnodes:\n  - id: n\n    agent: claude\n",
        )
        .unwrap();
        let wf = Workflow::from_path(&path).unwrap();
        assert_eq!(wf.name, "ondisk");
    }

    #[test]
    fn from_path_surfaces_io_error_with_path_context() {
        let err = Workflow::from_path("/no/such/wf.yaml").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/no/such/wf.yaml"), "msg = {msg}");
    }

    #[test]
    fn trigger_defaults_issueless_to_false() {
        let yaml = "name: x\nnodes:\n  - id: n\n    agent: claude\n";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(!wf.trigger.issueless);
        // Sanity: the other Trigger defaults are unchanged.
        assert!(wf.trigger.manual);
        assert!(!wf.trigger.autonomous);
    }

    #[test]
    fn trigger_parses_issueless_true_from_yaml() {
        let yaml = "\
name: audit
trigger:
  issueless: true
nodes:
  - id: scan
    agent: claude
";
        let wf = Workflow::from_str_at(yaml, "/audit.yaml").unwrap();
        assert!(wf.trigger.issueless);
    }

    #[test]
    fn trigger_rejects_autonomous_and_issueless_together() {
        // Autonomous mode only fires against open issues; a workflow
        // can't be "issueless" *and* autonomous-eligible at once. The
        // parser must reject the combo so the conflict surfaces at
        // workflow-load time rather than silently never-firing.
        let yaml = "\
name: nonsense
trigger:
  autonomous: true
  issueless: true
nodes:
  - id: n
    agent: claude
";
        let err = Workflow::from_str_at(yaml, "/x.yaml").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mutually exclusive"),
            "expected conflict message; got: {msg}",
        );
        assert!(
            msg.contains("nonsense"),
            "should name the workflow; got: {msg}"
        );
    }

    #[test]
    fn node_lookup_by_id_returns_first_match() {
        let yaml = "\
name: x
nodes:
  - id: a
    agent: claude
  - id: b
    agent: claude
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(wf.node("b").is_some());
        assert!(wf.node("c").is_none());
    }

    // === Stage 1: scheduler + PR node-kind parsing ===

    #[test]
    fn loop_field_absent_resolves_to_none() {
        let yaml = "name: x\nnodes: []";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert!(wf.loop_interval.is_none());
    }

    #[test]
    fn loop_field_parses_humantime_durations() {
        for (input, expected_secs) in [
            ("1h", 3600u64),
            ("30m", 1800),
            ("6h30m", 6 * 3600 + 30 * 60),
            ("90s", 90),
        ] {
            let yaml = format!("name: x\nloop: {input}\nnodes: []\n");
            let wf = Workflow::from_str_at(&yaml, "/x").unwrap();
            assert_eq!(
                wf.loop_interval,
                Some(Duration::from_secs(expected_secs)),
                "input `{input}` should parse to {expected_secs}s"
            );
        }
    }

    #[test]
    fn loop_rejects_durations_below_floor() {
        let yaml = "name: x\nloop: 30s\nnodes: []\n";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("below the 60s minimum"), "got: {msg}");
    }

    #[test]
    fn loop_rejects_unparseable_duration_with_humantime_hint() {
        let yaml = "name: x\nloop: 'twice a day'\nnodes: []\n";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a valid humantime duration"), "got: {msg}");
        assert!(msg.contains("1h"), "should hint humantime examples; got: {msg}");
    }

    #[test]
    fn loop_and_autonomous_together_are_rejected() {
        let yaml = "\
name: clash
trigger:
  autonomous: true
loop: 1h
nodes:
  - id: n
    agent: claude
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("mutually exclusive"), "got: {msg}");
        assert!(msg.contains("clash"), "should name the workflow; got: {msg}");
    }

    #[test]
    fn on_overlap_defaults_to_skip() {
        let yaml = "name: x\nloop: 1h\nnodes: []\n";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert_eq!(wf.on_overlap, OnOverlap::Skip);
    }

    #[test]
    fn on_overlap_parses_explicit_run() {
        let yaml = "name: x\nloop: 1h\non_overlap: run\nnodes: []\n";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        assert_eq!(wf.on_overlap, OnOverlap::Run);
    }

    #[test]
    fn parses_pr_list_with_filter_and_bind_per() {
        let yaml = "\
name: x
nodes:
  - id: pick
    type: pr-list
    filter:
      ci_status: failure
      state: open
      labels: [needs-review]
    bind: per
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::PrList { filter, bind } => {
                assert_eq!(filter.ci_status, Some(CiStatus::Failure));
                assert_eq!(filter.state, PrState::Open);
                assert_eq!(filter.labels, vec!["needs-review".to_string()]);
                assert_eq!(*bind, Some(PrBindMode::Per));
            }
            other => panic!("expected PrList, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_list_with_no_filter_and_no_bind_uses_defaults() {
        let yaml = "\
name: x
nodes:
  - id: pick
    type: pr-list
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::PrList { filter, bind } => {
                assert!(filter.ci_status.is_none());
                assert_eq!(filter.state, PrState::Open);
                assert!(filter.labels.is_empty());
                assert!(bind.is_none());
            }
            other => panic!("expected PrList, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_checks_node_with_default_pr_reference() {
        let yaml = "\
name: x
nodes:
  - id: probe
    type: pr-checks
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::PrChecks { pr } => assert!(pr.is_none()),
            other => panic!("expected PrChecks, got {other:?}"),
        }
    }

    #[test]
    fn parses_pr_checks_node_with_explicit_pr_reference() {
        let yaml = "\
name: x
nodes:
  - id: probe
    type: pr-checks
    pr: '${pick.prs.0.number}'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::PrChecks { pr } => assert_eq!(pr.as_deref(), Some("${pick.prs.0.number}")),
            other => panic!("expected PrChecks, got {other:?}"),
        }
    }

    #[test]
    fn parses_create_pr_with_all_fields() {
        let yaml = "\
name: x
nodes:
  - id: open_pr
    type: create-pr
    title: 'fleet change'
    body: 'see commit'
    base: main
    head: feature/foo
    draft: true
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::CreatePr {
                title,
                body,
                base,
                head,
                draft,
            } => {
                assert_eq!(title, "fleet change");
                assert_eq!(body, "see commit");
                assert_eq!(base.as_deref(), Some("main"));
                assert_eq!(head.as_deref(), Some("feature/foo"));
                assert!(*draft);
            }
            other => panic!("expected CreatePr, got {other:?}"),
        }
    }

    #[test]
    fn parses_create_pr_with_defaulted_base_head_draft() {
        let yaml = "\
name: x
nodes:
  - id: open_pr
    type: create-pr
    title: t
    body: b
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::CreatePr {
                base, head, draft, ..
            } => {
                assert!(base.is_none());
                assert!(head.is_none());
                assert!(!*draft);
            }
            other => panic!("expected CreatePr, got {other:?}"),
        }
    }

    #[test]
    fn create_pr_missing_title_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: open_pr
    type: create-pr
    body: b
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("create-pr node `open_pr` missing `title:`"));
    }

    #[test]
    fn create_pr_missing_body_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: open_pr
    type: create-pr
    title: t
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("create-pr node `open_pr` missing `body:`"));
    }

    #[test]
    fn parses_pr_comment_with_explicit_pr_reference() {
        let yaml = "\
name: x
nodes:
  - id: announce
    type: pr-comment
    body: 'fleet pushed a fix'
    pr: '42'
";
        let wf = Workflow::from_str_at(yaml, "/x").unwrap();
        match &wf.nodes[0].kind {
            NodeKind::PrComment { body, pr } => {
                assert_eq!(body, "fleet pushed a fix");
                assert_eq!(pr.as_deref(), Some("42"));
            }
            other => panic!("expected PrComment, got {other:?}"),
        }
    }

    #[test]
    fn pr_comment_missing_body_errors_clearly() {
        let yaml = "\
name: x
nodes:
  - id: announce
    type: pr-comment
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        assert!(format!("{err:#}").contains("pr-comment node `announce` missing `body:`"));
    }

    #[test]
    fn unknown_node_type_error_lists_pr_kinds_in_allowed() {
        let yaml = "\
name: x
nodes:
  - id: weird
    type: turbocharger
";
        let err = Workflow::from_str_at(yaml, "/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("pr-list"), "got: {msg}");
        assert!(msg.contains("create-pr"), "got: {msg}");
    }
}
