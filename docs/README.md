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

- [`SMOKE_EGRESS.md`](./SMOKE_EGRESS.md) — manual smoke checklist
  for the egress proxy. Run after any egress-related change to
  verify the `curl evil.example.com` → blocked / `curl
  api.anthropic.com` → allowed contract on the supported (OS,
  adapter) combinations.
