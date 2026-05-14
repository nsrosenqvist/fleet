# Filesystem sandbox

This page covers the **filesystem-isolation** boundary fleet enforces around
AO workers. The companion documents are [`auth.md`](./auth.md) for identity
brokering and [`network.md`](./network.md) for egress control.

## Two users in the VM

The `fleet-vm` guest runs two distinct users:

- **`lima`** (UID 1000) — fleet's host-side admin point. Has passwordless
  sudo via Lima's default cloud-init. Used for `tmux attach` wrapping,
  tinyproxy reloads, tracker-tool installs, and the ACL sync that lets
  aoworker write to project mounts. Does **not** run AO or any agent
  process.
- **`aoworker`** (UID 2000, primary group `aousers`) — runs AO's
  orchestrator, every worker tmux pane, and every agent child process.
  Has **no sudo** and no special group membership. Compromising it
  bounds the blast radius to aoworker's own files + the bind-mounted
  worktrees.

Both users are in the `aousers` group (lima as a supplementary), and
the AO worktree mount has a default ACL granting that group rwx, so the
TUI's `Enter`-to-attach `cd` flow (which lands as lima) can still walk
the worktree before exec'ing `sudo -u aoworker tmux attach`.

## What's mounted

Fleet's Lima template (`templates/fleet-vm.yaml`) bind-mounts the
following host paths:

| Host path              | Inside VM                            | Mode           | Purpose                                     |
| ---------------------- | ------------------------------------ | -------------- | ------------------------------------------- |
| `~/.agent-orchestrator` | `/home/aoworker/.agent-orchestrator` | **writable**   | AO worktrees + session state (matches aoworker's `$HOME` so AO's `homedir()` resolution lands here) |
| `~/.config/fleet`      | `~/.config/fleet`                    | **read-only**  | AO catalog (`agent-orchestrator.yaml`) + worker AGENTS.md |
| project source repos   | (same absolute path)                 | **writable**   | Auto-derived from `projects.*.path` in `agent-orchestrator.yaml`. Required so AO's worktree plugin can `git worktree add` against the host repo's `.git/`. ACL-granted to `aoworker` at `fleet start`. |
| local plugin paths     | (same absolute path)                 | **read-only**  | Auto-derived from `plugins[].path` when `source: local`. Loaded by AO at start. |

Everything else under aoworker's `$HOME` inside the VM lives on the
VM's private ext4 disk and never touches the host filesystem. That
includes:

- `~/.claude/` (OAuth credentials, settings)
- `~/.gitconfig` (materialized at `fleet start` time from `[git]` in
  `~/.config/fleet/config.toml` or the host's own `~/.gitconfig`)
- `~/.config/gh/` (gh CLI auth — see `auth.md`)
- `~/.local/share/gh/extensions/` (gh extensions like `gh-dash`)
- `~/.cache/`, `~/.npm/`, `~/.cargo/`, …

The project-mounts list is rebuilt by fleet on each VM bring-up; adding
a new `projects.*.path` to `agent-orchestrator.yaml` requires a VM
rebuild (Lima's mounts are static — see "Upgrading an existing VM"
below). Preflight detects projects listed in the AO catalog whose path
isn't in the running VM's mounts and surfaces the "rebuild required"
modal.

## Customizing the VM environment

The embedded template ships Node, `gh`, `git-bug`, and a few other
basics — but no language toolchains. If you want an agent inside the VM
to run `go build`, `cargo test`, or anything else that needs an extra
package, drop a script at `~/.config/fleet/provision.sh` on the host:

```sh
#!/usr/bin/env bash
set -euo pipefail
apt-get install -y golang-go
```

The file reaches the VM via the read-only mount above. On every boot,
a `provision:` block hashes the script and runs it as root if the hash
hasn't been seen before. That means:

- **First boot after dropping the script** → it runs.
- **Edit and reboot** → it runs again (new hash, new marker).
- **Reboot with no edits** → no-op (marker matches).
- **`limactl delete fleet-vm` and recreate** → it runs again, because
  markers live on the VM's private disk and recreation wipes them.

To re-trigger after editing while the VM is running:

```sh
limactl stop  fleet-vm
limactl start fleet-vm
```

Output goes to `/var/log/fleet/user-provision.log` inside the VM —
`limactl shell fleet-vm sudo cat /var/log/fleet/user-provision.log`
from the host. If the script exits non-zero, no marker is written
and the next boot retries; fix and reboot.

**Trust caveat.** This hook runs as root with no `HTTPS_PROXY` set,
so it bypasses the egress allowlist documented in
[`network.md`](./network.md) by design. Anything it installs is
whatever you trust the source (apt mirrors, `curl | sh`, etc.) to
ship. Agents running inside the VM go through `HTTPS_PROXY` and remain
subject to the allowlist; this hook is the one place you, the
operator, get to step outside it.

## What this protects against

- An AO worker (running as `aoworker`) cannot read host `~/.ssh`,
  `~/.gnupg`, browser profile data, password-manager state, or any
  unrelated host directory outside the mount table above.
- An AO worker cannot write into the host's `~/.config/fleet/` directory.
  Specifically, it cannot append a `projects:` entry to
  `agent-orchestrator.yaml` that points at an arbitrary host path and
  thereby get a "legitimate" worktree there on the next spawn.
- An AO worker has **no sudo** inside the VM. It cannot `apt install`
  things, edit `/etc`, kill the tinyproxy systemd unit, or read other
  users' files via `sudo cat`. The `lima` user retains sudo (necessary
  for fleet's host-side admin path; see "Two users in the VM" above)
  but is not where agent code runs.
- The host's own `claude` CLI is unaffected by what fleet does inside the
  VM — VM-side mutations to `~/.claude/.credentials.json`, `~/.claude.json`,
  or `~/.claude/settings.json` (e.g. `skipDangerousModePermissionPrompt`)
  land on the VM's private disk, never on the host.

## What it does NOT protect against

- **Other workers in the same VM.** All AO sessions inside one
  `fleet-vm` share the `aoworker` UID. fleet does not isolate workers
  from each other.
- **Network egress.** See [`network.md`](./network.md) for the (opt-in)
  egress allow-list. Phase 1 leaves the VM with unrestricted outbound
  internet by default.
- **The Lima kernel boundary itself.** Lima is a QEMU-backed VM; a kernel
  escape would break out. fleet does not add nested sandboxing inside the
  guest (no seccomp, no AppArmor profile, no rootless container layer).
- **A compromised `lima`.** lima keeps broad passwordless sudo. fleet's
  threat model treats lima as host-side admin infrastructure (its only
  actions are short, fleet-driven scripts that touch `/etc/tinyproxy/`,
  install tracker binaries, and shell-out to aoworker). If someone
  pivots into a lima shell via a different channel they have root
  inside the VM. Narrowing lima's sudo to per-command allowlists is
  tracked in the roadmap.

## Upgrading an existing VM

The mount layout above is enforced by the **template**. Lima copies the
resolved template into `~/.lima/fleet-vm/lima.yaml` at first start and
ignores subsequent template edits — so a VM created before this change
keeps its old wide mount until it's rebuilt.

Fleet's preflight detects two reasons a VM needs rebuilding:

1. **Pre-Phase-1 layout** — the running VM still has the host home
   bind-mounted writable. Every dotfile is exposed.
2. **Project missing from mount list** — a `projects.*.path` was added
   to `agent-orchestrator.yaml` after the VM was created, so AO can't
   reach the host repo's `.git/`.

Both fail with:

```
✗ fleet-vm has a stale mount layout

  The running VM was created from an older fleet template
  whose `mounts:` block doesn't match the current set. …
```

To migrate:

```sh
limactl stop  fleet-vm
limactl delete fleet-vm
fleet ui   # re-creates from the bundled template
```

`~/.agent-orchestrator/` lives on the host side already, so existing
worktrees survive the rebuild. Anything written into the lima user's
`$HOME` from inside the old VM (npm caches, gh extensions, etc.) is on
the deleted VM disk and won't be carried over.
