# Per-session git worktrees

This page covers fleet's **session-isolation** model for workspaces.
Every workflow session — every `fleet workflow run`, every autonomous
spawn, every replay — gets its own `git worktree` on a fleet-managed
branch. The agent never runs against the host's shared working tree.

## What gets created

When you run `fleet workflow run standard` from a git repo:

1. Fleet mints a session id, e.g. `s-018f3a2c8e9d-0001`.
2. The CLI runs `git worktree add -b fleet/session-<short-id>
   .fleet/sessions/<id>/worktree HEAD`. That:
   - Creates a new branch `fleet/session-018f3a2c8e9d` based off the
     host's current `HEAD`.
   - Checks the branch out into `.fleet/sessions/<id>/worktree/`.
   - Leaves the host's working tree completely untouched.
3. The executor bind-mounts that worktree path as the container's
   `/workspace`. The agent reads and writes there.
4. The path + branch are persisted on the session's `meta.json` as
   `worktree_path` and `branch`, so `replay`/`resume`/`prune` can
   find them later.

Branch names are `fleet/session-<first 12 chars of session id>`. The
prefix is grep-friendly — `git branch --list 'fleet/session-*'`
shows every fleet-managed branch on the repo.

## What this enables

- **Parallel sessions don't stomp on each other.** Two autonomous
  runs against the same repo each get their own worktree on their
  own branch. They can edit the same files without conflict; the
  isolation is filesystem-level.
- **Replay sees the prior run's code state.** `fleet workflow
  replay <src> --rerun-from review` creates a *new* worktree based
  off `<src>`'s branch tip. The reviewer node runs against the
  exact diff the implementer node produced in the original run —
  not whatever's in the host worktree right now.
- **Resume continues where it paused.** When a workflow pauses at a
  human gate and you resume hours later, the resumed run picks up
  the same worktree the prior step was editing. In-progress files
  survive the pause/resume boundary.
- **Agent commits are real commits on a real branch.** When the
  agent runs `git commit` inside the container, it lands on
  `fleet/session-<id>`. You can `git checkout fleet/session-<id>`
  on the host afterwards to inspect, cherry-pick, or merge the
  work — even after `fleet sessions prune` removes the worktree
  directory.

## What this does NOT protect against

- **Uncommitted host changes don't ride along.** `git worktree
  add HEAD` checks out the commit `HEAD` points at, not the host's
  working-tree state. Files modified on the host but not committed
  are invisible to the new session. Commit (or stash + apply, by
  hand) before running fleet when you need those changes.
- **Untracked host files don't ride along.** Same root cause:
  `git worktree add` populates from the object database, not from
  the working tree.
- **Non-git workspaces have no isolation.** Run fleet outside a
  git repo and the agent runs against the shared repo root with a
  warning logged. Two parallel non-git sessions *will* stomp on
  each other. Replay loses its code-state snapshot guarantees.
- **Branch conflicts on long sessions.** If the host repo's `HEAD`
  moves forward while a long-running session is in progress, the
  session's branch ages out of sync. Merging the session branch
  back becomes a regular three-way merge with whatever conflicts
  that implies.

## Lifecycle

```
fleet workflow run standard
  └─ git worktree add -b fleet/session-<id> .fleet/sessions/<id>/worktree HEAD
     └─ agent runs in /workspace = .fleet/sessions/<id>/worktree
        └─ session reaches Completed
           ├─ default: worktree stays on disk
           └─ with cleanup.auto_prune_completed: true → worktree removed,
              branch kept

fleet workflow replay <src> --rerun-from <node>
  └─ git worktree add -b fleet/session-<new-id>
       .fleet/sessions/<new-id>/worktree
       fleet/session-<src-short-id>   ← src's branch tip, not HEAD
     └─ new session runs from <node> against the same code state src ended at

fleet workflow resume <session>
  └─ reuses the session's existing worktree_path
     └─ executor bind-mounts that same path; agent sees in-progress files

fleet sessions prune <id>
  └─ git worktree remove --force .fleet/sessions/<id>/worktree
     └─ Session.worktree_path = None on meta.json
     └─ branch is KEPT — `git checkout fleet/session-<id>` still works

fleet sessions prune <id> --with-branch
  └─ same as above, then
  └─ git branch -D fleet/session-<id>
     └─ Session.branch = None on meta.json
     └─ commits an agent made are gone unless they were merged or cherry-picked
```

## Configuration

`cleanup.auto_prune_completed: bool` in `.fleet/config.yaml` —
default `false`. Set it to `true` to have fleet inline-prune the
worktree whenever a workflow reaches `Completed`:

```yaml
cleanup:
  auto_prune_completed: true
```

`Failed` and `Crashed` sessions are never auto-pruned regardless of
the setting — they're forensic data. Reach for `fleet sessions
prune --all` when you're ready to reclaim those too.

## Operations

**See what's on disk for a session.**

```sh
fleet sessions show <id>
```

Surfaces the worktree path and branch name when present. The TUI
detail pane shows the same fields under the per-run header.

**Inspect a session's commits without checking out.**

```sh
git log --oneline fleet/session-<id>
git diff fleet/session-<id> --stat
```

**Cherry-pick an agent's commit onto your branch.**

```sh
git checkout my-feature
git cherry-pick fleet/session-<id>
```

**Free disk after a batch of successful runs.**

```sh
fleet sessions prune --completed                   # keep branches
fleet sessions prune --completed --with-branch     # reap branches too
```

**Drop every terminal-state session's worktree and branch.**

```sh
fleet sessions prune --all --with-branch
```

`--all` covers `Completed`, `Failed`, and `Crashed`. Running
sessions are refused (kill or wait first).

**Reap stale `git worktree` administrative entries** after
deleting `.fleet/sessions/<id>/` manually.

```sh
git worktree prune
```

(Fleet's prune command does this automatically when it notices the
worktree directory is already gone.)

## Implementation notes

The worktree module is `src/worktree.rs` — pure helpers over
`ProcessInvoker` so the git invocations are mockable. Session
fields live on `src/session/aggregate.rs::Session::{worktree_path,
branch}`. CLI wiring is in `src/cli/workflow.rs` (run / resume /
replay) and `src/cli/sessions.rs` (prune).

`SessionStore::create` tolerates a pre-existing per-session
directory because the CLI creates `.fleet/sessions/<id>/` before
the executor's `Session::new` so `git worktree add` has a parent
to land in. Collision detection moved to the meta.json path
specifically.
