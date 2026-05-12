# Agent guidelines — `fleet`

Notes for AI assistants (Claude Code, Codex, etc.) and human contributors working **on this repository**.

This file is **not** the rule sheet AO worker sessions read when they're spawned. Those rules live next to whatever codebase AO is running against — referenced via the `agentRulesFile` field in `agent-orchestrator.yaml`. A starter template you can copy into that codebase lives at [`templates/AGENTS.md`](./templates/AGENTS.md). Don't conflate the two.

## What this repo is

`fleet` is a Rust binary — `src/main.rs` is the entry point. It wraps the [Composio Agent Orchestrator](https://github.com/ComposioHQ/agent-orchestrator) (AO) running inside a Lima VM:

- Resolves the Claude OAuth token from env / macOS Keychain / 1Password.
- Crosses the host → VM boundary via `limactl shell` so the token never appears on a host command line.
- Provides a ratatui dashboard (`fleet ui`) for inspecting running AO sessions, attaching their tmux panes, and driving AO's lifecycle.

The repo also contains one Node package: `packages/tracker-git-bug/`. It's a [git-bug](https://github.com/git-bug/git-bug) tracker plugin AO loads as an ESM module at runtime. That's the only TypeScript code here; the plugin contract (`@aoagents/ao-core`'s `Tracker` interface) forces it to be TS, so it lives in its own package with its own tooling (oxc, vitest, tsc).

## Layout

```
.
├── Cargo.toml             single binary crate — fleet
├── Cargo.lock
├── src/                   Rust source (cli/, tui/, ao/, secrets/, …)
├── target/                cargo output (gitignored)
│
├── agent-orchestrator.yaml AO project config; auto-merge is off
├── scripts/               postCreate hook helpers AO runs in worktrees
│
├── packages/
│   └── tracker-git-bug/   The one Node package. Self-contained:
│       ├── package.json   own deps, own scripts (format/lint/test/build)
│       ├── tsconfig.json
│       ├── .oxlintrc.json
│       ├── .oxfmtrc.json
│       ├── vitest.config.ts
│       └── src/
│
├── templates/
│   └── AGENTS.md          Starter rules to drop into a codebase AO runs against
│
└── Makefile               unified verify / build across both stacks
```

## Stack defaults

**Rust is the default.** New CLIs, inspectors, harness code, or supporting binaries → add a module to `src/` (this is a single binary crate, not a workspace; if a second binary is genuinely needed, split into a workspace at that point — not before). TypeScript is reserved for AO plugin packages under `packages/` because AO loads those as ESM modules at runtime; there's no choice there.

## Verification

```sh
make verify         # both stacks
make verify-rust    # just cargo fmt --check + clippy + test
make verify-tracker # just the TS plugin (cd packages/tracker-git-bug && pnpm verify)
```

Every gate is a hard requirement before commit. No "fix forward" by stacking commits on top of broken verification. Lint warnings are errors.

If a clippy / oxlint rule is genuinely wrong for this codebase, change the rule in `Cargo.toml`'s `[lints]` block or in the tracker's `.oxlintrc.json` — don't sprinkle `#[allow]` / `// eslint-disable` per file.

## Commit messages

Conventional Commits v1.0.0 by convention (no automated lint hook — keep yourself honest). Allowed types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `chore`, `build`, `ci`, `style`, `revert`. Breaking changes go in the footer (`BREAKING CHANGE: …`).

## Conventions — Rust (`src/`)

- **Edition 2024.** MSRV is pinned in `Cargo.toml`'s `rust-version`.
- **Errors:** `anyhow::Result` at module boundaries; `thiserror` for typed errors. Never panic in library paths.
- **Subprocess:** go through `crate::process::ProcessInvoker` (capture-output, used by state queries) or `crate::process::run_interactive` (inherited stdio, used by user-facing commands). Direct `std::process::Command` is a smell — it's untestable.
- **Secrets:** wrap in `secrecy::SecretString`. Never log; never expose via `Debug`.
- **TUI mutations:** all state lives on `App`. Drawing is a pure fn over `App`. Key events mutate `App`. The refresh loop runs on a background thread and ships updates via `mpsc::channel` so render never blocks.

## Conventions — TypeScript (`packages/tracker-git-bug/`)

- ESM + NodeNext module resolution. No CommonJS.
- Node 20+. pnpm as the package manager (lockfile lives inside the package — there is no root npm tree).
- Explicit `.js` extensions on relative imports.
- No `any`. Use `unknown` + narrowing.
- Shell out to `git-bug` for tracker behavior — don't vendor its on-disk ref format.
- Logging: stderr only. Stdout is reserved for AO ↔ plugin protocol output where applicable.

## `fleet` key reference (TUI)

| Key               | Action                                                                  |
| ----------------- | ----------------------------------------------------------------------- |
| `↑/↓` `j/k`       | Navigate sessions                                                       |
| `Enter`           | Suspend TUI, drop into the selected session's tmux pane                 |
| `t`               | Open `git-bug termui` (tracker preview)                                 |
| `c`               | `$EDITOR agent-orchestrator.yaml`                                       |
| `r`               | Force-refresh now                                                       |
| `Shift+K` / `Del` | Kill selected session (confirm via status bar `y/N`)                    |
| `Shift+S`         | Start AO orchestrator + dashboard (token-injecting)                     |
| `Shift+X`         | Stop AO (confirm via status bar `y/N`)                                  |
| `Shift+W`         | Open `http://localhost:3000/projects/<p>/sessions/<id>` in host browser |
| `q` / `Ctrl-C`    | Quit                                                                    |

## Things not to touch without coordination

- `~/.lima/fleet-vm.yaml` — VM definition. Editing it can require a rebuild.
- `agent-orchestrator.yaml` — especially do not flip `approved-and-green.auto` to `true`.
