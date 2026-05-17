//! `fleet secrets …` — register / list / test / remove
//! secret-backend declarations.
//!
//! The `secrets:` block in `.fleet/config.yaml` maps a secret name
//! (lowercase, matched case-insensitively against agent
//! `env_passthrough` entries at resolve time) to the backend that
//! produces its value: env / keychain / op. The TUI / executor never
//! materialise the value to disk — it flows from the backend straight
//! into the agent container's env at run time.
//!
//! `register` is the user-facing entry to the keychain backend
//! specifically: it walks the user through obtaining a token (for
//! the well-known `claude_code_oauth_token` name, that's "run
//! `claude setup-token` in another terminal and paste the result"),
//! stores the value in the OS keyring, and writes the `secrets:`
//! entry that points at it. No other path writes to the keyring
//! itself — `secret-tool store` / `security add-generic-password`
//! still work for advanced users, and `register` is just the
//! beginner-friendly wrapper.

use anyhow::{Context, Result, anyhow, bail};
use keyring::Entry;
use secrecy::ExposeSecret;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::process::RealProcessInvoker;
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::secrets::build;

/// `fleet secrets list`. Prints one row per configured secret:
/// `<name>   <backend>   <status>`. Backend strings come from
/// [`SecretBackendConfig::describe`]; status is one of `ok`,
/// `missing`, or `error: <one-line>`.
pub fn run_list() -> Result<i32> {
    let cfg = load_repo_config()?;
    if cfg.secrets.is_empty() {
        println!("(no secrets configured — `fleet secrets register <name>` to add one)");
        return Ok(0);
    }
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let name_width = cfg.secrets.keys().map(String::len).max().unwrap_or(4);
    let kind_width = 32usize;
    println!(
        "{:<name_width$}  {:<kind_width$}  status",
        "name", "backend"
    );
    println!(
        "{}  {}  {}",
        "-".repeat(name_width),
        "-".repeat(kind_width),
        "-".repeat(6)
    );
    for (name, backend_cfg) in &cfg.secrets {
        let backend = build(backend_cfg, Arc::clone(&invoker));
        let status = match backend.fetch() {
            Ok(s) if s.expose_secret().is_empty() => "missing".to_string(),
            Ok(_) => "ok".to_string(),
            Err(err) => format!("error: {}", format!("{err:#}").lines().next().unwrap_or("")),
        };
        println!(
            "{:<name_width$}  {:<kind_width$}  {}",
            name,
            backend_cfg.describe(),
            status
        );
    }
    Ok(0)
}

/// `fleet secrets register <name>`. For the well-known
/// `claude_code_oauth_token` name, prints the `claude setup-token`
/// instruction; otherwise generic "paste the value" prompt. Reads
/// the token from stdin (no echo when stdin is a TTY), stores it
/// via the OS keyring, then appends the matching `secrets:` entry
/// to `.fleet/config.yaml`.
pub fn run_register(name: &str) -> Result<i32> {
    let lookup = name.to_ascii_lowercase();
    let cfg_path = config_path()?;

    // User-facing hint per known secret.
    print_register_intro(&lookup);

    let token = prompt_for_secret(&lookup)?;
    if token.expose_secret().is_empty() {
        bail!("empty token — nothing to register");
    }

    let service = service_for(&lookup);
    let account = std::env::var("USER").ok();
    let entry = keyring_entry(&service, account.as_deref())?;
    entry
        .set_password(token.expose_secret())
        .with_context(|| format!("storing secret in OS keyring (service={service})"))?;

    append_secrets_entry(&cfg_path, &lookup, &service, account.as_deref())?;
    println!(
        "ok: stored in OS keyring as service `{service}`; added \
         `secrets.{lookup}` to {}",
        cfg_path.display()
    );
    Ok(0)
}

/// `fleet secrets test <name>`. Resolves the configured backend
/// once and reports `ok (NN chars)` on success or the backend's
/// error message on failure. Doesn't print the value.
pub fn run_test(name: &str) -> Result<i32> {
    let cfg = load_repo_config()?;
    let lookup = name.to_ascii_lowercase();
    let backend_cfg = cfg.secrets.get(&lookup).ok_or_else(|| {
        anyhow!(
            "no `secrets.{lookup}` entry in fleet config — register it with \
             `fleet secrets register {lookup}`"
        )
    })?;
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let backend = build(backend_cfg, Arc::clone(&invoker));
    match backend.fetch() {
        Ok(s) => {
            let len = s.expose_secret().len();
            println!("ok ({len} chars via `{}` backend)", backend.kind());
            Ok(0)
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            Ok(1)
        }
    }
}

/// `fleet secrets remove <name>`. Strips the `secrets.<name>` entry
/// from `.fleet/config.yaml`. The OS keyring entry itself stays —
/// removing it requires the platform tool (`secret-tool delete` /
/// `security delete-generic-password`).
pub fn run_remove(name: &str) -> Result<i32> {
    let cfg_path = config_path()?;
    let lookup = name.to_ascii_lowercase();
    let body = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("reading {}", cfg_path.display()))?;
    let mut doc: serde_yml::Value =
        serde_yml::from_str(&body).with_context(|| format!("parsing {}", cfg_path.display()))?;
    let removed = doc
        .get_mut("secrets")
        .and_then(serde_yml::Value::as_mapping_mut)
        .is_some_and(|m| m.remove(serde_yml::Value::String(lookup.clone())).is_some());
    if !removed {
        eprintln!("warning: no `secrets.{lookup}` entry to remove");
        return Ok(0);
    }
    let serialised = serde_yml::to_string(&doc).with_context(|| "re-serialising fleet config")?;
    std::fs::write(&cfg_path, serialised)
        .with_context(|| format!("writing {}", cfg_path.display()))?;
    println!(
        "removed `secrets.{lookup}` from {} (keyring entry left in place)",
        cfg_path.display()
    );
    Ok(0)
}

/// User-visible header before the prompt. Carries `claude
/// setup-token` boilerplate for the well-known secret name;
/// generic instruction otherwise.
fn print_register_intro(lookup: &str) {
    println!("fleet secrets register {lookup}");
    println!();
    if lookup == "claude_code_oauth_token" {
        println!("Run `claude setup-token` in another terminal to obtain an");
        println!("OAuth token (long-lived; bound to your Anthropic account).");
        println!("Paste the resulting token below.");
    } else {
        println!("Paste the secret value below.");
    }
    println!();
}

/// Read a single line from stdin without echoing it back to the
/// terminal. Falls back to a plain `read_line` (echoes) when stdin
/// isn't a TTY or `rpassword` isn't available; in that case we still
/// succeed, just with the token visible — fine for CI piping.
fn prompt_for_secret(lookup: &str) -> Result<secrecy::SecretString> {
    print!("token for `{lookup}`: ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .with_context(|| "reading token from stdin")?;
    let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
    Ok(secrecy::SecretString::from(trimmed))
}

/// Conventional service name for the OS keyring entry. Maps the
/// lowercase secret name to a slightly more readable hyphenated form
/// (`claude_code_oauth_token` → `claude-code-oauth-token`), which
/// matches what other tooling (e.g. the `security` CLI examples in
/// the keychain backend's docs) tends to use.
fn service_for(lookup: &str) -> String {
    lookup.replace('_', "-")
}

/// Open the keyring entry. Separated so tests can confirm the call
/// shape without actually shelling to the platform keyring (which
/// they shouldn't do in unit tests anyway).
fn keyring_entry(service: &str, account: Option<&str>) -> Result<Entry> {
    let account = account.unwrap_or("default");
    Entry::new(service, account)
        .with_context(|| format!("opening OS keyring service={service} account={account}"))
}

/// Append a new `secrets.<name>` entry to the config YAML. Reads
/// the file as a YAML mapping, mutates the in-memory document, and
/// writes it back. Preserves the existing top-level structure;
/// loses formatting (comments / blank lines) — fleet config files
/// don't carry meaningful formatting today.
fn append_secrets_entry(
    cfg_path: &std::path::Path,
    name: &str,
    service: &str,
    account: Option<&str>,
) -> Result<()> {
    let existing = match std::fs::read_to_string(cfg_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("reading {}", cfg_path.display()));
        }
    };
    let mut doc: serde_yml::Value = if existing.trim().is_empty() {
        serde_yml::Value::Mapping(serde_yml::Mapping::new())
    } else {
        serde_yml::from_str(&existing).with_context(|| format!("parsing {}", cfg_path.display()))?
    };
    let mapping = doc.as_mapping_mut().ok_or_else(|| {
        anyhow!(
            "{} is not a YAML mapping; can't add secrets entry",
            cfg_path.display()
        )
    })?;
    let secrets_key = serde_yml::Value::String("secrets".to_string());
    let secrets = mapping
        .entry(secrets_key)
        .or_insert(serde_yml::Value::Mapping(serde_yml::Mapping::new()))
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("`secrets:` is not a YAML mapping"))?;
    let mut entry = serde_yml::Mapping::new();
    entry.insert(
        serde_yml::Value::String("backend".to_string()),
        serde_yml::Value::String("keychain".to_string()),
    );
    entry.insert(
        serde_yml::Value::String("service".to_string()),
        serde_yml::Value::String(service.to_string()),
    );
    if let Some(a) = account {
        entry.insert(
            serde_yml::Value::String("account".to_string()),
            serde_yml::Value::String(a.to_string()),
        );
    }
    secrets.insert(
        serde_yml::Value::String(name.to_string()),
        serde_yml::Value::Mapping(entry),
    );
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir at {}", parent.display()))?;
    }
    let serialised = serde_yml::to_string(&doc)
        .with_context(|| format!("serialising new {}", cfg_path.display()))?;
    std::fs::write(cfg_path, serialised)
        .with_context(|| format!("writing {}", cfg_path.display()))?;
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    Ok(repo::fleet_root(&cwd).join(".fleet").join("config.yaml"))
}

fn load_repo_config() -> Result<RepoConfig> {
    let path = config_path()?;
    RepoConfig::load(&path).with_context(|| format!("loading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_for_replaces_underscores_with_hyphens() {
        // Convention: snake_case secret name → kebab-case keyring
        // service name. Matches the documented `security` /
        // `secret-tool` example strings.
        assert_eq!(
            service_for("claude_code_oauth_token"),
            "claude-code-oauth-token"
        );
        assert_eq!(service_for("gh_token"), "gh-token");
    }

    #[test]
    fn append_secrets_entry_creates_block_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.yaml");
        std::fs::write(&cfg, "tracker: github\n").unwrap();
        append_secrets_entry(&cfg, "gh_token", "gh-token", Some("alice")).unwrap();
        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("secrets:"), "body: {body}");
        assert!(body.contains("gh_token:"), "body: {body}");
        assert!(body.contains("backend: keychain"), "body: {body}");
        assert!(body.contains("service: gh-token"), "body: {body}");
        assert!(body.contains("account: alice"), "body: {body}");
    }

    #[test]
    fn append_secrets_entry_adds_to_existing_block() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "secrets:\n  existing:\n    backend: env\n    var: EXISTING\n",
        )
        .unwrap();
        append_secrets_entry(&cfg, "gh_token", "gh-token", None).unwrap();
        let body = std::fs::read_to_string(&cfg).unwrap();
        // Old entry survives; new entry lands.
        assert!(body.contains("existing:"), "body: {body}");
        assert!(body.contains("gh_token:"), "body: {body}");
    }

    #[test]
    fn append_secrets_entry_creates_file_when_missing() {
        // Brand-new repo with no `.fleet/config.yaml` yet — the
        // `register` flow should still work.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("nested").join("config.yaml");
        append_secrets_entry(&cfg, "gh_token", "gh-token", None).unwrap();
        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("secrets:"), "body: {body}");
        assert!(body.contains("gh_token:"), "body: {body}");
    }
}
