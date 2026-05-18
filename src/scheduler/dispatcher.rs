#![allow(dead_code)]
//! Scheduler → subprocess dispatcher.
//!
//! Stage 4 ships the data types (`SchedulerSpawn`, `SpawnSeed`) so the
//! engine can return spawn commands in a stable shape. Stage 7 wires
//! `dispatch_spawns` to actually invoke `fleet workflow run …` per
//! spawn — at that point the `run_for_pr` CLI path exists and the
//! dispatcher can call it without needing a forward declaration.
//!
//! Today this module exposes:
//! - `SchedulerSpawn` / `SpawnSeed`: what the engine emits.
//! - `build_run_for_pr_argv` / `build_run_anonymous_argv`: argv
//!   builders the dispatcher will hand to `std::process::Command`.
//!   Pure functions so stage 8's CLI-side tests can pin the exact
//!   argv shape before stage 7's subprocess plumbing lands.

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
