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
pub mod init;
pub mod issues;
pub mod passthrough;
pub mod runtime;
pub mod sessions;
pub mod spawn;
pub mod ui;
pub mod vm;
pub mod workflow;

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

    /// v2 runtime-adapter commands (devcontainer + Podman / Apple Container
    /// / Docker). Currently parallel to the AO path; the v2 design is in
    /// `~/.claude/plans/declarative-wobbling-quasar.md`.
    Runtime {
        #[command(subcommand)]
        sub: RuntimeSub,
    },

    /// Scaffold `.fleet/` + a minimal `.devcontainer/devcontainer.json`
    /// in the current repo. Idempotent: re-running only fills in the
    /// pieces that are missing.
    Init,

    /// v2 workflow commands — list / validate / run YAML workflows
    /// under `.fleet/workflows/`. Phase 1 supports linear traversal
    /// of agent + bash nodes; gate / assert / fanout / `loop_back_to`
    /// are parsed but rejected at execute time.
    Workflow {
        #[command(subcommand)]
        sub: WorkflowSub,
    },

    /// v2 session inspection — list / show / logs against
    /// `.fleet/sessions/<id>/`. Sibling to (not a replacement for)
    /// the AO-passthrough `session` command above.
    Sessions {
        #[command(subcommand)]
        sub: SessionsSub,
    },

    /// v2 issue tracker browser — host-side `git-bug` / `gh` via the
    /// new `crate::tracker` module. Sibling to (not a replacement for)
    /// the AO `session` passthrough.
    Issues {
        #[command(subcommand)]
        sub: IssuesSub,
    },
}

#[derive(Debug, Subcommand)]
pub enum IssuesSub {
    /// Print one line per issue from the configured tracker, with open
    /// issues first.
    List,
}

#[derive(Debug, Subcommand)]
pub enum SessionsSub {
    /// List sessions discovered under `.fleet/sessions/`.
    List,
    /// Print a session's metadata and the log file list.
    Show { id: String },
    /// Print captured logs for a session. With `--node`, only that node's
    /// log; without, every log preceded by a `=== <node> ===` header.
    Logs {
        id: String,
        #[arg(long)]
        node: Option<String>,
    },
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
        /// Issue id to act on (matches the tracker's `human_id`:
        /// numbers for GitHub, short hashes for git-bug).
        #[arg(long)]
        issue: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RuntimeSub {
    /// Probe the host for container engines and report what's usable.
    /// Exit code 0 when at least one engine is available, 1 otherwise.
    Doctor,

    /// Build the image declared by the current repo's devcontainer. Prints
    /// the resulting image id on stdout — scriptable.
    Build,

    /// Start (or replace) the container for the current repo's devcontainer.
    /// Prints the container id on stdout.
    Up,

    /// Exec a command inside the running container. Bypasses the in-memory
    /// adapter container-id map and addresses the workspace directly, so
    /// it survives across separate CLI invocations.
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
        /// Command to run inside the container; defaults to `bash`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },

    /// Print a container's current state. Exits with the container's exit
    /// code when it has exited, or 2 for dead/unknown states.
    Inspect { id: String },
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
        Command::Runtime { sub } => match sub {
            RuntimeSub::Doctor => Ok(runtime::run_doctor()),
            RuntimeSub::Build => runtime::run_build(),
            RuntimeSub::Up => runtime::run_up(),
            RuntimeSub::Exec { argv } => runtime::run_exec(&argv),
            RuntimeSub::Stop { id } => runtime::run_stop(&id),
            RuntimeSub::Inspect { id } => runtime::run_inspect(&id),
            RuntimeSub::Attach { id, argv } => runtime::run_attach(&id, &argv),
        },
        Command::Init => init::run(),
        Command::Workflow { sub } => match sub {
            WorkflowSub::List => workflow::run_list(),
            WorkflowSub::Validate { name } => workflow::run_validate(&name),
            WorkflowSub::Run { name, issue } => workflow::run_run(&name, issue.as_deref()),
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
