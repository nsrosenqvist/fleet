//! `fleet orchestrator …` — the per-repo interactive planning +
//! coordination agent, run on the host inside a tmux session.
//!
//! Single-session by design: there is exactly one orchestrator per
//! repo. The CLI surface is intentionally minimal:
//!
//! - `fleet orchestrator` (no subcommand) — reuse-or-spawn. If the
//!   orchestrator exists and its tmux pane is alive, attach. If it
//!   exists but the pane is gone (e.g. the user Ctrl-C'd out of
//!   the agent or killed it explicitly), spawn the agent fresh in
//!   place and attach. If no orchestrator exists yet, mint one and
//!   attach. Either way the caller's terminal ends up inside a
//!   live agent pane.
//! - `fleet orchestrator kill` — tear down the tmux pane and mark
//!   meta as Closed. The next `fleet orchestrator` will spawn a
//!   fresh agent.
//!
//! The orchestrator is transient by design — a respawn after a
//! dead pane always starts the agent fresh rather than trying to
//! resume the prior conversation. The previous design pre-minted
//! a `claude --session-id <uuid>` and tried to `--resume <uuid>`
//! on respawn, but that broke whenever Claude's local session
//! store had evicted the conversation ("No conversation found with
//! session ID …"), leaving the user unable to re-enter the
//! orchestrator at all. Fresh-spawn always works; the
//! `tmux pipe-pane` transcript preserves the prior conversation
//! for audit.
//!
//! The orchestrator agent has full shell access via tmux and reaches
//! fleet via the CLI — `fleet plan …`, `fleet issues …`,
//! `fleet sessions …`. The system prompt (in
//! `crate::orchestrator::prompt`) documents the available
//! subcommands and is delivered to the agent via a per-agent
//! launcher strategy (see `crate::orchestrator::launcher`) — the
//! prompt body is passed as an argv flag tuned for each binary
//! (`claude --append-system-prompt`, `aider --read`, etc.) rather
//! than left as a file the agent might never read.
//!
//! There's intentionally no HTTP server / token plumbing: the
//! agent is already on the host, the bridge's security boundary
//! doesn't apply, and a parallel HTTP surface would just duplicate
//! what the CLI already does.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::orchestrator::launcher::launcher_for;
use crate::orchestrator::prompt::{RepoSnapshot, render_prompt_for_repo};
use crate::orchestrator::store::OrchestratorStore;
use crate::orchestrator::tmux;
use crate::orchestrator::{OrchestratorSession, OrchestratorState, TMUX_SESSION_NAME};
use crate::plans::store::PlanStore;
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::session::now_ms;
use crate::tracker;

/// `fleet orchestrator` (no subcommand). Reuse-or-spawn, then
/// attach. Returns when the user detaches or the agent exits.
pub fn run_default() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);

    tmux::probe(invoker.as_ref())?;

    // `orchestrator.agent` in `.fleet/config.yaml` overrides the
    // default (`claude`); load it here so the user can swap in
    // `claude-code`, `aider`, `codex`, or a wrapper script without
    // touching fleet itself.
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .with_context(|| format!("loading repo config at {}", root.display()))?;
    let store = OrchestratorStore::for_repo(&root);

    // Reap stale meta first so an entry whose tmux pane was killed
    // externally is correctly Closed before we decide what to do.
    if let Err(err) = crate::orchestrator::reaper::reap(&store, invoker.as_ref(), now_ms()) {
        eprintln!("warning: orchestrator reap failed: {err:#}");
    }

    // Dispatch on the *union* of "meta on disk" and "tmux session
    // alive" — either is sufficient to say "an orchestrator is in
    // play; just attach". A common asymmetric state to defend
    // against is `rm -rf .fleet/orchestrator/` (a reset script, or
    // a previous failed-but-side-effect-succeeded spawn) leaving
    // tmux alive while meta is gone. Without this, the next launch
    // tries spawn_fresh, hits `tmux new-session` against an
    // already-existing session, and dies with `duplicate session:
    // fl-orchestrator`.
    let meta_exists = store.exists();
    let tmux_alive = tmux::has_session(invoker.as_ref(), TMUX_SESSION_NAME);
    if meta_exists || tmux_alive {
        // Synthesize the meta when tmux is up but meta was wiped,
        // so the rest of the flow has something to load. The agent
        // name comes from config — that's what spawn_fresh would
        // have used, and matches what's actually executing in the
        // tmux pane (the launcher inherits from config too).
        if !meta_exists {
            let session = OrchestratorSession::new(&config.orchestrator.agent, now_ms());
            store
                .save(&session)
                .with_context(|| "synthesising orchestrator meta after tmux-alive / meta-missing")?;
        }
        ensure_alive(&store, &root, &invoker)?;
    } else {
        spawn_fresh(&store, &config, &root, &invoker)?;
    }

    let session = store.load()?;
    println!(
        "orchestrator: {TMUX_SESSION_NAME} (agent: {})",
        session.agent
    );

    attach_and_finalize(&store, invoker.as_ref())
}

/// `fleet orchestrator kill` — kill the tmux pane, flip meta to
/// Closed. Idempotent. A subsequent `fleet orchestrator` will
/// spawn a fresh agent.
pub fn run_kill() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = OrchestratorStore::for_repo(&root);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);

    tmux::kill_session(invoker.as_ref(), TMUX_SESSION_NAME)
        .with_context(|| format!("killing tmux session `{TMUX_SESSION_NAME}`"))?;
    if store.exists() {
        let mut session = store.load()?;
        session.state = OrchestratorState::Closed;
        session.updated_at_ms = now_ms();
        store.save(&session)?;
    }
    println!("orchestrator killed");
    Ok(0)
}

/// Guarantee the orchestrator's tmux pane is alive on return.
/// If the pane is gone, spawn a fresh agent in place, re-pipe the
/// transcript, and flip the meta back to Active. No-op when the
/// pane is already alive.
fn ensure_alive(
    store: &OrchestratorStore,
    root: &Path,
    invoker: &Arc<dyn ProcessInvoker>,
) -> Result<()> {
    let mut session = store.load()?;
    if tmux::has_session(invoker.as_ref(), TMUX_SESSION_NAME) {
        return Ok(());
    }

    // Rebuild the prompt body from the current repo snapshot so a
    // respawn sees up-to-date open issues / active plans, then
    // overwrite the on-disk prompt for audit continuity.
    let snapshot = snapshot_or_empty(root, invoker);
    let prompt_body = render_prompt_lenient(root, &snapshot);
    let prompt_path = store.prompt_path();
    std::fs::write(&prompt_path, &prompt_body)
        .with_context(|| format!("writing prompt at {}", prompt_path.display()))?;

    let agent = session.agent.clone();
    let launcher = launcher_for(&agent);
    if let Some(warning) = launcher.unsupported_warning() {
        eprintln!("warning: agent `{agent}`: {warning}");
    }
    let argv = launcher.argv(&agent, &prompt_path, &prompt_body);
    let env = orchestrator_env(&prompt_path);

    eprintln!("respawning orchestrator (agent `{agent}`) — previous tmux pane was killed");
    tmux::new_session(invoker.as_ref(), TMUX_SESSION_NAME, &argv, &env)
        .with_context(|| format!("respawning tmux session `{TMUX_SESSION_NAME}`"))?;

    // pipe-pane appends to the existing transcript so the
    // conversation log is continuous across respawns even though
    // the agent itself starts fresh.
    let transcript_path = store.transcript_path();
    if let Err(err) = tmux::pipe_pane_to(invoker.as_ref(), TMUX_SESSION_NAME, &transcript_path) {
        eprintln!("warning: tmux pipe-pane failed (no transcript will be captured): {err:#}");
    }

    session.state = OrchestratorState::Active;
    session.updated_at_ms = now_ms();
    store.save(&session)?;
    Ok(())
}

/// Spawn the orchestrator for the first time: persist meta + prompt
/// before spawning so a crash mid-spawn leaves a recoverable disk
/// state, then `tmux new-session` and start the transcript pipe.
fn spawn_fresh(
    store: &OrchestratorStore,
    config: &RepoConfig,
    root: &Path,
    invoker: &Arc<dyn ProcessInvoker>,
) -> Result<()> {
    let agent = &config.orchestrator.agent;
    let session = OrchestratorSession::new(agent.as_str(), now_ms());

    let snapshot = snapshot_or_empty(root, invoker);
    let prompt_body = render_prompt_lenient(root, &snapshot);
    let prompt_path = store.prompt_path();

    let launcher = launcher_for(agent);
    if let Some(warning) = launcher.unsupported_warning() {
        eprintln!("warning: agent `{agent}`: {warning}");
    }
    let argv = launcher.argv(agent, &prompt_path, &prompt_body);

    store.save(&session)?;
    std::fs::write(&prompt_path, &prompt_body)
        .with_context(|| format!("writing prompt at {}", prompt_path.display()))?;

    let env = orchestrator_env(&prompt_path);
    tmux::new_session(invoker.as_ref(), TMUX_SESSION_NAME, &argv, &env)
        .with_context(|| format!("spawning tmux session `{TMUX_SESSION_NAME}`"))?;

    let transcript_path = store.transcript_path();
    if let Err(err) = tmux::pipe_pane_to(invoker.as_ref(), TMUX_SESSION_NAME, &transcript_path) {
        eprintln!("warning: tmux pipe-pane failed (no transcript will be captured): {err:#}");
    }

    Ok(())
}

/// `tmux attach`, blocking until the user detaches or the agent
/// exits, then flip the meta to Detached (pane still alive) or
/// Closed (pane gone) so the next reuse pass knows what to do.
///
/// Before the attach we inject a fleet-flavoured status bar (purple
/// session name + `ctrl+b d to detach` hint) so the user always
/// knows how to get back to the management TUI. Options are
/// `set-option -t <session>` so they only affect this orchestrator's
/// session — we don't leak into any other tmux state the user keeps.
/// We also flip the window to `window-size latest` so the attach
/// reaches the user's full host terminal rather than the narrower
/// dims the TUI pinned for its capture-pane preview. (Re-pinning to
/// panel dims after detach is the TUI's job — see
/// `AppState::request_orchestrator_repin`.)
fn attach_and_finalize(store: &OrchestratorStore, invoker: &dyn ProcessInvoker) -> Result<i32> {
    let script = build_attach_script(TMUX_SESSION_NAME);
    let status = std::process::Command::new("bash")
        .args(["-c", &script])
        .status()
        .with_context(|| format!("running attach script for `{TMUX_SESSION_NAME}`"))?;
    if !status.success() {
        eprintln!("warning: tmux attach exited non-zero ({status})");
    }

    let mut updated = store.load()?;
    updated.state = if tmux::has_session(invoker, TMUX_SESSION_NAME) {
        OrchestratorState::Detached
    } else {
        OrchestratorState::Closed
    };
    updated.updated_at_ms = now_ms();
    store.save(&updated)?;
    Ok(0)
}

/// Bash one-liner that customises the target session's status bar
/// and then attaches. `set-option -t <session>` is per-session so
/// other tmux sessions in the user's environment keep whatever they
/// had configured. We tolerate set-option errors with `|| true` — if
/// tmux is older / the session has gone away between selection and
/// attach, the attach itself will fail with a useful error rather
/// than the status-option setup masking it.
///
/// Status bar:
///   - Left:   ` <session-name>  │  ctrl+b d to detach `
///   - Window list rides to the right of `status-left` via tmux's
///     built-in `status-window-format`.
///   - Colour: fleet purple (`colour141`) for the session name +
///     active-window marker; muted gray for everything else.
// `cli` modules are reached only through `crate::cli::dispatch` so
// they're effectively private; the `pub(crate)` here lets the TUI
// and `fleet sessions attach` reuse the script without copying the
// styling.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn build_attach_script(session: &str) -> String {
    let s = shell_single_quote(session);
    let status_left =
        " #[fg=colour141,bold]#S#[default] #[fg=brightblack]│#[default] ctrl+b d to detach ";
    let q_status_left = shell_single_quote(status_left);
    format!(
        "set -e
tmux set-option -t {s} status on >/dev/null 2>&1 || true
tmux set-option -t {s} status-style 'bg=default,fg=colour250' >/dev/null 2>&1 || true
tmux set-option -t {s} status-left {q_status_left} >/dev/null 2>&1 || true
tmux set-option -t {s} status-left-length 60 >/dev/null 2>&1 || true
tmux set-option -t {s} window-status-current-style 'fg=colour141,bold' >/dev/null 2>&1 || true
tmux set-option -t {s} window-status-style 'fg=colour250' >/dev/null 2>&1 || true
tmux set-option -t {s} window-size latest >/dev/null 2>&1 || true
exec tmux attach -t {s}
"
    )
}

/// POSIX single-quote escape (a→'a', a'b → 'a'\''b'). Mirrors the
/// helper in `orchestrator/tmux.rs` but inlined here so the attach
/// script doesn't reach across module boundaries for a 5-line utility.
#[must_use]
fn shell_single_quote(s: &str) -> String {
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

/// Environment passed into the tmux session — just the prompt path
/// pointer for agents (or future tooling) that want to read it
/// themselves rather than relying on launcher-injected argv.
fn orchestrator_env(prompt_path: &Path) -> Vec<(String, String)> {
    vec![(
        "FLEET_ORCHESTRATOR_PROMPT".to_string(),
        prompt_path.display().to_string(),
    )]
}

/// `build_snapshot` but degrade to an empty snapshot on any
/// failure, with a stderr breadcrumb. Spawning shouldn't be blocked
/// by a missing tracker or an unreadable plan dir.
fn snapshot_or_empty(root: &Path, invoker: &Arc<dyn ProcessInvoker>) -> RepoSnapshot {
    build_snapshot(root, invoker).unwrap_or_else(|err| {
        eprintln!("warning: building repo snapshot failed: {err:#}");
        RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        }
    })
}

/// Render the prompt body, falling back to the built-in template if
/// the user's `.fleet/prompts/orchestrator.md` override is unreadable.
fn render_prompt_lenient(root: &Path, snapshot: &RepoSnapshot) -> String {
    render_prompt_for_repo(root, snapshot).unwrap_or_else(|err| {
        eprintln!("warning: orchestrator prompt override unreadable: {err:#}");
        crate::orchestrator::prompt::render_prompt(snapshot)
    })
}

/// Build the repo snapshot fed to the orchestrator system prompt:
/// open tickets via the configured tracker, active plans via the
/// `PlanStore`. Both halves degrade independently — a missing
/// tracker (or none configured) returns an empty issue list rather
/// than failing the whole snapshot.
fn build_snapshot(root: &Path, invoker: &Arc<dyn ProcessInvoker>) -> Result<RepoSnapshot> {
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .with_context(|| format!("loading repo config at {}", root.display()))?;
    let open_issues = match tracker::build(config.tracker, Arc::clone(invoker)) {
        Some(t) => t
            .list_issues(root)
            .with_context(|| "listing tracker issues")?
            .into_iter()
            .filter(|i| i.status == "open")
            .collect(),
        None => Vec::new(),
    };
    let plan_store = PlanStore::for_repo(root);
    let active_plans = plan_store
        .list_active()
        .with_context(|| "listing active plans")?;
    Ok(RepoSnapshot {
        open_issues,
        active_plans,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_env_only_carries_the_prompt_path_pointer() {
        // Spawn-time tmux env is intentionally minimal: just the
        // pointer to the rendered prompt file. The launcher delivers
        // the actual prompt body via argv per agent, so additional
        // env vars would be redundant and noisy in
        // `tmux show-environment`.
        let env = orchestrator_env(Path::new("/repo/.fleet/orchestrator/prompt.md"));
        assert_eq!(
            env,
            vec![(
                "FLEET_ORCHESTRATOR_PROMPT".to_string(),
                "/repo/.fleet/orchestrator/prompt.md".to_string(),
            )]
        );
    }

    #[test]
    fn attach_script_ends_with_exec_attach_against_the_target_session() {
        // `exec tmux attach` so tmux replaces the bash wrapper —
        // keeps the process tree clean and lets tmux own the TTY
        // for the duration of the attach.
        let script = build_attach_script(TMUX_SESSION_NAME);
        assert!(
            script.contains(&format!("exec tmux attach -t '{TMUX_SESSION_NAME}'")),
            "script: {script}"
        );
    }

    #[test]
    fn attach_script_sets_styled_status_left_with_detach_hint() {
        // The bottom-bar instruction (`ctrl+b d to detach`) is the
        // user's only built-in hint for how to get back to the
        // management TUI — verify the wording stays in the script.
        let script = build_attach_script(TMUX_SESSION_NAME);
        assert!(script.contains("ctrl+b d to detach"), "script: {script}");
    }

    #[test]
    fn attach_script_turns_status_bar_on() {
        // Some agents (and some user-level tmux configs) ship with
        // `status off`; without an explicit `status on` the styled
        // status-left would never render.
        let script = build_attach_script(TMUX_SESSION_NAME);
        assert!(
            script.contains(&format!("set-option -t '{TMUX_SESSION_NAME}' status on")),
            "script: {script}"
        );
    }

    #[test]
    fn attach_script_switches_window_size_to_latest_before_attach() {
        // The session is normally pinned to `window-size manual` by
        // the TUI's refresh thread so capture-pane returns lines at
        // the panel width. Without flipping back to `latest`, the
        // attaching client would see the agent at the panel's narrow
        // width instead of their full host terminal.
        let script = build_attach_script(TMUX_SESSION_NAME);
        let ws_idx = script
            .find("window-size latest")
            .expect("attach script should switch window-size to latest");
        let attach_idx = script
            .find("exec tmux attach")
            .expect("attach script should exec tmux attach");
        assert!(
            ws_idx < attach_idx,
            "window-size latest must be set before the attach (script: {script})"
        );
    }

    #[test]
    fn attach_script_single_quotes_pathological_session_names() {
        // The session name is a constant today, but the helper has
        // to stay defensive against future renames — anything with
        // shell metachars must be single-quoted so it can't escape
        // the surrounding bash.
        let script = build_attach_script("fl-4; rm -rf /");
        assert!(
            script.contains("'fl-4; rm -rf /'"),
            "name should be wrapped in single quotes; script: {script}"
        );
        // And the dangerous tail must never appear as a bare token.
        assert!(
            !script.contains(" fl-4; rm -rf / "),
            "name must not appear unquoted; script: {script}"
        );
    }
}
