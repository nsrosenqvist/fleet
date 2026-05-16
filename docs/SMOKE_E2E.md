# Fleet end-to-end smoke test

End-to-end walkthrough that drives every surface fleet exposes,
against a throwaway TODO MVC project under `tmp/`. Organised in
tiers so you can stop at any level depending on what's installed.

The repo lives under `tmp/todomvc/` (already gitignored).

## What this covers

| Tier | Surface                                | Needs                                    |
| ---- | -------------------------------------- | ---------------------------------------- |
| 0    | Build + binary sanity                  | cargo                                    |
| 1    | CLI: issues, deps, plan, sessions list | git-bug                                  |
| 2    | TUI (`fleet ui`)                       | git-bug (for the sidebar to populate)    |
| 3    | Workflow execution                     | container runtime + `claude-code` agent  |
| 4    | Autonomous supervisor                  | tier 3 + Anthropic API credentials       |
| 5    | Brainstorm agent                       | `tmux` + agent + API credentials         |

## Prereqs

```bash
# Always:
cargo --version
git --version
git-bug --version           # 0.8+ recommended; one-time identity setup below

# Tier 2:
tmux -V

# Tier 3+:
# Any one of these is enough — podman + docker are both first-class on
# Linux; `container` is Apple's CLI on macOS 26+ only.
podman --version
docker --version
# Devcontainer CLI: fleet's doc-comment names the Rust crate as the
# primary install path; the upstream Node CLI is the documented fallback.
# Either binary just needs to be named `devcontainer` on PATH.
devcontainer --version      # `cargo install devcontainer`           (Rust, fleet's primary)
                            # or: `npm install -g @devcontainers/cli` (Node fallback)
which claude-code           # or whatever agent your workflows reference
echo "$ANTHROPIC_API_KEY"   # the agent needs this exported
```

One-time git-bug identity (needs to happen *before* `fleet issues
create`; git-bug refuses writes without an identity):

```bash
git-bug user create --non-interactive \
  --name  "Smoke Test" \
  --email "smoke@example.com"
```

---

## Tier 0 — Build

```bash
cd ~/Code/fleet
cargo build --bin fleet --bin fleet-tracker
cargo test  --bin fleet --tests          # 1000+ tests, should be all green
./target/debug/fleet --help | head -25   # sanity: subcommands listed
```

> **Tip:** alias the binary so the rest of the guide stays short:
>
> ```bash
> alias FLEET=$PWD/target/debug/fleet
> ```

---

## Tier 1 — CLI surface (no agent, no runtime)

### 1.1 Bootstrap the test repo

```bash
rm -rf tmp/todomvc                       # idempotent reset
mkdir -p tmp/todomvc && cd tmp/todomvc
git init -q
FLEET init
```

Expect a `Created:` block listing `.fleet/config.yaml`, three
workflows under `.fleet/workflows/`, three prompts under
`.fleet/prompts/`, and `.devcontainer/devcontainer.json`. The
"Bootstrap your environment" tail flags missing host tools — note
what's missing but don't worry about Tier 3+ tools yet.

### 1.2 Tracker CRUD

```bash
FLEET issues list                        # (none)
T_PROBE=$(FLEET issues create "Probe ticket" --body "to be closed")
echo "T_PROBE=$T_PROBE"

FLEET issues comment "$T_PROBE" "First comment from smoke test"
FLEET issues add-label "$T_PROBE" smoke
FLEET issues set-status "$T_PROBE" closed
FLEET issues list                        # one row, closed, with [smoke] label
```

> If `fleet issues create` exits 0 but prints a *git-bug error*
> instead of a ticket id, your identity isn't set — re-run the
> `git-bug user create` from Prereqs.

### 1.3 Contracts-first decomposition (TODO MVC vertical slice)

This is the walk-through the brainstorm agent's prompt encodes.
You'll file three tickets, wire `blocked_on` edges, and create a
plan that the supervisor (Tier 4) will pick up in order.

```bash
T_CONTRACTS=$(FLEET issues create "API contracts for TODO MVC" \
  --body "OpenAPI spec for /todos + TodoItem TypeScript types in a shared package." \
  --label contracts --label priority-high)

T_BACKEND=$(FLEET issues create "Backend: TODO API in Rust" \
  --body "Implement REST API per the contracts ticket. axum + sqlite." \
  --label backend)

T_FRONTEND=$(FLEET issues create "Frontend: TODO web client" \
  --body "React UI consuming the API per the contracts ticket." \
  --label frontend)

echo "contracts=$T_CONTRACTS  backend=$T_BACKEND  frontend=$T_FRONTEND"

FLEET deps add "$T_BACKEND"  --blocked-on "$T_CONTRACTS"
FLEET deps add "$T_FRONTEND" --blocked-on "$T_CONTRACTS"
FLEET deps list                          # 2 edges, both [ticket] kind

FLEET plan new "TODO MVC" \
  --tickets "$T_CONTRACTS,$T_BACKEND,$T_FRONTEND"

PLAN_ID=$(FLEET plan list | tail -1 | awk '{print $2}')
FLEET plan show "$PLAN_ID"               # 3 pending items in order
```

### 1.4 Plan lifecycle

```bash
FLEET plan pause   "$PLAN_ID"            # supervisor skips the plan
FLEET plan list                          # state column flips to `paused`
FLEET plan resume  "$PLAN_ID"            # back to `active`

# Mid-plan inject: add a fourth ticket between backend and frontend.
T_DB=$(FLEET issues create "Backend: SQLite schema migration" --label backend)
FLEET plan inject "$PLAN_ID" "$T_DB" --before "$T_FRONTEND"
FLEET plan show   "$PLAN_ID"             # 4 items now, $T_DB at index 2
```

### 1.5 Deps cycle refusal

The supervisor relies on deps being acyclic. `fleet deps add`
refuses to create a cycle:

```bash
# Already: backend → contracts. Adding contracts → backend would
# close a 2-cycle. Expect exit 1 and a clear error.
FLEET deps add "$T_CONTRACTS" --blocked-on "$T_BACKEND"
echo "exit=$?"                           # exit=1

FLEET deps list                          # graph unchanged
```

### 1.6 Sessions list (empty for now)

```bash
FLEET sessions list                      # (no sessions yet — that's tier 3)
```

---

## Tier 2 — TUI exploration (`fleet ui`)

```bash
FLEET ui
```

Drive each surface in turn. Press `q` to quit when done.

### 2.1 Sessions view + doctor pane

- The sidebar shows `Sessions (0)` (no workflow runs yet) but the
  TODO MVC tickets aren't here — this view lists sessions, not
  tickets. That's expected.
- Press `d` — Doctor pane appears. Confirms adapter, hardening,
  agents, tracker. Press `d` or `Esc` to return.

### 2.2 Plans view (Tab focus, item cursor)

- Press `p` — Plans view. Sidebar shows the **TODO MVC** plan
  with `0/4` progress.
- Press `Tab` — focus flips to the items pane; cursor lands on
  item 0 with a `▸` glyph.
- `j` / `k` — move the item cursor. Title bar reads
  `<plan-id> · items`.
- `Tab` — focus returns to the sidebar.

### 2.3 Plan state actions

- With the plan selected, press `Shift+P` — sidebar marker flips
  from `●` (active) to `⏸` (paused). Press again to flip back.
- Press `Shift+C` — state becomes `✓` (completed). The change is
  persisted; quit + relaunch the TUI and the state survives.
- Re-mark it active in the shell for the next tiers:

```bash
FLEET plan resume "$PLAN_ID"
```

### 2.4 Plan unblock from the TUI

- Press `Tab` to focus items, `j` to land on the backend row.
- Press `u`. Status bar reads `ticket "<T_BACKEND>": cleared 1 dep edge`.
- Quit (`q`). Confirm in the shell:

```bash
FLEET deps list                          # one edge left: frontend → contracts
```

Re-add the edge for later tiers:

```bash
FLEET deps add "$T_BACKEND" --blocked-on "$T_CONTRACTS"
```

### 2.5 ⚠ cycle marker

Force a cycle by hand-editing `.fleet/deps.json` (the supervisor's
defence is `fleet deps add`'s pre-check, so to *see* the marker
you need to skip that check):

```bash
# Take a backup; we'll restore it.
cp .fleet/deps.json .fleet/deps.json.bak

# Append a back-edge: contracts → backend, creating a 2-cycle with
# the existing backend → contracts edge.
python3 - <<'PY'
import json
d = json.load(open(".fleet/deps.json"))
d["edges"].append({
    "blocked": "REPLACE_T_CONTRACTS",
    "blocked_on": "REPLACE_T_BACKEND",
    "reason": "ticket",
    "created_at_ms": 1,
})
open(".fleet/deps.json","w").write(json.dumps(d, indent=2))
PY
# Replace the placeholders:
sed -i "s/REPLACE_T_CONTRACTS/$T_CONTRACTS/; s/REPLACE_T_BACKEND/$T_BACKEND/" .fleet/deps.json
```

Relaunch `FLEET ui`, press `p` — the plan row in the sidebar now
carries a leading `⚠` glyph because two of its items sit on a
cycle. Quit, restore, and confirm it clears:

```bash
mv .fleet/deps.json.bak .fleet/deps.json
FLEET deps list                          # back to 2 edges, both forward
# (relaunch UI, plan row has no warning marker now)
```

### 2.6 Brainstorms sidebar (Tab + Shift+B + Enter)

> Needs `tmux`. Tier 5 covers driving the actual brainstorm agent;
> here we're just verifying the key bindings work.

- Launch a brainstorm without leaving the TUI: press `Shift+B`.
  Fleet suspends the alternate screen, runs `fleet brainstorm`,
  and re-enters when you detach (`Ctrl-b d` inside tmux).
- Back in the Sessions view, the sidebar now shows a
  `Brainstorms (1)` section.
- Press `Tab` — focus flips to Brainstorms. `j`/`k` move the
  cursor among brainstorm rows.
- Press `Enter` — fleet suspends and runs `fleet brainstorm attach
  <id>`. Detach with `Ctrl-b d` to return to the TUI.

---

## Tier 3 — Workflow execution (container runtime + agent)

> **Gating prereqs:**
>
> - A container runtime: `fleet runtime doctor` must report ✓ for
>   podman, docker, or apple-container.
> - The `claude-code` CLI on PATH (the default workflows reference
>   it).
> - `ANTHROPIC_API_KEY` exported.
> - `devcontainer` CLI on PATH.

### 3.1 Runtime probe

```bash
FLEET runtime doctor
```

Look for ✓ on at least one engine and on `devcontainer`. When
`tracker: git-bug` is configured (the init default), `git-bug`
should also show ✓ — if it doesn't, install it before continuing
(`cargo install git-bug` or `brew install git-bug`).

### 3.2 Workflow inspection

```bash
FLEET workflow list                      # standard, hotfix, review-only
FLEET workflow validate standard         # `standard ok (6 nodes)`
```

### 3.3 Run the standard workflow on the contracts ticket

```bash
FLEET workflow run standard --issue "$T_CONTRACTS"
```

This:

1. Mints a session id (`s-<hex-ms>-<counter>`).
2. Creates a git worktree off `main` at
   `tmp/todomvc/.fleet/worktrees/<session-id>/`.
3. Builds the devcontainer image (first run is slow — go get
   coffee).
4. Boots a container with the worktree bind-mounted +
   `fleet-tracker` bridged in.
5. Streams the planner → implementer → reviewer agent passes.

Watch progress in another terminal:

```bash
FLEET sessions list                      # rows update as nodes advance
SID=$(FLEET sessions list | tail -1 | awk '{print $2}')
FLEET sessions show  "$SID"
FLEET sessions logs  "$SID" --node implement
```

On success the workflow opens a PR (the `open_pr` node uses `gh
pr create` and will refuse cleanly if you don't have `gh` auth set
up — that's fine for a smoke test).

If the implementer reports `outcome: blocked` and recommends a
follow-up ticket, the `file-dep` node files it via
`tracker-create` and links a `blocked_on` edge automatically.
Confirm:

```bash
FLEET deps list                          # may have grown by one edge
FLEET issues list                        # may have a new auto-filed ticket
```

---

## Tier 4 — Autonomous supervisor

> **Gating:** same as Tier 3 plus a clean tracker state — the
> supervisor scans for unclaimed open tickets and starts sessions
> for them, respecting plan order + deps edges.

### 4.1 One tick

```bash
FLEET autonomous run --once
```

What you should see:

- The supervisor reads plans + deps + sessions.
- It picks `$T_CONTRACTS` first (only ticket whose deps are
  satisfied — backend and frontend are blocked-on it).
- It refuses to start more than `autonomous.max_active_sessions`
  concurrent sessions (default 1 in the shipped config — edit
  `.fleet/config.yaml` to raise).
- After contracts merges and closes, a subsequent
  `fleet autonomous run --once` finds backend + frontend now
  eligible and starts them (parallel if budget allows).

### 4.2 Watch mode (Ctrl-C to stop)

```bash
FLEET autonomous run --watch
```

Sleeps `autonomous.scan_interval_secs` between ticks. Useful for
leaving it running in a side terminal while you watch the TUI in
another.

---

## Tier 5 — Brainstorm agent

> **Gating:** `tmux` + agent CLI + API credentials.

### 5.1 Drive a contracts-first plan interactively

```bash
FLEET brainstorm                         # spawns a fresh agent in tmux
```

You're now in a tmux pane with the brainstorm agent loaded. Try:

```
I want to add a "share a todo by link" feature. Walk me through
what tickets and dep edges we'd need.
```

The agent should:

1. Read repo state (`fleet issues list`, `fleet plan list`,
   `fleet deps list`).
2. Propose a *contracts ticket* first — either an OpenAPI delta
   or an ADR depending on the scope — followed by per-component
   implementation tickets.
3. State each proposed `fleet issues create` / `fleet deps add` /
   `fleet plan new` it intends to run, and **wait for your
   confirmation** before executing.
4. Capture the new ids and use them in subsequent commands.

Detach the brainstorm session with `Ctrl-b d` — fleet's reaper
will mark it `Detached`; re-attach later with:

```bash
FLEET brainstorm list                    # see all brainstorm sessions
FLEET brainstorm attach <id>             # resume the conversation
FLEET brainstorm kill   <id>             # tear down for good
```

### 5.2 Confirm the prompt's guidance is actually loaded

```bash
# Inspect what the agent saw on startup:
cat .fleet/planning/<id>/prompt.md | head -120
```

You should see the contracts-first bullet under *Behaviour
expectations* and the worked CLI walkthrough under *Workflow*.
If you have a per-repo override at `.fleet/prompts/brainstorm.md`
it replaces the static template; the repo snapshot still gets
appended.

---

## Cleanup

```bash
cd ~/Code/fleet
# Nuke the test repo + any brainstorm tmux sessions left over:
tmux ls 2>/dev/null | grep '^fleet-brainstorm-' | cut -d: -f1 | xargs -r -n1 tmux kill-session -t
rm -rf tmp/todomvc
```

---

## Known quirks

- **`fleet issues create` exits 0 on a git-bug identity error.**
  The error text is printed to stdout but the exit code doesn't
  propagate. Always inspect the first line of stdout — if it's
  not a short hash, treat it as a failure.
- **First `fleet workflow run` is slow.** The devcontainer image
  builds from scratch. Subsequent runs reuse the image.
- **`open_pr` node fails without `gh auth`.** Not a fleet bug —
  expected when the repo isn't a real GitHub remote. The session
  still records every prior node's artifacts.
