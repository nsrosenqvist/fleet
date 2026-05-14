//! Token-injecting subcommands: `start`, `spawn`, `batch-spawn`.
//!
//! The token never lands on a command line — it's set as an env var on the
//! child `limactl` process, which forwards it into the guest via
//! `--preserve-env` + `LIMA_SHELLENV_ALLOW=CLAUDE_CODE_OAUTH_TOKEN`.

use anyhow::{Context, Result, bail};
use secrecy::ExposeSecret;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, ResolvedAuthMode};
use crate::lima::{Lima, VmStatus};
use crate::process::{CommandSpec, RealProcessInvoker, run_interactive_spec};
use crate::secrets::{self, SecretBackendConfig};

const SECRET_KEY: &str = "claude_code_oauth_token";

/// Provider env vars forwarded into the VM by `passthrough` mode. AO's
/// agent plugins read whichever subset they need; fleet doesn't have
/// to know the per-agent mapping. Extending this list is cheap — add
/// the var here and Lima will pass it through.
const PASSTHROUGH_ENV_ALLOW: &str =
    "ANTHROPIC_API_KEY,OPENAI_API_KEY,GEMINI_API_KEY,GOOGLE_API_KEY,COMPOSIO_API_KEY";

pub fn run_start(repo_root: &Path, no_dashboard: bool, no_orchestrator: bool) -> Result<i32> {
    ensure_vm_running()?;
    sync_network_filter()?;
    sync_aoworker_mount_acls();
    // Resolve the project from cwd so AO sets up lifecycle polling for
    // it on startup. Without an explicit project, AO's project-supervisor
    // reconcile loop is the only path that registers a project in
    // `running.json`, and it can take up to 60s — long enough that an
    // immediate `ao spawn` after `ao start` hits "AO is not polling
    // project <foo>". Resolution failure is non-fatal: AO will fall
    // back to its own cwd / single-project logic.
    let project = project_for_repo(repo_root);
    let spec = build_start_spec(project.as_deref(), no_dashboard, no_orchestrator)?;
    run_interactive_spec(&spec)
}

/// Push the latest tinyproxy allowlist into the VM. Idempotent; runs
/// before every `fleet start` so a change to `network.extra_allow` in
/// the user's config takes effect on the next start without manual
/// proxy reload. Best-effort: a failure here logs and continues
/// rather than blocking AO startup, since the proxy will simply
/// continue to serve the previous filter.
pub fn sync_network_filter() -> Result<()> {
    let cfg = Config::load()?;
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    if let Err(e) = crate::network::sync_in_vm_filter(&cfg, &invoker) {
        tracing::warn!(error = ?e, "tinyproxy filter sync failed; using previous filter");
    }
    Ok(())
}

/// Apply a `setfacl -m u:aoworker:rwX` (recursive + default) on every
/// writable project source mount. Lima's bind mount maps host UID
/// 1000 (the host user / lima) into the guest, so aoworker (UID
/// 2000) needs explicit ACL grant to write into the worktree
/// admin files git creates under `<host-repo>/.git/worktrees/`.
///
/// Idempotent — re-applying the same ACL is a no-op at the kernel
/// level. Runs as lima via sudo (lima has passwordless sudo via
/// Lima's default cloud-init). Best-effort: log + continue on
/// failure, since a stale ACL just means worker commits fail in a
/// noisy way the user can investigate, not that fleet should refuse
/// to start.
///
/// ACLs are stored on the host's underlying filesystem; once
/// applied they survive VM rebuilds. We re-apply on every start so
/// a newly-added project (whose path joined the AO catalog after
/// the VM was created) picks up the right ACL without a
/// `setfacl` ritual the user has to remember.
pub fn sync_aoworker_mount_acls() {
    let Ok(Some((_, ao))) = crate::ao::config::AoConfig::load() else {
        return;
    };
    let writable_paths: Vec<String> = ao
        .projects
        .values()
        .map(|p| p.path.display().to_string())
        .collect();
    if writable_paths.is_empty() {
        return;
    }
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    let Some(workdir) = crate::ao::config::AoConfig::workdir() else {
        return;
    };
    let quoted: Vec<String> = writable_paths
        .iter()
        .map(|p| shell_quote_single(p))
        .collect();
    let paths_argv = quoted.join(" ");
    let script = format!(
        r#"set -eu
for p in {paths_argv}; do
    sudo setfacl -R  -m  u:aoworker:rwX "$p" 2>/dev/null || true
    sudo setfacl -R  -d  -m u:aoworker:rwX "$p" 2>/dev/null || true
done"#
    );
    if let Err(e) = lima.shell(&workdir, vec!["bash".to_string(), "-c".to_string(), script]) {
        tracing::warn!(error = ?e, "aoworker mount ACL sync failed; worker may not be able to write to project mount");
    }
}

pub fn run_spawn(
    repo_root: &Path,
    issue: &str,
    prompt: Option<&str>,
    agent: Option<&str>,
) -> Result<i32> {
    ensure_vm_running()?;
    let spec = build_spawn_spec(repo_root, issue, prompt, agent)?;
    run_interactive_spec(&spec)
}

pub fn run_batch_spawn(repo_root: &Path, issues: &[String]) -> Result<i32> {
    ensure_vm_running()?;
    let spec = build_batch_spawn_spec(repo_root, issues)?;
    run_interactive_spec(&spec)
}

/// Build the spec for `ao start [<project>] [--no-dashboard]
/// [--no-orchestrator]`. Includes the full OAuth credentials-write
/// bootstrap when the resolved auth mode is `claude-oauth`. Always
/// runs in daemon mode: `ao start` is a foreground supervisor by
/// design, so the bash bootstrap `setsid`s it and polls
/// `running.json` for readiness so the caller sees a real exit code
/// within ~seconds instead of hanging on a child that never returns.
///
/// `project` is the AO project key for the current scope (e.g.
/// `"fleet"`). When `Some`, fleet passes it as the positional arg to
/// `ao start` AND has the readiness poll wait for
/// `running.json.projects` to contain it — otherwise AO declares
/// success the moment `register()` writes the file but the lifecycle
/// supervisor's reconcile loop may not have attached the project yet,
/// so an immediate `ao spawn` fails with "AO is not polling project
/// <foo>". When `None` (e.g. fleet started outside any project dir)
/// we fall back to file-existence polling only.
pub fn build_start_spec(
    project: Option<&str>,
    no_dashboard: bool,
    no_orchestrator: bool,
) -> Result<CommandSpec> {
    let mut argv = vec!["start".to_string()];
    if let Some(p) = project {
        argv.push(p.to_string());
    }
    if no_dashboard {
        argv.push("--no-dashboard".to_string());
    }
    if no_orchestrator {
        argv.push("--no-orchestrator".to_string());
    }
    build_with_token(&argv, /* daemonize */ true, project)
}

/// Build the spec for `ao spawn <qualified-issue> [--prompt …] [--agent …]`.
pub fn build_spawn_spec(
    repo_root: &Path,
    issue: &str,
    prompt: Option<&str>,
    agent: Option<&str>,
) -> Result<CommandSpec> {
    let qualified = qualify_issue(repo_root, issue)?;
    let mut argv = vec!["spawn".to_string(), qualified];
    if let Some(p) = prompt {
        argv.push("--prompt".to_string());
        argv.push(p.to_string());
    }
    if let Some(a) = agent {
        argv.push("--agent".to_string());
        argv.push(a.to_string());
    }
    build_with_token(&argv, /* daemonize */ false, None)
}

/// Build the spec for `ao batch-spawn <qualified-issue> …`.
pub fn build_batch_spawn_spec(repo_root: &Path, issues: &[String]) -> Result<CommandSpec> {
    let mut argv = vec!["batch-spawn".to_string()];
    for issue in issues {
        argv.push(qualify_issue(repo_root, issue)?);
    }
    build_with_token(&argv, /* daemonize */ false, None)
}

/// `wait_project` is the project key the daemon poll waits to see in
/// `running.json.projects` before declaring readiness. Only meaningful
/// when `daemonize` is true; ignored for `spawn` / `batch-spawn` /
/// `passthrough` invocations.
fn build_with_token(
    ao_argv: &[String],
    daemonize: bool,
    wait_project: Option<&str>,
) -> Result<CommandSpec> {
    ensure_worktree_workspace()?;
    let cfg = Config::load()?;
    match cfg.agent_auth.resolve() {
        ResolvedAuthMode::ClaudeOauth => {
            build_claude_oauth_spec(ao_argv, &cfg, daemonize, wait_project)
        }
        ResolvedAuthMode::Passthrough => build_passthrough_spec(ao_argv, daemonize, wait_project),
    }
}

/// Resolve the AO project covering `repo_root`, returning `None` when
/// the AO config is absent / unparseable or `repo_root` doesn't fall
/// under any `projects.*.path:`. Non-fatal; callers proceed without
/// an explicit project arg in that case.
fn project_for_repo(repo_root: &Path) -> Option<String> {
    let (_, ao) = crate::ao::config::AoConfig::load().ok().flatten()?;
    ao.project_for_cwd(repo_root).map(String::from)
}

/// Refuse to hand off to AO when `defaults.workspace` in the AO yaml
/// isn't `worktree`. The TUI's preflight catches this for `fleet ui`,
/// but `fleet spawn` / `fleet start` / `fleet batch-spawn` from a
/// shell bypass preflight entirely — this guard is the only thing
/// preventing AO from running the agent inside the host checkout.
///
/// `Ok(())` (no-op) when the AO yaml doesn't exist yet — the caller
/// will produce a more specific error about the missing config when
/// it tries to qualify the issue.
fn ensure_worktree_workspace() -> Result<()> {
    let Some((_, ao)) = crate::ao::config::AoConfig::load()? else {
        return Ok(());
    };
    if ao.defaults.workspace_is_worktree() {
        return Ok(());
    }
    bail!(
        "agent-orchestrator.yaml has defaults.workspace = {} (must be `worktree`).\n\
         Spawning would let the agent run inside the host repo's working tree.\n\
         Edit ~/.config/fleet/agent-orchestrator.yaml and set:\n\n  defaults:\n    workspace: worktree",
        ao.defaults.workspace.as_deref().unwrap_or("(unset)")
    );
}

/// Like [`crate::ao::config::AoConfig::workdir`] but bails with a
/// pointed error message when the workdir isn't usable for `ao` —
/// either because we can't resolve a path at all (no $HOME) or
/// because the yaml inside it doesn't exist (no projects registered).
///
/// Spawn / start / passthrough need a real config in place before AO
/// is invoked, since AO will otherwise auto-scaffold one in an
/// unexpected location and report "No config found" first.
fn ao_workdir_or_bail() -> Result<PathBuf> {
    let dir = crate::ao::config::AoConfig::workdir()
        .context("no $HOME / $XDG_CONFIG_HOME — can't resolve the AO config directory")?;
    let yaml = crate::ao::config::AoConfig::default_xdg_path()
        .expect("workdir() returned Some, so default_xdg_path() must too");
    if !yaml.is_file() {
        bail!(
            "no AO config at {}. Launch fleet's TUI and press `c` to scaffold it, or create the \
             file manually with at least one `projects.*` entry whose `path:` covers your repo.",
            yaml.display()
        );
    }
    // Materialize the worker AGENTS.md alongside the AO yaml so
    // `agentRulesFile: <xdg>/templates/AGENTS.md` entries resolve on
    // every code path that talks to AO, not just the ones the TUI
    // walked through. Non-fatal — log and continue.
    if let Err(e) = crate::templates_sync::ensure_worker_agents_md() {
        tracing::warn!(error = ?e, "failed to materialize worker AGENTS.md");
    }
    Ok(dir)
}

/// Re-write a bare issue id (`42`, `INT-100`) as the project-prefixed
/// form (`fl/42`) AO expects when there are multiple projects in the
/// catalog. The project comes from `project_for_cwd(repo_root)` — the
/// deepest `projects.*.path:` that covers the launch directory.
///
/// Already-prefixed ids (anything containing `/`) pass through, so
/// users who type `fl/42` in the spawn picker aren't double-prefixed
/// into `fl/fl/42`.
fn qualify_issue(repo_root: &Path, issue: &str) -> Result<String> {
    if issue.contains('/') {
        return Ok(issue.to_string());
    }
    let (_, ao) = crate::ao::config::AoConfig::load()
        .context("loading AO config")?
        .with_context(|| {
            let path = crate::ao::config::AoConfig::default_xdg_path().map_or_else(
                || "~/.config/fleet/agent-orchestrator.yaml".to_string(),
                |p| p.display().to_string(),
            );
            format!("no AO config at {path}. Press `c` in fleet's TUI to scaffold one.")
        })?;
    let key = ao.project_for_cwd(repo_root).with_context(|| {
        format!(
            "no `projects.*` entry in agent-orchestrator.yaml covers `{}`. Add one whose `path:` \
             matches this directory.",
            repo_root.display()
        )
    })?;
    let project = ao
        .projects
        .get(key)
        .expect("project_for_cwd returned a key not in the map");
    let prefix = project.session_prefix.as_deref().with_context(|| {
        format!(
            "project `{key}` has no `sessionPrefix`. Add one so AO can disambiguate issues by \
             project."
        )
    })?;
    Ok(format!("{prefix}/{issue}"))
}

/// Pre-`ao start` flow for the Claude Code subscription path: write
/// `~/.claude/.credentials.json` inside the VM with the OAuth token,
/// scrub the env, anchor a tmux server, then hand off to AO. This is
/// the historical default and refuses to start if `ANTHROPIC_API_KEY`
/// is set in the shell — that var would make claude prefer API auth
/// over the OAuth credential we just wrote and silently bypass the
/// handoff.
fn build_claude_oauth_spec(
    ao_argv: &[String],
    cfg: &Config,
    daemonize: bool,
    wait_project: Option<&str>,
) -> Result<CommandSpec> {
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker.clone(), "fleet-vm");
    // Note: no `ensure_vm_running` here. The TUI's Shift+S chains a
    // `limactl start fleet-vm` phase before this spec runs, so the VM
    // can be stopped at *build* time. CLI callers (`fleet start`,
    // `fleet spawn`, …) enforce the running-VM precondition at the
    // `run_*` layer above.
    let workdir = ao_workdir_or_bail()?;

    // Refusal only fires under claude-oauth: under `passthrough`, an
    // ANTHROPIC_API_KEY is exactly what Codex / Aider users need to
    // pass into the VM.
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        bail!(
            "ANTHROPIC_API_KEY is set in this shell; under `agent_auth = \"claude-oauth\"` it \
             would override the subscription token. Either `unset ANTHROPIC_API_KEY` or switch \
             to `agent_auth = \"passthrough\"` in your fleet config."
        );
    }

    // Resolve secret.
    let secret_cfg: &SecretBackendConfig = cfg.secrets.get(SECRET_KEY).with_context(|| {
        format!(
            "no secret configured under `[secrets.{SECRET_KEY}]`. Run `fleet config init` to \
             scaffold ~/.config/fleet/config.toml, then `fleet config edit` to point at your \
             secret backend."
        )
    })?;
    let backend = secrets::build(secret_cfg, invoker.clone());
    let token = backend.fetch().with_context(|| {
        format!(
            "resolving secret `{SECRET_KEY}` via `{}` backend",
            backend.kind()
        )
    })?;

    // Brokered identity material. Both calls are best-effort: a worker
    // can still do useful work without either, so resolution failures
    // ("no gh login on host", "no [git] section and no host
    // ~/.gitconfig") downgrade to None rather than aborting `fleet
    // start`. The specific operations that need them (gh issue list,
    // git commit) surface their own errors when they fire.
    let gh_token = crate::identity::resolve_gh_token(cfg, &invoker)?;
    let gitconfig = crate::identity::resolve_gitconfig(cfg);

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
    let template = if daemonize {
        BOOTSTRAP_SCRIPT_DAEMON
    } else {
        BOOTSTRAP_SCRIPT
    };
    let bash_script = template
        .replace("__AO_CMD__", &ao_cmd)
        .replace("__WAIT_PROJECT__", wait_project.unwrap_or(""))
        .replace("__IDENTITY_PRELUDE__", IDENTITY_PRELUDE);

    let token_str = token.expose_secret().to_string();
    let ao_global_config = crate::ao::config::AoConfig::default_xdg_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut env_set: Vec<(String, String)> = vec![
        ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token_str),
        // AO_GLOBAL_CONFIG points AO's `getGlobalConfigPath()` at
        // fleet's XDG config. Without it, the project-supervisor
        // reconcile loop reads ~/.agent-orchestrator/config.yaml
        // (the upstream default), finds no project, silently
        // bails — and `running.json.projects` stays empty so an
        // immediate `ao spawn` fails with "AO is not polling
        // project <foo>".
        ("AO_GLOBAL_CONFIG".to_string(), ao_global_config),
        // The Ubuntu 24.04 VM doesn't carry terminfo for newer host
        // terminals (`xterm-ghostty`, `wezterm`, …); tmux bails with
        // "missing or unsuitable terminal". Force a widely-known TERM
        // for the in-VM bash + tmux server. Worker panes are unaffected
        // — tmux still sets `screen-256color` inside its panes.
        ("TERM".to_string(), "xterm-256color".to_string()),
    ];
    let mut allow_keys = vec!["CLAUDE_CODE_OAUTH_TOKEN", "AO_GLOBAL_CONFIG"];
    push_identity_env(
        &mut env_set,
        &mut allow_keys,
        gh_token.as_ref(),
        gitconfig.as_deref(),
    );
    push_network_env(&mut env_set, &mut allow_keys, cfg);
    env_set.push(("LIMA_SHELLENV_ALLOW".to_string(), allow_keys.join(",")));
    let args = build_aoworker_argv(&workdir, lima.vm_name(), &bash_script, &allow_keys);
    Ok(CommandSpec {
        program: "limactl".to_string(),
        args,
        env_set,
        env_unset: vec!["ANTHROPIC_API_KEY".to_string()],
    })
}

/// Append brokered identity entries (`GH_TOKEN`, `FLEET_GITCONFIG_B64`)
/// to `env_set` and the `LIMA_SHELLENV_ALLOW` allowlist. Pulled out so
/// both `build_claude_oauth_spec` and `build_passthrough_spec` share
/// the same surface — workers spawned under codex / aider also need
/// git identity and gh auth.
///
/// `gh_token` / `gitconfig` are `Option`s: when neither is configured,
/// nothing is added. The bootstrap scripts handle the missing case
/// (`[ -n "${FLEET_GITCONFIG_B64:-}" ]`) so an absent var is a no-op.
fn push_identity_env(
    env_set: &mut Vec<(String, String)>,
    allow_keys: &mut Vec<&'static str>,
    gh_token: Option<&secrecy::SecretString>,
    gitconfig: Option<&str>,
) {
    if let Some(token) = gh_token {
        env_set.push(("GH_TOKEN".to_string(), crate::identity::expose(token)));
        allow_keys.push("GH_TOKEN");
    }
    if let Some(rendered) = gitconfig {
        env_set.push((
            "FLEET_GITCONFIG_B64".to_string(),
            crate::identity::gitconfig_env_value(rendered),
        ));
        allow_keys.push("FLEET_GITCONFIG_B64");
    }
}

/// Append egress-proxy entries (`HTTPS_PROXY`, `HTTP_PROXY`,
/// `NO_PROXY`) when fleet's `[network] mode = "allowlist"`. The
/// proxy itself (tinyproxy) is installed at VM provisioning time
/// and listens on 127.0.0.1:8888. fleet writes the allowlist file
/// via `network::sync_in_vm_filter` before this spec runs, so by
/// the time AO connects the proxy is configured.
///
/// `NO_PROXY` excludes the loopback addresses so AO's
/// dashboard / orchestrator IPC doesn't bounce back through the
/// proxy unnecessarily (and so its `ao acknowledge`-style
/// localhost-only HTTP calls aren't rejected as off-allowlist).
fn push_network_env(
    env_set: &mut Vec<(String, String)>,
    allow_keys: &mut Vec<&'static str>,
    cfg: &Config,
) {
    if !cfg.network.is_allowlist() {
        return;
    }
    let proxy = "http://127.0.0.1:8888".to_string();
    env_set.push(("HTTPS_PROXY".to_string(), proxy.clone()));
    env_set.push(("HTTP_PROXY".to_string(), proxy));
    env_set.push((
        "NO_PROXY".to_string(),
        "127.0.0.1,localhost,::1".to_string(),
    ));
    allow_keys.extend_from_slice(&["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"]);
}

/// Pre-`ao start` flow for non-Claude agents (Codex, Aider, …) — no
/// credentials handoff, no env scrubbing, just forward a known set of
/// provider env vars into the VM and run `ao` directly. Lima's
/// `--preserve-env` carries the env over; `LIMA_SHELLENV_ALLOW` opts
/// each var into the guest shell. When `daemonize` is true, the bash
/// script `setsid`s `ao __AO_CMD__` and polls `running.json` for
/// readiness — same daemonize pattern as the claude-oauth path,
/// without the credentials prefix.
fn build_passthrough_spec(
    ao_argv: &[String],
    daemonize: bool,
    wait_project: Option<&str>,
) -> Result<CommandSpec> {
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker.clone(), "fleet-vm");
    // See note in `build_claude_oauth_spec` about why there's no
    // `ensure_vm_running` at the builder layer.
    let workdir = ao_workdir_or_bail()?;
    let cfg = Config::load()?;
    let gh_token = crate::identity::resolve_gh_token(&cfg, &invoker)?;
    let gitconfig = crate::identity::resolve_gitconfig(&cfg);

    let ao_cmd = ao_argv
        .iter()
        .map(|a| shell_quote_single(a))
        .collect::<Vec<_>>()
        .join(" ");

    // The passthrough fast-path (`format!("ao {ao_cmd}")`) had no
    // pre-AO prelude. With identity brokering in place, the prelude
    // is required even there — gitconfig has to be decoded into
    // $HOME/.gitconfig before AO launches the worker.
    let bash_script = if daemonize {
        PASSTHROUGH_SCRIPT_DAEMON
            .replace("__AO_CMD__", &ao_cmd)
            .replace("__WAIT_PROJECT__", wait_project.unwrap_or(""))
            .replace("__IDENTITY_PRELUDE__", IDENTITY_PRELUDE)
    } else {
        format!("{IDENTITY_PRELUDE}\nao {ao_cmd}")
    };

    let mut env_set: Vec<(String, String)> = vec![
        // See `build_claude_oauth_spec` for why AO_GLOBAL_CONFIG is
        // forwarded.
        (
            "AO_GLOBAL_CONFIG".to_string(),
            crate::ao::config::AoConfig::default_xdg_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
        ("TERM".to_string(), "xterm-256color".to_string()),
    ];
    let allow_static: Vec<&'static str> = PASSTHROUGH_ENV_ALLOW
        .split(',')
        .chain(std::iter::once("AO_GLOBAL_CONFIG"))
        .collect();
    let mut allow_keys = allow_static;
    push_identity_env(
        &mut env_set,
        &mut allow_keys,
        gh_token.as_ref(),
        gitconfig.as_deref(),
    );
    push_network_env(&mut env_set, &mut allow_keys, &cfg);
    env_set.push(("LIMA_SHELLENV_ALLOW".to_string(), allow_keys.join(",")));
    let args = build_aoworker_argv(&workdir, lima.vm_name(), &bash_script, &allow_keys);
    Ok(CommandSpec {
        program: "limactl".to_string(),
        args,
        env_set,
        env_unset: Vec::new(),
    })
}

/// Compose the `limactl shell` argv that lands the bootstrap script
/// inside the VM as the `aoworker` user.
///
/// Today's chain (Phase 5):
///
/// ```text
/// limactl shell --preserve-env --workdir <wd> fleet-vm \
///     sudo -u aoworker --preserve-env=<keys> \
///     bash -c '<bash_script>'
/// ```
///
/// The lima user (default in `limactl shell`) has narrow sudo via
/// `/etc/sudoers.d/fleet-aoworker` (provisioned by `templates/fleet-vm.yaml`)
/// that allows exactly `lima ALL=(aoworker) NOPASSWD: /bin/bash, /usr/bin/tmux`.
/// The matching `env_keep +=` in that sudoers drop-in is the *gate*
/// for which env vars survive the sudo hop — `--preserve-env=<keys>`
/// on the sudo command line is necessary but not sufficient unless
/// each key is also in `env_keep`. We pass the explicit allowlist on
/// the command line so an unexpected expansion of `allow_keys`
/// doesn't silently grant new vars; the sudoers drop-in is the
/// authoritative ceiling.
///
/// `LIMA_SHELLENV_ALLOW` itself is excluded from the sudo
/// `--preserve-env=` list. It controls what crosses Lima's
/// host→guest boundary, not the lima→aoworker boundary; once we're
/// inside the guest it's just clutter that pollutes aoworker's env.
fn build_aoworker_argv(
    workdir: &Path,
    vm_name: &str,
    bash_script: &str,
    allow_keys: &[&'static str],
) -> Vec<String> {
    let preserve_env = allow_keys
        .iter()
        .copied()
        .filter(|k| *k != "LIMA_SHELLENV_ALLOW")
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "shell".to_string(),
        "--preserve-env".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        vm_name.to_string(),
        "sudo".to_string(),
        "-u".to_string(),
        "aoworker".to_string(),
        format!("--preserve-env={preserve_env}"),
        "bash".to_string(),
        "-c".to_string(),
        bash_script.to_string(),
    ]
}

/// CLI-layer precondition: the Lima VM must be running before we hand
/// off to `ao`. The TUI path (`tui::terminal::start_ao` and friends)
/// drives lifecycle through `AoTask` phases, prepending a `limactl
/// start fleet-vm` phase when the VM is stopped — those callers do
/// **not** go through this check; they call `build_*_spec` directly.
fn ensure_vm_running() -> Result<()> {
    let invoker: Arc<dyn crate::process::ProcessInvoker> = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    match lima.status() {
        VmStatus::Running => Ok(()),
        VmStatus::Stopped => bail!(
            "Lima VM `{}` is stopped. Start it with: fleet vm-start",
            lima.vm_name()
        ),
        VmStatus::Missing => bail!(
            "Lima VM `{}` not found. Launch `fleet ui` to bring it up from the bundled template.",
            lima.vm_name()
        ),
    }
}

/// Shared identity prelude prepended to every bootstrap variant. Today
/// it materializes the brokered gitconfig blob into `$HOME/.gitconfig`
/// (VM-private, since `~` is no longer bind-mounted). `GH_TOKEN` stays
/// in the env unchanged — `gh` and any worker shell read it directly.
///
/// The prelude is a no-op when neither var is set (e.g. running an
/// older script against a newer fleet), so it's safe to leave in place
/// during rollout.
const IDENTITY_PRELUDE: &str = r#"
# Identity prelude (fleet Phase 2):
#   - Decode FLEET_GITCONFIG_B64 into $HOME/.gitconfig (worker commit identity).
#   - GH_TOKEN stays in env so gh CLI inside the worker finds it.
if [ -n "${FLEET_GITCONFIG_B64:-}" ]; then
    umask 077
    printf '%s' "$FLEET_GITCONFIG_B64" | base64 -d > "$HOME/.gitconfig"
    unset FLEET_GITCONFIG_B64
fi
"#;

/// In-VM bootstrap: write claude's credentials file + onboarding marker,
/// scrub the OAuth env, anchor a tmux server, then run `ao __AO_CMD__`.
/// `__AO_CMD__` is replaced by [`build_claude_oauth_spec`] with the shell-escaped
/// argv to pass to `ao`. We intentionally use a sentinel rather than
/// `format!()` so the JSON / printf / Node literals don't need brace
/// escaping. `__IDENTITY_PRELUDE__` is replaced with [`IDENTITY_PRELUDE`]
/// at build time.
const BOOTSTRAP_SCRIPT: &str = r#"
set -e
mkdir -p "$HOME/.claude"
umask 077

__IDENTITY_PRELUDE__

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

/// Daemon variant for `ao start`. Same credentials prefix as
/// [`BOOTSTRAP_SCRIPT`]; the tail `setsid`s `ao __AO_CMD__` so it
/// reparents to init and survives this script's exit, then polls
/// `~/.agent-orchestrator/running.json` for readiness. AO writes that
/// file atomically after both the dashboard and orchestrator are up
/// and `register()` has run — the same signal AO's own
/// `isAlreadyRunning` check reads. On daemon death or timeout we
/// surface the tail of the captured log so [`crate::tui::ao_task::
/// AoTask::failure_hint`] can pick the real error line out of it.
///
/// The tmux anchor here has no `trap` kill: we want the server alive
/// after this script exits so AO's orchestrator session can attach
/// to it. Subsequent invocations are idempotent — `has-session` short-
/// circuits if a server is already up.
const BOOTSTRAP_SCRIPT_DAEMON: &str = r#"
set -e
mkdir -p "$HOME/.claude"
umask 077

__IDENTITY_PRELUDE__

# 1. Credentials file (claude reads this in interactive mode).
printf '{"claudeAiOauth":{"accessToken":"%s","scopes":["user:inference"],"subscriptionType":"subscription"}}\n' "$CLAUDE_CODE_OAUTH_TOKEN" > "$HOME/.claude/.credentials.json"
chmod 600 "$HOME/.claude/.credentials.json"

# 2. Mark onboarding complete in ~/.claude.json.
node -e '
const fs = require("fs");
const path = process.env.HOME + "/.claude.json";
let data = {};
try { data = JSON.parse(fs.readFileSync(path, "utf8")); } catch (e) {}
data.hasCompletedOnboarding = true;
data.bypassPermissionsModeAccepted = true;
fs.writeFileSync(path, JSON.stringify(data, null, 2));
'

# 2b. Suppress the bypass-permissions dialog.
node -e '
const fs = require("fs");
const path = process.env.HOME + "/.claude/settings.json";
let data = {};
try { data = JSON.parse(fs.readFileSync(path, "utf8")); } catch (e) {}
data.skipDangerousModePermissionPrompt = true;
fs.writeFileSync(path, JSON.stringify(data, null, 2));
'

# 3. Strip OAuth env so claude takes the file-based auth path.
unset CLAUDE_CODE_OAUTH_TOKEN

# 4. Anchor tmux server. No `trap` kill — daemon mode needs the
#    server alive after this script exits so AO can attach its
#    orchestrator session to it.
if ! tmux has-session 2>/dev/null; then
    tmux new-session -d -s _fleet_bootstrap
fi

# 5. Daemonize `ao __AO_CMD__` and poll for readiness.
LOG_DIR="$HOME/.agent-orchestrator"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/ao-start.log"
RUNNING_JSON="$LOG_DIR/running.json"

# Clear the readiness marker so we wait for a fresh write. Safe in
# the daemon path because fleet only invokes `ao start` when AO is
# already down (TCP probe on the dashboard port is false). A stale
# running.json's PID is also self-cleaning inside AO's `getRunning`,
# so a missed delete here can't promote a stale daemon to "ready".
rm -f "$RUNNING_JSON"
: > "$LOG_FILE"

setsid nohup ao __AO_CMD__ > "$LOG_FILE" 2>&1 < /dev/null &
DAEMON_PID=$!

# 120s ceiling. Cold start with the dashboard bundle cached lands in
# 5-15s; the headroom covers a constrained host (qemu on battery,
# paging) plus AO's project-supervisor reconcile cycle (60s) when
# we need to wait for project registration in running.json.
#
# Readiness signal:
#   - Always: `running.json` exists (the dashboard + orchestrator are up
#     and AO has called `register()`).
#   - When WAIT_PROJECT is non-empty: also wait until `running.json.projects`
#     contains "<WAIT_PROJECT>". AO's `register()` writes `projects:
#     listLifecycleWorkers()`, and the supervisor only attaches a
#     lifecycle worker for a project with a non-terminal session — there's
#     a window where running.json exists but the project isn't polled
#     yet. Without this wait, an immediate `ao spawn` after `ao start`
#     fails with "AO is not polling project <foo>".
WAIT_PROJECT="__WAIT_PROJECT__"
DEADLINE=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if [ -f "$RUNNING_JSON" ]; then
        if [ -z "$WAIT_PROJECT" ] || grep -q "\"$WAIT_PROJECT\"" "$RUNNING_JSON"; then
            exit 0
        fi
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "Error: ao start exited before becoming ready" >&2
        tail -50 "$LOG_FILE" >&2
        exit 1
    fi
    sleep 1
done
if [ -n "$WAIT_PROJECT" ] && [ -f "$RUNNING_JSON" ] && ! grep -q "\"$WAIT_PROJECT\"" "$RUNNING_JSON"; then
    echo "Error: ao start brought the daemon up but did not register polling for project \"$WAIT_PROJECT\" within 120s" >&2
else
    echo "Error: ao start did not become ready within 120s" >&2
fi
tail -50 "$LOG_FILE" >&2
exit 1
"#;

/// Passthrough variant for `ao start` (non-Claude agents). Same
/// daemonize-and-poll tail as [`BOOTSTRAP_SCRIPT_DAEMON`], minus the
/// credentials prefix and OAuth env scrub.
const PASSTHROUGH_SCRIPT_DAEMON: &str = r#"
set -e

__IDENTITY_PRELUDE__

# Anchor tmux server (no trap — see BOOTSTRAP_SCRIPT_DAEMON).
if ! tmux has-session 2>/dev/null; then
    tmux new-session -d -s _fleet_bootstrap
fi

# Daemonize `ao __AO_CMD__` and poll for readiness.
LOG_DIR="$HOME/.agent-orchestrator"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/ao-start.log"
RUNNING_JSON="$LOG_DIR/running.json"
rm -f "$RUNNING_JSON"
: > "$LOG_FILE"

setsid nohup ao __AO_CMD__ > "$LOG_FILE" 2>&1 < /dev/null &
DAEMON_PID=$!

WAIT_PROJECT="__WAIT_PROJECT__"
DEADLINE=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if [ -f "$RUNNING_JSON" ]; then
        if [ -z "$WAIT_PROJECT" ] || grep -q "\"$WAIT_PROJECT\"" "$RUNNING_JSON"; then
            exit 0
        fi
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "Error: ao start exited before becoming ready" >&2
        tail -50 "$LOG_FILE" >&2
        exit 1
    fi
    sleep 1
done
if [ -n "$WAIT_PROJECT" ] && [ -f "$RUNNING_JSON" ] && ! grep -q "\"$WAIT_PROJECT\"" "$RUNNING_JSON"; then
    echo "Error: ao start brought the daemon up but did not register polling for project \"$WAIT_PROJECT\" within 120s" >&2
else
    echo "Error: ao start did not become ready within 120s" >&2
fi
tail -50 "$LOG_FILE" >&2
exit 1
"#;

/// Quote a string for inclusion in a single-quoted bash word.
/// Each embedded `'` becomes `'\''` (close, escaped quote, reopen).
pub fn shell_quote_single(s: &str) -> String {
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
