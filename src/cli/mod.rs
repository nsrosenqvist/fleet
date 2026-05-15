//! CLI surface for fleet. clap derive + thin dispatch.
//!
//! All commands are v2 (devcontainer + per-repo `.fleet/` state). The
//! AO/Lima passthrough surface that lived here previously has been
//! removed; the binary now talks to the host's container engine
//! directly via the runtime adapter trait.

use clap::{Parser, Subcommand};

pub mod init;
pub mod issues;
pub mod runtime;
pub mod sessions;
pub mod workflow;

/// fleet — standalone Rust binary for running devcontainer-based
/// agent workflows in any repo.
#[derive(Debug, Parser)]
#[command(name = "fleet", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold `.fleet/` + a minimal `.devcontainer/devcontainer.json`
    /// in the current repo. Idempotent.
    Init,

    /// Container-runtime adapter commands: probe the host, build the
    /// devcontainer image, start/stop/attach/inspect a container, exec
    /// a one-shot command.
    Runtime {
        #[command(subcommand)]
        sub: RuntimeSub,
    },

    /// Workflow commands: list / validate / run YAML workflows under
    /// `.fleet/workflows/`. Phase-1 executor supports agent + bash
    /// nodes; gate / assert / fanout / `loop_back_to` are parsed and
    /// rejected at execute time.
    Workflow {
        #[command(subcommand)]
        sub: WorkflowSub,
    },

    /// Session inspection: list / show / logs against
    /// `.fleet/sessions/<id>/`.
    Sessions {
        #[command(subcommand)]
        sub: SessionsSub,
    },

    /// Issue tracker browser via the configured tracker (git-bug / gh).
    Issues {
        #[command(subcommand)]
        sub: IssuesSub,
    },

    /// Launch the ratatui session browser.
    Ui,
}

#[derive(Debug, Subcommand)]
pub enum RuntimeSub {
    /// Probe the host for container engines and report what's usable.
    /// Exit code 0 when at least one engine is available, 1 otherwise.
    Doctor,

    /// Build the image declared by the current repo's devcontainer.
    /// Prints the resulting image id on stdout.
    Build,

    /// Start (or replace) the container for the current repo's
    /// devcontainer. Prints the container id on stdout.
    Up,

    /// Exec a command inside the running container. Bypasses the in-
    /// memory adapter container-id map and addresses the workspace
    /// directly so it survives across separate CLI invocations.
    Exec {
        /// Command to run; everything after `--` is passed through.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        argv: Vec<String>,
    },

    /// Stop a container by id. Idempotent.
    Stop { id: String },

    /// Attach an interactive PTY to a running container. The caller's
    /// terminal is inherited; the inner process's exit code becomes
    /// fleet's. With no argv, defaults to `bash`; override via `--`.
    Attach {
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },

    /// Print a container's current state. Exits with the container's
    /// exit code when it has exited, or 2 for dead/unknown states.
    Inspect { id: String },
}

#[derive(Debug, Subcommand)]
pub enum WorkflowSub {
    /// List workflows discovered under `.fleet/workflows/`.
    List,

    /// Parse and DAG-validate a workflow without executing it.
    Validate { name: String },

    /// Run a workflow end-to-end against the configured adapter.
    /// Prints the session id on stdout. Exit code 0 on Completed,
    /// 1 on Failed. With `--issue <id>`, resolves the issue through
    /// the configured tracker and exposes it to nodes via
    /// `FLEET_ISSUE_ID` / `FLEET_ISSUE_HUMAN_ID` / `FLEET_ISSUE_TITLE`.
    Run {
        name: String,
        /// Issue id to act on (matches the tracker's `human_id`).
        #[arg(long)]
        issue: Option<String>,
    },

    /// Resume a workflow paused at a gate. Loads the persisted session,
    /// re-parses its workflow YAML, and continues from the node after
    /// the gate.
    Resume { session: String },
}

#[derive(Debug, Subcommand)]
pub enum SessionsSub {
    /// List sessions discovered under `.fleet/sessions/`.
    List,
    /// Print a session's metadata and the log file list.
    Show { id: String },
    /// Print captured logs for a session. With `--node`, only that
    /// node's log; without, every log preceded by a header.
    Logs {
        id: String,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum IssuesSub {
    /// Print one line per issue from the configured tracker, open
    /// issues first.
    List,
}

/// Dispatch the parsed CLI. Returns the exit code to propagate.
pub fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    let command = cli.command.unwrap_or(Command::Ui);
    match command {
        Command::Init => init::run(),
        Command::Ui => tui::run(),
        Command::Runtime { sub } => match sub {
            RuntimeSub::Doctor => Ok(runtime::run_doctor()),
            RuntimeSub::Build => runtime::run_build(),
            RuntimeSub::Up => runtime::run_up(),
            RuntimeSub::Exec { argv } => runtime::run_exec(&argv),
            RuntimeSub::Stop { id } => runtime::run_stop(&id),
            RuntimeSub::Attach { id, argv } => runtime::run_attach(&id, &argv),
            RuntimeSub::Inspect { id } => runtime::run_inspect(&id),
        },
        Command::Workflow { sub } => match sub {
            WorkflowSub::List => workflow::run_list(),
            WorkflowSub::Validate { name } => workflow::run_validate(&name),
            WorkflowSub::Run { name, issue } => workflow::run_run(&name, issue.as_deref()),
            WorkflowSub::Resume { session } => workflow::run_resume(&session),
        },
        Command::Sessions { sub } => match sub {
            SessionsSub::List => sessions::run_list(),
            SessionsSub::Show { id } => sessions::run_show(&id),
            SessionsSub::Logs { id, node } => sessions::run_logs(&id, node.as_deref()),
        },
        Command::Issues { sub } => match sub {
            IssuesSub::List => issues::run_list(),
        },
    }
}

use crate::tui;
