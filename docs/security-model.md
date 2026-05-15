# Security model

fleet's job is to give agents enough access to do useful software work
while keeping them away from anything else on the host. This page is the
umbrella view of how it does that and what it deliberately does not
protect against.

## Threat model

The agent reads and writes the user's source code, executes arbitrary
shell commands (build, test, lint, package managers, `curl`, `git`),
and has network access for registry / docs / API fetches. The code the
agent reads is **implicitly untrusted** — READMEs and dependency docs
are prompt-injection vectors. We defend against:

- Agent doing damage outside the per-session worktree.
- Agent reading host secrets (`~/.ssh`, `~/.aws`, `~/.gnupg`, the
  browser profile, password-manager state, the host's own
  `~/.claude/`).
- Agent network exfiltration to attacker-controlled hosts.
- Most kernel exploits, where the runtime adapter provides hardening.

We do **not** defend against side-channel attacks, an adversary the
host has invited in (host compromise), or any of the listed non-goals
at the bottom of this page.

## The three boundaries

```
┌─────────────────────────── host ───────────────────────────┐
│                                                            │
│   .fleet/sessions/<id>/  ─ rw bind ──┐                     │
│   .fleet/sessions/<id>/worktree/ rw ─┤                     │
│                                      ▼                     │
│                              ┌─── container ───┐           │
│                              │  /workspace     │           │
│                              │  /artifacts     │           │
│                              │  agent process  │           │
│                              │       │         │           │
│                              │       ▼         │           │
│                              │  HTTP_PROXY ────┼─► tinyproxy
│                              │                 │  (allowlist)
│                              └─────────────────┘           │
│                                                            │
│   secrets via env_passthrough on the container spec        │
│   only — never persisted in the image, never on disk       │
└────────────────────────────────────────────────────────────┘
```

The container is realized by the chosen `RuntimeAdapter` —
Podman + rootless + optional gVisor (Linux), Apple Containerization
microVM (macOS 26+), Docker (cross-platform fallback), or Local (no
isolation, explicit opt-in). Each adapter applies different hardening
to the same boundary; the per-adapter rows in
[`sandbox.md`](./sandbox.md) spell out which.

Three independent layers. Each has its own deep-dive:

### 1. Filesystem — [`sandbox.md`](./sandbox.md)

- The host's working tree is never exposed to the agent. Each session
  gets its own `git worktree` at `.fleet/sessions/<id>/worktree/`,
  bind-mounted into the container as `/workspace`. See
  [`worktrees.md`](./worktrees.md) for the per-session isolation
  guarantees.
- The session's artifacts directory is mounted as `/artifacts`.
  Nothing else crosses the boundary — not the host home, not
  `~/.ssh`, not the broader filesystem.
- On Apple Container the container is its own Virtualization.framework
  microVM (hypervisor-enforced). On Linux Podman it's a rootless
  user-namespace, optionally with gVisor's `runsc` for syscall
  filtering. Docker is the weakest; Local has no isolation at all.

**Protects against:** an agent reading or writing host `~/.ssh`,
`~/.gnupg`, `~/.config/gh`, host `~/.claude`, browser data,
password-manager state, or any host directory outside `.fleet/`.
Parallel sessions can't stomp on each other's working files.

**Does not protect against:** kernel exploits without gVisor (Linux
Podman without runsc), namespace-bypass primitives in Docker rootful
mode, the Local adapter (none), or a process inside one Apple
Container microVM exploiting Virtualization.framework itself.

### 2. Identity — [`auth.md`](./auth.md)

- Secrets reach the agent via `env_passthrough` in `.fleet/config.yaml`:
  fleet reads the named env var from its own process environment and
  injects it into the container at start. The image never embeds it;
  no file holds it on disk.
- Trackers use the host's existing auth: `gh` for GitHub
  (`~/.config/gh/hosts.yml`), nothing for git-bug (project-local — it
  reads the repo's git refs directly).

**Protects against:** secrets leaking into image layers or persisting
in the container filesystem. Token rotation stays fully under the
host user's control.

**Does not protect against:** an agent that has the env var inside
the container reading it back. The threat model is about *getting*
secrets to the agent safely, not preventing the agent (which must use
them) from echoing them somewhere. Parallel sessions inside the same
container engine see different env vars — the secrets aren't a shared
resource.

### 3. Network — [`network.md`](./network.md)

- `runtime.network.policy: allowlist` routes outbound HTTPS through a
  tinyproxy sidecar (Linux + Podman) or a host-side tinyproxy
  (macOS + Apple Container / Docker), filtering on the SNI / CONNECT
  hostname.
- Default allowlist: the configured tracker's host
  (`api.github.com` + `github.com` for `gh`), the LLM provider implied
  by the agents' `env_passthrough` (Anthropic / OpenAI), and the
  registry hosting the proxy image. Plus the user's `extra_hosts`.
- Smoke verification: [`SMOKE_EGRESS.md`](./SMOKE_EGRESS.md).

**Protects against:** an agent accidentally fetching from an
unintended CDN, registry, or third-party host. A clear deny instead
of a silent leak.

**Does not protect against:** DNS exfiltration on macOS (the
Linux + Podman path now ships a DNS-stub sidecar that NXDOMAINs
non-allowlist names; macOS lacks the network-internal primitive
needed to make this work and remains env-only), SNI-on-IP bypass (a
client that resolves an allowlisted name to an attacker IP), or
kernel-level egress on the macOS path (the macOS enforcer is env-only
— a hostile agent that unsets `HTTPS_PROXY` reaches the network
directly).

## Explicit non-goals

These are NOT what fleet's security model promises today. Calling them
out so you can build your own layered controls if you need them:

- **DNS-level egress filtering on macOS.** The Linux + Podman path
  ships a DNS-stub sidecar (`--internal` network + pre-resolved
  allowlist + NXDOMAIN for everything else). The macOS path can't
  match this until Apple Container provides a network-internal
  primitive — until then, `HostProxyEnforcer` is env-only and DNS
  queries from the workflow container reach the host resolver.
- **Kernel-level egress on macOS.** Apple Container has not shipped
  a `--internal`-network primitive equivalent to Podman's. The macOS
  enforcer relies on `HTTP_PROXY` env-var injection — strong against
  honest-mistake agents, soft against an active attacker that
  `unset HTTPS_PROXY` and dials raw sockets.
- **Anti-exfiltration against a malicious agent.** The allowlist is
  a guardrail, not a hard boundary; see SNI-bypass + DNS gaps above.
- **Detection / auditing.** No in-container IDS, no network capture,
  no filesystem audit log shipped today.
- **Defense against host compromise.** The whole sandbox model
  assumes the host the user is sitting at is theirs and not already
  compromised. fleet protects the host from the agents, not the
  agents from a compromised host.
- **Multi-user fleet on shared infrastructure.** Each fleet process
  is single-tenant against the local repo. Running fleet on a shared
  build host with multiple users isn't a scenario the model covers.

## Roadmap

Tracked as follow-ups in the v2 plan; the broad themes:

- **Apple Container `--internal` network pinning.** Promotes the
  macOS HostProxyEnforcer from env-only to engine-enforced once
  Apple ships the network primitive (or fleet's workarounds settle
  enough to ship a real boundary). The DNS-stub piece on Linux +
  Podman has shipped; the macOS equivalent is blocked on the same
  Apple-side primitive landing.
- **First-run bootstrap UX.** `fleet runtime doctor` reports what's
  missing; an interactive install-prompt would close the loop for
  users who don't have Podman / runsc / devcontainer-CLI configured.

If you need stricter guarantees today than fleet provides, layer your
own controls outside the container (host firewall, dedicated user
account, dedicated machine).
