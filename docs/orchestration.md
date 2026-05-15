# Orchestration: plans, blocking, and the brainstorm agent

This page covers how fleet sequences work across multiple sessions, how
agents signal they can't proceed, how new tickets get filed in response,
and the interactive brainstorm surface for planning.

If you're looking for individual-session mechanics, see
[`worktrees.md`](./worktrees.md). If you're looking for how the agent
gets permission to mutate the tracker, see
[`auth.md`](./auth.md#per-tracker-auth).

> **Status.** This page documents the v2-extension design. As of this
> writing, the bridge CLI, outcome convention, plans, and brainstorm
> agent are designed but not all phases have shipped — see the table at
> the bottom of this page for current state. Behaviour described here
> is the target shape; check the linked plan or recent commits if
> something doesn't match.

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

Agent nodes in a workflow declare an outcome in their output:

```yaml
- id: implement
  agent: claude-code
  prompt_file: prompts/implementer.md
  outputs:
    outcome: result.outcome          # progress | blocked | done
    summary: result.summary
    blocker: result.blocker          # required when outcome=blocked
    recommend_ticket:                # optional, only when blocked
      title: result.recommend_ticket.title
      body:  result.recommend_ticket.body
      labels: result.recommend_ticket.labels
```

The agent writes `artifacts/result.json` with these keys; fleet's
executor extracts them and surfaces them to downstream nodes via the
`outputs:` accumulator (the same mechanism that's powered review-vs-
revise routing since v2).

Workflows branch on the outcome. The default `standard.yaml` shape:

```yaml
- id: implement
  agent: claude-code
  outputs: { outcome: result.outcome, ... }

- id: review
  depends_on: [implement]
  when: 'implement.outcome == "done"'
  agent: claude-code

- id: file-dep
  depends_on: [implement]
  when: 'implement.outcome == "blocked" && implement.recommend_ticket'
  type: tracker-create
  from: implement.recommend_ticket
  link_parent: true
  add_to_plan: same

- id: block-comment
  depends_on: [implement]
  when: 'implement.outcome == "blocked"'
  type: bash
  script: 'fleet-tracker comment "Blocked: $(jq -r .blocker artifacts/result.json)"'

- id: block-gate
  depends_on: [block-comment]
  when: 'implement.outcome == "blocked"'
  type: gate
  summary: "Implementer blocked. See ticket comment."
```

A blocked outcome routes to: (a) post a comment via the bridge CLI,
(b) optionally file a follow-up ticket via `tracker-create`, (c) park
the session at a gate for human attention. The reviewer never starts
on incomplete work because its `when:` requires `outcome == "done"`.

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

Cross-arch is best-effort: a linux/amd64 fleet host running a
linux/arm64 container (or vice versa) bind-mounts a non-matching
binary that fails the same "exec format error" the agent's
`fleet-tracker` call would otherwise see. The bridge's HTTP endpoint
remains reachable. Container-arch detection is on the v2 list.

## Recommending new tickets

When an agent's `outcome` is `blocked` and it would need a new ticket
to unblock, it includes a `recommend_ticket` in its result. The
workflow's `tracker-create` node turns the recommendation into reality:

1. Reads `recommend_ticket` from upstream outputs.
2. Calls fleet's host-side tracker → creates the issue.
3. Posts cross-link comments on both the original and the new
   ticket (configurable via `link_parent: true|false`).
4. Records the dependency: `<original> blocked_on [<new>]` in
   `.fleet/deps.json`.
5. If the original ticket is in an active plan, injects the new
   ticket into the plan immediately before its dependent.

The agent can't call `create` directly — only the workflow can, via
the `tracker-create` node. This is by design: the bridge CLI is
scoped to one ticket; creation is a different authority that lives in
the workflow definition and the supervisor.

A per-session cap (`max_recommended_tickets`, default 3) prevents a
runaway agent from spawning a hundred follow-ups in one session.

## Plans

A plan is a named, ordered list of tickets the supervisor walks
through in sequence:

```yaml
# .fleet/plans/plan-018f-7c2a3b9d-0001.yaml
id: plan-018f-7c2a3b9d-0001
name: "Parser refactor"
state: active                       # active | paused | completed | abandoned
on_item_failure: stop               # stop | continue | retry-once
epic_ref:
  tracker: github
  id: "200"
items:
  - { ticket_id: "42", state: completed, session_id: s-abc... }
  - { ticket_id: "43", state: completed, session_id: s-def... }
  - { ticket_id: "44", state: in-progress, session_id: s-ghi... }
  - { ticket_id: "45", state: pending }
  - { ticket_id: "46", state: pending }
created_at_ms: 1747...
updated_at_ms: 1747...
```

The supervisor's tick:

1. List active plans (oldest-first).
2. For each plan, find the first `pending` item whose ticket isn't
   blocked on any open ticket.
3. Spawn the first candidate (respecting `autonomous.max_parallel`).
4. If no candidate from any plan, fall back to "any open issue."

Plans and the blocked-on graph compose:

- A plan is `[A, B, C, D]`. The agent on B says it's blocked on a
  newly-recommended X. The `tracker-create` node files X, the plan
  becomes `[A, X, B, C, D]` with B's state staying `pending` but the
  blocked-on graph holding it back until X closes.
- Multiple active plans round-robin (oldest first). Item-level
  parallelism is bounded by `autonomous.max_parallel` globally.

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

Manual unblock:

```sh
fleet sessions unblock 42
fleet sessions unblock 42 --reason "apt mirror recovered"
```

The `--reason` posts a comment on the bound ticket explaining the
unblock — useful audit trail when you come back to it weeks later.

### Cycle detection

If you try to record `A → blocked_on: B` and `B` is already
(transitively) blocked on `A`, fleet refuses the addition and
surfaces "cycle detected" via:

- `fleet sessions list`: the affected session row shows `⚠ cycle: A ↔ B`.
- TUI Plans view: the plan's status shows ⚠ with detail.
- The `tracker-create` node that would create the cycle fails the
  workflow rather than silently doing the wrong thing.

Resolve via `fleet sessions unblock` on one side, or surgical edit of
`.fleet/deps.json`.

### What if a plan item just fails?

`on_item_failure` controls behaviour:

- `stop` (default) — the plan transitions to `paused`. Other plans
  continue. User inspects the failed session, decides whether to
  retry, edit the prompt, abandon the item, etc.
- `continue` — supervisor moves to the next item.
- `retry-once` — re-mark the failed item `pending` once; on second
  failure, behave as `stop`.

Set per-plan in the YAML, or globally in `.fleet/config.yaml` under
`plans.on_item_failure`.

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
`fleet-brainstorm-<id>` so you can detach and reattach freely (or
even attach from a second terminal to watch). The transcript is
captured to `.fleet/planning/<id>/transcript.log`.

### In the TUI

The Sessions view has two sections — Workflows (the executor's
sessions) and Brainstorms (planning sessions). `Enter` on a
brainstorm row attaches (TUI temporarily releases the terminal,
execs `tmux attach`, returns on detach). `Shift+B` spawns a new
brainstorm. Same nav keys (`j/k`, `r`, `q`).

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
| 1 | Bridge CLI + `Tracker` write methods | **Not yet shipped** |
| 2 | Outcome convention + `tracker-create` node + deps.json | **Not yet shipped** |
| 3 | Plans data model + CLI + TUI Plans view | **Not yet shipped** |
| 4 | Supervisor plan consumption + failure policy | **Not yet shipped** |
| 5 | Brainstorm agent + TUI attach/detach | **Not yet shipped** |

The implementation plan lives at
`~/.claude/plans/recursive-careful-orchestrator.md` (contributor-side).
This page is the user-facing target shape and will be updated as
phases land.
