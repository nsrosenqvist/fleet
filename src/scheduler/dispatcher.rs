#![allow(dead_code)]
//! Scheduler → subprocess dispatcher.
//!
//! The engine (`scheduler::mod`) is pure; this module turns its
//! `SchedulerSpawn` outputs into actual `fleet workflow run …`
//! subprocess invocations. Each spawn becomes one detached run; each
//! detached run mints its own tmux session and its own on-disk
//! session row in `.fleet/sessions/`.
//!
//! The argv builders (`build_run_for_pr_argv`,
//! `build_run_anonymous_argv`) are pure so tests can pin the exact
//! shape without spawning subprocesses; `dispatch_spawns` wraps them
//! in `std::process::Command::spawn`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::code_host::PrSummary;

/// One spawn the scheduler asks the dispatcher to materialise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerSpawn {
    /// Workflow name the new session runs.
    pub workflow: String,
    /// When `Some`, the executor skips every node up to and including
    /// the named node — used to skip the `pr-list bind: per` root
    /// that the scheduler already consumed before minting this
    /// session. `None` for plain `loop:` workflows.
    pub start_after_node: Option<String>,
    /// Per-spawn binding. `Pr` carries the candidate that triggered
    /// this session; `Anonymous` is the unbound issueless case.
    pub seed: SpawnSeed,
}

/// What identifies / contextualises a single spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnSeed {
    /// A pull request from the host's enumeration. The dispatcher
    /// passes `--pr <number>` to the subprocess; `run_for_pr`
    /// re-fetches the rest of the PR fields from the code host so
    /// stale data from the scheduler doesn't leak into the session.
    Pr(PrSummary),
    /// No candidate binding — the workflow runs against a fresh
    /// worktree like a hand-fired `fleet workflow run <name>` would.
    Anonymous,
}

/// Build the argv for `fleet workflow run <workflow> --pr <n>
/// [--start-after-node <id>] --detached`. Pure so stage 7's tests
/// can assert the exact shape without spawning real subprocesses.
#[must_use]
pub fn build_run_for_pr_argv(
    fleet_binary: &str,
    workflow: &str,
    pr_number: u32,
    start_after_node: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        fleet_binary.to_string(),
        "workflow".to_string(),
        "run".to_string(),
        workflow.to_string(),
        "--pr".to_string(),
        pr_number.to_string(),
        "--detached".to_string(),
    ];
    if let Some(node) = start_after_node {
        argv.push("--start-after-node".to_string());
        argv.push(node.to_string());
    }
    argv
}

/// Build the argv for an anonymous `fleet workflow run <workflow>
/// --detached`. The dispatcher uses this for plain `loop:` workflows
/// that don't enumerate per-PR candidates.
#[must_use]
pub fn build_run_anonymous_argv(fleet_binary: &str, workflow: &str) -> Vec<String> {
    vec![
        fleet_binary.to_string(),
        "workflow".to_string(),
        "run".to_string(),
        workflow.to_string(),
        "--detached".to_string(),
    ]
}

/// Spawn one subprocess per `SchedulerSpawn`. Each child is a
/// fully-detached `fleet workflow run … --detached` invocation; the
/// child mints its own tmux session and on-disk session row. The
/// dispatcher does not wait for the child — `--detached` returns
/// immediately, and the scheduler tick has already debounced.
///
/// Returns one result per spawn so the caller can log per-workflow
/// failures without one bad spawn poisoning the rest of the slate.
pub fn dispatch_spawns(spawns: &[SchedulerSpawn], fleet_binary: &Path) -> Vec<Result<()>> {
    spawns
        .iter()
        .map(|s| dispatch_one(s, fleet_binary))
        .collect()
}

fn dispatch_one(spawn: &SchedulerSpawn, fleet_binary: &Path) -> Result<()> {
    let argv = match &spawn.seed {
        SpawnSeed::Pr(pr) => build_run_for_pr_argv(
            &fleet_binary.to_string_lossy(),
            &spawn.workflow,
            pr.number,
            spawn.start_after_node.as_deref(),
        ),
        SpawnSeed::Anonymous => {
            build_run_anonymous_argv(&fleet_binary.to_string_lossy(), &spawn.workflow)
        }
    };
    // argv[0] is the program; the rest are flags. spawn() returns as
    // soon as the child is up — the child's own `--detached` then
    // backgrounds the actual workflow run via tmux.
    Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .with_context(|| format!("spawning `{}` for workflow `{}`", argv.join(" "), spawn.workflow))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_run_for_pr_argv_includes_pr_flag_and_detached() {
        let argv = build_run_for_pr_argv("/usr/local/bin/fleet", "ci-fix", 42, Some("pick"));
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/fleet",
                "workflow",
                "run",
                "ci-fix",
                "--pr",
                "42",
                "--detached",
                "--start-after-node",
                "pick",
            ]
        );
    }

    #[test]
    fn build_run_for_pr_argv_omits_start_after_node_when_absent() {
        let argv = build_run_for_pr_argv("fleet", "ci-fix", 7, None);
        assert!(!argv.iter().any(|a| a == "--start-after-node"));
        assert!(argv.iter().any(|a| a == "--pr"));
    }

    #[test]
    fn build_run_anonymous_argv_omits_pr_flag() {
        let argv = build_run_anonymous_argv("fleet", "hourly");
        assert!(!argv.iter().any(|a| a == "--pr"));
        assert!(argv.iter().any(|a| a == "--detached"));
    }
}
