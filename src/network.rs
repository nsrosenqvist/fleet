//! Egress allowlist for AO workers.
//!
//! Phase 3 routes worker outbound traffic through an in-VM `tinyproxy`
//! configured with a hostname allowlist. The list is the union of:
//!
//! - [`BUILT_IN_ALLOW`] — hosts AO itself needs to function (Anthropic,
//!   GitHub, npm registry, Ubuntu mirrors). Always allowed when mode is
//!   `allowlist`; the user can't remove these without breaking the
//!   stack.
//! - `[network] extra_allow` in `~/.config/fleet/config.toml` — the
//!   user's per-project / per-stack additions.
//!
//! At `fleet start`, the resolved list is rendered to
//! `/etc/tinyproxy/fleet-allow.conf` inside the VM (via `limactl shell
//! --user root`). The worker's bootstrap exports `HTTPS_PROXY`,
//! `HTTP_PROXY`, and `NO_PROXY` so cooperating tools route through the
//! proxy. See `docs/network.md` for the rest of the story (including
//! what this does NOT enforce).
//!
//! The list is ALSO rendered into the worker AGENTS.md so the agent
//! sees exactly which hosts are reachable and gives a clean
//! "host X is not on the allowlist" error to the user rather than
//! retrying a 30-second timeout against a blocked CDN.
//!
//! Hostnames here are bare (no `https://`), matched suffix-wise by
//! tinyproxy's URL filter. An entry of `example.com` covers
//! `api.example.com`, `cdn.example.com`, etc.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{Config, NetworkConfig};
use crate::process::ProcessInvoker;

/// Hosts the AO + claude-code + git-bug worker stack itself needs.
/// Always present when `mode = "allowlist"`. Keep this list small and
/// audit-able — every entry here is a hostname every fleet user
/// trusts unconditionally.
pub const BUILT_IN_ALLOW: &[&str] = &[
    // Anthropic API — Claude calls this for every prompt.
    "api.anthropic.com",
    // GitHub: gh CLI (api + raw + the web URL for PR previews),
    // git over HTTPS, raw user content (gh-dash + agent docs).
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
    "cli.github.com",
    // npm registry — claude-code + AO are npm packages.
    "registry.npmjs.org",
    // Node.js NodeSource deb (provisioning step; tracker-tool
    // reinstalls from preflight).
    "deb.nodesource.com",
    "nodejs.org",
    // Ubuntu apt mirrors (NodeSource setup + ad-hoc package
    // installs). Two pools cover every Ubuntu region.
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ports.ubuntu.com",
    // git-bug release binary (preflight auto-install).
    "git-bug.org",
];

/// Resolve the active allow-list. Built-in baseline + user extras,
/// deduped, stable-sorted for predictable rendering. Returns an
/// empty Vec when `mode != allowlist` — that signals "no allowlist
/// applies" to all the downstream consumers (proxy renderer, AGENTS.md
/// renderer) so they can no-op cleanly without each duplicating the
/// mode check.
pub fn resolve_allow_list(cfg: &NetworkConfig) -> Vec<String> {
    if !cfg.is_allowlist() {
        return Vec::new();
    }
    let mut out: Vec<String> = BUILT_IN_ALLOW.iter().map(|s| (*s).to_string()).collect();
    out.extend(cfg.extra_allow.iter().cloned());
    out.sort();
    out.dedup();
    out
}

/// Render the `Filter` file tinyproxy reads. One regex per line, each
/// anchored so a `github.com` entry doesn't accidentally allow
/// `evilgithub.com.attacker.example`. tinyproxy's `Filter` directive
/// applies these regexes to the **CONNECT host** for HTTPS traffic
/// and the **Host header / URL** for HTTP, so the same file gates
/// both.
///
/// `FilterDefaultDeny Yes` in the tinyproxy.conf flips the default:
/// anything not matching one of these regexes is rejected with a 403.
pub fn render_tinyproxy_filter(allow: &[String]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for host in allow {
        // Escape regex metacharacters in the hostname. Hostnames
        // shouldn't contain regex specials but `.` does — treat it
        // as a literal dot, not "any character".
        let escaped: String = host
            .chars()
            .map(|c| {
                if matches!(c, '.' | '*' | '?' | '+' | '(' | ')' | '[' | ']' | '\\') {
                    format!("\\{c}")
                } else {
                    c.to_string()
                }
            })
            .collect();
        let _ = writeln!(out, "(^|\\.){escaped}$");
    }
    out
}

/// Render the "Network access" section appended to the worker
/// AGENTS.md. Empty when no allowlist is active — the renderer in
/// `templates_sync` skips appending anything in that case.
pub fn render_agents_md_section(allow: &[String]) -> String {
    use std::fmt::Write;
    if allow.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n## Network access\n\
         \n\
         This sandbox only allows outbound traffic to the following hosts \
         (via the in-VM `tinyproxy`):\n\n",
    );
    for host in allow {
        let _ = writeln!(out, "- `{host}`");
    }
    out.push_str(
        "\n\
         Requests to any other host return a 403 from the proxy. If you \
         need an additional host, **stop and ask the user** to add it to \
         `network.extra_allow` in `~/.config/fleet/config.toml`. Do not \
         retry in a loop — the proxy isn't going to change its mind.\n",
    );
    out
}

/// Path inside the VM where tinyproxy reads its `Filter` regex file.
/// Set by the cloud-init `tinyproxy.conf` we ship in
/// `templates/fleet-vm.yaml`. Must match the `Filter` directive there.
pub const IN_VM_FILTER_PATH: &str = "/etc/tinyproxy/fleet-allow.filter";

/// Sync the resolved allowlist into the running VM and reload
/// tinyproxy so the new filter takes effect. No-op when fleet's
/// network mode is `open` (tinyproxy may still be running but its
/// filter file is empty / the proxy is unreachable from worker env
/// because `HTTPS_PROXY` isn't set).
///
/// Runs `limactl shell --user root fleet-vm bash -c '<script>'` —
/// idempotent, safe to call on every `fleet start` invocation. The
/// script base64-decodes the rendered filter (avoids quoting
/// concerns for the parens / backslashes / dollar-anchors the
/// regexes contain) into a tempfile under /run, atomically renames
/// into `IN_VM_FILTER_PATH`, and `systemctl reload tinyproxy.service`.
///
/// Best-effort by design: a failure here downgrades to a warning,
/// not a hard fail of `fleet start`. The proxy may be stale until
/// the next start, but AO + workers continue to launch.
pub fn sync_in_vm_filter(cfg: &Config, invoker: &Arc<dyn ProcessInvoker>) -> Result<()> {
    if !cfg.network.is_allowlist() {
        return Ok(());
    }
    let allow = resolve_allow_list(&cfg.network);
    let rendered = render_tinyproxy_filter(&allow);
    let encoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(rendered.as_bytes())
    };
    let script = format!(
        r#"set -eu
mkdir -p /etc/tinyproxy
TMP=$(mktemp /run/fleet-allow.filter.XXXXXX)
printf '%s' '{encoded}' | base64 -d > "$TMP"
chmod 644 "$TMP"
mv "$TMP" {IN_VM_FILTER_PATH}
systemctl reload tinyproxy.service 2>/dev/null || systemctl restart tinyproxy.service
"#
    );
    invoker
        .run(
            "limactl",
            vec![
                "shell".to_string(),
                "--user".to_string(),
                "root".to_string(),
                "fleet-vm".to_string(),
                "bash".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("syncing tinyproxy allowlist into fleet-vm")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_mode_yields_empty_list() {
        let cfg = NetworkConfig::default();
        assert!(resolve_allow_list(&cfg).is_empty());
    }

    #[test]
    fn allowlist_mode_includes_baseline() {
        let cfg = NetworkConfig {
            mode: crate::config::NetworkMode::Allowlist,
            extra_allow: Vec::new(),
        };
        let list = resolve_allow_list(&cfg);
        assert!(list.iter().any(|h| h == "api.anthropic.com"));
        assert!(list.iter().any(|h| h == "github.com"));
        // Built-in list is non-trivial — guard against accidental
        // truncation.
        assert!(list.len() >= 5);
    }

    #[test]
    fn allowlist_mode_merges_extras_and_dedupes() {
        let cfg = NetworkConfig {
            mode: crate::config::NetworkMode::Allowlist,
            extra_allow: vec![
                "crates.io".to_string(),
                "github.com".to_string(), // dupe with built-in
            ],
        };
        let list = resolve_allow_list(&cfg);
        assert_eq!(list.iter().filter(|h| *h == "github.com").count(), 1);
        assert!(list.iter().any(|h| h == "crates.io"));
    }

    #[test]
    fn render_filter_escapes_dots_and_anchors() {
        let list = vec!["github.com".to_string()];
        let filter = render_tinyproxy_filter(&list);
        // The escaped dot prevents `evilgithubXcom` from matching;
        // the `(^|\.)…$` anchor prevents `evilgithub.com.bad` too.
        assert!(filter.contains("(^|\\.)github\\.com$"));
        assert!(!filter.contains("(^|\\.)github.com$"));
    }

    #[test]
    fn render_section_empty_when_no_allowlist() {
        assert!(render_agents_md_section(&[]).is_empty());
    }

    #[test]
    fn render_section_lists_hosts_and_advises_no_retry() {
        let list = vec!["api.anthropic.com".to_string(), "github.com".to_string()];
        let md = render_agents_md_section(&list);
        assert!(md.contains("## Network access"));
        assert!(md.contains("`api.anthropic.com`"));
        assert!(md.contains("`github.com`"));
        // The "don't retry" guidance is what stops a confused worker
        // from burning ten minutes on a blocked CDN. Lock it down
        // with a string test so a future rewrite doesn't drop it.
        assert!(md.contains("Do not retry"));
    }
}
