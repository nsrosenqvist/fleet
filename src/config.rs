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

    fn merge(&mut self, other: Self) {
        for (k, v) in other.secrets {
            self.secrets.insert(k, v);
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
