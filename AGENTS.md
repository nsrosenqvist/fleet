# Agent guidelines — `fleet`

Notes for AI assistants (Claude Code, Codex, etc.) and human contributors working **on this repository**. If you're an agent that fleet spawned into a container, this is also the file your devcontainer's `AGENTS.md` convention surfaces — same file, same audience.

## What this repo is

`fleet` is a single Rust binary — `src/main.rs` is the entry point — that runs devcontainer-defined agent workflows in any git repo. It is *not* an orchestrator-in-a-VM wrapper; the v2 rewrite (mid-2026) replaced the prior AO + Lima architecture with:

- **A runtime adapter trait** (`src/runtime/`) with concrete impls for **Podman** (Linux), **Apple Container** (macOS 26+), **Docker** (cross-platform fallback), and **Local** (no isolation, explicit opt-in). The adapter speaks devcontainer-CLI under the hood, so `.devcontainer/devcontainer.json` in the user's repo defines the agent's environment.
- **A workflow engine** (`src/workflow/`) that runs YAML DAGs from `.fleet/workflows/`. Five node kinds: `agent`, `bash`, `gate`, `assert`, `fanout`. Supports `when:` gating, `loop_back_to` cycles, fanout siblings, artifact hand-off, declared outputs, and resume-after-gate.
- **Per-session git worktrees** (`src/worktree.rs`, see [`docs/worktrees.md`](./docs/worktrees.md)) so each `fleet workflow run` lands on its own `fleet/session-<id>` branch in `.fleet/sessions/<id>/worktree/`. Replay sees the prior run's branch tip; resume picks back up where the agent was editing.
- **Per-workflow egress enforcement** (`src/egress.rs`, see [`docs/SMOKE_EGRESS.md`](./docs/SMOKE_EGRESS.md)) — tinyproxy sidecar on Linux+Podman, host proxy on macOS, allowlist enforced per session.
- **A ratatui TUI** (`src/tui/`) for inspecting sessions, attaching containers, and driving autonomous mode (`Shift+A`).
- **Host-side trackers** (`src/tracker/`) — `git-bug` and `gh` shelled directly from the host, no plugin runtime.

The full v2 design (and what's intentionally out of scope) is in [`~/.claude/plans/declarative-wobbling-quasar.md`](file:///home/niklas/.claude/plans/declarative-wobbling-quasar.md) — read that first if you're picking up a feature from the follow-up list.

User-facing docs live under [`docs/`](./docs/). Start with [`docs/README.md`](./docs/README.md). Note that `docs/sandbox.md` / `docs/auth.md` / `docs/network.md` predate the v2 rewrite and reference AO + Lima — treat them as historical until they're rewritten. The current-state docs are `docs/worktrees.md` and `docs/SMOKE_EGRESS.md`.

## Workspace model

Every workflow session — every `fleet workflow run`, every autonomous spawn, every replay — gets its own git worktree at `.fleet/sessions/<id>/worktree/` on a branch named `fleet/session-<short-id>`, based off the host's current `HEAD` (or, for `replay`, the source session's branch tip). The agent's container bind-mounts that path as `/workspace`. The host's own working tree is never exposed to the agent.

Full lifecycle, configuration, and operations cookbook in [`docs/worktrees.md`](./docs/worktrees.md). Implementation in `src/worktree.rs` + the CLI wiring in `src/cli/workflow.rs` (run / resume / replay) and `src/cli/sessions.rs` (prune).

Non-git workspaces are supported but lose isolation between parallel sessions and lose replay's code-state snapshot guarantee. The CLI logs a warning and falls back to the shared repo root.

## Layout

```
.
├── Cargo.toml          single binary crate — fleet
├── Cargo.lock
├── Makefile            unified verify (Rust + legacy TS plugin)
│
├── src/                Rust source
│   ├── main.rs         clap entry → cli::dispatch
│   ├── cli/            command surface (workflow, sessions, runtime, autonomous, issues, init, ui)
│   ├── tui/            ratatui session browser + autonomous toggle
│   ├── workflow/       YAML DSL, DAG validator, executor (agent/bash/gate/assert/fanout)
│   ├── session/        Session value object, state machine, on-disk store, reaper
│   ├── runtime/        adapter trait + Podman / AppleContainer / Docker / Local impls
│   ├── agent/          agent registry, prompt-prefix builder, cost parsing
│   ├── tracker/        git-bug / gh / (no-op) host-side trackers
│   ├── egress.rs       per-workflow egress enforcer + tinyproxy / HostProxy backends
│   ├── worktree.rs     per-session `git worktree` helpers
│   ├── autonomous.rs   supervisor engine driving TUI `Shift+A` + `fleet autonomous run`
│   ├── repo.rs         fleet root discovery
│   ├── repo_config.rs  `.fleet/config.yaml` parsing
│   └── process.rs      ProcessInvoker trait + RealProcessInvoker + run_interactive
│
├── docs/               user-facing docs (security-model, network, sandbox, auth,
│                       worktrees, SMOKE_EGRESS)
│
├── packages/
│   └── tracker-git-bug/   LEGACY: AO plugin from the pre-v2 era. Nothing in `src/`
│                          links to it; the live git-bug integration is
│                          `src/tracker/git_bug.rs`. Slated for removal.
│
└── scripts/
    └── fleet-postcreate-trust-cwd.cjs   LEGACY: AO-era postCreate helper.
                                          Slated for removal.
```

## Stack defaults

**Rust is the only live stack.** New CLIs, inspectors, harness code, or supporting binaries → add a module under `src/` (single binary crate, not a workspace; split only if a second binary is genuinely needed). The TypeScript package under `packages/` is legacy — do not extend it; do not add new TS code.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings    # pedantic + nursery on, see Cargo.toml [lints]
cargo test                                    # 670+ tests at last count
```

Every gate is a hard requirement before commit. No "fix forward" by stacking commits on top of broken verification. Lint warnings are errors.

The `Makefile` still has `make verify` which also runs the legacy TS package's checks; CI runs the Rust gates directly. If a clippy rule is genuinely wrong for this codebase, change the rule in `Cargo.toml`'s `[lints]` block — don't sprinkle `#[allow]` per file. Per-call-site allows are fine when they're documenting an intentional exception (e.g. `#[allow(dead_code)] // wired by <subsequent commit>`).

## Commit messages

Conventional Commits v1.0.0 by convention (no automated lint hook — keep yourself honest). Allowed types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `chore`, `build`, `ci`, `style`, `revert`. Breaking changes go in the footer (`BREAKING CHANGE: …`).

Always include the co-author trailer for Claude-authored commits:

```
Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

Two-commit cadence is the established rhythm for non-trivial chunks (data/logic in one, wiring/display in another); single commit is fine when the change is genuinely small.

## Conventions — Rust (`src/`)

- **Edition 2024.** MSRV is pinned in `Cargo.toml`'s `rust-version`.
- **Errors:** `anyhow::Result` at module boundaries; `thiserror` for typed errors where they help. Never panic in library paths.
- **Subprocess:** go through `crate::process::ProcessInvoker` (capture-output, mockable) or `crate::process::run_interactive` (inherited stdio for user-facing commands). Direct `std::process::Command` is a smell — it's untestable.
- **Tests:** inline at module bottom under `#[cfg(test)] mod tests`. Use `MockProcessInvoker` + `tempfile::tempdir` for any test that would otherwise touch real subprocesses or the filesystem. Pure helpers (free functions) live next to their consumers and are unit-tested directly — `autonomous::engine`, `workflow::expr`, `session::reaper::classify`, `agent::cost`, `egress::resolve_allowlist`, `worktree::*` are the pattern.
- **Adapters:** `RuntimeAdapter` trait stays stable. Concrete impls are separate `Send + Sync` structs in `src/runtime/{podman,apple_container,docker,local}.rs`. New adapters go behind the same trait.
- **Session state machine:** the nested-match transition table in `src/session/state.rs` is the source of truth. Don't restructure it — the doc-comment explains why. Add new states by extending the table directly.
- **Secrets:** wrap in `secrecy::SecretString`. Never log; never expose via `Debug`. Env-passthrough from `.fleet/config.yaml` is the contract for getting them into the container.
- **TUI mutations:** all state lives on `AppState`. Drawing is a pure function over `AppState` (`render_*` helpers, several with pure `*_pairs` helpers behind them for testability). Key events mutate `AppState`. The refresh loop runs on a background thread and ships updates via `mpsc::channel` so render never blocks.

## TUI key reference

Sessions view (the default):

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Navigate sessions |
| `r` | Force refresh |
| `d` | Toggle doctor view |
| `n` | Open spawn picker |
| `Shift+K` | Kill / mark failed (confirm via status bar `y/N`) |
| `Shift+A` | Toggle autonomous mode |
| `q` / `Esc` | Quit |

Doctor view:

| Key | Action |
| --- | --- |
| `r` | Re-probe host |
| `Esc` / `d` | Back to sessions |
| `q` | Quit |

Spawn picker:

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Navigate workflows |
| `Enter` | Spawn the selected workflow |
| `Esc` / `q` | Cancel |

## Things not to touch without coordination

- **`src/session/state.rs` transition table.** The nested match is deliberate; see its doc-comment. Add states by extending the table, not by reshaping it.
- **`RuntimeAdapter` trait shape.** Stable across v2. New backends implement it; the trait itself doesn't grow without checking the four existing impls.
- **`ExecuteRequest` struct.** ~50 construction sites in tests; field additions cost real churn. Group field additions when feasible.
- **CI matrix (`.github/workflows/`).** Cross-arch builds for `x86_64`/`aarch64` × `linux`/`darwin` — adding a target tuple means a real cost increase.

## Honesty rules

- **Don't claim end-to-end behaviour you only wired the plumbing for.** The egress module's docstring and `docs/SMOKE_EGRESS.md` are the template: plumbing landed, real verification needs a manual host smoke pass.
- **Update or remove stale comments alongside the code change** that makes them stale. When you rename a struct, fix the doc-comments that named it. When you remove a module, sweep for prose references to it. Comment rot is silent and compounds — the only way to keep it out is to treat it as part of the change that introduced it.
- **Surface uncertainty rather than hiding it.** When a change has a real gap (uncommitted host changes don't ride into a worktree, DNS exfiltration still leaks, replay's `outputs:` accumulator rebuilds empty), document it in the docstring or the user-facing doc — not just in the commit message.
