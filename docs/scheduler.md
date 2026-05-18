# Scheduled, loop-driven workflows

Fleet's scheduler fires workflows on an elapsed-interval cadence,
independent of tracker issues. Combined with the `pr-list bind: per`
node kind, it mints one independent session per candidate pull request
each tick — the building block for "fix any PR whose CI is failing"
loops.

The scheduler is opt-in (`fleet scheduler enable`) and driveable from
cron via `fleet scheduler tick --once`. Same engine the TUI will
eventually surface, but the CLI path is the source of truth.

## Quick start

Drop a workflow with `loop:` under `.fleet/workflows/`:

```yaml
# .fleet/workflows/ci-fix-loop.yaml
name: ci-fix-loop
description: Hourly sweep — fix failing CI on open PRs
trigger:
  manual: true       # also runnable on demand via `fleet workflow run`
  issueless: true    # no tracker ticket required
loop: 1h             # humantime duration; 60s minimum
on_overlap: skip     # skip / run; default skip
nodes:
  - id: pick
    type: pr-list
    filter:
      ci_status: failure
      state: open
    bind: per          # one session per PR
  - id: probe
    depends_on: [pick]
    type: pr-checks
    outputs:
      has_failures: has_failures
  - id: fix
    depends_on: [probe]
    when: 'probe.has_failures == "true"'
    agent: claude-code
    persona: ci-doctor
    prompt_file: .fleet/prompts/ci-doctor.md
  - id: announce
    depends_on: [fix]
    type: pr-comment
    body: "fleet ci-fix-loop pushed a fix attempt; see commit ${FLEET_PR_HEAD_SHA}."
```

Enable the scheduler and either run it once or wire it into cron:

```sh
fleet scheduler enable
fleet scheduler tick --once

# crontab(5): every 10 minutes, log to a per-day file
*/10 * * * *  cd /path/to/repo && fleet scheduler tick --once >> /tmp/fleet-scheduler.log 2>&1
```

## Workflow YAML reference

### `loop:` (top-level)

A [humantime](https://docs.rs/humantime/) duration string. Examples:
`1h`, `30m`, `6h30m`, `90s`.

- **Minimum**: 60 seconds. Anything shorter is rejected at parse time
  with a clear error — the scheduler's debounce isn't fine-grained
  enough to fire sub-minute intervals without hot-spinning.
- **Mutual exclusion**: a workflow cannot set both `loop:` and
  `trigger.autonomous: true`. The two engines would each claim the
  workflow on every tick. Parse-time error.

### `on_overlap:` (top-level)

Policy when a scheduler tick is ready to fire but a session of the
same workflow is already in `Running` / `AwaitingGate`:

- `skip` (default) — defer to the next tick; don't advance
  `last_run_at`. The safe choice — a stalled session can't trigger a
  runaway slate.
- `run` — spawn anyway. Reasonable for workflows whose candidate
  enumeration is naturally disjoint (e.g. each tick mints sessions
  bound to PRs that weren't seen on the previous tick).

### PR-aware node kinds

Four new `type:` values, all host-side (run on the host, not in the
agent's container — same model as `bash` and `tracker-create`):

#### `pr-list`

Enumerate pull requests via the configured [`CodeHost`](#code_host).

```yaml
- id: pick
  type: pr-list
  filter:
    ci_status: failure    # success | failure | pending | skipped
    state: open           # open | closed | merged (default open)
    labels: [needs-ci-fix] # OR-match: at least one label required
  bind: per               # optional; when set, scheduler consumes
```

- Without `bind:` — runs inside the current session and emits
  outputs:
  - `count` — number of PRs matched (scalar).
  - `prs` — JSON-stringified `Vec<PrSummary>` for downstream parsing.
- With `bind: per` — must be the workflow's *first* node (validator
  enforces no `depends_on:`). The scheduler consumes this node before
  any session runs: it calls the code host's `list_prs`, mints one
  independent session per result, and each minted session resumes the
  workflow from the *next* node with a [`PrContext`](#fleet_pr_-env)
  bound. The PR-list node itself never runs inside a minted session.

#### `pr-checks`

Read the CI check rollup for a PR via the code host.

```yaml
- id: probe
  type: pr-checks
  pr: '${pick.prs.0.number}'   # optional: numeric literal | dotted ref | default
```

`pr:` resolution order: literal `"42"` → dotted `<node>.<output>` →
session's bound PR.

Outputs (so downstream `when:` predicates branch on scalars):

- `has_failures` (`"true"` / `"false"`)
- `failed_count` (integer string)
- `pending_count` (integer string)
- `checks` (JSON-stringified `Vec<Check>`)

#### `create-pr`

Open a pull request via the code host. Supersedes the legacy
`bash 'gh pr create …'` escape hatch in `standard.yaml`.

```yaml
- id: open_pr
  depends_on: [review]
  when: 'review.decision == "approve"'
  type: create-pr
  title: '${FLEET_ISSUE_TITLE:-fleet change}'
  body: 'Automated PR for issue ${FLEET_ISSUE_HUMAN_ID:-?}'
  base: main             # optional; default: remote HEAD via `git symbolic-ref`
  head: feat/x           # optional; default: current worktree branch
  draft: false           # default false
```

Outputs: `number`, `url`, `head_sha`.

#### `pr-comment`

Post a comment on a PR.

```yaml
- id: announce
  type: pr-comment
  body: "fleet pushed a fix"
  pr: '${probe.number}'  # optional; same resolution as pr-checks
```

No outputs.

### `FLEET_PR_*` env

Sessions bound to a PR (via `--pr <n>` or the scheduler's
`bind: per` fanout) expose these vars to every `agent:` and `bash:`
node:

| Env var               | Description                              |
|-----------------------|------------------------------------------|
| `FLEET_PR_NUMBER`     | Numeric PR number (e.g. `42`)            |
| `FLEET_PR_HUMAN_ID`   | `pr:<number>` (e.g. `pr:42`)             |
| `FLEET_PR_TITLE`      | PR title at bind time                    |
| `FLEET_PR_HEAD_REF`   | PR's head branch on the remote           |
| `FLEET_PR_HEAD_SHA`   | Commit SHA at the PR's head at bind time |
| `FLEET_PR_BASE_REF`   | PR's base branch on the remote           |
| `FLEET_PR_URL`        | PR URL                                   |

Coexists with `FLEET_ISSUE_*` — a session can be bound to both
(`--pr 42 --issue 17`), to either alone, or to neither.

## `.fleet/config.yaml` — `code_host:`

PRs are a separate backend from tracker issues. A repo with
`tracker: git-bug` can still have `code_host: github`.

```yaml
code_host: auto   # auto | github (default: auto)
```

- `auto` runs `git remote get-url origin` and matches the host. Today
  recognises `github.com` (HTTPS, SSH-scp form `git@github.com:…`,
  `ssh://git@github.com/…`). Unknown remote → no code host configured,
  PR-aware nodes fail loudly.
- `github` skips auto-detection and uses the GitHub host directly.

Shells `gh` host-side; auth is the operator's `gh auth login`. Fleet
does not manage GitHub credentials.

## Worktrees for PR-bound sessions

When a session has a `PrContext`, fleet provisions its worktree
differently from the usual `fleet/session-<id>` cut-from-HEAD shape:

1. `git fetch origin +refs/pull/<n>/head:fleet/pr-<n>` brings the PR's
   head ref into a local branch.
2. The session branch (`fleet/session-<short-id>`) is cut from
   `fleet/pr-<n>`.
3. Commits made by the agent land on the session branch. Pushing them
   to the PR requires an explicit `git push origin HEAD:<head_ref>`
   from the workflow (kept explicit so the local PR snapshot stays
   read-only by default).

Caveats:

- **Non-git workspaces** are rejected for `--pr` runs (we won't share
  the host tree for a PR fix).
- **Force-pushes between ticks** can produce a non-fast-forward `git
  push` for sessions still working on the old `fleet/pr-<n>` snapshot.
  `on_overlap: skip` mitigates by keeping the still-running session
  as the owner.
- **`fleet/pr-*` refs accumulate** — one per scheduled fanout per PR.
  A future `fleet sessions prune --pr-refs` will reclaim them; for now
  `git for-each-ref refs/heads/fleet/pr-* | git update-ref -d` does it
  manually.

## `fleet-pr` — in-container PR reads

When the bridge is up and the host's `fleet-pr` binary lives next to
`fleet`, agents inside the sandbox can call:

```sh
fleet-pr read [--number <n>]                # PrDetail JSON
fleet-pr checks [--number <n>]              # ChecksSummary JSON
fleet-pr check-logs <name> [--number <n>]   # failed-step logs
```

All three default to the session's bound PR when `--number` is
omitted. Read-only by design — writes (open PR, comment) flow through
workflow nodes, not the bridge.

The host's `gh auth` token stays on the host. The agent never sees a
GitHub credential.

## CLI reference

| Command                       | Effect                                          |
|-------------------------------|-------------------------------------------------|
| `fleet scheduler enable`      | Creates `.fleet/scheduler.enabled`. Idempotent. |
| `fleet scheduler disable`     | Removes the flag file. Idempotent.              |
| `fleet scheduler status`      | Lists every `loop:` workflow with last-run + next-due. Read-only. |
| `fleet scheduler tick --once` | Runs one tick. No-op when disabled. Exits non-zero if any spawn failed. |
| `fleet workflow run <name> --pr <n>` | Run a workflow against PR `<n>`. Worktree is provisioned from the PR's head. |

## Operational notes

- **Single driver assumption.** The scheduler-state file is written
  atomically (tmp + rename) so a partial write can't corrupt it, but
  it's last-writer-wins. Don't run `fleet scheduler tick --once` and
  a TUI-driven tick path against the same `.fleet/` simultaneously.
- **Empty PR-list still advances the clock.** If `list_prs` returns
  zero candidates, the scheduler still marks the workflow as run for
  this tick — otherwise it would re-query an empty result every tick
  until something matches.
- **Code-host errors don't advance the clock.** `gh: not authenticated`
  or transient API failures leave `last_run_at` untouched so the next
  tick retries.
- **Overlap skip does not advance the clock either.** A stalled
  session won't burn through subsequent intervals; the workflow gets
  a fresh shot at the next tick once the prior session terminates.
