//! Per-repo fleet configuration: `.fleet/config.yaml`.
//!
//! Distinct from the user-global XDG config in [`crate::config`] (TOML,
//! AO-era settings). This module models the YAML schema scaffolded by
//! [`crate::cli::init`] and consumed by v2 commands.
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoConfig {
    pub runtime: RuntimeConfig,
    pub tracker: Tracker,
    pub agents: AgentsConfig,
    pub workflows: WorkflowsConfig,
}

/// `runtime:` block — which adapter to instantiate, how to harden it, where
/// the devcontainer lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub adapter: AdapterChoice,
    pub hardening: HardeningChoice,
    /// Relative to the repo root. `fleet init` defaults to
    /// `.devcontainer/devcontainer.json`.
    pub devcontainer: PathBuf,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            adapter: AdapterChoice::Auto,
            hardening: HardeningChoice::Auto,
            devcontainer: PathBuf::from(".devcontainer/devcontainer.json"),
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
}

#[derive(Deserialize, Default)]
struct RawRuntime {
    #[serde(default)]
    adapter: Option<AdapterChoice>,
    #[serde(default)]
    hardening: Option<HardeningChoice>,
    #[serde(default)]
    devcontainer: Option<PathBuf>,
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
    fn from(raw: Raw) -> Self {
        // Destructure defaults up front so each field can be moved into the
        // matching `unwrap_or(...)` slot without aliasing the same struct
        // twice (which would trip the borrow checker on the closure paths).
        let Self {
            runtime: default_runtime,
            tracker: default_tracker,
            agents: default_agents,
            workflows: default_workflows,
        } = Self::default();

        let runtime = match raw.runtime {
            Some(r) => RuntimeConfig {
                adapter: r.adapter.unwrap_or(default_runtime.adapter),
                hardening: r.hardening.unwrap_or(default_runtime.hardening),
                devcontainer: r.devcontainer.unwrap_or(default_runtime.devcontainer),
            },
            None => default_runtime,
        };
        let tracker = raw.tracker.unwrap_or(default_tracker);
        let agents = match raw.agents {
            Some(a) => {
                let default = a.default.unwrap_or(default_agents.default);
                let registry = a
                    .registry
                    .map_or(default_agents.registry, |user| {
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
        Self {
            runtime,
            tracker,
            agents,
            workflows,
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
        // The plan's schema is evolving — `network`, `autonomous`, etc.
        // aren't modelled yet. The parser must not refuse to read them.
        let yaml = "\
runtime:
  adapter: docker
  network:
    policy: allowlist
autonomous:
  max_parallel: 3
";
        let cfg = RepoConfig::from_str_at(yaml, "/x").unwrap();
        assert_eq!(cfg.runtime.adapter, AdapterChoice::Docker);
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
