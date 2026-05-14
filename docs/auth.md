# Identity brokering

This page covers the **identity-material** boundary: how Claude OAuth,
`gh` tokens, and the worker `.gitconfig` reach the VM now that the host
home is no longer bind-mounted. See [`sandbox.md`](./sandbox.md) for the
filesystem story this depends on, and [`network.md`](./network.md) for
egress control.

## Why brokering at all

Before Phase 1, `gh`/`git`/`claude` inside the VM read host-side files
transparently via the `~` bind-mount. After Phase 1, the lima user's
`$HOME` inside the VM is on private ext4 — `~/.config/gh/`,
`~/.gitconfig`, and `~/.claude/` are completely absent unless fleet
actively puts them there.

The fix: **resolve identity material on the host immediately before
`fleet start`, forward it through Lima as env vars, and have the
bootstrap script materialize it into VM-private files.** Every brokered
value follows the same pipeline:

```
host secret backend
   ↓ resolve at fleet start
process env var on `limactl shell`
   ↓ --preserve-env + LIMA_SHELLENV_ALLOW
guest bash env
   ↓ bootstrap script reads, writes a 600-mode file in $HOME, unsets the var
worker process env / dotfile in $HOME
```

## What fleet brokers

| What                    | Env var (host → guest)   | Where it lands inside VM           |
| ----------------------- | ------------------------ | ---------------------------------- |
| Claude OAuth token      | `CLAUDE_CODE_OAUTH_TOKEN` | `~/.claude/.credentials.json`     |
| gh CLI token            | `GH_TOKEN`               | (stays in env — `gh` reads it)     |
| Worker git identity     | `FLEET_GITCONFIG_B64`    | `~/.gitconfig` (decoded from base64) |

## Claude OAuth

Resolution: `[secrets.claude_code_oauth_token]` in
`~/.config/fleet/config.toml` (env / keychain / 1Password).

Inside the VM the bootstrap script writes a `chmod 600` credentials file
in the now-VM-private `~/.claude/`, then `unset CLAUDE_CODE_OAUTH_TOKEN`
so claude takes the file-based auth path (env-token mode shows the login
menu interactively in claude 2.1.139). The same write also stamps
`hasCompletedOnboarding: true` and `skipDangerousModePermissionPrompt:
true` into the VM's claude config — host claude is no longer mutated as
a side effect.

## gh CLI

Two resolution paths, in order:

1. **`[secrets.gh_token]`** in `~/.config/fleet/config.toml`. Use this
   when you want agents to act with a separate identity from yours
   (recommended for any setup beyond personal projects):
   ```toml
   [secrets.gh_token]
   backend = "op"
   ref = "op://Employee/Fleet Agent GitHub Token/credential"
   ```
2. **`gh auth token` on the host**, run by fleet during `fleet start`.
   Lazy default for the personal-use case: you already ran `gh auth
   login` on the host, fleet just relays the same token to the VM.

The forwarded value lives in the worker shell as `GH_TOKEN`. `gh`
prefers that over any on-disk config, so `gh issue list`, `gh pr
create`, etc. work without an in-VM `gh auth login`. Tmux propagates
the env into AO's worker panes, so the value persists for the lifetime
of the AO server.

Neither resolution required: if both paths fail (no fleet secret AND
host `gh` not logged in), `GH_TOKEN` is simply absent. `gh` inside the
VM will fail with its own clear "not authenticated" message when called.

## Git identity

Resolution, in order:

1. **`[git]`** in `~/.config/fleet/config.toml`:
   ```toml
   [git]
   user_name  = "Worker Bot"
   user_email = "bot@example.com"
   ```
2. **The host's own `~/.gitconfig`** — `user.name` and `user.email` are
   read out of the `[user]` section. Signing keys and any other section
   are intentionally not forwarded (they typically reference host-side
   paths or a host gpg-agent that doesn't exist inside the VM).
3. **Skipped.** Workers will fail to commit until you set one of the
   above; git itself surfaces a clear "Please tell me who you are" error.

The resolved content is base64-encoded into `FLEET_GITCONFIG_B64` (so
multi-line INI survives the env-var round trip), decoded by the
bootstrap script into `$HOME/.gitconfig`, and the env var is `unset` so
it doesn't leak into worker env dumps.

To override per-worker: add a project-level `postCreate` hook in
`agent-orchestrator.yaml` that `git config --local user.email` against
the worktree. The brokered `~/.gitconfig` is global; project-specific
identities should override locally rather than mutating it.

## Threats and limits

- **`GH_TOKEN` is visible to every process inside the VM.** A
  compromised worker can `cat /proc/<pid>/environ` and read the token.
  Inside a single-tenant VM with one worker that's expected; for
  multi-worker setups consider rotating to a scoped fleet agent PAT
  via `[secrets.gh_token]` so the host token isn't on the line.
- **Claude credentials file is VM-private but readable by the lima
  user.** Any worker can read `~/.claude/.credentials.json`. Same
  trust boundary as `GH_TOKEN`.
- **Resolution happens at `fleet start` time, not per spawn.** If you
  rotate a token on the host, restart the fleet AO stack (Shift+X →
  Shift+S in the TUI, or `fleet stop && fleet start`) to refresh the
  brokered value.
- **Signing keys are intentionally not forwarded.** If you need GPG/SSH
  commit signing inside the VM, configure it manually — fleet won't
  silently mis-sign agent commits with the host user's identity.
