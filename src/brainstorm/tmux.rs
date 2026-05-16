//! tmux invocation primitives for the brainstorm subsystem.
//!
//! Brainstorm sessions live inside tmux panes so the user can
//! attach / detach without losing context. This module wraps the
//! `tmux` CLI behind a small Rust surface — every call shells
//! through [`ProcessInvoker`] so tests can stub the binary without
//! tmux on the host.
//!
//! Operations:
//! - [`probe`] — is `tmux` on PATH? Returns the version string when
//!   present, an error describing the absence when not.
//! - [`new_session`] — `tmux new-session -d -s <name> …` to spawn a
//!   detached session running the brainstorm agent.
//! - [`has_session`] — `tmux has-session -t <name>`.
//! - [`kill_session`] — `tmux kill-session -t <name>`.
//! - [`list_session_names`] — `tmux list-sessions -F '#S'`.
//! - [`server_pid_for`] — `tmux display-message -p -t <name>
//!   '#{pid}'` for liveness checks.
//!
//! Interactive attach is intentionally NOT here — `tmux attach` has
//! to exec-replace (or inherit stdio) the calling process, which
//! happens in the `fleet brainstorm attach` CLI (P5-C6). Putting
//! that here would muddy the ProcessInvoker-based abstraction.

use anyhow::{Context, Result, anyhow, bail};
use std::sync::Arc;

use crate::process::ProcessInvoker;

/// Run `tmux -V` and return the version string. Errors when `tmux`
/// isn't on PATH (the error message points the user at the install
/// commands for their OS — see `runtime doctor`'s output).
pub fn probe(invoker: &dyn ProcessInvoker) -> Result<String> {
    let out = invoker.run("tmux", vec!["-V".to_string()]).context(
        "tmux not found on PATH — install with `brew install tmux` (macOS) or your \
             distro's package manager, then re-run",
    )?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        bail!("`tmux -V` returned empty output");
    }
    Ok(trimmed.to_string())
}

/// Spawn a detached tmux session named `name` running `command`
/// with `env` populated. Returns `Ok(())` on success — the tmux
/// pane is now alive and the user can attach to it.
///
/// `command` is the agent command (e.g. `["claude", "code"]`).
/// `env` is a list of `(KEY, VAL)` pairs; tmux applies them with
/// `set-environment` after the session starts (so the agent sees
/// the brainstorm tool-server URL + bearer token).
pub fn new_session(
    invoker: &dyn ProcessInvoker,
    name: &str,
    command: &[String],
    env: &[(String, String)],
) -> Result<()> {
    if command.is_empty() {
        bail!("tmux new_session needs at least the program name in `command`");
    }
    let argv = new_session_argv(name, command, env);
    invoker
        .run("tmux", argv)
        .with_context(|| format!("`tmux new-session -d -s {name}` failed"))?;
    Ok(())
}

/// Argv for `tmux new-session -d -s <name> [-e KEY=VAL]… --
/// <command…>`. Pure so the shape can be locked in by tests
/// without spawning a real tmux.
#[must_use]
fn new_session_argv(name: &str, command: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut argv = vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        name.to_string(),
    ];
    for (k, v) in env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }
    argv.push("--".to_string());
    argv.extend(command.iter().cloned());
    argv
}

/// `tmux has-session -t <name>`. Exit code 0 → session exists;
/// non-zero → doesn't. The `ProcessInvoker` trait maps non-zero
/// exits to errors, so we treat "any error" as "no session" — the
/// real distinction (server gone vs server up but session absent)
/// doesn't matter to callers in practice.
#[must_use]
pub fn has_session(invoker: &dyn ProcessInvoker, name: &str) -> bool {
    invoker
        .run(
            "tmux",
            vec![
                "has-session".to_string(),
                "-t".to_string(),
                name.to_string(),
            ],
        )
        .is_ok()
}

/// `tmux kill-session -t <name>`. Idempotent: a missing session
/// is not an error, matching the contract on other store-level
/// "stop" operations (the bridge, container stoppers, etc.).
pub fn kill_session(invoker: &dyn ProcessInvoker, name: &str) -> Result<()> {
    let result = invoker.run(
        "tmux",
        vec![
            "kill-session".to_string(),
            "-t".to_string(),
            name.to_string(),
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            let msg = format!("{err:#}");
            if is_missing_session_error(&msg) {
                Ok(())
            } else {
                Err(err.context(format!("`tmux kill-session -t {name}` failed")))
            }
        }
    }
}

/// Tmux's "no such session" wording, factored out so both
/// [`kill_session`] and (future) other lifecycle helpers can
/// share the predicate.
#[must_use]
fn is_missing_session_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("can't find session")
        || lower.contains("no server running")
        || lower.contains("session not found")
}

/// `tmux list-sessions -F '#S'` → vec of session names. Returns
/// an empty vec when no tmux server is running (common on a fresh
/// shell), distinguishing "no sessions" from "tmux missing"
/// upfront via [`probe`]. Reserved for a future brainstorm reaper
/// pass; not wired in v1.
#[allow(dead_code)]
pub fn list_session_names(invoker: &dyn ProcessInvoker) -> Result<Vec<String>> {
    let result = invoker.run(
        "tmux",
        vec![
            "list-sessions".to_string(),
            "-F".to_string(),
            "#S".to_string(),
        ],
    );
    match result {
        Ok(stdout) => Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()),
        Err(err) => {
            let msg = format!("{err:#}");
            if is_missing_session_error(&msg) {
                Ok(Vec::new())
            } else {
                Err(err.context("`tmux list-sessions` failed"))
            }
        }
    }
}

/// `tmux display-message -p -t <name> '#{pid}'` → the tmux server
/// pid. Reserved for the brainstorm reaper (detect a dead tmux
/// server and mark the session Closed without leaving stale meta
/// on disk); not wired in v1.
#[allow(dead_code)]
pub fn server_pid_for(invoker: &dyn ProcessInvoker, name: &str) -> Result<u32> {
    let stdout = invoker
        .run(
            "tmux",
            vec![
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                name.to_string(),
                "#{pid}".to_string(),
            ],
        )
        .with_context(|| format!("`tmux display-message -t {name}` failed"))?;
    stdout
        .trim()
        .parse::<u32>()
        .map_err(|e| anyhow!("parsing tmux pid `{}`: {e}", stdout.trim()))
}

/// Construct an `Arc<dyn ProcessInvoker>` from the real shell so
/// production code paths don't have to. Test code constructs its
/// own mock. Convenience helper for the brainstorm CLI; not yet
/// consumed since the CLI builds its invoker inline.
#[must_use]
#[allow(dead_code)]
pub fn real_invoker() -> Arc<dyn ProcessInvoker> {
    Arc::new(crate::process::RealProcessInvoker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    /// Canned invoker: each (program, args) → fixed stdout; any
    /// mismatch returns an error. Same shape as the per-adapter
    /// `invoker_with` helpers in runtime/.
    fn invoker_with(
        responses: Vec<(&'static str, Vec<String>, String)>,
    ) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        for (prog, args, out) in responses {
            mock.expect_run()
                .with(eq(prog), eq(args))
                .returning(move |_, _| Ok(out.clone()));
        }
        mock.expect_run()
            .returning(|prog, args| Err(anyhow!("unexpected call: {prog} {args:?}")));
        Arc::new(mock)
    }

    #[test]
    fn probe_returns_trimmed_version_on_success() {
        let invoker = invoker_with(vec![(
            "tmux",
            vec!["-V".to_string()],
            "tmux 3.4\n".to_string(),
        )]);
        let v = probe(invoker.as_ref()).unwrap();
        assert_eq!(v, "tmux 3.4");
    }

    #[test]
    fn probe_surfaces_missing_binary_with_install_hint() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("no such file or directory: tmux")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let err = probe(invoker.as_ref()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tmux not found"), "got: {msg}");
        assert!(msg.contains("brew install tmux"), "got: {msg}");
    }

    #[test]
    fn probe_errors_on_empty_output() {
        // Defensive — tmux normally prints a version. If it
        // doesn't, fail loudly rather than treating it as
        // available.
        let invoker = invoker_with(vec![("tmux", vec!["-V".to_string()], String::new())]);
        let err = probe(invoker.as_ref()).unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn new_session_argv_emits_detached_named_session_with_command() {
        let argv = new_session_argv(
            "fleet-brainstorm-b-1",
            &["claude".to_string(), "code".to_string()],
            &[],
        );
        assert_eq!(
            argv,
            vec![
                "new-session",
                "-d",
                "-s",
                "fleet-brainstorm-b-1",
                "--",
                "claude",
                "code",
            ]
        );
    }

    #[test]
    fn new_session_argv_includes_env_vars_via_repeated_e_flags() {
        let argv = new_session_argv(
            "s",
            &["sh".to_string()],
            &[
                (
                    "FLEET_BRAINSTORM_URL".to_string(),
                    "http://127.0.0.1:54321".to_string(),
                ),
                ("FLEET_BRAINSTORM_TOKEN".to_string(), "secret".to_string()),
            ],
        );
        // -e KEY=VAL pairs come *before* the `--` separator.
        let dash_dash = argv.iter().position(|a| a == "--").unwrap();
        let env_segment = &argv[..dash_dash];
        assert!(env_segment.contains(&"-e".to_string()));
        assert!(env_segment.contains(&"FLEET_BRAINSTORM_URL=http://127.0.0.1:54321".to_string()));
        assert!(env_segment.contains(&"FLEET_BRAINSTORM_TOKEN=secret".to_string()));
    }

    #[test]
    fn new_session_rejects_empty_command() {
        let invoker = invoker_with(vec![]);
        let err = new_session(invoker.as_ref(), "s", &[], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("program name"));
    }

    #[test]
    fn has_session_returns_true_when_invoker_exits_zero() {
        let invoker = invoker_with(vec![(
            "tmux",
            vec!["has-session".to_string(), "-t".to_string(), "s".to_string()],
            String::new(),
        )]);
        assert!(has_session(invoker.as_ref(), "s"));
    }

    #[test]
    fn has_session_returns_false_when_invoker_errors() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("can't find session: s")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        assert!(!has_session(invoker.as_ref(), "s"));
    }

    #[test]
    fn kill_session_is_idempotent_for_missing_session() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("can't find session: s")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        kill_session(invoker.as_ref(), "s").unwrap();
    }

    #[test]
    fn kill_session_propagates_unexpected_errors() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("disk full")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let err = kill_session(invoker.as_ref(), "s").unwrap_err();
        assert!(format!("{err:#}").contains("disk full"));
    }

    #[test]
    fn list_session_names_parses_one_per_line() {
        let invoker = invoker_with(vec![(
            "tmux",
            vec![
                "list-sessions".to_string(),
                "-F".to_string(),
                "#S".to_string(),
            ],
            "fleet-brainstorm-b-1\nfleet-brainstorm-b-2\n\n".to_string(),
        )]);
        let names = list_session_names(invoker.as_ref()).unwrap();
        assert_eq!(names, vec!["fleet-brainstorm-b-1", "fleet-brainstorm-b-2"]);
    }

    #[test]
    fn list_session_names_returns_empty_when_no_server_running() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("no server running on /tmp/tmux-1000/default")));
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        assert!(list_session_names(invoker.as_ref()).unwrap().is_empty());
    }

    #[test]
    fn server_pid_for_parses_decimal_pid() {
        let invoker = invoker_with(vec![(
            "tmux",
            vec![
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "s".to_string(),
                "#{pid}".to_string(),
            ],
            "12345\n".to_string(),
        )]);
        assert_eq!(server_pid_for(invoker.as_ref(), "s").unwrap(), 12345);
    }

    #[test]
    fn server_pid_for_errors_on_unparseable_output() {
        let invoker = invoker_with(vec![(
            "tmux",
            vec![
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "s".to_string(),
                "#{pid}".to_string(),
            ],
            "not-a-number\n".to_string(),
        )]);
        let err = server_pid_for(invoker.as_ref(), "s").unwrap_err();
        assert!(format!("{err:#}").contains("not-a-number"));
    }
}
