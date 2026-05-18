//! Per-repo fleet configuration: `.fleet/config.yaml`.
//!
//! Models the YAML schema scaffolded by [`crate::cli::init`] and consumed
//! by every workflow / runtime / autonomous command.
//!
//! Design choices:
//! - All fields are optional in YAML; missing keys fall back to compiled-in
//!   defaults (each [`Default`] impl documents what those are). This keeps
//!   `fleet init`'s template minimal *and* lets the binary run in any repo
//!   that even has an empty `.fleet/config.yaml`.
//! - Unknown fields are accepted silently. The plan's schema is still
//!   evolving; rejecting unknown keys would break users on every release.
//!   When a field's type is wrong (e.g. `adapter: 42`), serde surfaces the
//!   error with file-path context.
//! - Enum variants use lowercase / kebab-case so the on-disk YAML reads
//!   like the documented schema (`adapter: apple-container`, not
//!   `AppleContainer`).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::agent::AgentRegistry;

/// Top-level repo config. Held by callers and threaded down into the runtime
/// factory, agent registry, etc.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoConfig {
    pub runtime: RuntimeConfig,
    pub tracker: Tracker,
    pub agents: AgentsConfig,
    pub workflows: WorkflowsConfig,
    pub autonomous: AutonomousConfig,
    pub cleanup: CleanupConfig,
    pub cost: CostConfig,
    pub orchestrator: OrchestratorConfig,
    /// Per-name secret-backend declarations. Each entry maps an env
    /// var name (e.g. `claude_code_oauth_token`, `gh_token`) to the
    /// backend that resolves it. Used by [`crate::workflow::executor::build_agent_env`]
    /// — when an agent's `env_passthrough` lists a name with a
    /// matching entry here, the resolved secret is injected as the
    /// container env value. Empty by default.
    pub secrets: std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig>,
}

/// `runtime:` block — which adapter to instantiate, how to harden it, where
/// the devcontainer lives, and the network egress policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub adapter: AdapterChoice,
    pub hardening: HardeningChoice,
    /// Relative to the repo root. `fleet init` defaults to
    /// `.devcontainer/devcontainer.json`.
    pub devcontainer: PathBuf,
    /// `runtime.network` — egress policy applied to every workflow
    /// container. Default is `open` (no enforcement) so a fresh
    /// install still works; security-conscious users opt into
    /// `allowlist` per-repo.
    pub network: NetworkConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            adapter: AdapterChoice::Auto,
            hardening: HardeningChoice::Auto,
            devcontainer: PathBuf::from(".devcontainer/devcontainer.json"),
            network: NetworkConfig::default(),
        }
    }
}

/// `runtime.network:` block — per-workflow egress enforcement.
///
/// - `policy: open` (default): no enforcement; the container has full
///   outbound access (whatever the engine and host allow). Matches the
///   pre-Phase 3 behaviour.
/// - `policy: none`: hard-block all egress. Useful for fully-offline
///   workflows that should never touch the network. Implementation:
///   the adapter pins the container to an isolated network with no
///   route to the outside.
/// - `policy: allowlist`: route egress through a proxy that admits
///   only the hosts in `extra_hosts` plus fleet's built-in defaults
///   (tracker host + LLM provider host the configured agents need).
///
/// The set of "built-in defaults" is derived at proxy-setup time from
/// the rest of the config — see [`NetworkConfig::resolved_allowlist`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    pub policy: NetworkPolicy,
    /// Hosts admitted in addition to the built-in defaults. Wildcards
    /// are *not* expanded; `*.crates.io` is a literal string match
    /// against the SNI / Host header. The proxy implementation may
    /// or may not honour wildcards depending on backend.
    pub extra_hosts: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            policy: NetworkPolicy::Open,
            extra_hosts: Vec::new(),
        }
    }
}

impl NetworkConfig {
    /// Effective allowlist for the current run: `extra_hosts` plus
    /// fleet-supplied defaults (LLM provider hosts implied by the
    /// agents registry, tracker host). The defaults are computed
    /// elsewhere; this getter is the call site we'd extend if the
    /// schema later supports `omit_defaults: true`.
    #[must_use]
    #[allow(dead_code)]
    pub fn extra_hosts(&self) -> &[String] {
        &self.extra_hosts
    }
}

/// Egress policy variants. Surfaced verbatim in the doctor view so
/// users can see what's in force on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkPolicy {
    #[default]
    Open,
    None,
    Allowlist,
}

impl NetworkPolicy {
    #[must_use]
    #[allow(dead_code)]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::None => "none",
            Self::Allowlist => "allowlist",
        }
    }
}

/// Adapter selection from `runtime.adapter`. `Auto` resolves at factory time
/// against the probe report; explicit choices are pinned regardless of what's
/// installed (the factory surfaces a clear error if the pinned engine is
/// missing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterChoice {
    #[default]
    Auto,
    Podman,
    Docker,
    AppleContainer,
    Local,
}

impl AdapterChoice {
    /// Stable display string for the doctor view and config errors. Mirrors
    /// the on-disk YAML wording so users can grep for the value they wrote.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Podman => "podman",
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
            Self::Local => "local",
        }
    }
}

/// Hardening selection from `runtime.hardening`. `Auto` means "use the strongest
/// hardening the chosen adapter supports on this host" — concretely: enable
/// gVisor on Podman when `runsc` is present; do nothing otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HardeningChoice {
    #[default]
    Auto,
    Gvisor,
    None,
}

impl HardeningChoice {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Gvisor => "gvisor",
            Self::None => "none",
        }
    }
}

/// `tracker:` — issue source. Phase 1 ships git-bug and GitHub; Linear and
/// Jira are placeholders until their adapters land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tracker {
    #[default]
    GitBug,
    Github,
    Linear,
    Jira,
}

impl Tracker {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitBug => "git-bug",
            Self::Github => "github",
            Self::Linear => "linear",
            Self::Jira => "jira",
        }
    }
}

/// `agents:` block. `default` names the agent used when a workflow node
/// doesn't specify one; `registry` overlays user-authored entries on top
/// of the built-in defaults (see [`AgentRegistry::with_overrides`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsConfig {
    pub default: String,
    pub registry: AgentRegistry,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default: "claude-code".to_string(),
            registry: AgentRegistry::default(),
        }
    }
}

/// `workflows:` block. Same Phase-1 minimalism — only the default workflow
/// name is consumed today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowsConfig {
    pub default: String,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            default: "standard".to_string(),
        }
    }
}

/// `autonomous:` block — bounds + cadence for the TUI's `Shift+A`
/// autonomous mode. All fields optional; defaults are conservative
/// enough that a user who flips the toggle without configuring
/// anything still gets a sane supervisor. [`Self::filter_labels`]
/// gates which open tickets the supervisor considers, and the same
/// list is stamped onto tickets fleet creates so the loop sees its
/// own follow-ups on the next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutonomousConfig {
    /// Maximum concurrent in-flight workflow runs autonomous mode is
    /// allowed to drive. In-flight = `Running` or `AwaitingGate`.
    pub max_parallel: u32,
    /// Default workflow fired when no [`Self::routing`] rule matches a
    /// candidate issue. Must exist under `.fleet/workflows/` and have
    /// `trigger.autonomous: true`.
    pub workflow: String,
    /// Minimum seconds between tracker scans. Real `gh` / `git-bug`
    /// shells cost hundreds of ms — debouncing keeps the TUI cheap.
    pub scan_interval_secs: u32,
    /// Seconds to wait after spawning before the engine considers the
    /// next slot. Lets `fleet workflow run` materialise its session
    /// row so claim-avoidance sees the new in-flight session.
    pub spawn_cooldown_secs: u32,
    /// Label-driven workflow selection. Rules are tried in order;
    /// the first whose `labels` intersect the issue's labels wins.
    /// Empty = always use [`Self::workflow`]. A rule with empty
    /// `labels` is a wildcard match — useful for staging a workflow
    /// override last in the list as a catch-all.
    pub routing: Vec<RoutingRule>,
    /// Repo-wide label gate. To be a candidate for the supervisor, an
    /// open ticket must carry **every** label in this list (ALL
    /// semantics — see [`Self::ticket_matches_filter`]). The same
    /// list is unioned onto every ticket fleet creates (via
    /// `fleet issues create` or a `tracker-create` workflow node) so
    /// fleet-spawned follow-ups pass the gate on the next tick — see
    /// [`Self::stamp_creation_labels`]. Empty (the default) =
    /// pre-feature behaviour: no filter and no stamp, byte-for-byte.
    ///
    /// Scope is intentionally limited to the autonomous tick + the
    /// two ticket-creation call sites. `fleet issues list` and the
    /// TUI spawn modal stay unfiltered so humans can deliberately
    /// pick non-matching tickets.
    pub filter_labels: Vec<String>,
}

/// One label-driven routing rule: if any of `labels` matches a label
/// on the candidate issue, fire `workflow` instead of the autonomous
/// default. Order in the config file is the precedence order — first
/// match wins. Empty `labels` matches every issue (catch-all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRule {
    pub labels: Vec<String>,
    pub workflow: String,
}

impl AutonomousConfig {
    /// Pick the workflow to fire for an issue whose labels are
    /// `issue_labels`. Walks [`Self::routing`] in order; first rule
    /// whose `labels` intersect wins. Falls back to
    /// [`Self::workflow`]. Pure; the engine consults it per
    /// candidate.
    #[must_use]
    pub fn resolve_workflow_for(&self, issue_labels: &[String]) -> &str {
        for rule in &self.routing {
            if rule_matches(rule, issue_labels) {
                return &rule.workflow;
            }
        }
        &self.workflow
    }

    /// True if `issue_labels` carries every label in
    /// [`Self::filter_labels`]. An empty filter is a no-op (returns
    /// true), preserving the pre-feature behaviour where every open
    /// ticket is a candidate. Pure; the autonomous tick consults it
    /// per candidate before passing the list to `rank_candidates_by_plan`.
    #[must_use]
    pub fn ticket_matches_filter(&self, issue_labels: &[String]) -> bool {
        self.filter_labels
            .iter()
            .all(|needed| issue_labels.iter().any(|have| have == needed))
    }

    /// Merge [`Self::filter_labels`] into `supplied`, preserving the
    /// caller's order and appending any missing filter labels in
    /// config order. Used at both ticket-creation call sites
    /// (`fleet issues create` and the `tracker-create` workflow node)
    /// so a fleet-spawned ticket always carries the labels needed to
    /// pass [`Self::ticket_matches_filter`] on the next tick.
    #[must_use]
    pub fn stamp_creation_labels(&self, supplied: &[String]) -> Vec<String> {
        let mut out: Vec<String> = supplied.to_vec();
        for needed in &self.filter_labels {
            if !out.iter().any(|l| l == needed) {
                out.push(needed.clone());
            }
        }
        out
    }
}

/// A rule matches when its `labels` is empty (wildcard) or when at
/// least one of its labels is present in the issue's labels. Free
/// function so the policy is unit-tested without a full
/// `AutonomousConfig`.
fn rule_matches(rule: &RoutingRule, issue_labels: &[String]) -> bool {
    if rule.labels.is_empty() {
        return true;
    }
    rule.labels
        .iter()
        .any(|l| issue_labels.iter().any(|il| il == l))
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            max_parallel: 3,
            workflow: "standard".to_string(),
            scan_interval_secs: 10,
            spawn_cooldown_secs: 2,
            routing: Vec::new(),
            filter_labels: Vec::new(),
        }
    }
}

/// `cleanup:` block — opt-in disk-hygiene knobs. Conservative
/// defaults: nothing auto-prunes unless the user asks. Add a field
/// here when introducing a new cleanup behaviour we want gated
/// behind a config switch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupConfig {
    /// When `true`, a workflow that reaches `Completed` triggers the
    /// equivalent of `fleet sessions prune <id>` automatically. The
    /// branch + logs + artifacts are still preserved; only the
    /// worktree dir is reclaimed. Default `false` (keep until
    /// explicit prune).
    pub auto_prune_completed: bool,
}

/// `orchestrator:` block — interactive planning-session config.
/// `agent` is the binary fleet exec's inside the tmux pane (e.g.
/// `claude`, `claude-code`, `aider`, ...). Defaults to `claude`
/// because that's what most fleet users are running today; the
/// orchestrator prompt assumes Claude Code's tool-use semantics but
/// any agent that takes a system prompt + shell access works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorConfig {
    /// Command fleet exec's. A bare program name (no spaces) so
    /// the tmux invocation stays simple. v1 doesn't support
    /// multi-word commands; embed any flags via a wrapper script.
    pub agent: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            agent: String::from("claude"),
        }
    }
}

/// `cost:` block — spend guardrails. Both fields default to `None`
/// (no budget). Budgets are *hard stops on new agent spawns*; an
/// in-flight agent isn't interrupted mid-run when the budget is
/// crossed. Honest scope: no projection logic — fleet counts
/// reported cost after each agent finishes; the next agent's start
/// checks the totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostConfig {
    /// Maximum USD this *session* may spend across all its agent
    /// nodes. The next agent node's start is refused when the
    /// session's `total_cost_usd()` already meets or exceeds this
    /// value. `None` = unlimited.
    pub per_session_budget_usd: Option<f64>,
    /// Maximum USD all sessions in this repo may have spent in
    /// total. The next agent node's start is refused when the
    /// lifetime sum already meets or exceeds this value. Computed
    /// at agent-start time by walking the session store, so the
    /// check sees real costs (not projections). `None` = unlimited.
    pub lifetime_budget_usd: Option<f64>,
}

impl RepoConfig {
    /// Parse a config from a string. `path_hint` is used purely for error
    /// context (so the user sees which file failed); it does not have to
    /// exist on disk. Empty input yields the full default config — this
    /// matches the "drop a `.fleet/config.yaml` and it Just Works" UX.
    pub fn from_str_at(input: &str, path_hint: impl AsRef<Path>) -> Result<Self> {
        let hint = path_hint.as_ref();
        if input.trim().is_empty() {
            return Ok(Self::default());
        }
        let raw: Raw = serde_yml::from_str(input)
            .with_context(|| format!("parsing fleet repo config at {}", hint.display()))?;
        Ok(raw.into())
    }

    /// Load from disk. Returns the default config (not an error) when the
    /// file is missing — repos may legitimately predate `fleet init`. IO
    /// errors other than "not found" surface unchanged with file-path context.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_str_at(&s, path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(anyhow::anyhow!(err))
                .with_context(|| format!("reading fleet repo config at {}", path.display())),
        }
    }
}

// Raw deserialisation form: every field optional so partial configs are valid.
#[derive(Deserialize, Default)]
struct Raw {
    #[serde(default)]
    runtime: Option<RawRuntime>,
    #[serde(default)]
    tracker: Option<Tracker>,
    #[serde(default)]
    agents: Option<RawAgents>,
    #[serde(default)]
    workflows: Option<RawWorkflows>,
    #[serde(default)]
    autonomous: Option<RawAutonomous>,
    #[serde(default)]
    cleanup: Option<RawCleanup>,
    #[serde(default)]
    cost: Option<RawCost>,
    #[serde(default)]
    orchestrator: Option<RawOrchestrator>,
    #[serde(default)]
    secrets: Option<std::collections::BTreeMap<String, crate::secrets::SecretBackendConfig>>,
}

#[derive(Deserialize, Default)]
struct RawOrchestrator {
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawCleanup {
    #[serde(default)]
    auto_prune_completed: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawCost {
    #[serde(default)]
    per_session_budget_usd: Option<f64>,
    #[serde(default)]
    lifetime_budget_usd: Option<f64>,
}

#[derive(Deserialize, Default)]
struct RawAutonomous {
    #[serde(default)]
    max_parallel: Option<u32>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    scan_interval_secs: Option<u32>,
    #[serde(default)]
    spawn_cooldown_secs: Option<u32>,
    /// Routing rules parsed as `[{ labels: [...], workflow: "..." }]`.
    /// Absent in the YAML → empty Vec → engine always uses the default
    /// workflow, matching pre-routing behaviour exactly.
    #[serde(default)]
    routing: Option<Vec<RawRoutingRule>>,
    /// Repo-wide label gate. Absent in the YAML → empty Vec → no
    /// filtering and no creation-stamp, matching pre-feature
    /// behaviour exactly.
    #[serde(default)]
    filter_labels: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawRoutingRule {
    #[serde(default)]
    labels: Option<Vec<String>>,
    workflow: String,
}

#[derive(Deserialize, Default)]
struct RawRuntime {
    #[serde(default)]
    adapter: Option<AdapterChoice>,
    #[serde(default)]
    hardening: Option<HardeningChoice>,
    #[serde(default)]
    devcontainer: Option<PathBuf>,
    #[serde(default)]
    network: Option<RawNetwork>,
}

#[derive(Deserialize, Default)]
struct RawNetwork {
    #[serde(default)]
    policy: Option<NetworkPolicy>,
    #[serde(default)]
    extra_hosts: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawAgents {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    registry: Option<AgentRegistry>,
}

#[derive(Deserialize, Default)]
struct RawWorkflows {
    #[serde(default)]
    default: Option<String>,
}

impl From<Raw> for RepoConfig {
    #[allow(clippy::too_many_lines)]
    fn from(raw: Raw) -> Self {
        // Destructure defaults up front so each field can be moved into the
        // matching `unwrap_or(...)` slot without aliasing the same struct
        // twice (which would trip the borrow checker on the closure paths).
        let Self {
            runtime: default_runtime,
            tracker: default_tracker,
            agents: default_agents,
            workflows: default_workflows,
            autonomous: default_autonomous,
            cleanup: default_cleanup,
            cost: default_cost,
            orchestrator: default_orchestrator,
            secrets: default_secrets,
        } = Self::default();

        let runtime = match raw.runtime {
            Some(r) => {
                let network = match r.network {
                    Some(n) => NetworkConfig {
                        policy: n.policy.unwrap_or(default_runtime.network.policy),
                        extra_hosts: n.extra_hosts.unwrap_or(default_runtime.network.extra_hosts),
                    },
                    None => default_runtime.network,
                };
                RuntimeConfig {
                    adapter: r.adapter.unwrap_or(default_runtime.adapter),
                    hardening: r.hardening.unwrap_or(default_runtime.hardening),
                    devcontainer: r.devcontainer.unwrap_or(default_runtime.devcontainer),
                    network,
                }
            }
            None => default_runtime,
        };
        let tracker = raw.tracker.unwrap_or(default_tracker);
        let agents = match raw.agents {
            Some(a) => {
                let default = a.default.unwrap_or(default_agents.default);
                let registry = a.registry.map_or(default_agents.registry, |user| {
                    // Overlay user entries on the defaults so the
                    // built-in `claude-code` survives unless the user
                    // explicitly replaces it.
                    AgentRegistry::default().with_overrides(user)
                });
                AgentsConfig { default, registry }
            }
            None => default_agents,
        };
        let workflows = match raw.workflows {
            Some(w) => WorkflowsConfig {
                default: w.default.unwrap_or(default_workflows.default),
            },
            None => default_workflows,
        };
        let autonomous = match raw.autonomous {
            Some(a) => AutonomousConfig {
                max_parallel: a.max_parallel.unwrap_or(default_autonomous.max_parallel),
                workflow: a.workflow.unwrap_or(default_autonomous.workflow),
                scan_interval_secs: a
                    .scan_interval_secs
                    .unwrap_or(default_autonomous.scan_interval_secs),
                spawn_cooldown_secs: a
                    .spawn_cooldown_secs
                    .unwrap_or(default_autonomous.spawn_cooldown_secs),
                routing: a.routing.map_or(default_autonomous.routing, |raw_rules| {
                    raw_rules
                        .into_iter()
                        .map(|r| RoutingRule {
                            labels: r.labels.unwrap_or_default(),
                            workflow: r.workflow,
                        })
                        .collect()
                }),
                filter_labels: a.filter_labels.unwrap_or(default_autonomous.filter_labels),
            },
            None => default_autonomous,
        };
        let cleanup = match raw.cleanup {
            Some(c) => CleanupConfig {
                auto_prune_completed: c
                    .auto_prune_completed
                    .unwrap_or(default_cleanup.auto_prune_completed),
            },
            None => default_cleanup,
        };
        let cost = match raw.cost {
            Some(c) => CostConfig {
                per_session_budget_usd: c
                    .per_session_budget_usd
                    .or(default_cost.per_session_budget_usd),
                lifetime_budget_usd: c.lifetime_budget_usd.or(default_cost.lifetime_budget_usd),
            },
            None => default_cost,
        };
        let orchestrator = match raw.orchestrator {
            Some(b) => OrchestratorConfig {
                agent: b.agent.unwrap_or(default_orchestrator.agent),
            },
            None => default_orchestrator,
        };
        let secrets = raw.secrets.unwrap_or(default_secrets);
        Self {
            runtime,
            tracker,
            agents,
            workflows,
            autonomous,
            cleanup,
            cost,
            orchestrator,
            secrets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_fleet_init_scaffold() {
        let cfg = RepoConfig::default();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Auto);
        assert_eq!(cfg.runtime.hardening, HardeningChoice::Auto);
        assert_eq!(
            cfg.runtime.devcontainer,
            PathBuf::from(".devcontainer/devcontainer.json")
        );
        assert_eq!(cfg.tracker, Tracker::GitBug);
        assert_eq!(cfg.agents.default, "claude-code");
        assert_eq!(cfg.workflows.default, "standard");
        assert_eq!(cfg.autonomous.max_parallel, 3);
        assert_eq!(cfg.autonomous.workflow, "standard");
        assert_eq!(cfg.autonomous.scan_interval_secs, 10);
        assert_eq!(cfg.autonomous.spawn_cooldown_secs, 2);
        assert!(
            !cfg.cleanup.auto_prune_completed,
            "default must be keep-until-explicit-prune"
        );
    }

    #[test]
    fn parses_cleanup_auto_prune_completed_true() {
        let yaml = "cleanup:\n  auto_prune_completed: true\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert!(cfg.cleanup.auto_prune_completed);
    }

    #[test]
    fn missing_cleanup_block_falls_back_to_defaults() {
        // Pre-cleanup-feature configs (and the `fleet init` scaffold)
        // omit `cleanup:` entirely. Default behaviour: nothing
        // auto-prunes.
        let yaml = "tracker: github\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert!(!cfg.cleanup.auto_prune_completed);
    }

    #[test]
    fn orchestrator_agent_defaults_to_claude() {
        let cfg = RepoConfig::default();
        assert_eq!(cfg.orchestrator.agent, "claude");
    }

    #[test]
    fn orchestrator_agent_can_be_overridden_per_repo() {
        let yaml = "orchestrator:\n  agent: aider\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.orchestrator.agent, "aider");
    }

    #[test]
    fn missing_orchestrator_block_keeps_the_default() {
        let yaml = "tracker: github\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.orchestrator.agent, "claude");
    }

    #[test]
    fn parses_cost_budgets_both_fields() {
        let yaml = "\
cost:
  per_session_budget_usd: 5.0
  lifetime_budget_usd: 100.0
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.cost.per_session_budget_usd, Some(5.0));
        assert_eq!(cfg.cost.lifetime_budget_usd, Some(100.0));
    }

    #[test]
    fn parses_cost_budgets_partial() {
        // Only lifetime — per-session stays unlimited.
        let yaml = "\
cost:
  lifetime_budget_usd: 50.0
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.cost.per_session_budget_usd, None);
        assert_eq!(cfg.cost.lifetime_budget_usd, Some(50.0));
    }

    #[test]
    fn missing_cost_block_falls_back_to_unlimited_defaults() {
        let yaml = "tracker: github\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.cost.per_session_budget_usd, None);
        assert_eq!(cfg.cost.lifetime_budget_usd, None);
    }

    #[test]
    fn parses_full_autonomous_block() {
        let yaml = "\
autonomous:
  max_parallel: 5
  workflow: hotfix
  scan_interval_secs: 30
  spawn_cooldown_secs: 5
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.autonomous.max_parallel, 5);
        assert_eq!(cfg.autonomous.workflow, "hotfix");
        assert_eq!(cfg.autonomous.scan_interval_secs, 30);
        assert_eq!(cfg.autonomous.spawn_cooldown_secs, 5);
    }

    #[test]
    fn partial_autonomous_fills_remaining_with_defaults() {
        let yaml = "autonomous:\n  max_parallel: 1\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.autonomous.max_parallel, 1);
        // Unspecified fields fall back to defaults.
        assert_eq!(cfg.autonomous.workflow, "standard");
        assert_eq!(cfg.autonomous.scan_interval_secs, 10);
        assert_eq!(cfg.autonomous.spawn_cooldown_secs, 2);
        assert!(cfg.autonomous.routing.is_empty());
    }

    #[test]
    fn routing_absent_yields_empty_vec() {
        // Pre-routing configs must still parse and behave exactly as
        // before — the engine falls back to autonomous.workflow.
        let yaml = "autonomous:\n  workflow: standard\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert!(cfg.autonomous.routing.is_empty());
    }

    #[test]
    fn routing_parses_full_block() {
        let yaml = "\
autonomous:
  workflow: standard
  routing:
    - labels: [bug, hotfix]
      workflow: hotfix
    - labels: [docs]
      workflow: docs-only
    - workflow: catch-all
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.autonomous.routing.len(), 3);
        assert_eq!(cfg.autonomous.routing[0].labels, vec!["bug", "hotfix"]);
        assert_eq!(cfg.autonomous.routing[0].workflow, "hotfix");
        assert_eq!(cfg.autonomous.routing[1].labels, vec!["docs"]);
        assert_eq!(cfg.autonomous.routing[1].workflow, "docs-only");
        // Catch-all: no `labels:` key → empty Vec → matches every issue.
        assert!(cfg.autonomous.routing[2].labels.is_empty());
        assert_eq!(cfg.autonomous.routing[2].workflow, "catch-all");
    }

    #[test]
    fn resolve_workflow_no_rules_returns_default() {
        let cfg = AutonomousConfig::default();
        assert_eq!(cfg.resolve_workflow_for(&["bug".to_string()]), "standard");
        assert_eq!(cfg.resolve_workflow_for(&[]), "standard");
    }

    #[test]
    fn resolve_workflow_first_matching_rule_wins() {
        let cfg = AutonomousConfig {
            routing: vec![
                RoutingRule {
                    labels: vec!["bug".to_string()],
                    workflow: "hotfix".to_string(),
                },
                RoutingRule {
                    labels: vec!["bug".to_string()],
                    workflow: "later".to_string(),
                },
            ],
            ..AutonomousConfig::default()
        };
        assert_eq!(
            cfg.resolve_workflow_for(&["bug".to_string()]),
            "hotfix",
            "first matching rule must win"
        );
    }

    #[test]
    fn resolve_workflow_returns_default_when_no_rule_matches() {
        let cfg = AutonomousConfig {
            routing: vec![RoutingRule {
                labels: vec!["bug".to_string()],
                workflow: "hotfix".to_string(),
            }],
            ..AutonomousConfig::default()
        };
        assert_eq!(
            cfg.resolve_workflow_for(&["feature".to_string()]),
            "standard"
        );
        assert_eq!(cfg.resolve_workflow_for(&[]), "standard");
    }

    #[test]
    fn resolve_workflow_intersects_rather_than_subset() {
        // Issue with multiple labels matches a rule that names any
        // one of them. Designed for the common case "open issues are
        // tagged with several labels; we route on the most specific
        // one we know about."
        let cfg = AutonomousConfig {
            routing: vec![RoutingRule {
                labels: vec!["hotfix".to_string()],
                workflow: "fast-track".to_string(),
            }],
            ..AutonomousConfig::default()
        };
        assert_eq!(
            cfg.resolve_workflow_for(&[
                "p1".to_string(),
                "hotfix".to_string(),
                "backend".to_string(),
            ]),
            "fast-track"
        );
    }

    #[test]
    fn filter_labels_default_is_empty_and_matches_everything() {
        let cfg = AutonomousConfig::default();
        assert!(cfg.filter_labels.is_empty());
        assert!(cfg.ticket_matches_filter(&[]));
        assert!(cfg.ticket_matches_filter(&["anything".to_string()]));
    }

    #[test]
    fn filter_labels_all_must_match_to_pass() {
        let cfg = AutonomousConfig {
            filter_labels: vec!["fleet".to_string(), "agent".to_string()],
            ..AutonomousConfig::default()
        };
        // Exact set or superset → match.
        assert!(cfg.ticket_matches_filter(&["fleet".to_string(), "agent".to_string()]));
        assert!(cfg.ticket_matches_filter(&[
            "fleet".to_string(),
            "agent".to_string(),
            "p1".to_string(),
        ]));
        // Subset, single label, or empty → drop.
        assert!(!cfg.ticket_matches_filter(&["fleet".to_string()]));
        assert!(!cfg.ticket_matches_filter(&["agent".to_string()]));
        assert!(!cfg.ticket_matches_filter(&[]));
        // Unrelated labels alone → drop.
        assert!(!cfg.ticket_matches_filter(&["p1".to_string(), "docs".to_string()]));
    }

    #[test]
    fn parses_filter_labels_block_from_yaml() {
        let yaml = "\
autonomous:
  workflow: standard
  filter_labels: [fleet, agent]
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(
            cfg.autonomous.filter_labels,
            vec!["fleet".to_string(), "agent".to_string()]
        );
    }

    #[test]
    fn filter_labels_absent_yields_empty_vec() {
        // Pre-feature configs must still parse and behave exactly as
        // before — empty Vec → no filtering, no stamping.
        let yaml = "autonomous:\n  workflow: standard\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert!(cfg.autonomous.filter_labels.is_empty());
    }

    #[test]
    fn stamp_creation_labels_unions_preserving_supplied_order() {
        let cfg = AutonomousConfig {
            filter_labels: vec!["fleet".to_string(), "agent".to_string()],
            ..AutonomousConfig::default()
        };
        // Supplied labels keep their order; missing filter labels are
        // appended in config order.
        assert_eq!(
            cfg.stamp_creation_labels(&["p1".to_string()]),
            vec!["p1".to_string(), "fleet".to_string(), "agent".to_string()]
        );
        assert_eq!(
            cfg.stamp_creation_labels(&[]),
            vec!["fleet".to_string(), "agent".to_string()]
        );
    }

    #[test]
    fn stamp_creation_labels_dedups() {
        let cfg = AutonomousConfig {
            filter_labels: vec!["fleet".to_string(), "agent".to_string()],
            ..AutonomousConfig::default()
        };
        // `fleet` is already supplied — must not appear twice.
        assert_eq!(
            cfg.stamp_creation_labels(&["fleet".to_string(), "p1".to_string()]),
            vec!["fleet".to_string(), "p1".to_string(), "agent".to_string()]
        );
        // Every filter label already supplied — no growth, no reorder.
        assert_eq!(
            cfg.stamp_creation_labels(&["agent".to_string(), "fleet".to_string()]),
            vec!["agent".to_string(), "fleet".to_string()]
        );
    }

    #[test]
    fn stamp_creation_labels_empty_filter_returns_supplied_unchanged() {
        let cfg = AutonomousConfig::default();
        assert!(cfg.filter_labels.is_empty());
        assert_eq!(
            cfg.stamp_creation_labels(&["p1".to_string(), "docs".to_string()]),
            vec!["p1".to_string(), "docs".to_string()]
        );
        assert!(cfg.stamp_creation_labels(&[]).is_empty());
    }

    #[test]
    fn resolve_workflow_empty_labels_rule_is_wildcard() {
        // A rule with `labels: []` matches every issue. The intended
        // use is "default override" — staged last in the list as a
        // catch-all.
        let cfg = AutonomousConfig {
            routing: vec![
                RoutingRule {
                    labels: vec!["bug".to_string()],
                    workflow: "hotfix".to_string(),
                },
                RoutingRule {
                    labels: Vec::new(),
                    workflow: "fallback".to_string(),
                },
            ],
            ..AutonomousConfig::default()
        };
        // Untagged issue falls through to the wildcard, not the
        // top-level default.
        assert_eq!(cfg.resolve_workflow_for(&[]), "fallback");
        // Tagged issue still hits the earlier specific rule.
        assert_eq!(cfg.resolve_workflow_for(&["bug".to_string()]), "hotfix");
    }

    #[test]
    fn empty_input_yields_defaults() {
        let cfg = RepoConfig::from_str_at("", "/repo/.fleet/config.yaml").unwrap();
        assert_eq!(cfg, RepoConfig::default());
    }

    #[test]
    fn whitespace_only_input_yields_defaults() {
        let cfg = RepoConfig::from_str_at("   \n  \n", "/x").unwrap();
        assert_eq!(cfg, RepoConfig::default());
    }

    #[test]
    fn parses_full_config() {
        let yaml = "\
runtime:
  adapter: podman
  hardening: gvisor
  devcontainer: .devcontainer/rust.json
tracker: github
agents:
  default: aider
workflows:
  default: hotfix
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Podman);
        assert_eq!(cfg.runtime.hardening, HardeningChoice::Gvisor);
        assert_eq!(
            cfg.runtime.devcontainer,
            PathBuf::from(".devcontainer/rust.json")
        );
        assert_eq!(cfg.tracker, Tracker::Github);
        assert_eq!(cfg.agents.default, "aider");
        assert_eq!(cfg.workflows.default, "hotfix");
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let yaml = "runtime:\n  adapter: docker\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Docker);
        // Other runtime fields fall back to defaults.
        assert_eq!(cfg.runtime.hardening, HardeningChoice::Auto);
        assert_eq!(
            cfg.runtime.devcontainer,
            PathBuf::from(".devcontainer/devcontainer.json")
        );
        // Other top-level sections also default.
        assert_eq!(cfg.tracker, Tracker::GitBug);
        assert_eq!(cfg.agents.default, "claude-code");
    }

    #[test]
    fn accepts_apple_container_kebab_case() {
        let yaml = "runtime:\n  adapter: apple-container\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::AppleContainer);
    }

    #[test]
    fn accepts_local_adapter() {
        let yaml = "runtime:\n  adapter: local\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Local);
    }

    #[test]
    fn rejects_unknown_adapter_value_with_path_context() {
        let yaml = "runtime:\n  adapter: turbocharger\n";
        let err = RepoConfig::from_str_at(yaml, "/repo/.fleet/config.yaml").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/repo/.fleet/config.yaml"),
            "error must mention the file path, got: {msg}"
        );
    }

    #[test]
    fn tolerates_unknown_top_level_keys() {
        let yaml = "\
runtime:
  adapter: docker
autonomous:
  max_parallel: 3
unknown_root_key:
  whatever: 42
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Docker);
    }

    #[test]
    fn defaults_network_policy_to_open() {
        // No network block in YAML → no enforcement, no extra hosts.
        // Matches pre-Phase 3 behaviour exactly.
        let cfg = RepoConfig::from_str_at("runtime:\n  adapter: docker\n", "/x").unwrap();
        assert_eq!(cfg.runtime.network.policy, NetworkPolicy::Open);
        assert!(cfg.runtime.network.extra_hosts.is_empty());
    }

    #[test]
    fn parses_network_allowlist_block() {
        let yaml = "\
runtime:
  adapter: podman
  network:
    policy: allowlist
    extra_hosts:
      - api.github.com
      - crates.io
      - pypi.org
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.network.policy, NetworkPolicy::Allowlist);
        assert_eq!(
            cfg.runtime.network.extra_hosts,
            vec![
                "api.github.com".to_string(),
                "crates.io".to_string(),
                "pypi.org".to_string()
            ]
        );
    }

    #[test]
    fn parses_network_policy_none_with_no_hosts() {
        // `none` means "hard-block egress"; extra_hosts is informational
        // but the policy itself wins.
        let yaml = "\
runtime:
  network:
    policy: none
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.network.policy, NetworkPolicy::None);
    }

    #[test]
    fn network_policy_as_str_is_stable_for_doctor() {
        assert_eq!(NetworkPolicy::Open.as_str(), "open");
        assert_eq!(NetworkPolicy::None.as_str(), "none");
        assert_eq!(NetworkPolicy::Allowlist.as_str(), "allowlist");
    }

    #[test]
    fn load_returns_defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = RepoConfig::load(dir.path().join("does-not-exist.yaml")).unwrap();
        assert_eq!(cfg, RepoConfig::default());
    }

    #[test]
    fn load_reads_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "runtime:\n  adapter: docker\n").unwrap();
        let cfg = RepoConfig::load(&path).unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Docker);
    }

    #[test]
    fn enum_as_str_matches_on_disk_wording() {
        assert_eq!(AdapterChoice::AppleContainer.as_str(), "apple-container");
        assert_eq!(AdapterChoice::Auto.as_str(), "auto");
        assert_eq!(HardeningChoice::Gvisor.as_str(), "gvisor");
        assert_eq!(Tracker::GitBug.as_str(), "git-bug");
    }

    #[test]
    fn agents_default_carries_claude_code_registry_entry() {
        let cfg = RepoConfig::default();
        assert!(cfg.agents.registry.get("claude-code").is_some());
    }

    #[test]
    fn user_authored_agents_registry_overlays_on_defaults() {
        let yaml = "\
agents:
  default: aider
  registry:
    aider:
      command: [aider]
      env_passthrough: [OPENAI_API_KEY]
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        // User added `aider`...
        assert!(cfg.agents.registry.get("aider").is_some());
        // ...without dropping `claude-code` from the defaults.
        assert!(cfg.agents.registry.get("claude-code").is_some());
        assert_eq!(cfg.agents.default, "aider");
    }

    #[test]
    fn secrets_block_round_trips_three_backends() {
        let yaml = "\
secrets:
  claude_code_oauth_token:
    backend: keychain
    service: claude-code-oauth-token
  gh_token:
    backend: env
    var: GH_TOKEN
  sentry_dsn:
    backend: op
    ref: op://Eng/Sentry/dsn
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        // All three entries land, keyed by their declared names.
        assert_eq!(cfg.secrets.len(), 3);
        assert_eq!(
            cfg.secrets
                .get("claude_code_oauth_token")
                .map(crate::secrets::SecretBackendConfig::kind),
            Some("keychain"),
        );
        assert_eq!(
            cfg.secrets
                .get("gh_token")
                .map(crate::secrets::SecretBackendConfig::kind),
            Some("env"),
        );
        assert_eq!(
            cfg.secrets
                .get("sentry_dsn")
                .map(crate::secrets::SecretBackendConfig::kind),
            Some("op"),
        );
    }

    #[test]
    fn secrets_block_defaults_to_empty_when_absent() {
        // A config without a `secrets:` block parses fine and lands
        // with an empty map — the resolver then falls through to the
        // host file / env passthrough fallbacks.
        let yaml = "tracker: github\n";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert!(cfg.secrets.is_empty());
    }

    #[test]
    fn user_can_redefine_the_default_claude_code_entry() {
        let yaml = "\
agents:
  registry:
    claude-code:
      command: [my-claude]
      env_passthrough: [MY_TOKEN]
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        let cc = cfg.agents.registry.get("claude-code").unwrap();
        assert_eq!(cc.command, vec!["my-claude"]);
        assert_eq!(cc.env_passthrough, vec!["MY_TOKEN"]);
    }

    #[test]
    fn fleet_init_template_is_loadable_against_this_parser() {
        // Smoke test: the default config.yaml that `fleet init` writes must
        // parse to RepoConfig::default(). The CLI-init test already proves
        // the YAML is well-formed; this proves it's *semantically* a no-op.
        let template = crate::cli::init::default_fleet_config_yaml_for_tests();
        let cfg = RepoConfig::from_str_at(template, "/repo/.fleet/config.yaml").unwrap();
        assert_eq!(cfg, RepoConfig::default());
    }
}
