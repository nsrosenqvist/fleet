//! `fleet config show|edit|init` — manage per-engineer secret-backend config.

use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::config::Config;
use crate::process::run_interactive;

pub fn run_show(repo_root: &Path) -> Result<i32> {
    let cfg = Config::load(repo_root)?;
    println!("# Effective fleet config");
    println!(
        "# XDG path:  {}",
        Config::xdg_path().map_or_else(|| "<no HOME>".into(), |p| p.display().to_string()),
    );
    println!("# Repo override: {}/.fleet.local.toml", repo_root.display());
    println!();
    if cfg.secrets.is_empty() {
        println!("# (no secrets configured)");
    } else {
        for (name, backend) in &cfg.secrets {
            println!("[secrets.{name}]");
            println!("backend = \"{}\"", backend.kind());
            match backend {
                crate::secrets::SecretBackendConfig::Env { var } => {
                    println!("var = \"{var}\"");
                }
                crate::secrets::SecretBackendConfig::Keychain { service, account } => {
                    println!("service = \"{service}\"");
                    if let Some(a) = account {
                        println!("account = \"{a}\"");
                    }
                }
                crate::secrets::SecretBackendConfig::Op { reference } => {
                    println!("ref = \"{reference}\"");
                }
            }
            println!();
        }
    }
    Ok(0)
}

pub fn run_edit() -> Result<i32> {
    let path = Config::xdg_path().context("$HOME is not set; cannot resolve XDG path")?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, default_template())
            .with_context(|| format!("writing template config to {}", path.display()))?;
        eprintln!("Wrote template to {}", path.display());
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    run_interactive(&editor, &[path.display().to_string()], &[], &[])
}

pub fn run_init() -> Result<i32> {
    let path = Config::xdg_path().context("$HOME is not set; cannot resolve XDG path")?;
    if path.exists() {
        bail!(
            "{} already exists. Use `fleet config edit` to modify it.",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, default_template())
        .with_context(|| format!("writing template config to {}", path.display()))?;
    println!("Created {}", path.display());
    println!("Edit it to point at your Claude OAuth token storage backend.");
    Ok(0)
}

fn default_template() -> &'static str {
    r#"# fleet — per-engineer config.
# Pick ONE backend for the `claude_code_oauth_token` secret.
#
# Option A: OS keyring (cross-platform — macOS Keychain, Linux Secret
# Service / GNOME Keyring / KWallet, Windows Credential Manager).
#
# Populate the entry once, before pressing Shift+S:
#   macOS:    security add-generic-password -a $USER -s claude-code-oauth-token -w 'sk-ant-oat01-...'
#   Linux:    secret-tool store --label='fleet' service claude-code-oauth-token account $USER
#             (then paste the token at the prompt)
#   Windows:  cmdkey /generic:claude-code-oauth-token /user:%USERNAME% /pass:'sk-ant-oat01-...'
#
# [secrets.claude_code_oauth_token]
# backend = "keychain"
# service = "claude-code-oauth-token"
# # account = "<override $USER>"
#
# Option B: 1Password CLI (requires `op` in PATH; biometric prompt on first use)
# [secrets.claude_code_oauth_token]
# backend = "op"
# ref = "op://<vault>/<item>/<field>"
#
# Option C: env var (explicit; usually CI only)
# [secrets.claude_code_oauth_token]
# backend = "env"
# var = "CLAUDE_CODE_OAUTH_TOKEN"
"#
}
