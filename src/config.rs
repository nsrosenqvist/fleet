//! Per-engineer configuration. Loaded from `~/.config/fleet/config.toml`
//! (XDG-style on macOS too, not the `AppKit` `Library/Application Support`
//! convention — engineers and CI scripts expect dotfiles under `.config/`).
//! The file is optional; a missing file yields `Config::default()`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::secrets::SecretBackendConfig;

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub secrets: HashMap<String, SecretBackendConfig>,
    /// What `fleet start` should do about agent authentication before
    /// running `ao start`. See [`AgentAuthMode`].
    #[serde(default)]
    pub agent_auth: AgentAuthMode,
    /// Optional `[git]` section. Materialized into the worker VM's
    /// `$HOME/.gitconfig` at fleet start. When empty, fleet falls
    /// back to the host's own `~/.gitconfig` (`user.name`/`user.email`
    /// only). See [`GitConfig`].
    #[serde(default)]
    pub git: GitConfig,
    /// Optional `[network]` section. Controls the worker egress
    /// allowlist. Default mode is `open` (no enforcement) so existing
    /// users see no behaviour change after upgrading. See
    /// [`NetworkConfig`].
    #[serde(default)]
    pub network: NetworkConfig,
}

/// Egress filtering for AO workers. Phase 3 wires this into the
/// in-VM `tinyproxy` instance: a cooperative `HTTPS_PROXY` env var
/// directs worker tools (`gh`, `git`, `npm`, `cargo`, `curl`,
/// `claude`, …) at a hostname-allowlisted proxy. When `mode =
/// "open"` the proxy isn't engaged and traffic goes direct.
///
/// **Cooperative, not enforcing.** A worker process that explicitly
/// `unset HTTPS_PROXY` can still reach arbitrary hosts. fleet's
/// kernel-level enforcement (nftables egress-block) is on the
/// roadmap but not in this phase; see `docs/network.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub mode: NetworkMode,
    /// Hostnames added on top of the built-in baseline (see
    /// `network::BUILT_IN_ALLOW`). Suffix-match: an entry of
    /// `example.com` covers `api.example.com`,
    /// `cdn.example.com`, etc. — the proxy's `dstdomain` semantics.
    /// Bare hostname (no `https://`); no glob characters.
    #[serde(default)]
    pub extra_allow: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    /// Worker traffic goes direct, no proxy. The default — matches
    /// fleet's pre-Phase-3 behaviour so an upgrade doesn't quietly
    /// start dropping connections.
    #[default]
    Open,
    /// Worker traffic is routed through the in-VM tinyproxy with the
    /// resolved allowlist applied. Anything not on the allowlist
    /// returns a 403 from the proxy.
    Allowlist,
}

impl NetworkConfig {
    pub fn is_allowlist(&self) -> bool {
        matches!(self.mode, NetworkMode::Allowlist)
    }
}

/// Per-user git identity that fleet should bake into the worker
/// `$HOME/.gitconfig` inside the VM. Required because the host home
/// is no longer bind-mounted — workers can't see `~/.gitconfig`
/// directly.
///
/// Resolution order at `fleet start`:
/// 1. This struct, if either field is set in `~/.config/fleet/config.toml`.
/// 2. The host's own `~/.gitconfig` (`user.name` + `user.email` only —
///    signing keys and any other section are intentionally not copied,
///    since they often reference host-side paths and may be sensitive).
/// 3. Skipped: workers commit with whatever defaults git falls back to
///    (host `gh` configures `user.name`/`user.email` lazily, but with
///    no source we leave it alone).
///
/// Signing keys (`signingkey`, `commit.gpgsign`) are not forwarded.
/// They're typically tied to a hardware key or host gpg-agent that
/// doesn't exist inside the VM; even if the user wanted to forward
/// them, doing so silently would mis-sign worker commits with the
/// host user's identity. A future `forward_signing = true` opt-in
/// can add this when needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// `user.name` for worker commits.
    #[serde(default)]
    pub user_name: Option<String>,
    /// `user.email` for worker commits.
    #[serde(default)]
    pub user_email: Option<String>,
}

impl GitConfig {
    /// True when neither field is configured; the fleet start path
    /// uses this to decide whether to fall back to the host
    /// `~/.gitconfig`.
    pub fn is_empty(&self) -> bool {
        self.user_name.is_none() && self.user_email.is_none()
    }
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
    pub fn resolve(self) -> ResolvedAuthMode {
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
    /// Load the XDG config. Returns `Config::default()` when the file
    /// doesn't exist.
    pub fn load() -> Result<Self> {
        let Some(xdg) = Self::xdg_path() else {
            return Ok(Self::default());
        };
        if !xdg.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&xdg).with_context(|| format!("reading {}", xdg.display()))?;
        toml::from_str::<Self>(&text).with_context(|| format!("parsing {}", xdg.display()))
    }

    /// Compute the XDG config path: `$XDG_CONFIG_HOME/fleet/config.toml` if
    /// that env var is set, else `$HOME/.config/fleet/config.toml`.
    pub fn xdg_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("fleet").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // NB: env tests in this module set `XDG_CONFIG_HOME` which is a
    // process-global. `cargo test` runs unit tests in parallel by
    // default, so every test that mutates the env must hold the
    // shared lock from `crate::test_env::xdg_lock` for the duration
    // of its set / read / restore sequence (see `with_isolated_xdg`
    // below). Tests that only mutate XDG inline acquire the lock
    // explicitly.

    #[test]
    fn agent_auth_defaults_to_auto_when_unset() {
        let cfg = Config::default();
        assert_eq!(cfg.agent_auth, AgentAuthMode::Auto);
    }

    /// Run `body` with `XDG_CONFIG_HOME` pointed at `tmp` so the AO-yaml
    /// lookup can't fall through to the host developer's real
    /// `~/.config/fleet/agent-orchestrator.yaml`. Also ensures the
    /// `<tmp>/fleet/` subdir exists since the loader looks for
    /// `<XDG_CONFIG_HOME>/fleet/agent-orchestrator.yaml`. Process-global
    /// state, so callers must not nest these or run in parallel with
    /// other XDG-mutating tests.
    fn with_isolated_xdg(tmp: &Path, body: impl FnOnce()) {
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
    fn write_xdg_ao_yaml(tmp: &Path, body: &str) {
        std::fs::write(tmp.join("fleet").join("agent-orchestrator.yaml"), body)
            .expect("write agent-orchestrator.yaml");
    }

    #[test]
    fn auto_resolves_to_claude_oauth_when_yaml_missing() {
        // Backwards compat: no AO yaml configured → historical
        // claude-oauth behaviour.
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            assert_eq!(AgentAuthMode::Auto.resolve(), ResolvedAuthMode::ClaudeOauth);
        });
    }

    #[test]
    fn auto_resolves_to_claude_oauth_when_agent_is_claude_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            write_xdg_ao_yaml(tmp.path(), "defaults:\n  agent: claude-code\n");
            assert_eq!(AgentAuthMode::Auto.resolve(), ResolvedAuthMode::ClaudeOauth);
        });
    }

    #[test]
    fn auto_resolves_to_passthrough_for_non_claude_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_isolated_xdg(tmp.path(), || {
            write_xdg_ao_yaml(tmp.path(), "defaults:\n  agent: codex\n");
            assert_eq!(AgentAuthMode::Auto.resolve(), ResolvedAuthMode::Passthrough);
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
            assert_eq!(
                AgentAuthMode::Passthrough.resolve(),
                ResolvedAuthMode::Passthrough
            );
        });
    }

    #[test]
    fn agent_auth_parses_passthrough() {
        let cfg: Config = toml::from_str("agent_auth = \"passthrough\"").expect("parse");
        assert_eq!(cfg.agent_auth, AgentAuthMode::Passthrough);
    }

    #[test]
    fn empty_when_no_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Point XDG at an empty dir so xdg_path() resolves to a missing file.
        // SAFETY: env mutation is process-global; restored before return.
        let guard = crate::test_env::xdg_lock();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let cfg = Config::load().expect("load ok");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        drop(guard);
        assert!(cfg.secrets.is_empty());
    }
}
