//! Token-injecting subcommands: `start`, `spawn`, `batch-spawn`.
//!
//! The token never lands on a command line — it's set as an env var on the
//! child `limactl` process, which forwards it into the guest via
//! `--preserve-env` + `LIMA_SHELLENV_ALLOW=CLAUDE_CODE_OAUTH_TOKEN`.

use anyhow::{Context, Result, bail};
use secrecy::ExposeSecret;
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::lima::{Lima, VmStatus};
use crate::process::{RealProcessInvoker, run_interactive};
use crate::secrets::{self, SecretBackendConfig};

const SECRET_KEY: &str = "claude_code_oauth_token";

pub fn run_start(repo_root: &Path, no_dashboard: bool, no_orchestrator: bool) -> Result<i32> {
    let mut argv = vec!["start".to_string()];
    if no_dashboard {
        argv.push("--no-dashboard".to_string());
    }
    if no_orchestrator {
        argv.push("--no-orchestrator".to_string());
    }
    run_with_token(repo_root, &argv)
}

pub fn run_spawn(
    repo_root: &Path,
    issue: &str,
    prompt: Option<&str>,
    agent: Option<&str>,
) -> Result<i32> {
    let mut argv = vec!["spawn".to_string(), issue.to_string()];
    if let Some(p) = prompt {
        argv.push("--prompt".to_string());
        argv.push(p.to_string());
    }
    if let Some(a) = agent {
        argv.push("--agent".to_string());
        argv.push(a.to_string());
    }
    run_with_token(repo_root, &argv)
}

pub fn run_batch_spawn(repo_root: &Path, issues: &[String]) -> Result<i32> {
    let mut argv = vec!["batch-spawn".to_string()];
    argv.extend(issues.iter().cloned());
    run_with_token(repo_root, &argv)
}

fn run_with_token(repo_root: &Path, ao_argv: &[String]) -> Result<i32> {
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker.clone(), "fleet-vm");

    // Preflight: VM up?
    match lima.status() {
        VmStatus::Running => {}
        VmStatus::Stopped => bail!(
            "Lima VM `{}` is stopped. Start it with: fleet vm-start",
            lima.vm_name()
        ),
        VmStatus::Missing => bail!(
            "Lima VM `{}` not found. Create with the template at ~/.lima/fleet-vm.yaml.",
            lima.vm_name()
        ),
    }

    // Preflight: ANTHROPIC_API_KEY would override the subscription token.
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        bail!(
            "ANTHROPIC_API_KEY is set in this shell; it would override the subscription token. \
             Run: unset ANTHROPIC_API_KEY"
        );
    }

    // Resolve secret.
    let cfg = Config::load(repo_root)?;
    let secret_cfg: &SecretBackendConfig = cfg.secrets.get(SECRET_KEY).with_context(|| {
        format!(
            "no secret configured under `[secrets.{SECRET_KEY}]`. Run `fleet config init` to scaffold \
             ~/.config/fleet/config.toml, or create an in-repo `.fleet.local.toml`."
        )
    })?;
    let backend = secrets::build(secret_cfg, invoker);
    let token = backend.fetch().with_context(|| {
        format!(
            "resolving secret `{SECRET_KEY}` via `{}` backend",
            backend.kind()
        )
    })?;

    // Auth handoff to claude.
    // Three pieces have to be in place before AO's interactive `claude`
    // launch will skip the login menu in claude 2.1.139:
    //
    //   1. `~/.claude/.credentials.json` (mode 600) holding the OAuth token
    //      in the `claudeAiOauth` shape.
    //   2. `~/.claude.json` with `hasCompletedOnboarding: true`.
    //   3. `CLAUDE_CODE_OAUTH_TOKEN` MUST NOT be in claude's process env —
    //      env-token mode shows the login menu in interactive flows.
    //
    // The bootstrap tmux session anchors the server (`tmux start-server`
    // exits immediately without one, leaving the control socket missing).
    //
    // The token reaches the VM via Lima `--preserve-env` + `LIMA_SHELLENV_ALLOW`
    // only, lives in the wrapper bash's env for a few milliseconds while it
    // writes the file, and never touches a host command line.
    let ao_cmd = ao_argv
        .iter()
        .map(|a| shell_quote_single(a))
        .collect::<Vec<_>>()
        .join(" ");
    let bash_script = BOOTSTRAP_SCRIPT.replace("__AO_CMD__", &ao_cmd);

    let limactl_argv = vec![
        "shell".to_string(),
        "--preserve-env".to_string(),
        "--workdir".to_string(),
        repo_root.display().to_string(),
        lima.vm_name().to_string(),
        "bash".to_string(),
        "-c".to_string(),
        bash_script,
    ];

    let token_str = token.expose_secret();
    run_interactive(
        "limactl",
        &limactl_argv,
        &[
            ("CLAUDE_CODE_OAUTH_TOKEN", token_str),
            ("LIMA_SHELLENV_ALLOW", "CLAUDE_CODE_OAUTH_TOKEN"),
            // The Ubuntu 24.04 VM doesn't carry terminfo for newer host
            // terminals (`xterm-ghostty`, `wezterm`, …); tmux bails with
            // "missing or unsuitable terminal". Force a widely-known TERM
            // for the in-VM bash + tmux server. Worker panes are unaffected
            // — tmux still sets `screen-256color` inside its panes.
            ("TERM", "xterm-256color"),
        ],
        &["ANTHROPIC_API_KEY"],
    )
}

/// In-VM bootstrap: write claude's credentials file + onboarding marker,
/// scrub the OAuth env, anchor a tmux server, then run `ao __AO_CMD__`.
/// `__AO_CMD__` is replaced by [`run_with_token`] with the shell-escaped
/// argv to pass to `ao`. We intentionally use a sentinel rather than
/// `format!()` so the JSON / printf / Node literals don't need brace
/// escaping.
const BOOTSTRAP_SCRIPT: &str = r#"
set -e
mkdir -p "$HOME/.claude"
umask 077

# 1. Credentials file (claude reads this in interactive mode).
printf '{"claudeAiOauth":{"accessToken":"%s","scopes":["user:inference"],"subscriptionType":"subscription"}}\n' "$CLAUDE_CODE_OAUTH_TOKEN" > "$HOME/.claude/.credentials.json"
chmod 600 "$HOME/.claude/.credentials.json"

# 2. Mark onboarding complete in ~/.claude.json (merge-update so we don't
#    clobber userID / firstStartTime / cached feature flags claude has
#    populated). The per-project `bypassPermissionsModeAccepted` gets set
#    by the project's `postCreate` hook (see agent-orchestrator.yaml) once
#    AO has created the worktree.
node -e '
const fs = require("fs");
const path = process.env.HOME + "/.claude.json";
let data = {};
try { data = JSON.parse(fs.readFileSync(path, "utf8")); } catch (e) {}
data.hasCompletedOnboarding = true;
data.bypassPermissionsModeAccepted = true;
fs.writeFileSync(path, JSON.stringify(data, null, 2));
'

# 2b. Set `skipDangerousModePermissionPrompt: true` in ~/.claude/settings.json.
#     This is the actual gate that suppresses the "Bypass Permissions" dialog
#     claude shows on every interactive launch with --dangerously-skip-permissions.
#     The per-project `bypassPermissionsModeAccepted` flag is necessary but not
#     sufficient; this top-level setting is what makes it stick.
node -e '
const fs = require("fs");
const path = process.env.HOME + "/.claude/settings.json";
let data = {};
try { data = JSON.parse(fs.readFileSync(path, "utf8")); } catch (e) {}
data.skipDangerousModePermissionPrompt = true;
fs.writeFileSync(path, JSON.stringify(data, null, 2));
'

# 3. Strip the OAuth env so claude takes the file-based auth path (the
#    interactive env-token path is buggy in 2.1.139 — shows the login menu).
unset CLAUDE_CODE_OAUTH_TOKEN

# 4. Anchor a tmux server (start-server alone exits without an anchor).
if ! tmux has-session 2>/dev/null; then
    tmux new-session -d -s _fleet_bootstrap
    trap 'tmux kill-session -t _fleet_bootstrap 2>/dev/null || true' EXIT
fi

# 5. Hand off to AO.
ao __AO_CMD__
"#;

/// Quote a string for inclusion in a single-quoted bash word.
/// Each embedded `'` becomes `'\''` (close, escaped quote, reopen).
fn shell_quote_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::shell_quote_single;

    #[test]
    fn quotes_plain_word() {
        assert_eq!(shell_quote_single("hello"), "'hello'");
    }
    #[test]
    fn quotes_word_with_single_quote() {
        assert_eq!(shell_quote_single("it's"), "'it'\\''s'");
    }
    #[test]
    fn quotes_word_with_dollar_and_spaces() {
        // Single-quoting prevents shell expansion of $VAR.
        assert_eq!(
            shell_quote_single("hello $WORLD what?"),
            "'hello $WORLD what?'"
        );
    }
}
