# Security model

fleet's job is to give AO workers enough access to do useful software
work while keeping them from touching anything else on the host. This
page is the umbrella view of how it does that and what it deliberately
does not protect against.

## The three boundaries

```
┌──────────────────────── host ────────────────────────┐
│                                                       │
│   ~/.config/fleet/  ── ro mount ──┐                   │
│   ~/.agent-orchestrator/ ── rw ──┐│                   │
│                                  ││                   │
│                                  ▼▼                   │
│                         ┌────── fleet-vm ──────┐      │
│                         │  lima user           │      │
│                         │   $HOME (private)    │      │
│                         │   AO + claude-code   │      │
│                         │      │               │      │
│                         │      ▼               │      │
│                         │  tinyproxy ──► outbound      │
│                         │  (allowlist filter)         │
│                         └──────────────────────┘      │
│                                                       │
└───────────────────────────────────────────────────────┘
```

Three independent layers. Each is documented in its own page.

### 1. Filesystem + privilege separation — [`sandbox.md`](./sandbox.md)

- Host home **is not** bind-mounted into the VM.
- Mounts that cross the boundary: `~/.agent-orchestrator/`,
  `~/.config/fleet/` (read-only), plus the project source repos
  auto-derived from `projects.*.path` (writable, ACL'd) and local
  plugin paths (read-only).
- The VM runs two users. `lima` (uid 1000, has sudo) is fleet's
  host-side admin point. `aoworker` (uid 2000, no sudo) runs AO and
  every agent process. A compromised worker can't escalate inside
  the VM.

**What it protects against:** an agent reading or writing host
`~/.ssh`, `~/.gnupg`, `~/.config/gh`, host `~/.claude`, browser
profile data, password-manager state, or any other host directory.

**What it doesn't:** other workers in the same VM (they share the
lima user's $HOME), kernel escapes (Lima is a QEMU VM, not a more
restrictive runtime), nested sandbox escapes.

### 2. Identity — [`auth.md`](./auth.md)

- Claude OAuth, gh tokens, and the worker `.gitconfig` are resolved
  on the host at `fleet start` time and forwarded into the VM as env
  vars.
- Bootstrap script materializes them into VM-private files in the
  lima user's `$HOME`.

**What it protects against:** the host's `claude` / `gh` / `git` state
being silently mutated by what fleet does inside the VM. Token rotation
on the host is fully under the host user's control.

**What it doesn't:** intra-VM secrecy. `GH_TOKEN` is visible to every
worker process inside the VM; the Claude credentials file is readable
by the lima user. Single-tenant VMs are fine; multi-worker setups
should consider a scoped `[secrets.gh_token]` PAT.

### 3. Network — [`network.md`](./network.md)

- `[network] mode = "allowlist"` routes worker outbound HTTPS through
  an in-VM tinyproxy.
- The allowlist is built-in baseline (Anthropic, GitHub, npm, Ubuntu
  apt, NodeSource, git-bug) ∪ user's `extra_allow`.
- The same list is rendered into the worker AGENTS.md.

**What it protects against:** an agent accidentally fetching from an
unintended CDN, registry, or third-party host. A clear "blocked,
ask the user" path instead of a 30-second timeout.

**What it doesn't:** an adversary actively trying to exfiltrate. The
proxy is cooperative — `unset HTTPS_PROXY` bypasses it. Kernel-level
enforcement (nftables egress block) is roadmap'd as v2.

## Explicit non-goals

These are NOT what fleet's security model promises today. Calling them
out so users can build their own layered controls if they need them:

- **Multi-tenant isolation between workers.** All AO workers inside
  one `fleet-vm` share the `aoworker` UID. fleet does not isolate
  workers from each other.
- **Anti-exfiltration against a malicious agent.** The network
  allowlist is cooperative, not enforcing.
- **Detection / auditing.** fleet does not run an in-VM intrusion
  detection layer, network capture, or filesystem audit log.
- **Defense against host compromise.** The whole sandbox model
  assumes the host the user is sitting at is theirs and not already
  compromised. fleet protects the host from the agents, not the
  agents from the host.
- **A second `fleet-vm`.** Today fleet pins to one Lima instance named
  `fleet-vm`. Multiple parallel sandboxes per host aren't supported.

## Roadmap

Tracked in the repo's issue tracker; the broad themes:

- **Narrow lima's sudo** — replace lima's broad passwordless sudo
  with a per-command allowlist (apt-get, install /usr/local/bin/...,
  setfacl, systemctl reload tinyproxy, `sudo -u aoworker` for the
  bash/tmux entries already permitted). A compromised lima could
  still run those commands but couldn't pivot to arbitrary root.
- **Kernel-level egress enforcement** (`network.md` v2) — nftables
  ruleset forcing all outbound traffic through tinyproxy (or dropping
  it), so `unset HTTPS_PROXY` no longer bypasses the allowlist.
- **In-VM worker isolation** — give each AO worker its own UID under
  a dedicated namespace, so workers can't read each other's $HOME
  inside the VM.
- **Optional rootless-container layer for tools** — `apt install`,
  `npm install` for agents could run inside a rootless container
  rather than as the lima user, reducing the "if a tool drops a
  setuid binary into /tmp" surface area.

If you need stricter guarantees today than fleet provides, layer your
own controls outside the VM (host firewall, separate dedicated user
account, dedicated machine).
