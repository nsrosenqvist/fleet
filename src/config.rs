//! Per-engineer configuration. Default location: `~/.config/fleet/config.toml`
//! (XDG-style on macOS too, not the `AppKit` `Library/Application Support`
//! convention — engineers and CI scripts expect dotfiles under `.config/`).
//! An in-repo override at `<repo>/.fleet.local.toml` overlays keys from the
//! XDG file; both files are optional.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::secrets::SecretBackendConfig;

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub secrets: HashMap<String, SecretBackendConfig>,
    /// What `fleet start` should do about agent authentication before
    /// running `ao start`. See [`AgentAuthMode`]. `None` means "not set
    /// in this file"; consumers read it via [`Self::agent_auth`] which
    /// defaults to `ClaudeOauth` so existing macOS-Claude-subscription
    /// setups keep working unchanged. The two-level shape (parsed
    /// option, resolved enum) is what lets a `.fleet.local.toml`
    /// overlay leave the field unset without clobbering an XDG value.
    #[serde(default)]
    pub agent_auth: Option<AgentAuthMode>,
}

/// Pre-`ao start` auth handoff strategy.
///
/// The Claude Code subscription flow (`claude-oauth`) writes a credentials
/// file inside the VM and refuses to start with `ANTHROPIC_API_KEY` set
/// in the shell — that env var would make claude prefer API auth over
/// the OAuth token and silently bypass the whole handoff.
///
/// Other AO-supported agents (Codex, Aider, …) use whatever env-var
/// flow their underlying provider expects; `passthrough` skips fleet's
/// claude-specific prep entirely and just forwards a common allowlist
/// of provider env vars into the VM.
///
/// `auto` (the new default) reads `agent-orchestrator.yaml` and picks
/// `claude-oauth` when the configured agent is `claude-code`, else
/// `passthrough`. Use the explicit variants only when you need to
/// override that mapping — e.g. running claude-code with API-key auth
/// instead of subscription.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAuthMode {
    /// Derive the flow from `agent-orchestrator.yaml`. Default.
    #[default]
    Auto,
    /// Claude Code subscription OAuth handoff.
    ClaudeOauth,
    /// No claude-specific prep — forwards provider env vars into the VM
    /// and runs `ao start` directly.
    Passthrough,
}

impl AgentAuthMode {
    /// Resolve to a concrete flow (`ClaudeOauth` or `Passthrough`).
    /// `Auto` reads the canonical XDG AO yaml and consults the
    /// configured agent; unreadable yaml or unset agent falls through
    /// to `ClaudeOauth` for backwards compatibility.
    pub fn resolve(self, _repo_root: &Path) -> ResolvedAuthMode {
        match self {
            Self::ClaudeOauth => ResolvedAuthMode::ClaudeOauth,
            Self::Passthrough => ResolvedAuthMode::Passthrough,
            Self::Auto => {
                let ao_cfg = crate::ao::config::AoConfig::load()
                    .ok()
                    .flatten()
                    .map(|(_, cfg)| cfg);
                let agent = ao_cfg.as_ref().and_then(|c| c.agent());
                match crate::ao::config::auth_mode_for_agent(agent) {
                    crate::ao::config::AuthModeHint::ClaudeOauth => ResolvedAuthMode::ClaudeOauth,
                    crate::ao::config::AuthModeHint::Passthrough => ResolvedAuthMode::Passthrough,
                }
            }
        }
    }
}

/// The concrete auth flow `fleet start` runs. `AgentAuthMode::Auto`
/// collapses into one of these via [`AgentAuthMode::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAuthMode {
    ClaudeOauth,
    Passthrough,
}

impl Config {
    /// Resolve config from XDG, optionally overlaid with repo-local file.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(xdg) = Self::xdg_path()
            && xdg.is_file()
        {
            let parsed = parse_file(&xdg)?;
            cfg.merge(parsed);
        }
        let repo_path = repo_root.join(".fleet.local.toml");
        if repo_path.is_file() {
            let parsed = parse_file(&repo_path)?;
            cfg.merge(parsed);
        }
        Ok(cfg)
    }

    /// Compute the XDG config path: `$XDG_CONFIG_HOME/fleet/config.toml` if
    /// that env var is set, else `$HOME/.config/fleet/config.toml`.
    pub fn xdg_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("fleet").join("config.toml"))
    }

    /// Resolved agent-auth mode, defaulting to `ClaudeOauth` when neither
    /// the XDG nor the repo overlay set it explicitly.
    pub fn agent_auth(&self) -> AgentAuthMode {
        self.agent_auth.unwrap_or_default()
    }

    fn merge(&mut self, other: Self) {
        for (k, v) in other.secrets {
            self.secrets.insert(k, v);
        }
        // Only overwrite agent_auth when the overlay sets it explicitly;
        // otherwise an unrelated `.fleet.local.toml` would silently
        // revert an XDG-level Passthrough back to ClaudeOauth.
        if other.agent_auth.is_some() {
            self.agent_auth = other.agent_auth;
        }
    }
}

fn parse_file(path: &Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str::<Config>(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    // NB: env tests in this module set `XDG_CONFIG_HOME` which is a
    // process-global. `cargo test` runs unit tests in parallel by
    // default, so every test that mutates the env must hold the
    // shared lock from `crate::test_env::xdg_lock` for the duration
    // of its set / read / restore sequence (see `with_isolated_xdg`
    // below). Tests that only mutate XDG inline acquire the lock
    // explicitly.

    #[test]
    fn merges_repo_overlay_onto_xdg() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("xdg");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::create_dir_all(&repo).unwrap();

        write(
            &xdg.join("fleet/config.toml"),
            r#"
                [secrets.claude_code_oauth_token]
                backend = "env"
                var = "FROM_XDG"
            "#,
        );
        write(
            &repo.join(".fleet.local.toml"),
            r#"
                [secrets.claude_code_oauth_token]
                backend = "env"
                var = "FROM_REPO"
            "#,
        );

        // SAFETY: env mutation is process-global; restored before
        // return. Lock held across the read so a parallel test can't
        // clobber the value mid-load.
        let guard = crate::test_env::xdg_lock();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }
        let cfg = Config::load(&repo).expect("load ok");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        drop(guard);
        let backend = cfg
            .secrets
            .get("claude_code_oauth_token")
            .expect("secret entry");
        match backend {
            SecretBackendConfig::Env { var } => assert_eq!(var, "FROM_REPO"),
            other => panic!("expected env backend, got {:?}", other.kind()),
        }
    }

    #[test]
    fn agent_auth_defaults_to_auto_when_unset() {
        let cfg = Config::default();
        assert_eq!(cfg.agent_auth(), AgentAuthMode::Auto);
    }

    /// Run `body` with `XDG_CONFIG_HOME` pointed at `tmp` so the AO-yaml
    /// lookup can't fall through to the host developer's real
    /// `~/.config/fleet/agent-orchestrator.yaml`. Also ensures the
    /// `<tmp>/fleet/` subdir exists since the loader looks for
    /// `<XDG_CONFIG_HOME>/fleet/agent-orchestrator.yaml`. Process-global
    /// state, so callers must not nest these or run in parallel with
    /// other XDG-mutating tests.
    fn with_isolated_xdg(tmp: &std::path::Path, body: impl FnOnce()) {
        std::fs::create_dir_all(tmp.join("fleet")).expect("mkdir fleet");
        // Lock guards against parallel XDG mutations from other tests.
        // SAFETY: env mutation is process-global; restored before return.
        let guard = crate::test_env::xdg_lock();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp) };
        body();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        drop(guard);
    }

    /// Write an AO yaml into the canonical XDG location under `tmp`.
    /// Pair with [`with_isolated_xdg`] in the same test body.
    fn write_xdg_ao_yaml(tmp: &std::path::Path, body: &str) {
        std::fs::write(tmp.join("fleet").join("agent-orchestrator.yaml"), body)
            .expect("write agent-orchestrator.yaml");
    }

    #[test]
    fn auto_resolves_to_claude_oauth_when_yaml_missing() {
        // Backwards compat: no AO yaml configured → historical
        // claude-oauth behaviour.
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            let resolved = AgentAuthMode::Auto.resolve(tmp.path());
            assert_eq!(resolved, ResolvedAuthMode::ClaudeOauth);
        });
    }

    #[test]
    fn auto_resolves_to_claude_oauth_when_agent_is_claude_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            write_xdg_ao_yaml(tmp.path(), "defaults:\n  agent: claude-code\n");
            let resolved = AgentAuthMode::Auto.resolve(tmp.path());
            assert_eq!(resolved, ResolvedAuthMode::ClaudeOauth);
        });
    }

    #[test]
    fn auto_resolves_to_passthrough_for_non_claude_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            write_xdg_ao_yaml(tmp.path(), "defaults:\n  agent: codex\n");
            let resolved = AgentAuthMode::Auto.resolve(tmp.path());
            assert_eq!(resolved, ResolvedAuthMode::Passthrough);
        });
    }

    #[test]
    fn explicit_modes_ignore_yaml() {
        // An explicit `agent_auth = "passthrough"` overrides whatever
        // the AO yaml says; the user is forcing the mode for a reason
        // (e.g. claude-code with API-key auth instead of subscription).
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            write_xdg_ao_yaml(tmp.path(), "defaults:\n  agent: claude-code\n");
            let resolved = AgentAuthMode::Passthrough.resolve(tmp.path());
            assert_eq!(resolved, ResolvedAuthMode::Passthrough);
        });
    }

    #[test]
    fn agent_auth_parses_passthrough() {
        let cfg: Config = toml::from_str("agent_auth = \"passthrough\"").expect("parse");
        assert_eq!(cfg.agent_auth(), AgentAuthMode::Passthrough);
    }

    #[test]
    fn agent_auth_overlay_does_not_revert_when_unset() {
        // Regression: an .fleet.local.toml that doesn't mention
        // agent_auth must not silently revert an XDG-level Passthrough
        // back to ClaudeOauth.
        let mut xdg: Config = toml::from_str("agent_auth = \"passthrough\"").expect("parse xdg");
        let repo: Config = toml::from_str("").expect("parse empty");
        xdg.merge(repo);
        assert_eq!(xdg.agent_auth(), AgentAuthMode::Passthrough);
    }

    #[test]
    fn agent_auth_overlay_wins_when_set() {
        let mut xdg: Config = toml::from_str("agent_auth = \"passthrough\"").expect("parse xdg");
        let repo: Config = toml::from_str("agent_auth = \"claude-oauth\"").expect("parse repo");
        xdg.merge(repo);
        assert_eq!(xdg.agent_auth(), AgentAuthMode::ClaudeOauth);
    }

    #[test]
    fn empty_when_no_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Point XDG at an empty dir; repo has no .fleet.local.toml either.
        // SAFETY: env mutation is process-global; restored before return.
        let guard = crate::test_env::xdg_lock();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let cfg = Config::load(tmp.path()).expect("load ok");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        drop(guard);
        assert!(cfg.secrets.is_empty());
    }
}
