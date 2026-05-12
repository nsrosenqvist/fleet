# Worker AGENTS.md — template

This is a starting point for the `AGENTS.md` you drop **inside a codebase AO will run workers against**. Copy it into that project's repo root, edit the `[bracketed]` spots, and reference it from your AO config:

```yaml
projects:
  <your-project>:
    path: /path/to/your/repo
    agentRulesFile: AGENTS.md  # AO reads this and includes it in every worker prompt
    tracker:
      plugin: git-bug
```

**This file lives next to the *code being changed*, not next to fleet.** It is read by the AO worker agent (claude-code) every time AO spawns it for an issue. fleet's own `AGENTS.md` (one level up from this template, at the repo root) is for engineers and AI assistants working on the fleet tooling itself; the two have nothing to do with each other.

Delete anything below that doesn't apply, and add project-specific rules where you see `[brackets]`.

---

# Worker agent rules — `[project name]`

You are a worker session spawned by the Agent Orchestrator (AO) against a git-bug issue. Read this whole file before doing anything. Your task is whatever the issue body describes; these are the rules of engagement.

## How issues work here

This repo uses [git-bug](https://github.com/git-bug/git-bug) — a distributed bug tracker stored in git refs. AO supplies the issue context up front via the tracker plugin (`getIssue` + `generatePrompt`), so you should not need to re-fetch it. If you do need fresh data:

- `git-bug bug show <bug-id>` — full bug + comments.
- `git-bug bug show <bug-id> --format json` — same, machine-parseable.
- `git-bug bug ls --status open` — list open bugs (no `ls` subcommand — `git-bug bug` is the list verb; `ls` is parsed as a query and matches nothing).

## How to record progress

AO tracks your lifecycle through these commands. Run them from the worker shell — they're how AO knows what you're doing.

- `ao acknowledge` — run this once after reading the issue. Confirms you've picked up the task.
- `ao report working` — entering the implementation phase.
- `ao report waiting` / `needs-input` — blocked on something you can't resolve.
- `ao report pr-created --pr-url <url>` — after opening a PR.
- `ao report ready-for-review` — after CI is green and the PR is ready for human review.

Do NOT self-report `done` or `terminated`. AO owns those transitions — it observes PR merge / session-kill and writes the terminal state itself.

You can also comment on the bug to leave a trail:

- `git-bug bug comment new <bug-id> -m "I tried X, ran into Y, going with Z"` — visible to whoever reviews the bug later.

## Branching and commits

- **Branch from `[default branch]`.** Name the branch `agent/<bug-id>` (this matches the worktree name AO created for you; don't rename it).
- **One PR per bug.** Link the bug id in the PR description (e.g. `Closes git-bug:<bug-id>`).
- **Conventional Commits v1.0.0** on every commit. Allowed types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `chore`, `build`, `ci`, `style`, `revert`. Breaking changes go in the footer (`BREAKING CHANGE: …`). Examples:
  - `feat([scope]): add the thing the bug asked for`
  - `fix([scope]): handle the edge case the bug describes`
- **Never bypass git hooks.** No `--no-verify`, no `--no-gpg-sign`.

## Verification gates (mandatory before every commit)

Run these in order. Each is a hard requirement — fix and re-run from the top if any fails.

```sh
[project's format command]
[project's lint command]
[project's typecheck command]
[project's test command]
```

Rules:

- All gates must pass on the **staged** state, not just the working tree. Re-run after `git add`.
- Lint warnings are errors. No "known warnings" backlog.
- New behaviour requires a new test. A PR that adds untested code is incomplete.
- Do not silence lint rules with file-level disables to pass verification. Either fix the code or change the rule in a separate PR with justification.

## Conventions

- `[Language / framework conventions specific to this repo.]`
- `[State management approach.]`
- `[Logging / observability wiring.]`
- `[Error model — exceptions vs Result types vs whatever you use here.]`
- `[Test layering — unit / integration / e2e definitions in this repo.]`

## Do not touch

- `[paths to auth / billing / customer-data / secrets code]` — human review required, do not modify.
- Database migrations — propose in the PR description, do not write.
- Public API contracts — do not change without explicit approval in the bug body.
- Secrets, env files, anything matching `*.pem` / `*.key` / `.env*`.
- CI configuration (`.github/workflows/*` etc.) — propose changes in the PR description first.

## What "done" means

- All verification gates pass on the staged code.
- CI is green on the PR.
- Conventional-commit messages on every commit in the branch.
- PR description names the git-bug bug id, summarizes the change, and explains the *why*.
- Any new behavior has a test.
- No new runtime dependencies without justification in the PR description.
- No `TODO`/`FIXME`/`XXX` left in code paths the PR touches; if a follow-up is needed, open a new git-bug issue and reference its id.
- Diff is focused: no incidental "while I was here" reformat / refactor outside the PR's scope.

## When stuck

- Re-read the bug body. AO supplied it; the answer to "what does done look like?" is often there.
- `git-bug bug comment new <bug-id> -m "…"` to document what's ambiguous, then `ao report waiting`. Do not invent acceptance criteria.
