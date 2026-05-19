//! fleet — standalone Rust binary for running devcontainer-based agent
//! workflows in any repo. See `~/.claude/plans/declarative-wobbling-quasar.md`
//! for the design.
//!
//! Module layout:
//! - `repo` / `repo_config` — per-repo state location + `.fleet/config.yaml`
//! - `runtime` — adapter trait + Local/Podman/Docker/AppleContainer impls,
//!   devcontainer CLI helper, host probing
//! - `session` — value object + on-disk store under `.fleet/sessions/<id>/`
//! - `agent` — named agent specs (command + env passthrough)
//! - `tracker` — host-side `git-bug` / `gh` issue listing
//! - `workflow` — YAML DSL + DAG validator + Phase-1 executor
//! - `tui` — minimal ratatui session browser
//! - `cli` — clap surface + dispatch
//! - `process` — `ProcessInvoker` trait + `RealProcessInvoker` + helpers
//! - `autonomous` — supervisor engine driving the TUI's `Shift+A` mode
//! - `worktree` — per-session git worktree management

mod agent;
mod autonomous;
mod bridge;
mod cli;
mod code_host;
mod deps;
mod egress;
mod orchestrator;
mod plans;
mod policy;
mod process;
mod repo;
mod repo_config;
mod runtime;
mod scheduler;
mod secrets;
mod session;
mod tracker;
mod tui;
mod workflow;
mod worktree;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Logs to stderr; level controlled via RUST_LOG.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cli = cli::Cli::parse();
    match cli::dispatch(cli) {
        // `u8::try_from` fails when `code` is negative (e.g. -1 from
        // `Command::status` when killed by signal) or >255 — both unusual.
        // Fold those into generic failure.
        Ok(code) => u8::try_from(code).map_or_else(|_| ExitCode::from(1), ExitCode::from),
        Err(err) => {
            eprintln!("fleet: {err:#}");
            ExitCode::from(1)
        }
    }
}
