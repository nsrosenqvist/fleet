# Filesystem sandbox

This page covers the **filesystem-isolation** boundary fleet enforces.
Companion documents are [`auth.md`](./auth.md) for identity brokering
and [`network.md`](./network.md) for egress control. The per-session
worktree mechanic that underpins all of this has its own page in
[`worktrees.md`](./worktrees.md).

## What gets mounted into the container

Two bind mounts. That's it.

| Container path | Host path                                       | Mode | Purpose                                                  |
| -------------- | ----------------------------------------------- | ---- | -------------------------------------------------------- |
| `/workspace`   | `<repo>/.fleet/sessions/<id>/worktree/`         | rw   | Per-session git worktree on `fleet/session-<id>` branch. |
| `/artifacts`   | `<repo>/.fleet/sessions/<id>/artifacts/`        | rw   | Inter-node artifact hand-off + agent outputs.            |

Both paths are scoped to the session's directory under `.fleet/`. The
host's broader filesystem — anything outside `.fleet/` — is invisible
to the agent. There is **no** mount for `~`, `~/.ssh`, `~/.config`,
`~/.aws`, `~/.gnupg`, `~/.claude/`, the host's `.gitconfig`, the
host's broader checkout outside of what the worktree sees, or
anything else on the host.

The container also doesn't see the host's other in-progress fleet
sessions — `.fleet/sessions/<other-id>/` isn't mounted in. Parallel
sessions are filesystem-disjoint by construction.

For non-git workspaces, `/workspace` falls back to the repo root with
a warning logged. There's no per-session isolation in that case;
parallel sessions can race on the same files. See
[`worktrees.md`](./worktrees.md#what-this-does-not-protect-against)
for the full caveat.

## Per-adapter isolation

The bind-mount layout above is the same across adapters, but the
*kernel boundary* around it varies:

| Adapter             | Hardening                                                                                  | Notes                                                              |
| ------------------- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| `podman` (Linux)    | Rootless user-namespace **always**. `--runtime=runsc` (gVisor) when detected on host.      | Strongest Linux config. gVisor adds a syscall filter; absence falls back to namespace isolation. |
| `apple-container` (macOS 26+) | Each container is its own Virtualization.framework microVM. Hypervisor-enforced.    | Strongest config overall — no shared kernel with the host.         |
| `docker`            | Rootless **if the host probe says it's available**, otherwise rootful.                     | Weakest of the isolated trio. No gVisor option. Fleet logs the negotiated mode every session — watch the warning if you're security-conscious. |
| `local`             | **None.** The agent runs directly on the host with no container at all.                    | Explicit opt-in only. Useful for fleet-on-fleet development; not for running untrusted code. |

`fleet runtime doctor` reports which adapter was selected and which
hardening flags applied. Run it after `fleet init` to verify the
config you got matches what you expected.

## Secrets handling at the filesystem level

Secrets reach the container as **environment variables**, never as
files. The `env_passthrough` list in `.fleet/config.yaml` names env
vars fleet reads from its own process environment and injects via the
runtime adapter's start_container at runtime. Image layers never
embed them; the container filesystem never persists them; killing the
container reclaims everything. See [`auth.md`](./auth.md) for the
full identity story.

## Trust caveat: postCreate / agent-installed tooling

Fleet builds the devcontainer image from the repo's
`.devcontainer/devcontainer.json`. Anything that devcontainer's
`postCreateCommand` runs at build time — `apt install`, `pip install`,
`npm install`, a `curl | sh` — runs **before** the agent does and is
**outside** the egress allowlist (the egress enforcer is applied per
*workflow*, not per *image build*). That's by design: image builds
need to reach package registries that aren't on the agent's
allowlist, and the registry isn't an attacker plant just because the
agent shouldn't see it.

Anything the agent itself installs at runtime — `cargo install`,
`pip install`, in-container `apt install` — does go through the
egress proxy. That's the place hostile prompt-injected content would
land if it got far enough.

## What this protects against

- An agent cannot read host `~/.ssh`, `~/.gnupg`, the browser
  profile, the password-manager state, the host's own `~/.claude/`,
  or anything outside `.fleet/<id>/`.
- An agent cannot write into the host's `.fleet/config.yaml`,
  `.fleet/workflows/`, or any session it doesn't own. Specifically,
  it cannot append a new agent spec to its own config and re-spawn
  with elevated `env_passthrough`.
- Parallel sessions can't stomp on each other's working files —
  each is on its own branch in its own worktree.
- The host's `claude` CLI and `gh` CLI are unaffected by what the
  agent does inside the container. Mutations to a container-side
  `~/.claude/.credentials.json` go to the container's filesystem,
  which is reclaimed when the session ends.

## What it does NOT protect against

- **Kernel exploits.** Podman without gVisor relies on user
  namespaces alone; a kernel vulnerability that breaks namespace
  isolation is a full escape. With gVisor (when `runsc` is on the
  host PATH) the agent talks to gVisor's user-space syscall layer,
  which substantially narrows the kernel attack surface but doesn't
  eliminate it. Apple Container's hypervisor is a stronger boundary
  but not infallible.
- **Docker without rootless.** Docker's host probe may return
  `rootless=false`; fleet warns at every session start in that case.
  A rootful Docker container that exploits a container-runtime CVE
  reaches the host as root.
- **The Local adapter.** No isolation. The agent has full host
  filesystem access subject only to the fleet user's permissions.
  This adapter is opt-in for a reason.
- **Other sessions on the same host.** Filesystem-disjoint by mount
  layout, but they share the kernel (Linux Podman) or the
  hypervisor (Apple Container). Cross-container side-channels are
  not defended against.
- **`postCreateCommand` and base-image content.** Whatever the
  devcontainer's image build pulls in runs before fleet's egress
  controls apply. Trust the registries you point your devcontainer
  at.

## Operations

**Confirm the adapter and hardening currently in force.**

```sh
fleet runtime doctor
```

Surfaces the adapter chosen, whether rootless was negotiated (Docker),
whether `runsc` was detected (Podman), and the network policy in
effect.

**Inspect a session's bind mounts.**

```sh
fleet runtime inspect <container-id>
```

Or, for live introspection inside the container:

```sh
fleet runtime exec -- mount | grep workspace
```

**Reclaim disk after a batch of completed sessions.**

```sh
fleet sessions prune --completed
```

See [`worktrees.md`](./worktrees.md#operations) for the full prune
cookbook. The session directory under `.fleet/sessions/<id>/`
shrinks dramatically once the worktree is removed; meta.json, logs,
and artifacts stay so you can still inspect what happened.

## Upgrading from the AO/Lima era

Fleet's v2 architecture replaced the Lima VM model entirely. There
are no `~/.lima/` artefacts to migrate; there is no `~/.agent-orchestrator/`
to preserve. If your repo still has an `agent-orchestrator.yaml` or a
`.devcontainer/` that targets the AO-Lima image, replace it with a
plain devcontainer pointing at a base image suitable for your
project. `fleet init` will scaffold a minimal one if you don't have
one.
