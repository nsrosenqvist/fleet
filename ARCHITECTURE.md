# Architecture

System-level view of `fleet`. The audience is a contributor trying to
find their bearings — where the major pieces live, how they fit
together, and what the load-bearing abstractions are. For the
*operator's* view (security boundaries, configuration, day-to-day
operations), start at [`docs/README.md`](./docs/README.md).

## Pillars

Six load-bearing design choices everything else hangs off of:

1. **Standalone Rust binary, per-repo state.** `fleet` runs from any
   directory. Per-repo state lives in `.fleet/` (worktrees, session
   metadata, workflow runs, artifacts). Global state lives under
   `$XDG_CONFIG_HOME/fleet/`. The binary makes no assumption about
   being run from this checkout — it's distributable.

2. **Runtime adapter trait, not coupling to one engine.** The
   `RuntimeAdapter` trait (`src/runtime/mod.rs`) is the only thing
   the rest of the binary talks to about containers. Concrete impls
   live as `Send + Sync` structs: `Podman` (Linux primary),
   `AppleContainer` (macOS 26+, primary), `Docker` (cross-platform
   fallback), `Local` (no isolation, explicit opt-in). Adapter
   selection is auto-with-override: a host probe picks the best
   available, `runtime.adapter` in `.fleet/config.yaml` pins.

3. **Devcontainer as the user-facing env spec.** Users describe their
   agent's environment in standard `.devcontainer/devcontainer.json`.
   Implementation: invoke the devcontainer CLI as a subprocess; the
   adapter chooses the engine flag and adds per-backend hardening.
   Multiple devcontainers per repo permitted; default is
   `.devcontainer/devcontainer.json`.

4. **Workflows are YAML DAGs.** First-class. The smallest workflow is
   one agent node ("the old behaviour"); the engine supports
   planner → coder → reviewer → PR with artifact hand-off,
   conditional routing on agent decisions (`when:`), human gates,
   bounded revision loops (`loop_back_to` / `max_loops`), and
   fanout. Workflows live at `.fleet/workflows/<name>.yaml`. The
   executor is `src/workflow/executor.rs`.

5. **A session is a workflow run.** Not "a Claude Code process."
   Fleet's session abstraction owns the worktree, the container(s)
   for each agent step, the artifacts directory, the timeline, and
   the bookkeeping for autonomous-mode slot accounting. The agent
   process inside a container is an implementation detail of a
   workflow node. The value object lives at
   `src/session/aggregate.rs`; the state machine at
   `src/session/state.rs`; the on-disk store at
   `src/session/store.rs`.

6. **Tracker is the work source, not the boss.** Trackers only list
   and read issues. Workflow nodes are responsible for *resolving* an
   issue (commenting, closing). Impls run host-side: `gh` for
   GitHub, `git-bug` for project-local. The trait is
   `src/tracker/mod.rs`.

## System diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│ fleet binary (single static-ish binary, per-repo invocation)         │
│                                                                      │
│  ┌────────────┐    ┌──────────────┐    ┌───────────────┐             │
│  │    TUI     │◄──►│   Session    │◄──►│   Workflow    │             │
│  │ (ratatui)  │    │   Manager    │    │    Engine     │             │
│  │            │    │   + Reaper   │    │   (DAG exec)  │             │
│  └────────────┘    └──────────────┘    └──────┬────────┘             │
│        ▲                  ▲                   │                      │
│        │                  │                   ▼                      │
│        │           ┌──────┴───────────────────┴──────┐               │
│        │           │  Runtime  Adapter trait         │               │
│        │           │  ─────────────────────────────  │               │
│        │           │  Podman │ AppleCont │ Docker │  │               │
│        │           │  Local                         │               │
│        │           └─────────────────┬──────────────┘               │
│        │                             │                              │
│        │   ┌────────────┐    ┌───────┴────────┐    ┌─────────────┐  │
│        │   │  Tracker   │    │    Worktree    │    │   Egress    │  │
│        │   │  (gh /     │    │  per-session   │    │  Enforcer   │  │
│        │   │   git-bug) │    │  git worktree  │    │  (tinyproxy)│  │
│        │   └────────────┘    └────────────────┘    └─────────────┘  │
│        │                                                            │
│   ┌────┴─────────────────────────────────────────────────────────┐  │
│   │  Per-repo state:  .fleet/{config.yaml, workflows/,           │  │
│   │                    sessions/<id>/{meta.json, worktree/,      │  │
│   │                                   artifacts/, logs/},        │  │
│   │                    prompts/}                                 │  │
│   │  Global state:    $XDG_CONFIG_HOME/fleet/                    │  │
│   └──────────────────────────────────────────────────────────────┘  │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
                                  │
                                  │  RuntimeAdapter::start_container
                                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  Container (engine-specific isolation; see docs/sandbox.md)          │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  /workspace  = .fleet/sessions/<id>/worktree/   (rw bind)    │    │
│  │  /artifacts  = .fleet/sessions/<id>/artifacts/  (rw bind)    │    │
│  │  env:        ANTHROPIC_API_KEY (via --remote-env)            │    │
│  │              HTTP_PROXY → tinyproxy (when egress enforced)   │    │
│  │  agent process — only thing fleet runs in here               │    │
│  └──────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

The boxes labelled "Runtime Adapter", "Tracker", "Worktree", and
"Egress Enforcer" are the four points of pluggability. Everything
above the line composes against trait surfaces (`RuntimeAdapter`,
`Tracker`, `EgressEnforcer`) plus a small set of free-function
helpers (`worktree::*`).

## Module map

```
src/
├── main.rs               clap parse → cli::dispatch
│
├── cli/                  command surface
│   ├── mod.rs            clap subcommand enum + dispatch
│   ├── init.rs           `fleet init` — scaffold .fleet/, .devcontainer/
│   ├── runtime.rs        `fleet runtime {doctor|build|up|exec|stop|attach|inspect}`
│   ├── workflow.rs       `fleet workflow {list|validate|run|resume|replay}`
│   ├── sessions.rs       `fleet sessions {list|show|logs|reap|prune}`
│   ├── issues.rs         `fleet issues list`
│   ├── autonomous.rs     `fleet autonomous run --once|--watch`
│   └── scheduler.rs      `fleet scheduler {enable|disable|status|tick --once}`
│
├── tui/mod.rs            ratatui session browser, Shift+A autonomous toggle
│
├── workflow/             YAML DAG: parse, validate, execute
│   ├── spec.rs           DSL types (serde)
│   ├── validate.rs       static checks (cycles, dangling deps, missing artifacts)
│   ├── executor.rs       run loop, run_node, run_fanout_node, replay, resume
│   └── expr.rs           `when:` predicate evaluator
│
├── session/              the value object + persistence
│   ├── aggregate.rs      Session struct + setters (current_node, cost,
│   │                     worktree, outputs)
│   ├── state.rs          state machine: nested-match transition table
│   ├── store.rs          on-disk store under .fleet/sessions/<id>/
│   ├── containers.rs     leaked-container reaping bookkeeping
│   └── reaper.rs         classify + sweep crashed sessions
│
├── runtime/              RuntimeAdapter trait + four impls + supporting types
│   ├── mod.rs            trait, ContainerSpec, ContainerState
│   ├── capabilities.rs   Capabilities struct (rootless? gvisor? net iso?)
│   ├── detect.rs         host probe (which engines are usable here)
│   ├── factory.rs        adapter construction from RuntimeConfig + probe
│   ├── devcontainer.rs   devcontainer.json parser
│   ├── devcontainer_cli.rs   invoke devcontainer CLI as subprocess
│   ├── podman.rs         Linux + rootless + optional gVisor
│   ├── apple_container.rs    macOS 26+, microVM-per-container
│   ├── docker.rs         cross-platform fallback, rootless if available
│   └── local.rs          no isolation — explicit opt-in
│
├── tracker/              host-side issue trackers
│   ├── mod.rs            Tracker trait, Issue shape
│   ├── github.rs         shells `gh issue list --json`
│   └── git_bug.rs        shells `git-bug bug --format json`
│
├── code_host/            host-side PR / CI / code-host backend
│   ├── mod.rs            CodeHost trait, PrSummary/PrDetail/ChecksSummary
│   └── github.rs         shells `gh pr {list,view,checks,comment,create}`
│
├── scheduler/            loop-based workflow scheduler
│   ├── mod.rs            SchedulerEngine state machine (pure)
│   ├── store.rs          .fleet/scheduler_state.json (last_run_at)
│   └── dispatcher.rs     SchedulerSpawn → `fleet workflow run` subprocess
│
├── agent/                agent registry + cost + prompt-prefix builder
│   ├── mod.rs            AgentSpec, AgentRegistry
│   ├── registry.rs       default registry (claude-code preconfigured)
│   └── cost.rs           parse $X.YYZZ cost lines from agent stdout
│
├── egress.rs             EgressEnforcer trait + NoopEnforcer +
│                         PodmanTinyproxyEnforcer + HostProxyEnforcer
│
├── worktree.rs           git worktree helpers (create/remove/branch/prune)
│
├── autonomous.rs         supervisor engine driving TUI Shift+A + CLI watch
│                         (one session per open tracker issue; orthogonal
│                         to scheduler/ which fires on elapsed intervals)
│
├── repo.rs               fleet root discovery (.fleet/ > .git/ > cwd)
├── repo_config.rs        .fleet/config.yaml schema + parsing
└── process.rs            ProcessInvoker trait (mockable) + RealProcessInvoker
```

## How a workflow run flows through this

A `fleet workflow run standard` (in a fresh repo) walks roughly:

1. **`cli::workflow::run_run`** parses `.fleet/config.yaml`, builds
   the workflow spec from `.fleet/workflows/standard.yaml`, picks an
   adapter via `runtime::factory::build_adapter`, mints a session id.
2. **`worktree::create_worktree`** creates the per-session branch and
   checks it out into `.fleet/sessions/<id>/worktree/`. The path becomes
   the executor's `workspace`.
3. **`egress::build_enforcer`** picks `PodmanTinyproxyEnforcer` (or
   the host-proxy / noop variants) based on adapter + policy.
4. **`WorkflowExecutor::execute`** validates the DAG, creates the
   session on disk via `SessionStore::create`, stamps the worktree
   path onto the Session, transitions to `Running`, sets up the
   egress enforcer, then enters `run_loop`.
5. **`run_loop`** walks the topological order. For each node:
   - Evaluates `when:` (skip + log if false).
   - Calls `run_agent_node` / `run_bash_node` / `run_fanout_node`
     etc. depending on `NodeKind`.
   - For agent nodes: `RuntimeAdapter::start_container` is invoked
     with a `ContainerSpec` carrying the worktree bind mount, the
     artifacts bind mount, the proxy env, and the agent's
     `env_passthrough` resolved against fleet's own env.
   - Captures stdout to `.fleet/sessions/<id>/logs/<node>.log`,
     parses cost lines into `Session::node_costs`.
   - Extracts declared `outputs:` artifacts, persists them into the
     session's `outputs` accumulator.
   - Resolves `loop_back_to` (cycles within `max_loops`) or
     advances `i`.
6. **Gate nodes** transition the session to `AwaitingGate` and
   return. The user resumes via `fleet workflow resume <id>`, which
   re-enters `run_loop` at the post-gate index against the same
   worktree.
7. **Finalize.** On normal exit, `Completed`. On error, `Failed`. On
   crash (driver process dies), the reaper detects on next startup
   and marks `Crashed` with a forensic snapshot.

Replay (`fleet workflow replay --rerun-from <node>`) is the same
flow but bases the new session's worktree off the *src* session's
branch tip (so the new session sees the prior run's code state) and
hands the src's `outputs:` accumulator forward.

## The four points of pluggability

| Trait                | What plugs in                                    | File                       |
| -------------------- | ------------------------------------------------ | -------------------------- |
| `RuntimeAdapter`     | Podman / AppleContainer / Docker / Local         | `src/runtime/mod.rs`       |
| `Tracker`            | GitHub / git-bug; future: Linear, Jira           | `src/tracker/mod.rs`       |
| `EgressEnforcer`     | Noop / PodmanTinyproxy / HostProxy               | `src/egress.rs`            |
| `ProcessInvoker`     | RealProcessInvoker (prod) / MockProcessInvoker (tests) | `src/process.rs`     |

Anything else is free functions + structs. The four traits are the
only abstraction surface fleet keeps stable across releases.

## State on disk

```
<repo>/
├── .devcontainer/
│   └── devcontainer.json
└── .fleet/                                gitignored by `fleet init`
    ├── config.yaml                        runtime/tracker/agents/workflows config
    ├── prompts/                           user-authored agent prompts
    ├── workflows/
    │   ├── standard.yaml
    │   ├── hotfix.yaml
    │   └── review-only.yaml
    └── sessions/
        └── <session-id>/
            ├── meta.json                  Session struct serde
            ├── worktree/                  per-session git worktree
            ├── artifacts/                 inter-node hand-off + agent outputs
            └── logs/
                ├── <node-a>.log
                └── <node-b>.log
```

## Where to learn more

- [`docs/security-model.md`](./docs/security-model.md) — the
  umbrella view of the three boundaries.
- [`docs/sandbox.md`](./docs/sandbox.md) — filesystem isolation
  deep-dive.
- [`docs/auth.md`](./docs/auth.md) — identity brokering deep-dive.
- [`docs/network.md`](./docs/network.md) — egress allowlist
  deep-dive.
- [`docs/worktrees.md`](./docs/worktrees.md) — per-session git
  worktree model.
- [`docs/SMOKE_EGRESS.md`](./docs/SMOKE_EGRESS.md) — manual smoke
  verification for the egress proxy.
- [`AGENTS.md`](./AGENTS.md) — contributor / agent guidelines
  (conventions, verification, TUI keys).
- [`~/.claude/plans/declarative-wobbling-quasar.md`](file:///home/niklas/.claude/plans/declarative-wobbling-quasar.md) —
  the original v2 design plan, kept for context. Code is now the
  source of truth where they disagree.
