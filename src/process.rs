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
                redact_argv_for_display(&args).join(" "),
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Mask `KEY=VALUE` arg entries whose `KEY` looks like it carries a
/// secret. Applied when formatting argv into error messages so a
/// failed `devcontainer up --remote-env CLAUDE_CODE_OAUTH_TOKEN=sk-…`
/// doesn't dump the live token into the session's transcript log.
/// Pure; exposed for tests.
#[must_use]
pub fn redact_argv_for_display(args: &[String]) -> Vec<String> {
    args.iter().map(|a| redact_arg(a)).collect()
}

fn redact_arg(arg: &str) -> String {
    // Two-segment split-once so values that themselves contain `=`
    // (rare for env vars, common for query strings) stay intact in
    // the redacted-value placeholder. The key is just everything
    // before the first `=`.
    let Some((key, value)) = arg.split_once('=') else {
        return arg.to_string();
    };
    if looks_like_secret_key(key) && !value.is_empty() {
        format!("{key}=<redacted>")
    } else {
        arg.to_string()
    }
}

/// Conservative substring match against common secret-bearing env
/// var names. Catches `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`, `GH_TOKEN`, `GITHUB_TOKEN`, `*_SECRET`,
/// `*_PASSWORD`, etc. without needing an explicit allowlist that has
/// to be updated for every new secret variant.
fn looks_like_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("API_KEY")
        || upper.contains("APIKEY")
        || upper.contains("OAUTH")
        || upper.ends_with("_KEY")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_passes_through_non_secret_args() {
        let args = ["--verbose".to_string(), "up".to_string(), "main".to_string()];
        let out = redact_argv_for_display(&args);
        assert_eq!(out, args);
    }

    #[test]
    fn redact_masks_value_of_known_secret_env_pairs() {
        let args = vec![
            "--remote-env".to_string(),
            "CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-cHIEsecret".to_string(),
            "--remote-env".to_string(),
            "ANTHROPIC_API_KEY=sk-ant-api03-bigsecret".to_string(),
            "--remote-env".to_string(),
            "FLEET_PERSONA=planner".to_string(),
        ];
        let out = redact_argv_for_display(&args);
        assert_eq!(out[1], "CLAUDE_CODE_OAUTH_TOKEN=<redacted>");
        assert_eq!(out[3], "ANTHROPIC_API_KEY=<redacted>");
        // Non-secret env entries pass through unchanged so callers
        // can still trace which workflow / persona the failed run
        // was for.
        assert_eq!(out[5], "FLEET_PERSONA=planner");
    }

    #[test]
    fn redact_catches_substring_variants() {
        for raw in [
            "GH_TOKEN=ghp_abc",
            "GITHUB_TOKEN=ghp_def",
            "MY_SECRET=hunter2",
            "DB_PASSWORD=hunter2",
            "OPENAI_API_KEY=sk-xyz",
            "STRIPE_APIKEY=sk-test",
            "ENCRYPTION_KEY=raw",
            "SOMETHING_OAUTH_TOKEN=raw",
        ] {
            let out = redact_argv_for_display(&[raw.to_string()]);
            assert!(
                out[0].ends_with("=<redacted>"),
                "expected {raw:?} to be redacted; got {:?}",
                out[0],
            );
        }
    }

    #[test]
    fn redact_preserves_empty_secret_value_for_diagnostic_clarity() {
        // An empty token value is more useful left in place — it
        // tells the operator the env var was set but to empty,
        // which is itself the bug (e.g. ANTHROPIC_API_KEY= with
        // nothing on the right). Redacting an empty value would
        // hide the diagnostic.
        let out = redact_argv_for_display(&["ANTHROPIC_API_KEY=".to_string()]);
        assert_eq!(out[0], "ANTHROPIC_API_KEY=");
    }

    #[test]
    fn redact_leaves_args_without_equals_alone() {
        let args = ["--remote-env".to_string(), "--workspace-folder".to_string()];
        assert_eq!(redact_argv_for_display(&args), args);
    }
}
