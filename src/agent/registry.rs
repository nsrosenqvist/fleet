//! Agent registry: named specs for the agent processes fleet spawns inside
//! container workspaces.
//!
//! An [`AgentSpec`] is a value object — argv to invoke, env vars to forward
//! from the host. The agent *process* itself (PTY plumbing, prompt prefix,
//! exit handling) is the workflow executor's job; this module only models
//! the static description.
//!
//! Defaults: the registry includes `claude-code` out of the box so an empty
//! `.fleet/config.yaml` Just Works. Users override entries by re-declaring
//! the same name in their config; per-key merging means a user spec for
//! `aider` doesn't drop the default `claude-code` entry.

use serde::Deserialize;
use std::collections::BTreeMap;

/// A named agent description. Stable wording for the YAML schema:
/// `command:` is the argv to invoke inside the container; `env_passthrough:`
/// is a whitelist of env var names that fleet forwards from the host's
/// environment to the container's `--remote-env`.
///
/// `BTreeMap` for the registry rather than `HashMap` so the on-disk order
/// is stable across rewrites — fleet doesn't write user configs, but tests
/// asserting on serialised forms benefit from determinism.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentSpec {
    /// argv to invoke. First element is the binary name; rest are its args.
    pub command: Vec<String>,
    /// Env-var names to forward from the host. Values are looked up at spawn
    /// time; missing vars are treated as empty (the agent decides whether to
    /// fail, e.g. claude-code fails if `ANTHROPIC_API_KEY` is empty).
    #[serde(default)]
    pub env_passthrough: Vec<String>,
}

/// Map of agent-name → [`AgentSpec`]. Wraps `BTreeMap` to give a typed
/// surface (`get`, `iter`) and a `Default` impl that seeds the well-known
/// entries.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct AgentRegistry {
    entries: BTreeMap<String, AgentSpec>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert("claude-code".to_string(), claude_code_default());
        Self { entries }
    }
}

impl AgentRegistry {
    /// Returns the entry for `name`, or `None` when not in the registry.
    /// Workflow executors call this to resolve an Agent node's `agent:` field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.entries.get(name)
    }

    /// Iterate over `(name, spec)` pairs in stable order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AgentSpec)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of registered agents. Cheap; backing map.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has any entries. The default registry is never
    /// empty, so this returning `true` indicates the caller explicitly
    /// emptied it — likely a misconfiguration.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Merge a user-authored partial registry on top of the defaults: each
    /// user-supplied name replaces the same-name default; names the user
    /// didn't mention keep their defaults. This is how we get "users
    /// override claude-code without losing it from the registry."
    #[must_use]
    pub fn with_overrides(mut self, overrides: Self) -> Self {
        for (name, spec) in overrides.entries {
            self.entries.insert(name, spec);
        }
        self
    }
}

fn claude_code_default() -> AgentSpec {
    AgentSpec {
        command: vec!["claude".to_string(), "code".to_string()],
        env_passthrough: vec![
            "ANTHROPIC_API_KEY".to_string(),
            "CLAUDE_OAUTH_TOKEN".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_includes_claude_code() {
        let r = AgentRegistry::default();
        let spec = r.get("claude-code").expect("claude-code must be present");
        assert_eq!(spec.command, vec!["claude".to_string(), "code".to_string()]);
        assert!(spec.env_passthrough.iter().any(|e| e == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn get_returns_none_for_unknown_name() {
        let r = AgentRegistry::default();
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn iter_yields_entries_in_name_sorted_order() {
        // Stable order — confirm BTreeMap-backed ordering survives. Add
        // two entries that would re-order under HashMap to make the test
        // actually probe the property.
        let mut r = AgentRegistry::default();
        r.entries.insert("zzz".to_string(), AgentSpec {
            command: vec!["zzz".to_string()],
            env_passthrough: vec![],
        });
        r.entries.insert("aaa".to_string(), AgentSpec {
            command: vec!["aaa".to_string()],
            env_passthrough: vec![],
        });
        let names: Vec<_> = r.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, vec!["aaa", "claude-code", "zzz"]);
    }

    #[test]
    fn deserialises_a_full_registry_from_yaml() {
        let yaml = "\
claude-code:
  command: [claude, code]
  env_passthrough: [ANTHROPIC_API_KEY]
aider:
  command: [aider]
  env_passthrough: [OPENAI_API_KEY]
";
        let r: AgentRegistry = serde_yml::from_str(yaml).unwrap();
        let cc = r.get("claude-code").unwrap();
        assert_eq!(cc.command, vec!["claude", "code"]);
        let aider = r.get("aider").unwrap();
        assert_eq!(aider.command, vec!["aider"]);
        assert_eq!(aider.env_passthrough, vec!["OPENAI_API_KEY"]);
    }

    #[test]
    fn missing_env_passthrough_defaults_to_empty() {
        let yaml = "\
minimal:
  command: [some-cmd]
";
        let r: AgentRegistry = serde_yml::from_str(yaml).unwrap();
        let spec = r.get("minimal").unwrap();
        assert!(spec.env_passthrough.is_empty());
    }

    #[test]
    fn with_overrides_replaces_matching_names_and_keeps_others() {
        let defaults = AgentRegistry::default();
        let mut overrides_entries = BTreeMap::new();
        overrides_entries.insert("claude-code".to_string(), AgentSpec {
            command: vec!["my-claude".to_string()],
            env_passthrough: vec!["MY_TOKEN".to_string()],
        });
        overrides_entries.insert("aider".to_string(), AgentSpec {
            command: vec!["aider".to_string()],
            env_passthrough: vec!["OPENAI_API_KEY".to_string()],
        });
        let overrides = AgentRegistry { entries: overrides_entries };

        let merged = defaults.with_overrides(overrides);

        // claude-code got replaced.
        assert_eq!(merged.get("claude-code").unwrap().command, vec!["my-claude"]);
        // aider got added.
        assert!(merged.get("aider").is_some());
        // No other defaults exist today, so len is exactly 2.
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn default_registry_is_not_empty() {
        let r = AgentRegistry::default();
        assert!(!r.is_empty());
        assert_eq!(r.len(), 1);
    }
}
