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

    /// Number of registered agents. Cheap; backing map. Public surface
    /// paired with [`Self::iter`]; tests exercise it, no v2 binary
    /// caller does — kept for symmetry + future TUI agents view.
    #[must_use]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has any entries. The default registry is
    /// never empty, so this returning `true` indicates the caller
    /// explicitly emptied it — likely a misconfiguration.
    #[must_use]
    #[allow(dead_code)]
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
    // Wrap claude in a tiny bash trampoline that materialises the
    // OAuth token (forwarded as `CLAUDE_CODE_OAUTH_TOKEN` env via the
    // secrets resolver / host-file auto-pickup) into the credentials
    // file claude reads, then `exec`s claude in print mode against
    // the rendered `FLEET_PROMPT`. The `exec` keeps the process tree
    // a single hop deep and lets claude own the TTY when invoked
    // through `attach_pty`. Falls through silently when no token is
    // available — claude itself surfaces a clear auth error in that
    // case, which is more discoverable than a bash error here.
    //
    // The fallback to `ANTHROPIC_API_KEY` keeps API-only users
    // working: claude reads it directly when present, no credentials
    // file required.
    AgentSpec {
        command: vec![
            "bash".to_string(),
            "-c".to_string(),
            CLAUDE_CODE_ENTRY_SCRIPT.to_string(),
        ],
        env_passthrough: vec![
            "ANTHROPIC_API_KEY".to_string(),
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            // Legacy spelling kept for users whose configs still
            // reference it. The is_claude_oauth_var helper in
            // `workflow::executor` accepts both.
            "CLAUDE_OAUTH_TOKEN".to_string(),
        ],
    }
}

/// Bash entrypoint for the default `claude-code` agent. Writes the
/// credentials file from whichever OAuth env var the secrets layer
/// resolved (canonical first, then legacy alias), then exec's
/// claude with the rendered prompt in print mode. Kept as a single
/// `const &str` so the registry stays a value object — no shell
/// templating, no string concatenation at registration time.
///
/// `--dangerously-skip-permissions` is intentional: fleet runs each
/// agent in a per-session devcontainer with a per-session worktree
/// and the `fleet-git` push guard already mounted — the container
/// is the sandbox. Claude's in-container `permissionMode: "default"`
/// would block every Write tool / Bash redirect on interactive
/// approval, which is impossible in an unattended pipeline and was
/// observed (transcript: "The environment does not have
/// `Write(/artifacts/**)` permission configured…") failing every
/// planner node.
#[allow(clippy::redundant_pub_crate)]
pub(crate) const CLAUDE_CODE_ENTRY_SCRIPT: &str = "\
set -e
TOKEN=\"${CLAUDE_CODE_OAUTH_TOKEN:-${CLAUDE_OAUTH_TOKEN:-}}\"
if [ -n \"$TOKEN\" ]; then
  mkdir -p \"$HOME/.claude\"
  printf '{\"claudeAiOauth\":{\"accessToken\":\"%s\",\"scopes\":[\"user:inference\"],\"subscriptionType\":\"subscription\"}}\\n' \"$TOKEN\" > \"$HOME/.claude/.credentials.json\"
  chmod 600 \"$HOME/.claude/.credentials.json\"
fi
PROMPT=\"${FLEET_PROMPT:-No prompt provided. Please summarise what claude code is in two sentences.}\"
if [ \"${FLEET_MERGE_CONFLICTS:-}\" = \"true\" ]; then
  PROMPT=\"# Chain merge conflicts — RESOLVE FIRST\n\nYour worktree is in MERGING state from chaining off multiple upstream tickets. Read \\`${FLEET_MERGE_CONTEXT_FILE:-/artifacts/MERGE_CONTEXT.md}\\` for context (parent tickets, branches, conflicted files). Resolve every conflict by preserving the intent of both upstream tickets — use \\`git log\\` against each parent branch or \\`fleet-tracker read <ticket-id>\\` if intent is unclear. \\`git add\\` resolved files, then \\`git commit\\` to finalize. \\`git status\\` must report a clean tree before you proceed with the task below.\n\n---\n\n${PROMPT}\"
fi
exec claude --dangerously-skip-permissions -p \"$PROMPT\"
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_includes_claude_code() {
        let r = AgentRegistry::default();
        let spec = r.get("claude-code").expect("claude-code must be present");
        // The default is now a `bash -c <script>` trampoline that
        // materialises the OAuth credentials file before exec'ing
        // claude in print mode.
        assert_eq!(spec.command[0], "bash");
        assert_eq!(spec.command[1], "-c");
        assert!(
            spec.command[2].contains("exec claude"),
            "command[2] should invoke claude: {}",
            spec.command[2]
        );
        // Both the canonical and legacy OAuth env var names ride
        // through so the secrets resolver and the host-file
        // auto-pickup can both feed the trampoline.
        assert!(
            spec.env_passthrough
                .iter()
                .any(|e| e == "ANTHROPIC_API_KEY")
        );
        assert!(
            spec.env_passthrough
                .iter()
                .any(|e| e == "CLAUDE_CODE_OAUTH_TOKEN")
        );
    }

    #[test]
    fn default_claude_code_invokes_with_dangerously_skip_permissions() {
        // The agent runs in a per-session devcontainer sandbox; an
        // interactive in-container permission prompt deadlocks the
        // unattended pipeline. Pin the flag so a future "clean up
        // dangerous-sounding flags" refactor can't silently re-break
        // every planner / coder / reviewer node.
        let script = &claude_code_default().command[2];
        assert!(
            script.contains("--dangerously-skip-permissions"),
            "trampoline must opt out of claude's in-container permission gate: {script}",
        );
    }

    #[test]
    fn default_claude_code_writes_credentials_from_token_env() {
        // The trampoline must wire the resolved env var into
        // `~/.claude/.credentials.json` so claude reads the OAuth
        // material it expects on disk. Pin the file path + JSON
        // shape so a future "let's use a different format" change
        // can't silently break the auth pipeline.
        let script = &claude_code_default().command[2];
        assert!(script.contains("$HOME/.claude/.credentials.json"));
        assert!(script.contains("claudeAiOauth"));
        assert!(script.contains("accessToken"));
        assert!(script.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        // Legacy spelling still accepted in the trampoline for
        // configs that haven't been updated yet.
        assert!(script.contains("CLAUDE_OAUTH_TOKEN"));
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
        r.entries.insert(
            "zzz".to_string(),
            AgentSpec {
                command: vec!["zzz".to_string()],
                env_passthrough: vec![],
            },
        );
        r.entries.insert(
            "aaa".to_string(),
            AgentSpec {
                command: vec!["aaa".to_string()],
                env_passthrough: vec![],
            },
        );
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
        overrides_entries.insert(
            "claude-code".to_string(),
            AgentSpec {
                command: vec!["my-claude".to_string()],
                env_passthrough: vec!["MY_TOKEN".to_string()],
            },
        );
        overrides_entries.insert(
            "aider".to_string(),
            AgentSpec {
                command: vec!["aider".to_string()],
                env_passthrough: vec!["OPENAI_API_KEY".to_string()],
            },
        );
        let overrides = AgentRegistry {
            entries: overrides_entries,
        };

        let merged = defaults.with_overrides(overrides);

        // claude-code got replaced.
        assert_eq!(
            merged.get("claude-code").unwrap().command,
            vec!["my-claude"]
        );
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
