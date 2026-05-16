//! `fleet brainstorm …` — interactive planning sessions backed by
//! tmux + an agent process (Claude Code by default).
//!
//! Subcommands:
//!
//! - `fleet brainstorm` (default) — mint a new session, write its
//!   meta + system prompt, spawn the tmux session with the agent,
//!   then exec the attach.
//! - `fleet brainstorm list` — list known sessions, marking ones
//!   whose tmux pane is still alive vs already-closed.
//! - `fleet brainstorm attach <id>` — re-attach an existing tmux
//!   session.
//! - `fleet brainstorm kill <id>` — kill the tmux session and
//!   mark the meta as Closed.
//!
//! The brainstorm agent has full shell access via tmux and reaches
//! fleet via the CLI — `fleet plan …`, `fleet issues …`,
//! `fleet sessions …`. The system prompt (in
//! `crate::brainstorm::prompt`) documents the available
//! subcommands. There's intentionally no HTTP server / token
//! plumbing: the agent is already on the host, the bridge's
//! security boundary doesn't apply, and a parallel HTTP surface
//! would just duplicate what the CLI already does.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::brainstorm::prompt::{RepoSnapshot, render_prompt_for_repo};
use crate::brainstorm::store::BrainstormStore;
use crate::brainstorm::tmux;
use crate::brainstorm::{
    BrainstormId, BrainstormIdSource, BrainstormSession, BrainstormState, ClockBrainstormIdSource,
};
use crate::plans::store::PlanStore;
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::session::now_ms;
use crate::tracker;

/// `fleet brainstorm` (no subcommand) — new session + attach.
pub fn run_default() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);

    tmux::probe(invoker.as_ref())?;

    // Build the repo snapshot — open tickets + active plans — that
    // gets baked into the prompt. Failures degrade gracefully: an
    // unreadable plan dir or a missing tracker shouldn't block the
    // spawn, but the user should see what happened.
    let snapshot = build_snapshot(&root, &invoker).unwrap_or_else(|err| {
        eprintln!("warning: building repo snapshot failed: {err:#}");
        RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        }
    });

    // `brainstorm.agent` in `.fleet/config.yaml` overrides the default
    // (`claude`); load it here so the user can swap in `claude-code`,
    // `aider`, or a wrapper script without touching fleet itself.
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .with_context(|| format!("loading repo config at {}", root.display()))?;
    let store = BrainstormStore::for_repo(&root);
    let id = ClockBrainstormIdSource.mint();
    let agent = &config.brainstorm.agent;
    let session = BrainstormSession::new(id.clone(), agent.as_str(), now_ms());
    store
        .create(&session)
        .with_context(|| format!("creating brainstorm session `{id}` on disk"))?;

    // Write the system prompt next to the meta.
    // `.fleet/prompts/brainstorm.md` overrides the built-in
    // template if present. A malformed-encoding override fails
    // loudly (it's a user-authored file); a plain absence falls
    // through to the built-in silently.
    let prompt_body = render_prompt_for_repo(&root, &snapshot).unwrap_or_else(|err| {
        eprintln!("warning: brainstorm prompt override unreadable: {err:#}");
        crate::brainstorm::prompt::render_prompt(&snapshot)
    });
    let prompt_path = store.prompt_path(&id);
    std::fs::write(&prompt_path, prompt_body)
        .with_context(|| format!("writing prompt at {}", prompt_path.display()))?;

    // Spawn the tmux session running the agent. The only env var
    // the agent needs is the prompt path — fleet CLI commands
    // (`fleet plan …`, `fleet issues …`, etc.) are how the agent
    // talks back to fleet, no HTTP server / token plumbing.
    let env: Vec<(String, String)> = vec![(
        "FLEET_BRAINSTORM_PROMPT".to_string(),
        prompt_path.display().to_string(),
    )];
    let command = vec![agent.clone()];
    tmux::new_session(invoker.as_ref(), &session.tmux_session, &command, &env)
        .with_context(|| format!("spawning tmux session `{}`", session.tmux_session))?;

    println!("brainstorm session: {id}");
    println!("attaching to tmux session: {}", session.tmux_session);

    // Block on `tmux attach` so the caller's terminal becomes the
    // brainstorm pane. Returns when the user detaches (Ctrl-B D)
    // or kills the session. We use `Command::status` rather than
    // `exec`-replace so the CLI gets a chance to mark the session
    // as Closed if tmux exited cleanly.
    let attach_status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", session.tmux_session.as_str()])
        .status()
        .with_context(|| format!("running `tmux attach -t {}`", session.tmux_session))?;
    if !attach_status.success() {
        eprintln!("warning: tmux attach exited non-zero ({attach_status})");
    }

    // If the tmux session is gone, mark this brainstorm as Closed.
    // If it's still alive (user just detached), leave the state as
    // Active so a future `fleet brainstorm attach <id>` works.
    let mut updated = store.load(&id)?;
    if tmux::has_session(invoker.as_ref(), &session.tmux_session) {
        updated.state = BrainstormState::Detached;
    } else {
        updated.state = BrainstormState::Closed;
    }
    updated.updated_at_ms = now_ms();
    store.save(&updated)?;
    Ok(0)
}

/// `fleet brainstorm list` — print each known brainstorm session
/// with its state (Active/Detached/Closed), agent name, and the
/// tmux pane name.
pub fn run_list() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = BrainstormStore::for_repo(&root);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let ids = store.list().with_context(|| {
        format!(
            "listing brainstorm sessions under {}",
            store.root().display()
        )
    })?;
    print!("{}", render_list(&store, &ids, invoker.as_ref()));
    Ok(0)
}

/// `fleet brainstorm attach <id>` — exec `tmux attach-session` so
/// the caller's terminal becomes the existing brainstorm pane.
pub fn run_attach(id: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = BrainstormStore::for_repo(&root);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let pid = BrainstormId::new(id);
    let session = store
        .load(&pid)
        .with_context(|| format!("loading brainstorm session `{id}`"))?;
    if !tmux::has_session(invoker.as_ref(), &session.tmux_session) {
        bail!(
            "brainstorm session `{id}` has no live tmux pane (looked for `{}`); \
             it was probably killed. Try `fleet brainstorm` to start a fresh session.",
            session.tmux_session
        );
    }
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", session.tmux_session.as_str()])
        .status()
        .with_context(|| format!("running `tmux attach -t {}`", session.tmux_session))?;
    if !status.success() {
        eprintln!("warning: tmux attach exited non-zero ({status})");
    }
    // After detach, re-evaluate state.
    let mut updated = store.load(&pid)?;
    updated.state = if tmux::has_session(invoker.as_ref(), &session.tmux_session) {
        BrainstormState::Detached
    } else {
        BrainstormState::Closed
    };
    updated.updated_at_ms = now_ms();
    store.save(&updated)?;
    Ok(0)
}

/// `fleet brainstorm kill <id>` — `tmux kill-session` + flip meta
/// to Closed.
pub fn run_kill(id: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = BrainstormStore::for_repo(&root);
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let pid = BrainstormId::new(id);
    let mut session = store
        .load(&pid)
        .with_context(|| format!("loading brainstorm session `{id}`"))?;
    tmux::kill_session(invoker.as_ref(), &session.tmux_session)
        .with_context(|| format!("killing tmux session `{}`", session.tmux_session))?;
    session.state = BrainstormState::Closed;
    session.updated_at_ms = now_ms();
    store.save(&session)?;
    println!("brainstorm session `{id}` killed");
    Ok(0)
}

/// Build the repo snapshot fed to the brainstorm system prompt:
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

/// Pure: build the user-visible `fleet brainstorm list` output.
/// Factored out so tests assert on the text without spinning up
/// real tmux + filesystem state.
#[must_use]
pub fn render_list(
    store: &BrainstormStore,
    ids: &[BrainstormId],
    invoker: &dyn ProcessInvoker,
) -> String {
    use std::fmt::Write as _;
    let mut out = format!("brainstorm sessions (root: {}):\n", store.root().display());
    if ids.is_empty() {
        out.push_str("  (none yet)\n");
        return out;
    }
    for id in ids {
        let Ok(session) = store.load(id) else {
            let _ = writeln!(out, "  ?       {id}  (unreadable)");
            continue;
        };
        // Don't trust meta; cross-check with tmux for liveness.
        let live = tmux::has_session(invoker, &session.tmux_session);
        let effective_state = if live {
            // Either Active or Detached — disambiguated by recorded
            // state; default to "alive" wording when uncertain.
            match session.state {
                BrainstormState::Active => "active",
                BrainstormState::Detached => "detached",
                BrainstormState::Closed => "alive?", // disk says closed but tmux says yes — surface it
            }
        } else {
            "closed"
        };
        let _ = writeln!(
            out,
            "  {effective_state:<9} {id}  agent={}  tmux={}",
            session.agent, session.tmux_session,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use anyhow::anyhow;
    use mockall::predicate::eq;
    use std::sync::Arc;

    fn sample_session(id: &str, agent: &str) -> BrainstormSession {
        BrainstormSession::new(BrainstormId::new(id), agent, 1_700_000_000_000)
    }

    fn invoker_with_has_session(name: &'static str, exists: bool) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("tmux"),
                eq(vec![
                    "has-session".to_string(),
                    "-t".to_string(),
                    name.to_string(),
                ]),
            )
            .returning(move |_, _| {
                if exists {
                    Ok(String::new())
                } else {
                    Err(anyhow!("can't find session: {name}"))
                }
            });
        Arc::new(mock)
    }

    #[test]
    fn render_list_with_no_sessions_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let invoker = MockProcessInvoker::new();
        let out = render_list(&store, &[], &invoker);
        assert!(out.contains("(none yet)"), "got: {out}");
    }

    #[test]
    fn render_list_marks_live_sessions_active() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let mut s = sample_session("b-1", "claude");
        s.state = BrainstormState::Active;
        store.create(&s).unwrap();
        let invoker = invoker_with_has_session("fleet-brainstorm-b-1", true);
        let out = render_list(&store, &[BrainstormId::new("b-1")], invoker.as_ref());
        assert!(out.contains("active"), "got: {out}");
        assert!(out.contains("b-1"), "got: {out}");
        assert!(out.contains("agent=claude"), "got: {out}");
        assert!(out.contains("tmux=fleet-brainstorm-b-1"), "got: {out}");
    }

    #[test]
    fn render_list_marks_detached_when_meta_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let mut s = sample_session("b-1", "claude");
        s.state = BrainstormState::Detached;
        store.create(&s).unwrap();
        let invoker = invoker_with_has_session("fleet-brainstorm-b-1", true);
        let out = render_list(&store, &[BrainstormId::new("b-1")], invoker.as_ref());
        assert!(out.contains("detached"), "got: {out}");
    }

    #[test]
    fn render_list_marks_closed_when_tmux_pane_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = BrainstormStore::at(dir.path());
        let mut s = sample_session("b-1", "claude");
        s.state = BrainstormState::Active; // disk says active
        store.create(&s).unwrap();
        // But tmux says no — the pane was killed externally. The
        // listing reflects tmux's truth.
        let invoker = invoker_with_has_session("fleet-brainstorm-b-1", false);
        let out = render_list(&store, &[BrainstormId::new("b-1")], invoker.as_ref());
        assert!(out.contains("closed"), "got: {out}");
    }
}
