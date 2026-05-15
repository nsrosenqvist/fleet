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

