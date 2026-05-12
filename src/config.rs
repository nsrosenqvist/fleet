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
/// of provider env vars into the VM. Users running those agents need
/// `passthrough` so the `ANTHROPIC_API_KEY` refusal doesn't block
/// `fleet start`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAuthMode {
    /// Claude Code subscription OAuth handoff. The historical default.
    #[default]
    ClaudeOauth,
    /// No claude-specific prep — forwards provider env vars into the VM
    /// and runs `ao start` directly.
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
    // process-global; tests in this module must not run in parallel against
    // each other. cargo test serializes within a single test binary's
    // failures, but to be safe we use distinct values per test.

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

        // SAFETY: env mutation is process-global; restored before return.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }
        let cfg = Config::load(&repo).expect("load ok");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
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
    fn agent_auth_defaults_to_claude_oauth_when_unset() {
        let cfg = Config::default();
        assert_eq!(cfg.agent_auth(), AgentAuthMode::ClaudeOauth);
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
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let cfg = Config::load(tmp.path()).expect("load ok");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert!(cfg.secrets.is_empty());
    }
}
