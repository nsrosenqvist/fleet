//! Subprocess invocation, behind a trait so backends and command paths can be
//! exercised in tests without spawning real processes.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// Run an external program. Implementations decide whether to actually spawn
/// a subprocess (production) or return canned responses (tests).
///
/// `args` is owned `Vec<String>` rather than `&[&str]` so the trait method
/// has no elided lifetimes — `mockall::automock` can't generate mocks for
/// methods with `&[&str]` slices.
#[cfg_attr(test, mockall::automock)]
pub trait ProcessInvoker: Send + Sync {
    /// Run `program` with `args`. Return trimmed stdout on success (exit 0),
    /// or an error containing stderr context on failure.
    fn run(&self, program: &str, args: Vec<String>) -> Result<String>;
}

/// The real implementation that shells out via `std::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealProcessInvoker;

impl ProcessInvoker for RealProcessInvoker {
    fn run(&self, program: &str, args: Vec<String>) -> Result<String> {
        let output = Command::new(program)
            .args(&args)
            .output()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        if !output.status.success() {
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "`{program} {}` exited with {code}: {}",
                args.join(" "),
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Helper for callers that have a fixed-size argv: avoids repetitive
/// `.to_string()` at each call site.
#[macro_export]
macro_rules! argv {
    ($($s:expr),* $(,)?) => {
        ::std::vec![$(::std::string::ToString::to_string(&$s)),*]
    };
}

/// Spawn `program` with inherited stdio (the caller's TTY passes through),
/// wait for it, return its exit code. Used by interactive CLI subcommands
/// (`spawn`, `attach`, `vm-shell`, …) where we want the user to see live
/// output and interact with the child.
///
/// `env_set` and `env_unset` mutate only the child's env. The current process
/// is unaffected.
pub fn run_interactive(
    program: &str,
    args: &[String],
    env_set: &[(&str, &str)],
    env_unset: &[&str],
) -> anyhow::Result<i32> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env_set {
        cmd.env(k, v);
    }
    for k in env_unset {
        cmd.env_remove(k);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    Ok(status.code().unwrap_or(-1))
}

/// Fully-resolved invocation for a subprocess: program path, argv, and
/// env mutations. Built by the `cli::spawn` and `cli::passthrough`
/// `build_*_spec` helpers and consumed both by the foreground CLI path
/// ([`run_interactive_spec`]) and by the in-TUI background path
/// ([`crate::tui::ao_task::AoTask`], which pipes stdio instead of
/// inheriting it).
///
/// Secret values (the Claude OAuth token, today) live in `env_set` as
/// plain `String`s — already past `ExposeSecret`. The spec is therefore
/// sensitive and short-lived: build it immediately before spawning, and
/// never log, serialise, or persist it.
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
}

impl CommandSpec {
    /// Borrowed view of `env_set` matching [`run_interactive`]'s
    /// `&[(&str, &str)]` parameter shape.
    #[must_use]
    pub fn env_set_refs(&self) -> Vec<(&str, &str)> {
        self.env_set
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// Borrowed view of `env_unset` matching [`run_interactive`]'s
    /// `&[&str]` parameter shape.
    #[must_use]
    pub fn env_unset_refs(&self) -> Vec<&str> {
        self.env_unset.iter().map(String::as_str).collect()
    }
}

/// Run a [`CommandSpec`] in foreground mode (inherited stdio). Thin
/// wrapper around [`run_interactive`] used by the CLI path.
pub fn run_interactive_spec(spec: &CommandSpec) -> anyhow::Result<i32> {
    run_interactive(
        &spec.program,
        &spec.args,
        &spec.env_set_refs(),
        &spec.env_unset_refs(),
    )
}
