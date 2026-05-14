# Filesystem sandbox

This page covers the **filesystem-isolation** boundary fleet enforces around
AO workers. The companion documents are [`auth.md`](./auth.md) for identity
brokering and [`network.md`](./network.md) for egress control.

## What's mounted

Fleet's Lima template (`templates/fleet-vm.yaml`) bind-mounts exactly two
host paths into the `fleet-vm` guest:

| Host path              | Inside VM            | Mode           | Purpose                                     |
| ---------------------- | -------------------- | -------------- | ------------------------------------------- |
| `~/.agent-orchestrator` | `~/.agent-orchestrator` | **writable**   | AO worktrees + session state                |
| `~/.config/fleet`      | `~/.config/fleet`    | **read-only**  | AO catalog (`agent-orchestrator.yaml`) + worker AGENTS.md |

Everything else under the lima user's `$HOME` inside the VM lives on the
VM's private ext4 disk and never touches the host filesystem. That
includes:

- `~/.claude/` (OAuth credentials, settings)
- `~/.gitconfig` (materialized at `fleet start` time from `[git]` in
  `~/.config/fleet/config.toml` or the host's own `~/.gitconfig`)
- `~/.config/gh/` (gh CLI auth — see `auth.md`)
- `~/.local/share/gh/extensions/` (gh extensions like `gh-dash`)
- `~/.cache/`, `~/.npm/`, `~/.cargo/`, …

## What this protects against

- An AO worker cannot read host `~/.ssh`, `~/.gnupg`, browser profile data,
  password-manager state, or anything else in the host's `$HOME` outside
  the two mount points above.
- An AO worker cannot write into the host's `~/.config/fleet/` directory.
  Specifically, it cannot append a `projects:` entry to
  `agent-orchestrator.yaml` that points at an arbitrary host path and
  thereby get a "legitimate" worktree there on the next spawn.
- The host's own `claude` CLI is unaffected by what fleet does inside the
  VM — VM-side mutations to `~/.claude/.credentials.json`, `~/.claude.json`,
  or `~/.claude/settings.json` (e.g. `skipDangerousModePermissionPrompt`)
  land on the VM's private disk, never on the host.

## What it does NOT protect against

- **Other workers in the same VM.** If you run multiple AO sessions
  concurrently, they share the lima user's `$HOME` inside the VM. fleet
  does not isolate workers from each other.
- **Network egress.** See [`network.md`](./network.md) for the (opt-in)
  egress allow-list. Phase 1 leaves the VM with unrestricted outbound
  internet by default.
- **The Lima kernel boundary itself.** Lima is a QEMU-backed VM; a kernel
  escape would break out. fleet does not add nested sandboxing inside the
  guest (no seccomp, no AppArmor profile, no rootless container layer).

## Sudo lockdown

Lima's cloud-init grants the `lima` user passwordless sudo by default.
The template's final provisioning step (after npm + apt installs are
done) removes that:

```bash
# templates/fleet-vm.yaml — last `mode: system` block
rm -f /etc/sudoers.d/*lima* /etc/sudoers.d/99_nopasswd_sudo
gpasswd -d lima sudo  || true
gpasswd -d lima admin || true
```

Once the lockdown step has run, the `lima` user has no sudo. Host-side
admin actions fleet still needs (e.g. installing missing tracker tools via
the preflight modal) go through `limactl shell --user root` rather than
relying on the worker user's sudo rights — see
[`src/tui/preflight.rs`](../src/tui/preflight.rs)'s `install_command`.

## Upgrading an existing VM

The mount layout above is enforced by the **template**. Lima copies the
resolved template into `~/.lima/fleet-vm/lima.yaml` at first start and
ignores subsequent template edits — so a VM created before this change
keeps its old wide mount until it's rebuilt.

Fleet's preflight detects this:

```
✗ fleet-vm has a stale mount layout

  The running VM was created from an older fleet template
  whose `mounts:` block bind-mounted your whole host home
  writable into the guest. …
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
