# Orchestration: plans, blocking, and the brainstorm agent

This page covers how fleet sequences work across multiple sessions, how
agents signal they can't proceed, how new tickets get filed in response,
and the interactive brainstorm surface for planning.

If you're looking for individual-session mechanics, see
[`worktrees.md`](./worktrees.md). If you're looking for how the agent
gets permission to mutate the tracker, see
[`auth.md`](./auth.md#per-tracker-auth).

> **Status.** All five phases have landed. The TUI's Shift+B /
> brainstorm-row Enter affordances remain deferred until the
> terminal-suspend dance gets wired (the underlying
> `fleet brainstorm` CLI works standalone); the table at the
> bottom of this page tracks what's actually in the binary.

## The model in 30 seconds

- An **agent** inside a workflow session can read, comment on, and
  transition status on **its bound ticket only** (the one the workflow
  is acting on). It does this via a small `fleet-tracker` CLI in the
  container that talks to fleet on the host.
- An agent can declare its **outcome**: `progress`, `blocked`, or
  `done`. A blocked outcome can include a recommendation for a new
  ticket the agent would need to unblock.
- A **`tracker-create` workflow node** turns recommendations into real
  tickets, posts cross-link comments, and records the dependency.
- A **plan** is a named, ordered list of tickets fleet's autonomous
  supervisor walks through in sequence. Plans encode *preference*;
  the blocked-on graph encodes *requirement*. They compose.
- The **brainstorm agent** (`fleet brainstorm`) is an attachable
  host-side agent session for planning, filing tickets in bulk, and
  managing plans. It's the interactive surface — workflows are the
  execution surface.

## Outcomes: how a workflow knows what just happened

Agent nodes in a workflow declare an outcome in their `outputs:`
block. The agent writes the matching keys into
`<node>.outputs.json` under `/artifacts`:

```yaml
- id: implement
  agent: claude-code
  prompt_file: prompts/implementer.md
  outputs:
    outcome: outcome
    blocker: blocker
    recommend_ticket: recommend_ticket
```

The output declarations are flat `local-name: source-key` pairs — the
executor reads `<node>.outputs.json`, looks up each `source-key` at
the top level, and stores the value under `(<node>, <local-name>)` in
the in-memory `OutputMap`. Scalars (string/bool/number) round-trip
verbatim; objects and arrays are JSON-encoded as a single string so
structured payloads like `recommend_ticket` can flow through; JSON
`null` maps to the empty string so the workflow author can use
`recommend_ticket != ""` as a presence check.

Validation runs immediately after extraction:

- When a node declares `outputs.outcome`, the value must be one of
  `"progress"`, `"blocked"`, or `"done"` (lowercase, exact). Anything
  else fails the node with a clear "got `<bad>`" message.
- When `outcome == "blocked"` AND the node also declared `blocker`
  in its outputs, the blocker must be non-empty (whitespace-trimmed).
  Workflows that omit `blocker` aren't held to this — they've opted
  out of capturing the reason.

The convention is opt-in. Nodes that don't declare `outcome` get the
pre-convention "node succeeded / failed" semantics, unchanged.

Workflows branch on the outcome. The default `standard.yaml` shape
(shipped by `fleet init`):

```yaml
- id: implement
  agent: claude-code
  outputs:
    outcome: outcome
    blocker: blocker
    recommend_ticket: recommend_ticket

- id: review
  depends_on: [implement]
  agent: claude-code
  outputs: { decision: decision }

- id: revise
  depends_on: [review]
  when: 'review.decision == "changes_requested"'
  agent: claude-code
  loop_back_to: review
  max_loops: 2

- id: open_pr
  depends_on: [review]
  when: 'review.decision == "approve"'
  type: bash
  script: 'gh pr create ...'

- id: file-dep
  depends_on: [implement]
  when: 'implement.outcome == "blocked" && implement.recommend_ticket != ""'
  type: tracker-create
  from: implement.recommend_ticket
  link_parent: true
```

A blocked outcome routes to the `file-dep` branch: file a follow-up
ticket via `tracker-create`, cross-link both tickets, record the
dependency. The review / open_pr / revise branches still run — the
reviewer's prompt (in `.fleet/prompts/reviewer.md`) is asked to
write `decision: "noop"` when there's no real diff to review,
which cleanly skips both forks. A future short-circuit fix to
`when:` evaluation will let `review` be gated on
`implement.outcome != "blocked"` directly; today the workaround
keeps the flow predictable without it.

The agent contract is documented in `.fleet/prompts/implementer.md`
(also shipped by `fleet init`). Required JSON shape:

```json
{
  "outcome": "progress" | "blocked" | "done",
  "summary": "<one line>",
  "blocker": null OR "<why you're stuck>",
  "recommend_ticket": null OR {
    "title": "<short>",
    "body":  "<paragraph>",
    "labels": ["..."]
  }
}
```

Every declared key must be present — use `null` to mean "doesn't
apply this run."

## The bridge CLI

Inside the workflow container, the agent has access to a binary called
`fleet-tracker`. It speaks to fleet on the host over a localhost HTTP
bridge with a per-session bearer token. The full surface:

```
fleet-tracker comment <body>
fleet-tracker status <open|in-progress|closed>
fleet-tracker add-label <label>
fleet-tracker remove-label <label>
fleet-tracker read            # the bound ticket
fleet-tracker read <id>       # any ticket (context only)
fleet-tracker list            # all open
```

**Writes are scoped** to the session's bound ticket. The bridge rejects
write requests targeting any other id with a 403. Reads are unscoped
so the agent can look at related tickets for context.

Credentials stay on the host. `gh`'s OAuth state, your Linear API
token, whatever else — none of it reaches the container. The agent has
a fleet-issued bearer that only authenticates against the bridge.

See [`auth.md`](./auth.md#how-a-secret-gets-to-the-agent) for the broader
secrets story.

**How `fleet-tracker` reaches the container** (v1 limitation): the
binary is bind-mounted read-only at `/usr/local/bin/fleet-tracker`
from the fleet host's own install location. The mount fires only when:

- The host is Linux. macOS fleet builds produce a Mach-O binary the
  container's Linux loader can't exec, so the mount is skipped and
  `fleet-tracker` calls inside the container fail with a clear
  "command not found" instead of a cryptic "exec format error". The
  bridge HTTP endpoint still works — agents that hit it directly via
  curl continue to function.
- The `fleet-tracker` binary is installed alongside `fleet` (e.g.
  `cargo install --path . --bin fleet --bin fleet-tracker`, or both
  binaries are dropped together by your packaging). Installations
  that only ship `fleet` skip the mount.

Cross-arch is handled: fleet inspects the container image's arch
via the engine's `image inspect` and skips the bind-mount when it
doesn't match the host's. The bridge HTTP endpoint stays reachable
either way, so curl-based agents still work.

## Recommending new tickets

When an agent's `outcome` is `blocked` and it would need a new ticket
to unblock, it includes a `recommend_ticket` in its result. The
workflow's `tracker-create` node turns the recommendation into reality:

1. Reads `recommend_ticket` from upstream outputs (the JSON-encoded
   `{title, body, labels?}` object that flowed through extraction).
2. Calls fleet's host-side tracker → creates the issue.
3. When `link_parent: true` (the default) and the session is bound
   to a ticket: posts cross-link comments on both the parent and
   the new ticket, calls `Tracker::link_parent` for the structural
   relationship, appends a `<parent> → blocked_on: <new>` edge to
   `.fleet/deps.json` (cycle-checked first), and — when an active
   fleet plan contains the bound ticket — injects the new ticket
   into the plan immediately before its dependent.

The agent can't call `create` directly — only the workflow can, via
the `tracker-create` node. This is by design: the bridge CLI is
scoped to one ticket; creation is a different authority that lives in
the workflow definition (and, later, in the supervisor and brainstorm
agent).

A per-session cap (`max_recommended_tickets`, default 3) prevents a
runaway agent from spawning a hundred follow-ups in one session.
The cap counts distinct `tracker-create` nodes in the workflow whose
`created_id` output is already populated; same-node re-fires via
`loop_back_to` don't inflate it (they overwrite the existing entry).
Exceeding the cap fails the node with a message naming the
configured limit. Set at workflow level (`max_recommended_tickets: N`
under the top-level workflow document, alongside `name:`).

The node's emitted outputs are:

- `created_id` — fleet's opaque id for the new ticket (`gh:87`,
  git-bug hash, …).
- `created_human_id` — the user-visible short id (`87`).

Downstream nodes can branch on either via `when:` predicates.

Plan-injection is wired up today (Phase 3a): if the bound ticket
is in an active fleet plan, the new ticket gets inserted into the
plan items just before its dependent, with `injected: true` so the
TUI / CLI can flag it as "added during execution." Paused /
completed / abandoned plans are intentionally not touched.
Supervisor-side consumption (the scheduler reading these injections)
is Phase 4.

## Plans

A plan is a named, ordered list of tickets the supervisor walks
through in sequence. The data model and CLI ship today (Phase 3a);
the supervisor's plan-consumption tick lands in Phase 4.

On-disk shape (`.fleet/plans/<id>.yaml`, one file per plan,
gitignored):

```yaml
id: plan-018f7c2a3b9-0001
name: Parser refactor
state: active                       # active | paused | completed | abandoned
on_item_failure: stop               # stop | continue | retry-once
epic_ref:                           # optional; populated by the brainstorm agent (Phase 5)
  tracker: github
  id: "200"
items:
  - { ticket_id: "42", state: completed, session_id: s-abc... }
  - { ticket_id: "43", state: completed, session_id: s-def... }
  - { ticket_id: "44", state: in-progress, session_id: s-ghi... }
  - { ticket_id: "51", state: pending, injected: true }   # added by tracker-create
  - { ticket_id: "45", state: pending }
  - { ticket_id: "46", state: pending }
created_at_ms: 1747...
updated_at_ms: 1747...
```

### CLI surface

```sh
fleet plan list                            # all plans, state + progress
fleet plan show <id>                       # full plan + per-item state
fleet plan new "<name>" --tickets 42,43,44 # create active plan
fleet plan edit <id>                       # $EDITOR on the YAML
fleet plan pause <id>
fleet plan resume <id>
fleet plan complete <id>
fleet plan abandon <id> --reason "..."
fleet plan inject <id> <ticket> [--before <other>]
```

Plan ids are `plan-<13 hex ms>-<4 hex counter>` — mirrors the
session id format so lexicographic ≈ chronological.

### Restricting the supervisor by label

By default, `fleet autonomous run` considers every open ticket on the
tracker. In repos where fleet shares a tracker with humans or other
tools, set `autonomous.filter_labels` so the supervisor only picks
tickets explicitly marked as fleet-domain:

```yaml
autonomous:
  workflow: standard
  filter_labels: [fleet, agent]
```

Semantics:

- **ALL match.** A candidate must carry *every* label in the list.
  `[fleet, agent]` → drops `[fleet]` alone, drops `[docs]`, keeps
  `[fleet, agent, p1]`.
- **Symmetric stamp.** The same list is unioned onto every ticket
  fleet creates — via `fleet issues create` (called by the orchestrator
  agent) or via a `tracker-create` workflow node. A workflow-spawned
  follow-up ticket carries the labels needed to pass the gate on the
  next supervisor tick.
- **Scope is the tick + creation only.** `fleet issues list` and the
  TUI spawn modal stay unfiltered, so a human can deliberately spawn
  a workflow against a non-fleet ticket.
- **Empty list = pre-feature behaviour.** No filtering, no stamping,
  byte-for-byte.

### How plans compose with the deps graph

The supervisor's tick:

1. List active plans (oldest-first by `created_at_ms`).
2. For each plan, walk items in order; for each `Pending` item
   whose ticket isn't deps-blocked, add it to the candidate list.
3. Append any open issue not currently in any active plan as the
   "any-open-issue" fallback tail.
4. Engine picks the first unclaimed entry (respecting
   `autonomous.max_parallel`).

Tickets that don't pass `autonomous.filter_labels` are dropped before
step 1, so neither the plan walk nor the fallback considers them.

A ticket is **deps-blocked** when at least one edge in
`.fleet/deps.json` keyed by it is still active: either a
`Ticket`-reason edge whose target ticket is still in the open
set, or a `Freeform` edge (which only clears via `fleet sessions
unblock`). `Ticket` edges whose target has already closed are
auto-cleared by the scheduler — the deps store retains them for
audit, but the supervisor stops acting on them.

Plans and the blocked-on graph compose:

- A plan is `[A, B, C, D]`. The agent on B says it's blocked on a
  newly-recommended X. The `tracker-create` node files X, the plan
  becomes `[A, X, B, C, D]` (X with `injected: true`) and the
  blocked-on graph holds B back until X closes.
- Multiple active plans round-robin (oldest first). Item-level
  parallelism is bounded by `autonomous.max_parallel` globally.

### Plan item lifecycle

The supervisor reconciles each plan's item states against the
current session set at the top of every tick (and the TUI does
the same on every `r` reload). The mapping:

- `SessionState::Running` | `AwaitingGate` → `PlanItemState::InProgress`
- `SessionState::Completed` → `PlanItemState::Completed`
- `SessionState::Failed` | `Crashed` → `PlanItemState::Failed`

When multiple sessions bind the same ticket (a retry), the newest
session by `updated_at_ms` wins.

### `on_item_failure` policy

When an item transitions into `Failed`, the plan's
`on_item_failure` policy fires:

- **`stop`** (default) — plan transitions to `Paused`. The user
  decides next steps (`fleet plan resume`, edit, abandon). The
  conservative choice for chained migrations where a mid-item
  failure would corrupt later items.
- **`continue`** — item stays `Failed`; plan stays `Active`; next
  pending item gets picked up on the following tick.
- **`retry-once`** — first failure re-marks the item `Pending`
  and bumps `retry_count` to 1; the supervisor re-spawns on the
  next tick. A second failure (`retry_count >= 1`) falls back to
  `stop` semantics.

Set per-plan in the YAML under `on_item_failure:`.

### Plans vs. epics: local vs. tracker-canonical

Plans live in `.fleet/plans/<id>.yaml`, gitignored by default. They
are **local execution state**, not a contract — they describe what
the supervisor on *this* machine is doing now. Moving to a new
machine doesn't carry plans across.

The tracker's native **epic** primitive *is* the persisted truth.
GitHub uses tracking issues with task lists; Linear has actual epics;
git-bug uses a `parent:<id>` label convention. When the brainstorm
agent creates a plan, it also creates the tracker-side epic (when one
exists) and stores the link in `plan.epic_ref`.

**Cross-machine continuity routes through brainstorm.** On a new
machine:

```
$ fleet brainstorm
> read epic gh #200 and recreate it as a fleet plan
agent: [reads the tracking issue's body, parses the task list]
       [calls fleet.plan_new("Parser refactor", [42, 43, 44, 45, 46],
                              epic_ref={"github", "200"})]
       OK, plan-018f-... created with 5 items in the original order.
> enable it
agent: it's active by default. autonomous mode will pick up items on
       the next tick.
```

A deterministic `fleet plan import-from-epic <id>` CLI is a planned
follow-up; brainstorm-driven import is the primary v1 path because it
also handles "the tracker's task list has shifted since the plan was
created" gracefully — the agent reconciles.

## Unblocking when there's no ticket to wait on

Plenty of blockers don't map to tickets: a flaky CI job that fixed
itself, a third-party outage, a missing dev-container tool that
someone installed offline. The dependency map supports both kinds of
edge:

- **Ticket dependency** (`42 → blocked_on: [#43]`): auto-clears when
  `43` closes. Supervisor re-spawns `42` on the next tick.
- **Free-form blocker** (`42 → blocked_on: "free:apt-mirror"`): no
  close event. Clears via explicit human action.

Manual unblock — the session id is the argument; fleet looks up the
session's bound ticket and clears every `blocked_on` edge keyed by
that ticket:

```sh
fleet sessions unblock s-abc123
fleet sessions unblock s-abc123 --reason "apt mirror recovered"
```

The `--reason` posts a comment on the bound ticket explaining the
unblock — useful audit trail when you come back to it weeks later.
Without `--reason`, the operation is purely local (no tracker
calls), so it works in repos with `tracker: none` configured.

### Cycle detection

Cycle detection is wired into the `tracker-create` workflow node
today: before persisting a new `parent → blocked_on: child` edge,
fleet walks the existing deps graph to check whether `child` already
has a path back to `parent`. If so, the workflow node fails with a
clear "would close a cycle" message — the new ticket has been filed,
but the structural link is refused; the user resolves manually
(`fleet sessions unblock` on one side, or surgical edit of
`.fleet/deps.json`) before re-running.

In practice tracker-create's checker can only fire when a future
write path re-uses an existing id (today's child is always fresh and
has no outgoing edges); the safety net belongs at the mutation
point regardless. The helper is also available as
`crate::deps::would_create_cycle` for future manual-unblock or
plan-editing surgery. Phase 3b surfaces cycle warnings in the TUI
(sessions list `⚠ cycle: A ↔ B`, Plans view per-plan banner) —
those visual flags ship with the TUI Plans view.

## The brainstorm agent

`fleet brainstorm` opens an interactive agent session for planning.
It runs on the host, not in a container — the user is in the loop on
every tracker mutation, so the scoping that makes sense for workflow
agents would be in the way.

### What it can do

The brainstorm agent has tools for:

- Reading any ticket; listing all open issues.
- Creating tickets (single or in bulk).
- Commenting, transitioning status, labelling — any ticket, not just
  one bound ticket.
- Creating the tracker's native epic primitive (tracking issue, Linear
  epic, parent-label group).
- Creating, editing, pausing, completing fleet plans.
- Listing fleet sessions, inspecting their outcomes, asking the
  supervisor to prioritise a ticket.
- Removing dependency edges; manually unblocking sessions.

It can't reach into a running workflow container — that's the bridge
CLI's job, and the bridge is per-session-scoped by design.

### Lifecycle

```sh
fleet brainstorm                  # new session, attaches immediately
fleet brainstorm list             # active sessions
fleet brainstorm attach <id>      # reattach
fleet brainstorm kill <id>        # end session
```

Each brainstorm runs inside a tmux session named
`fleet-brainstorm-<id>` so you can detach (Ctrl-B D) and reattach
freely from any terminal. The per-session directory at
`.fleet/planning/<id>/` carries:

- `meta.json` — the `BrainstormSession` value (id, agent, state,
  tmux session name, server pid, timestamps).
- `prompt.md` — the system prompt rendered at spawn-time (static
  template + snapshot of open issues + active plans).
- `transcript.log` — *reserved*; tmux pipe-pane capture isn't
  wired in v1.

### In the TUI

The Sessions view's sidebar shows a second section listing every
known brainstorm session with state markers (◐ active, ⏸
detached, ✗ closed) — tmux is the source of truth for liveness,
so an externally-killed pane shows as `closed` even when the
meta still says Active.

Spawning + attaching from inside the TUI are *not yet wired*
— they need ratatui's terminal-suspend dance plus a real tmux
to verify the round-trip. Use the `fleet brainstorm` CLI for
both today; the TUI surfaces what exists.

### Typical workflows

**Initial planning of a refactor.**

```
$ fleet brainstorm
> I want to refactor the parser to support trailing commas
agent: [reads the parser code via local filesystem access]
       [reads existing parser tickets via tracker tool]
       Here's a 5-ticket decomposition: [A, B, C, D, E]. Want me to
       file them and create an epic?
> yes
agent: [creates 5 tickets via tracker.create]
       [creates a GitHub tracking issue with the task list]
       [creates fleet plan "Parser refactor" with the 5 items]
       Done. plan-018f-... is active. Autonomous mode will start
       working through them on the next tick.
> [detaches via Ctrl-B D]
```

**Picking up on a new machine.**

```
$ fleet brainstorm
> recreate the parser-refactor plan from epic gh #200
agent: [reads #200's task list]
       [creates fleet plan with the same items in the same order,
        with epic_ref pointing at #200]
       Done. plan-018f-... created and active.
```

**Mid-execution check-in.**

```
$ fleet brainstorm
> where are we on the parser refactor?
agent: [reads plan, plan items, recent sessions]
       Item 3 (#44 "update grammar") just hit a blocker. The agent
       recommended a follow-up ticket #51 "fix lexer test harness"
       which was created and injected before #44. #51 is running
       now. Two items remaining after that.
```

## Operations summary

```sh
# Outcomes & blocking
fleet sessions list                       # see plan annotations on rows
fleet sessions show <id>                  # outcome + recommend_ticket if any
fleet sessions unblock <id> [--reason]    # clear blocker manually

# Plans
fleet plan list
fleet plan show <id>
fleet plan new "<name>" --tickets 42,43,44
fleet plan edit <id>
fleet plan pause <id>
fleet plan resume <id>
fleet plan complete <id>
fleet plan abandon <id> --reason "..."
fleet plan inject <id> <ticket> [--before <other>]

# Brainstorm
fleet brainstorm                          # new + attach
fleet brainstorm list
fleet brainstorm attach <id>
fleet brainstorm kill <id>

# Dependencies (rarely needed; mostly for surgery)
cat .fleet/deps.json
```

## What this does NOT protect against

- **A brainstorm agent that's been prompt-injected.** The brainstorm
  agent runs on the host with broad authority. Its system prompt asks
  for user confirmation before any tracker mutation, but the user is
  the last line of defence — read the agent's proposed actions before
  approving.
- **An agent spamming comments on its bound ticket.** The bridge
  scoping protects *other* tickets, not the bound one. A determined
  agent can drown its own ticket in comments. Mitigation: cost +
  comment-rate alerting on your tracker side; the agent's comments
  also show up in your `gh issue view` / Linear feed for review.
- **Cross-repo orchestration.** Plans and dependency edges are
  scoped to one repo's `.fleet/`. Cross-repo coordination requires
  manual stitching today.

## Current status

| Phase | What it ships | Status |
|---|---|---|
| 1 | Bridge CLI + `Tracker` write methods | **Shipped** |
| 2 | Outcome convention + `tracker-create` node + deps.json | **Shipped** |
| 3a | Plans data model + `fleet plan` CLI + `fleet sessions unblock` + cycle detection + plan injection | **Shipped** |
| 3b | TUI Plans view (key `p`) + sessions-row plan annotations + status-line plan count | **Shipped** |
| 4 | Supervisor plan-aware scheduling + deps-blocked filter + item reconciliation + `on_item_failure` policies | **Shipped** |
| 5 | Brainstorm agent — tmux integration, `fleet brainstorm {…}` CLI, CLI-based tool surface (`fleet plan/issues/sessions`), TUI two-section sidebar | **Shipped (one caveat)** |

**Phase 5 caveat:**

- The TUI's Shift+B (spawn brainstorm) and Enter-on-brainstorm-
  row (attach) affordances are deferred — they need ratatui's
  terminal-suspend dance plus a real tmux to verify. The
  Brainstorms section in the Sessions sidebar is live and
  shows state; spawning + attaching go through the
  `fleet brainstorm` CLI for now.

**Design note: no parallel HTTP server for brainstorm.** The
agent runs on the host inside tmux with full shell access. Every
operation the agent needs — plan management, ticket triage,
session unblocking — is a single `fleet …` CLI invocation. There
is no parallel HTTP surface because there's nothing it would add
over `Bash → fleet …` (the bridge's per-ticket scoping doesn't
apply on the host; the duplicate surface would just be code rot).
The bridge HTTP server still exists for the *workflow* container
case where the agent is sandboxed and needs scoped tracker
writes; brainstorm doesn't fit that shape.

The implementation plan lives at
`~/.claude/plans/recursive-careful-orchestrator.md` (contributor-side).
This page is the user-facing target shape and will be updated as
phases land.
