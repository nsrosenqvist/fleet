//! CLI surface for `fleet`. clap derive + thin dispatch.
//!
//! Three behavioral categories the dispatcher routes between:
//!   1. Token-injecting (start/spawn/batch-spawn) — resolves OAuth via the
//!      configured secret backend, passes it to AO inside the VM.
//!   2. Tmux attach (attach) — bypasses AO's terminal plugin and `tmux attach`
//!      directly through limactl for a real TTY.
//!   3. Passthrough (status/doctor/session/plugin/stop) — forwards args to
//!      `ao` inside the VM with inherited stdio.

use clap::{Parser, Subcommand};

pub mod attach;
pub mod config_cmd;
pub mod passthrough;
pub mod spawn;
pub mod ui;
pub mod vm;

/// fleet — host wrapper + ratatui inspector for the AO running inside Lima.
#[derive(Debug, Parser)]
#[command(name = "fleet", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start AO orchestrator + dashboard inside the VM. Resolves the OAuth
    /// token from the configured secret backend.
    Start {
        #[arg(long)]
        no_dashboard: bool,
        #[arg(long)]
        no_orchestrator: bool,
    },

    /// Stop AO orchestrator + dashboard. No secret needed.
    Stop,

    /// Spawn one worker session on an issue. Resolves OAuth.
    Spawn {
        /// Issue identifier (git-bug human id, github number, etc.)
        issue: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },

    /// Batch-spawn multiple sessions in one call.
    BatchSpawn {
        #[arg(required = true)]
        issues: Vec<String>,
    },

    /// Attach to a session's tmux pane with full TTY. Bypasses AO's terminal plugin.
    Attach { session: String },

    /// Pass-through to `ao session <args>`.
    Session {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Pass-through to `ao status <args>`.
    Status {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Pass-through to `ao doctor`.
    Doctor,

    /// Pass-through to `ao plugin <args>`.
    Plugin {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Open an interactive shell inside the Lima VM.
    VmShell,
    /// Print Lima VM status (`limactl list fleet-vm`).
    VmStatus,
    /// Start the Lima VM.
    VmStart,
    /// Stop the Lima VM.
    VmStop,

    /// Launch the ratatui dashboard (Stage 3).
    Ui,

    /// Manage per-engineer `fleet` configuration.
    Config {
        #[command(subcommand)]
        sub: ConfigSub,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigSub {
    /// Print effective config from the XDG path.
    Show,
    /// Open the XDG config in `$EDITOR`.
    Edit,
    /// Write a template config to the XDG path if it doesn't exist yet.
    Init,
}

/// Dispatch the parsed CLI. Returns the exit code to propagate.
pub fn dispatch(cli: Cli, repo_root: &std::path::Path) -> anyhow::Result<i32> {
    let command = cli.command.unwrap_or(Command::Ui);
    match command {
        Command::Start {
            no_dashboard,
            no_orchestrator,
        } => spawn::run_start(repo_root, no_dashboard, no_orchestrator),
        Command::Stop => passthrough::run(repo_root, &["stop".to_string()]),
        Command::Spawn {
            issue,
            prompt,
            agent,
        } => spawn::run_spawn(repo_root, &issue, prompt.as_deref(), agent.as_deref()),
        Command::BatchSpawn { issues } => spawn::run_batch_spawn(repo_root, &issues),
        Command::Attach { session } => attach::run(repo_root, &session),
        Command::Session { args } => passthrough::run_with_prefix(repo_root, "session", &args),
        Command::Status { args } => passthrough::run_with_prefix(repo_root, "status", &args),
        Command::Doctor => passthrough::run(repo_root, &["doctor".to_string()]),
        Command::Plugin { args } => passthrough::run_with_prefix(repo_root, "plugin", &args),
        Command::VmShell => vm::run_shell(repo_root),
        Command::VmStatus => vm::run_status(),
        Command::VmStart => vm::run_start(),
        Command::VmStop => vm::run_stop(),
        Command::Ui => ui::run(repo_root),
        Command::Config { sub } => match sub {
            ConfigSub::Show => config_cmd::run_show(),
            ConfigSub::Edit => config_cmd::run_edit(),
            ConfigSub::Init => config_cmd::run_init(),
        },
    }
}
