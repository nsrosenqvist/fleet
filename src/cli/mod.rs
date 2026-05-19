//! CLI surface for fleet. clap derive + thin dispatch.
//!
//! Every subcommand operates against the current repo's `.fleet/`
//! state, the chosen `RuntimeAdapter`, and the host's container
//! engine — no orchestrator-in-a-VM hop in between.

use clap::{Parser, Subcommand};

pub mod autonomous;
pub mod deps;
pub mod init;
pub mod issues;
pub mod orchestrator;
pub mod plan;
pub mod runtime;
#[path = "scheduler.rs"]
pub mod scheduler_cli;
pub mod secrets;
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

    /// Secrets management: register / list / test the secrets fleet
    /// resolves into the agent container's env at run time. Backed
    /// by the OS keychain by default (macOS Security framework /
    /// Linux `DBus` Secret Service / Windows Credential Manager).
    Secrets {
        #[command(subcommand)]
        sub: SecretsSub,
    },

    /// Issue tracker browser via the configured tracker (git-bug / gh).
    Issues {
        #[command(subcommand)]
        sub: IssuesSub,
    },

    /// Autonomous-mode supervisor as a CLI. Same engine the TUI's
    /// `Shift+A` mode uses, but driveable from scripts/cron.
    Autonomous {
        #[command(subcommand)]
        sub: AutonomousSub,
    },

    /// Loop-driven scheduler — fires workflows with `loop: <duration>`
    /// on their elapsed-interval cadence, mints one session per
    /// candidate (e.g. one per failing-CI PR). Same engine the TUI
    /// surfaces in its status bar; driveable from cron for systems
    /// that don't keep a TUI open.
    Scheduler {
        #[command(subcommand)]
        sub: SchedulerSub,
    },

    /// Plan management: ordered ticket lists the autonomous
    /// supervisor walks through in sequence. Plans encode preference
    /// (this is the order I want); the deps graph encodes
    /// requirement (this can't run until that closes).
    Plan {
        #[command(subcommand)]
        sub: PlanSub,
    },

    /// Orchestrator session: the single per-repo interactive
    /// planning + coordination agent (Claude Code by default). With
    /// no subcommand, reuses the existing orchestrator (respawning
    /// the agent if its tmux pane died) and attaches the caller's
    /// terminal. With `kill`, tears it down.
    Orchestrator {
        #[command(subcommand)]
        sub: Option<OrchestratorSub>,
    },

    /// Cross-session dependency graph: list / add / remove edges
    /// in `.fleet/deps.json`. The supervisor consults this graph
    /// to skip blocked tickets; the orchestrator agent records edges
    /// when filing contracts-first ticket trees.
    Deps {
        #[command(subcommand)]
        sub: DepsSub,
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
    ///
    /// `--detached` wraps the run in a `tmux new-session -d -s
    /// fleet-<session-id>` so the user can later attach for a live
    /// view of the agent inside its pane (via the TUI or
    /// `fleet sessions attach <id>`). Returns immediately after the
    /// tmux session is alive; exit code reflects the spawn, not the
    /// workflow outcome — poll `fleet sessions show <id>` for that.
    /// Without the flag, the run is inline and blocking as before
    /// (the contract CI / autonomous mode relies on).
    Run {
        name: String,
        /// Issue id to act on (matches the tracker's `human_id`).
        /// Mutually exclusive with `--pr`.
        #[arg(long, conflicts_with = "pr")]
        issue: Option<String>,
        /// Pull request number to act on. Resolves the PR through the
        /// configured code host and threads its head ref / sha into
        /// the session so the worktree checks out the PR's branch.
        /// Mutually exclusive with `--issue`.
        #[arg(long, conflicts_with = "issue")]
        pr: Option<u32>,
        /// Pre-minted session id (set by the `--detached` wrapper so
        /// the inner inherits the same id the wrapper printed). When
        /// absent, a fresh id is minted as before. Internal: not
        /// surfaced in `--help` output.
        #[arg(long, hide = true)]
        session_id: Option<String>,
        /// Skip every node up to and including the named id, then
        /// resume execution from the next topo position. Internal:
        /// the scheduler dispatcher sets this when minting a session
        /// from a `pr-list bind: per` root the scheduler already
        /// consumed.
        #[arg(long, hide = true)]
        start_after_node: Option<String>,
        /// Wrap the run in a tmux session for live attach. See the
        /// command-level docs above for the full lifecycle.
        #[arg(long)]
        detached: bool,
    },

    /// Resume a workflow paused at a gate. Loads the persisted session,
    /// re-parses its workflow YAML, and continues from the node after
    /// the gate.
    Resume { session: String },

    /// Internal subcommand used by the tmux-windows fanout
    /// dispatcher to run a single node in its own tmux window.
    /// Loads the session + workflow, runs the named node via the
    /// executor's `run_node`, writes a `FanoutOutcome` JSON file
    /// to `.fleet/sessions/<id>/fanout/<node>.outcome`, exits.
    /// Hidden from `--help` since it's never invoked directly by
    /// users — the executor spawns it as the command for each
    /// fanout sibling's tmux window.
    #[command(hide = true)]
    RunSibling {
        /// Session id (the parent fanout's owning session). The
        /// session must already exist on disk; this subcommand
        /// doesn't mint one.
        #[arg(long)]
        session_id: String,
        /// Node id to run. Must be one of the sibling nodes of a
        /// fanout in the session's workflow.
        node: String,
    },

    /// Replay a prior session from a chosen node. Mints a new session,
    /// copies the source session's artifacts/ into it, and runs the
    /// workflow starting at `--rerun-from` (continues to the end) or
    /// `--rerun-only` (fires exactly that node, then exits). Use case:
    /// iterate on a reviewer prompt without re-paying for upstream
    /// agents — `--rerun-only` keeps the iteration narrow to one node.
    Replay {
        /// Source session id to replay.
        session: String,
        /// Node id to start the rerun at. Continues through every
        /// successor node honouring `loop_back_to`, gates, fanouts.
        /// Mutually exclusive with `--rerun-only`.
        #[arg(long = "rerun-from", conflicts_with = "rerun_only")]
        rerun_from: Option<String>,
        /// Node id to run exactly once, then exit. No downstream nodes
        /// fire, no `loop_back_to` cycles trigger. `when:` still gates
        /// (false predicate skips, run completes with no node fired).
        /// Mutually exclusive with `--rerun-from`.
        #[arg(long = "rerun-only", conflicts_with = "rerun_from")]
        rerun_only: Option<String>,
    },
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
    /// Sweep `.fleet/sessions/` for sessions stuck in `running` whose
    /// driver process is gone; transition them to `crashed` and drop a
    /// forensic snapshot. Run automatically at TUI startup; this
    /// subcommand exists for scripted/manual use.
    Reap,

    /// Clear `.fleet/deps.json` edges where this session's bound
    /// ticket is `blocked`. With `--reason`, also posts a comment
    /// on the ticket explaining why the unblock was done. Errors
    /// when the session has no bound issue.
    Unblock {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },

    /// Attach to a session's live tmux pane for a live view of the
    /// agent. The session must have been launched with
    /// `fleet workflow run --detached` (or via the TUI spawn
    /// picker / autonomous mode, which both pass `--detached`).
    /// Errors clearly when the tmux pane is gone (workflow already
    /// finished) or never existed (foreground run).
    Attach { id: String },

    /// Remove a session's per-session git worktree, freeing the
    /// checked-out source code from disk. The branch and the
    /// session's logs/artifacts/meta.json are kept — `git checkout
    /// fleet/session-<id>` still works afterwards. Exactly one of
    /// `<id>`, `--completed`, or `--all` must be set.
    Prune {
        /// Session id to prune. Refuses to prune a still-Running
        /// session (use `fleet sessions reap` first or wait for it
        /// to finish).
        id: Option<String>,
        /// Prune every session in `Completed` state. Use to reclaim
        /// disk after a batch of successful workflow runs.
        #[arg(long, conflicts_with = "all")]
        completed: bool,
        /// Prune every session in any terminal state (Completed,
        /// Failed, Crashed).
        #[arg(long, conflicts_with = "completed")]
        all: bool,
        /// Also delete the `fleet/session-<id>` git branch after
        /// removing the worktree. Off by default — branches survive
        /// prune so commits an agent made can be inspected or merged.
        /// Use this to drop branches en masse after a batch of
        /// failed/abandoned sessions.
        #[arg(long = "with-branch")]
        with_branch: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretsSub {
    /// List every `secrets.<name>` entry in `.fleet/config.yaml`
    /// along with the backend kind it's resolved through and a
    /// status indicator (ok / unreachable / missing). Doesn't print
    /// the secret values themselves.
    List,

    /// Register a secret. For the well-known `claude_code_oauth_token`
    /// name, walks the user through `claude setup-token`, stores the
    /// resulting token in the OS keychain, and appends a `[secrets.…]`
    /// entry to `.fleet/config.yaml`. For arbitrary names, prompts
    /// for a value and stores it under the same backend.
    Register {
        /// Secret name (e.g. `claude_code_oauth_token`). Matched
        /// case-insensitively against agent `env_passthrough`
        /// declarations at resolve time, so `claude_code_oauth_token`
        /// here resolves `CLAUDE_CODE_OAUTH_TOKEN` in `env_passthrough`.
        name: String,
    },

    /// Resolve a registered secret through its configured backend
    /// and print "ok (NN chars)" — confirms the backend is wired
    /// up without revealing the value. Use after `register` to
    /// catch misconfigured keychain / 1Password references.
    Test {
        /// Secret name from the `secrets:` block.
        name: String,
    },

    /// Remove a secret. Drops the `secrets.<name>` entry from
    /// `.fleet/config.yaml`. The OS keychain entry itself is left
    /// in place — delete it via `secret-tool delete` / `security
    /// delete-generic-password` if you also want to purge the
    /// stored value.
    Remove {
        /// Secret name from the `secrets:` block.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum IssuesSub {
    /// Print one line per issue from the configured tracker, open
    /// issues first.
    List,

    /// File a new ticket. Prints the new ticket's id on stdout
    /// (scriptable). Body defaults to empty; pass `--body` for a
    /// description. Labels are zero or more `--label <name>` args.
    Create {
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "label")]
        labels: Vec<String>,
    },

    /// Post a comment on a ticket.
    Comment { id: String, body: String },

    /// Move a ticket's status. `<status>` must be `open`,
    /// `in-progress`, or `closed`.
    SetStatus { id: String, status: String },

    /// Add a label to a ticket. Idempotent.
    AddLabel { id: String, label: String },

    /// Remove a label from a ticket. Idempotent.
    RemoveLabel { id: String, label: String },
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorSub {
    /// Kill the orchestrator: tear down the tmux pane and mark
    /// meta as Closed. The default `fleet orchestrator` will
    /// respawn it on next invocation.
    Kill,
}

#[derive(Debug, Subcommand)]
pub enum PlanSub {
    /// List plans discovered under `.fleet/plans/`, sorted by id.
    List,

    /// Print a plan's metadata + every item with its state.
    Show { id: String },

    /// Create a new plan in the `Active` state. Mints a fresh id;
    /// prints it on stdout.
    New {
        /// Human-readable plan name (e.g. "Parser refactor").
        name: String,
        /// Comma-separated list of ticket ids the plan walks through
        /// in order. Whitespace around commas is trimmed.
        #[arg(long)]
        tickets: String,
    },

    /// Open the plan YAML in `$EDITOR` (or `$VISUAL`), parse the
    /// edited content back, and save it. The on-disk plan is left
    /// untouched if the edit produces malformed YAML.
    Edit { id: String },

    /// Pause an active plan. Supervisor stops picking items from
    /// it on subsequent ticks; existing in-flight sessions finish.
    Pause { id: String },

    /// Resume a paused plan. Returns to `Active` state.
    Resume { id: String },

    /// Mark a plan completed. Items aren't touched; user signal
    /// only.
    Complete { id: String },

    /// Mark a plan abandoned. Optional `--reason` is recorded to
    /// stderr (future: posted as a comment on the epic).
    Abandon {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },

    /// Manually inject a ticket id into a plan as a `Pending`
    /// item. Default position is the end; `--before <other>`
    /// inserts immediately before another item.
    Inject {
        id: String,
        /// Ticket id to insert as a new pending item.
        ticket: String,
        /// Existing item to insert before. Defaults to appending.
        #[arg(long)]
        before: Option<String>,
    },

    /// Reconcile a plan with its tracker-native epic. Reads the
    /// epic body, parses `- [ ] #<id>` / `- [x] #<id>` task-list
    /// lines, and appends any tickets that aren't already plan
    /// items. Idempotent and additive — existing items aren't
    /// reordered or removed, and the epic isn't written back.
    Sync { id: String },

    /// Reset every failed item in `<id>` back to `Pending` and
    /// resume the plan if it was paused by the `on_item_failure:
    /// stop` policy. Per-item `retry_count` and the prior session
    /// id are kept for forensics — re-runs spawn fresh sessions.
    Retry { id: String },
}

#[derive(Debug, Subcommand)]
pub enum DepsSub {
    /// Print every blocked-on edge in `.fleet/deps.json`. Empty
    /// when no edges are recorded — the common state for repos
    /// that don't use blocked-by orchestration yet.
    List,

    /// Record an edge: `<blocked>` cannot proceed until
    /// `--blocked-on` (a ticket id) or `--blocked-on-tag` (a
    /// freeform slug, stored as `free:<slug>`) resolves. Exactly
    /// one of the two flags is required. Refuses to create a
    /// cycle.
    Add {
        /// Ticket id that gets blocked.
        blocked: String,
        /// Ticket id this one is blocked on. Mutually exclusive
        /// with `--blocked-on-tag`.
        #[arg(long, conflicts_with = "blocked_on_tag")]
        blocked_on: Option<String>,
        /// Freeform tag (e.g. `apt-mirror`) this one is blocked
        /// on. Stored as `free:<tag>` so the supervisor
        /// distinguishes it from ticket edges. Mutually exclusive
        /// with `--blocked-on`.
        #[arg(long, conflicts_with = "blocked_on")]
        blocked_on_tag: Option<String>,
    },

    /// Surgically remove the single edge `(blocked, blocked_on)`.
    /// Idempotent — removing an edge that isn't there isn't an
    /// error.
    Remove {
        /// Ticket id on the left-hand side of the edge.
        blocked: String,
        /// Ticket id on the right-hand side. Mutually exclusive
        /// with `--blocked-on-tag`.
        #[arg(long, conflicts_with = "blocked_on_tag")]
        blocked_on: Option<String>,
        /// Freeform tag on the right-hand side. Same `free:<tag>`
        /// storage as `add`. Mutually exclusive with
        /// `--blocked-on`.
        #[arg(long, conflicts_with = "blocked_on")]
        blocked_on_tag: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AutonomousSub {
    /// Tick the supervisor. `--once` runs a single tick and exits
    /// (suitable for cron). `--watch` loops, sleeping
    /// `autonomous.scan_interval_secs` between ticks, until killed.
    /// Exactly one of the flags must be present.
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        watch: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SchedulerSub {
    /// Flip `.fleet/scheduler.enabled` on. Subsequent `tick --once`
    /// runs (and any TUI-driven tick path) will actually mint
    /// sessions; while disabled, the engine returns `Idle` and the
    /// scheduler-state file is left untouched.
    Enable,
    /// Flip `.fleet/scheduler.enabled` off.
    Disable,
    /// List every workflow that has a `loop:` field, alongside its
    /// last-run timestamp and next-due interval. Read-only — handy
    /// for debugging why an expected workflow didn't fire.
    Status,
    /// Run a single scheduler tick: load workflows + sessions, decide
    /// which are due, mint detached sessions for each candidate, and
    /// persist updated `last_run_at`. Honours the enabled flag —
    /// disabled runs are a no-op.
    Tick {
        /// Run one tick and exit. Required today (no `--watch` loop).
        #[arg(long)]
        once: bool,
    },
}

/// Dispatch the parsed CLI. Returns the exit code to propagate.
#[allow(clippy::too_many_lines)] // top-level match over every subcommand variant.
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
            WorkflowSub::Run {
                name,
                issue,
                pr,
                session_id,
                start_after_node,
                detached,
            } => workflow::run_run(
                &name,
                issue.as_deref(),
                pr,
                session_id.as_deref(),
                start_after_node.as_deref(),
                detached,
            ),
            WorkflowSub::Resume { session } => workflow::run_resume(&session),
            WorkflowSub::RunSibling { session_id, node } => {
                workflow::run_sibling(&session_id, &node)
            }
            WorkflowSub::Replay {
                session,
                rerun_from,
                rerun_only,
            } => workflow::run_replay(&session, rerun_from.as_deref(), rerun_only.as_deref()),
        },
        Command::Sessions { sub } => match sub {
            SessionsSub::List => sessions::run_list(),
            SessionsSub::Show { id } => sessions::run_show(&id),
            SessionsSub::Logs { id, node } => sessions::run_logs(&id, node.as_deref()),
            SessionsSub::Reap => sessions::run_reap(),
            SessionsSub::Unblock { id, reason } => sessions::run_unblock(&id, reason.as_deref()),
            SessionsSub::Attach { id } => sessions::run_attach(&id),
            SessionsSub::Prune {
                id,
                completed,
                all,
                with_branch,
            } => sessions::run_prune(id.as_deref(), completed, all, with_branch),
        },
        Command::Secrets { sub } => match sub {
            SecretsSub::List => secrets::run_list(),
            SecretsSub::Register { name } => secrets::run_register(&name),
            SecretsSub::Test { name } => secrets::run_test(&name),
            SecretsSub::Remove { name } => secrets::run_remove(&name),
        },
        Command::Issues { sub } => match sub {
            IssuesSub::List => issues::run_list(),
            IssuesSub::Create {
                title,
                body,
                labels,
            } => issues::run_create(&title, body.as_deref(), &labels),
            IssuesSub::Comment { id, body } => issues::run_comment(&id, &body),
            IssuesSub::SetStatus { id, status } => issues::run_set_status(&id, &status),
            IssuesSub::AddLabel { id, label } => issues::run_add_label(&id, &label),
            IssuesSub::RemoveLabel { id, label } => issues::run_remove_label(&id, &label),
        },
        Command::Scheduler { sub } => match sub {
            SchedulerSub::Enable => scheduler_cli::run_enable(),
            SchedulerSub::Disable => scheduler_cli::run_disable(),
            SchedulerSub::Status => scheduler_cli::run_status(),
            SchedulerSub::Tick { once } => scheduler_cli::run_tick(once),
        },
        Command::Autonomous { sub } => match sub {
            AutonomousSub::Run { once, watch } => autonomous::run(once, watch),
        },
        Command::Orchestrator { sub } => match sub {
            None => orchestrator::run_default(),
            Some(OrchestratorSub::Kill) => orchestrator::run_kill(),
        },
        Command::Deps { sub } => match sub {
            DepsSub::List => deps::run_list(),
            DepsSub::Add {
                blocked,
                blocked_on,
                blocked_on_tag,
            } => deps::run_add(&blocked, blocked_on.as_deref(), blocked_on_tag.as_deref()),
            DepsSub::Remove {
                blocked,
                blocked_on,
                blocked_on_tag,
            } => deps::run_remove(&blocked, blocked_on.as_deref(), blocked_on_tag.as_deref()),
        },
        Command::Plan { sub } => match sub {
            PlanSub::List => plan::run_list(),
            PlanSub::Show { id } => plan::run_show(&id),
            PlanSub::New { name, tickets } => plan::run_new(&name, &tickets),
            PlanSub::Edit { id } => plan::run_edit(&id),
            PlanSub::Pause { id } => plan::run_pause(&id),
            PlanSub::Resume { id } => plan::run_resume(&id),
            PlanSub::Complete { id } => plan::run_complete(&id),
            PlanSub::Abandon { id, reason } => plan::run_abandon(&id, reason.as_deref()),
            PlanSub::Retry { id } => plan::run_retry(&id),
            PlanSub::Sync { id } => plan::run_sync(&id),
            PlanSub::Inject { id, ticket, before } => {
                plan::run_inject(&id, &ticket, before.as_deref())
            }
        },
    }
}

use crate::tui;
