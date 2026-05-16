# fleet docs

User-facing documentation for fleet's behaviour and configuration. The
docs in this directory document **what fleet does** for the operator,
not how the Rust internals work — for those, read
[`ARCHITECTURE.md`](../ARCHITECTURE.md) at the repo root and the
source under `src/` (each module has a doc-comment explaining its
role).

## Security model

[`security-model.md`](./security-model.md) is the umbrella: a single
page describing the three boundaries fleet enforces and the explicit
non-goals. Start there.

The per-boundary deep-dives:

- [`sandbox.md`](./sandbox.md) — filesystem isolation. The two bind
  mounts that cross the boundary (`/workspace` per-session worktree,
  `/artifacts`), per-adapter hardening (Podman+rootless+optional gVisor,
  Apple Container microVM, Docker, Local), and what stays on the host.
- [`auth.md`](./auth.md) — identity brokering. How `env_passthrough`
  in `.fleet/config.yaml` gets a secret from fleet's process env into
  the agent container without ever touching the image or the
  container filesystem.
- [`network.md`](./network.md) — egress allowlist. The two enforcer
  backends (Podman `--internal` network + tinyproxy sidecar on Linux,
  host-side tinyproxy on macOS / Docker), the default allowlist
  derivation, and the cooperative-vs-enforced caveats.

## Workspace model

- [`worktrees.md`](./worktrees.md) — per-session git worktrees. Why
  every `fleet workflow run` gets its own branch + checked-out
  working directory, how replay re-uses the prior session's branch
  tip, and how `fleet sessions prune` reclaims disk while keeping
  agent commits inspectable.

## Orchestration & planning

- [`orchestration.md`](./orchestration.md) — plans, blocked-outcome
  handling, the `tracker-create` workflow node, the bridge CLI that
  gives agents scoped tracker write access, and the `fleet brainstorm`
  interactive planning agent. The full sequencing model:
  *plan = preference, blocked-on = requirement, brainstorm = the
  planning surface, supervisor = the execution surface*. Currently
  designed; phased delivery underway (status table at the bottom of
  the page).

## Verification

- [`SMOKE_E2E.md`](./SMOKE_E2E.md) — tiered end-to-end smoke test
  that drives every surface (CLI, TUI, workflow execution,
  autonomous, brainstorm) against a throwaway TODO MVC project
  under `tmp/`. The contracts-first decomposition pattern is the
  narrative thread. Pick the tier matching what's installed.
- [`SMOKE_EGRESS.md`](./SMOKE_EGRESS.md) — manual smoke checklist
  for the egress proxy. Run after any egress-related change to
  verify the `curl evil.example.com` → blocked / `curl
  api.anthropic.com` → allowed contract on the supported (OS,
  adapter) combinations.

## First-run bootstrap

`fleet init` scaffolds `.fleet/` and `.devcontainer/` as before. It
now also runs `fleet runtime doctor` at the end and appends a
**Bootstrap your environment** section listing any missing tools
with OS-tailored install commands (`brew install ...` on macOS,
`sudo apt/dnf install ...` on Linux). Silent when everything is
present. Each missing tool carries a note explaining when it's
actually required (e.g. `git-bug` only matters with `tracker:
git-bug`; `tinyproxy` only matters on macOS with `policy:
allowlist`).

## Cost budgets

Opt-in spend guardrails configured per-repo in `.fleet/config.yaml`:

```yaml
cost:
  per_session_budget_usd: 5.00       # null = unlimited (default)
  lifetime_budget_usd: 100.00        # null = unlimited (default)
```

Before each agent node starts, fleet checks the session's
accumulated cost and the lifetime sum across `.fleet/sessions/`. If
either limit is met, the next agent spawn is refused with a clear
error naming the limit + actual figure; the session transitions to
`Failed`. In-flight agents complete — only new spawns are blocked.
Honest scope: no projection logic. Fleet counts reported cost after
each agent finishes; the next spawn's check uses real totals.
