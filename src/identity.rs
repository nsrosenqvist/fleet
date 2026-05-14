//! Brokered identity material forwarded into the `fleet-vm` guest at
//! `fleet start` time.
//!
//! With the host home no longer bind-mounted (see `docs/sandbox.md`),
//! AO workers can no longer transparently read `~/.config/gh/` or
//! `~/.gitconfig` from the host. fleet resolves both on the host
//! immediately before invoking `limactl shell`, then forwards them as
//! env vars via Lima's `--preserve-env` + `LIMA_SHELLENV_ALLOW`. The
//! bootstrap script inside the VM materializes them into VM-private
//! files in the lima user's `$HOME` and `unset`s the env vars.
//!
//! What's brokered today:
//!
//! - **`gh` token** — `GH_TOKEN`. Resolved from `[secrets.gh_token]`
//!   in fleet's config (env / keychain / 1Password) with a fall-back
//!   to running `gh auth token` on the host. Persists in the worker's
//!   env so `gh issue list`, `gh pr create`, etc. just work.
//! - **git identity** — `FLEET_GITCONFIG_B64`, a base64-encoded INI
//!   gitconfig blob. Resolved from `[git]` in fleet's config, with a
//!   fallback to the host's `~/.gitconfig` (user.name + user.email
//!   only). The bootstrap decodes it into `$HOME/.gitconfig`.
//!
//! See `docs/auth.md` for the user-facing UX. The resolvers below
//! intentionally return `Option` rather than bailing: a worker can
//! still do plenty of useful work without git identity or without a
//! gh token; if a particular operation needs one, AO's worker
//! script surfaces the error on its own.

use anyhow::Result;
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::process::ProcessInvoker;
use crate::secrets;

/// Config key that selects a backend for the gh token, matching the
/// `[secrets.<name>]` table in `~/.config/fleet/config.toml`. Mirrors
/// the existing `claude_code_oauth_token` constant in `cli::spawn`.
pub const GH_TOKEN_SECRET_KEY: &str = "gh_token";

/// Resolve the gh token to forward as `GH_TOKEN` into the VM.
///
/// Order:
/// 1. `[secrets.gh_token]` in fleet config — env / keychain / op.
/// 2. `gh auth token` on the host (the lazy-default path: the user
///    has already authed with `gh auth login` and fleet just relays).
/// 3. Give up. Returns `Ok(None)` so the caller can carry on without
///    `GH_TOKEN` set — `gh` inside the VM will fail per-operation
///    with its own clear "not authenticated" message.
///
/// Errors are reserved for "the user *configured* a backend and it
/// failed to fetch" — a misconfigured 1Password reference shouldn't
/// silently fall back to the host's gh login.
pub fn resolve_gh_token(
    cfg: &Config,
    invoker: &Arc<dyn ProcessInvoker>,
) -> Result<Option<SecretString>> {
    if let Some(secret_cfg) = cfg.secrets.get(GH_TOKEN_SECRET_KEY) {
        let backend = secrets::build(secret_cfg, invoker.clone());
        let token = backend.fetch().map_err(|e| {
            anyhow::anyhow!(
                "resolving secret `{GH_TOKEN_SECRET_KEY}` via `{}` backend: {e:#}",
                backend.kind()
            )
        })?;
        return Ok(Some(token));
    }
    Ok(gh_auth_token_from_host(invoker))
}

/// Fallback to `gh auth token` on the host. Returns `None` when gh is
/// missing, the user isn't authed, or the command otherwise fails — all
/// of which are normal first-launch states. We don't propagate the
/// underlying error: `Some(token)` and `None` are the only meaningful
/// signals at this layer, and a missing gh login isn't a fleet bug.
fn gh_auth_token_from_host(invoker: &Arc<dyn ProcessInvoker>) -> Option<SecretString> {
    let out = invoker
        .run("gh", vec!["auth".to_string(), "token".to_string()])
        .ok()?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(SecretString::from(trimmed.to_string()))
}

/// Resolve the worker `$HOME/.gitconfig` content. Returns `None` when
/// neither fleet's `[git]` section nor the host's `~/.gitconfig` yield
/// a usable `user.name`/`user.email` pair (or at least one of them).
///
/// Output is a minimal INI gitconfig — only the keys fleet actually
/// brokers. Signing keys and other sections are intentionally not
/// forwarded; see [`GitConfig`].
pub fn resolve_gitconfig(cfg: &Config) -> Option<String> {
    if !cfg.git.is_empty() {
        return render_gitconfig(cfg.git.user_name.as_deref(), cfg.git.user_email.as_deref());
    }
    let host = host_gitconfig_path()?;
    let (name, email) = parse_user_section(&std::fs::read_to_string(host).ok()?);
    render_gitconfig(name.as_deref(), email.as_deref())
}

/// Base64-encode the gitconfig so it survives a Lima env var round-trip
/// without quoting concerns (newlines, `=`, equal-sign in INI, etc.).
/// The bootstrap script `base64 -d`s it back into a file.
pub fn gitconfig_env_value(rendered: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(rendered.as_bytes())
}

fn host_gitconfig_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".gitconfig"))
}

/// Minimal `[user]`-only gitconfig parser. `git config --get` would be
/// more robust but pulls in subprocess fragility; we only need the two
/// fields. Skips comments, allows arbitrary whitespace around `=`,
/// matches the `[user]` section header case-insensitively.
fn parse_user_section(text: &str) -> (Option<String>, Option<String>) {
    let mut in_user = false;
    let mut name = None;
    let mut email = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_user = line.eq_ignore_ascii_case("[user]");
            continue;
        }
        if !in_user {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"');
        match k.to_ascii_lowercase().as_str() {
            "name" if name.is_none() => name = Some(v.to_string()),
            "email" if email.is_none() => email = Some(v.to_string()),
            _ => {}
        }
    }
    (name, email)
}

/// Render `[user]` from name + email. Returns `None` when both are
/// absent — an INI with neither field would still tell git "use
/// these (empty) values" and override any global defaults, which is
/// worse than no gitconfig at all.
fn render_gitconfig(name: Option<&str>, email: Option<&str>) -> Option<String> {
    use std::fmt::Write;
    if name.is_none() && email.is_none() {
        return None;
    }
    let mut out = String::from("[user]\n");
    if let Some(n) = name {
        let _ = writeln!(out, "\tname = {n}");
    }
    if let Some(e) = email {
        let _ = writeln!(out, "\temail = {e}");
    }
    Some(out)
}

/// Expose the raw secret string for inclusion in `CommandSpec.env_set`.
/// Centralized here so the call site doesn't need to import
/// `secrecy::ExposeSecret` and the conversion to `String` (which copies
/// the secret out of the protected wrapper) happens exactly once at
/// the spawn boundary.
pub fn expose(secret: &SecretString) -> String {
    secret.expose_secret().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::secrets::SecretBackendConfig;
    use mockall::predicate::eq;

    #[test]
    fn parse_user_section_extracts_name_and_email() {
        let text = "[user]\n\tname = Niklas Rosenqvist\n\temail = niklas@example.com\n";
        let (n, e) = parse_user_section(text);
        assert_eq!(n.as_deref(), Some("Niklas Rosenqvist"));
        assert_eq!(e.as_deref(), Some("niklas@example.com"));
    }

    #[test]
    fn parse_user_section_ignores_other_sections() {
        let text = r"
[core]
    editor = nvim
[user]
    name = Alice
    email = alice@example.com
[alias]
    co = checkout
";
        let (n, e) = parse_user_section(text);
        assert_eq!(n.as_deref(), Some("Alice"));
        assert_eq!(e.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn parse_user_section_handles_quoted_value() {
        let text = "[user]\n\tname = \"Alice With Quote\"\n";
        let (n, _) = parse_user_section(text);
        assert_eq!(n.as_deref(), Some("Alice With Quote"));
    }

    #[test]
    fn parse_user_section_returns_none_when_absent() {
        let text = "[core]\n\teditor = nvim\n";
        assert_eq!(parse_user_section(text), (None, None));
    }

    #[test]
    fn render_gitconfig_skips_when_both_absent() {
        assert!(render_gitconfig(None, None).is_none());
    }

    #[test]
    fn render_gitconfig_emits_only_present_fields() {
        let rendered = render_gitconfig(Some("Alice"), None).expect("renders");
        assert!(rendered.contains("name = Alice"));
        assert!(!rendered.contains("email"));
    }

    #[test]
    fn resolve_gh_token_uses_fleet_secret_first() {
        // No need for a real backend — the gh fallback should NEVER
        // run when a secret is configured. Mockall would fail the
        // assertion if it ran unexpectedly.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().never();
        let mut cfg = Config::default();
        cfg.secrets.insert(
            "gh_token".to_string(),
            SecretBackendConfig::Env {
                var: "FLEET_TEST_GH_TOKEN_DUMMY".to_string(),
            },
        );
        // SAFETY: env mutation is process-global; restored before return.
        let guard = crate::test_env::xdg_lock();
        unsafe { std::env::set_var("FLEET_TEST_GH_TOKEN_DUMMY", "ghp_via_secret") };
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let resolved = resolve_gh_token(&cfg, &invoker).expect("resolve");
        unsafe { std::env::remove_var("FLEET_TEST_GH_TOKEN_DUMMY") };
        drop(guard);
        assert_eq!(
            resolved.map(|s| s.expose_secret().to_string()),
            Some("ghp_via_secret".to_string())
        );
    }

    #[test]
    fn resolve_gh_token_falls_back_to_host_gh_when_no_secret() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("gh"), eq(vec!["auth".to_string(), "token".to_string()]))
            .returning(|_, _| Ok("ghp_via_host_gh".to_string()));
        let cfg = Config::default();
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let resolved = resolve_gh_token(&cfg, &invoker).expect("resolve");
        assert_eq!(
            resolved.map(|s| s.expose_secret().to_string()),
            Some("ghp_via_host_gh".to_string())
        );
    }

    #[test]
    fn resolve_gh_token_returns_none_when_host_gh_fails() {
        // gh not installed or `gh auth status` rejected — both surface
        // here as an `Err` from `ProcessInvoker.run`. We return Ok(None)
        // rather than bubbling, so the caller can carry on without a
        // GH_TOKEN forwarded.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| anyhow::bail!("not found"));
        let cfg = Config::default();
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let resolved = resolve_gh_token(&cfg, &invoker).expect("ok-with-none");
        assert!(resolved.is_none());
    }
}
