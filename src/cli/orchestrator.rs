//! `fleet orchestrator …` — the per-repo interactive planning +
//! coordination agent, run on the host inside a tmux session.
//!
//! Single-session by design: there is exactly one orchestrator per
//! repo. The CLI surface is intentionally minimal:
//!
//! - `fleet orchestrator` (no subcommand) — reuse-or-spawn. If the
//!   orchestrator exists and its tmux pane is alive, attach. If it
//!   exists but the pane is gone (e.g. the user Ctrl-C'd out of
//!   the agent), respawn the agent in place — using its launcher's
//!   resume mechanism where supported — and attach. If no
//!   orchestrator exists yet, mint one and attach. Either way the
//!   caller's terminal ends up inside a live agent pane.
//! - `fleet orchestrator kill` — tear down the tmux pane and mark
//!   meta as Closed. The next `fleet orchestrator` will respawn
//!   it on the same `agent_session_id` when supported (claude
//!   `--resume <uuid>`), so killing is non-destructive of the
//!   conversation history for claude. For aider/codex the chat
//!   history file / `resume --last` machinery survives the kill
//!   too.
//!
//! The orchestrator agent has full shell access via tmux and reaches
//! fleet via the CLI — `fleet plan …`, `fleet issues …`,
//! `fleet sessions …`. The system prompt (in
//! `crate::orchestrator::prompt`) documents the available
//! subcommands and is delivered to the agent via a per-agent
//! launcher strategy (see `crate::orchestrator::launcher`) — the
//! prompt body is passed as an argv flag tuned for each binary
//! (`claude --append-system-prompt`, `aider --read`, etc.) rather
//! than left as a file the agent might never read. For agents that
//! support pre-assigned session ids (claude `--session-id`), fleet
//! mints one at first spawn and persists it in `meta.json` so a
//! respawn can `--resume <uuid>` deterministically. Aider relies on
//! `--restore-chat-history` against a fleet-pinned history file,
//! codex on `codex resume --last`; bare agents fall back to a
//! fresh spawn (history lost) since fleet has no continuity hook.
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

    if store.exists() {
        ensure_alive(&store, &root, &invoker)?;
    } else {
        spawn_fresh(&store, &config, &root, &invoker)?;
    }

    let session = store.load()?;
    println!("orchestrator: {TMUX_SESSION_NAME} (agent: {})", session.agent);

    attach_and_finalize(&store, invoker.as_ref())
}

/// `fleet orchestrator kill` — kill the tmux pane, flip meta to
/// Closed. Idempotent. A subsequent `fleet orchestrator` will
/// respawn the agent (resuming the conversation where the agent
/// supports it).
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
/// If the pane is gone, respawn the agent via its launcher's
/// resume argv, re-pipe the transcript, and flip the meta back
/// to Active. No-op when the pane is already alive.
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
    let argv = launcher.resume_argv(
        &agent,
        &prompt_path,
        &prompt_body,
        session.agent_session_id.as_deref(),
    );
    let env = orchestrator_env(&prompt_path);

    eprintln!("respawning orchestrator (agent `{agent}`) — previous tmux pane was killed");
    tmux::new_session(invoker.as_ref(), TMUX_SESSION_NAME, &argv, &env)
        .with_context(|| format!("respawning tmux session `{TMUX_SESSION_NAME}`"))?;

    // pipe-pane appends to the existing transcript so the
    // conversation log is continuous across respawns.
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
    let mut session = OrchestratorSession::new(agent.as_str(), now_ms());

    let snapshot = snapshot_or_empty(root, invoker);
    let prompt_body = render_prompt_lenient(root, &snapshot);
    let prompt_path = store.prompt_path();

    let launcher = launcher_for(agent);
    if let Some(warning) = launcher.unsupported_warning() {
        eprintln!("warning: agent `{agent}`: {warning}");
    }
    // If the launcher pre-mints an agent-side session id (claude
    // does, others don't), bake it into the spawn argv and persist
    // it so a later respawn can `--resume` the same conversation.
    let argv = if let Some(sid) = launcher.mint_session_id() {
        let argv = launcher.argv_with_session_id(agent, &prompt_path, &prompt_body, &sid);
        session.agent_session_id = Some(sid);
        argv
    } else {
        launcher.argv(agent, &prompt_path, &prompt_body)
    };

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
fn attach_and_finalize(store: &OrchestratorStore, invoker: &dyn ProcessInvoker) -> Result<i32> {
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", TMUX_SESSION_NAME])
        .status()
        .with_context(|| format!("running `tmux attach -t {TMUX_SESSION_NAME}`"))?;
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
}
