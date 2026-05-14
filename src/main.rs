//! fleet — host-side wrapper + ratatui inspector for the Composio Agent
//! Orchestrator (AO) running inside a Lima VM named `fleet-vm`.
//!
//! See `/Users/niklas/.claude/plans/tranquil-bubbling-charm.md` for the plan.

// TEMP: ao/state, ao/invoke, lima methods, tmux helpers are wired into the
// TUI in Stage 3. The CLI doesn't reference them yet. Lift this allow when
// Stage 3 lands.
#![allow(dead_code)]

mod agent;
mod ao;
mod cli;
mod config;
mod identity;
mod lima;
mod network;
mod process;
mod repo;
mod repo_config;
mod runtime;
mod secrets;
mod session;
mod templates_sync;
mod workflow;
#[cfg(test)]
mod test_env;
mod tmux;
mod tui;

use clap::Parser;
use std::path::PathBuf;
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
    let repo_root = repo_root();

    match cli::dispatch(cli, &repo_root) {
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

/// Best-effort repo root: ancestor that contains both `Cargo.toml` and
/// `agent-orchestrator.yaml`. Falls back to the current working directory.
fn repo_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file()
            && ancestor.join("agent-orchestrator.yaml").is_file()
        {
            return ancestor.to_path_buf();
        }
    }
    cwd
}
