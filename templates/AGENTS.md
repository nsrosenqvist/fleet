# Worker agent rules

You are a worker session spawned by the Agent Orchestrator (AO) against an issue in this repository. Read this whole file before doing anything. Your task is whatever the issue body describes; these are the rules of engagement.

This file is the canonical worker rules for **every** project AO manages from this fleet install. It is read directly via `agentRulesFile` in `~/.config/fleet/agent-orchestrator.yaml` — fleet materializes it from its embedded copy on every launch, so editing the on-disk file is not durable. To change the rules, edit `templates/AGENTS.md` in the fleet repo and rebuild.

## How issues work here

The issue body and metadata are supplied up front in your spawn prompt by the tracker plugin AO is configured with. You should not need to re-fetch the issue.

**Use the tracker AO told you.** Your spawn prompt has a `Project Context` block that ends with a `Tracker: <name>` line — that's authoritative. Pick the CLI section below that matches it:

- `Tracker: git-bug` → `git-bug bug show <bug-id>` (add `--format json` for raw fields). Issue ids are short hex strings like `f89ea1e`. Issue refs in commits/PRs are `Closes git-bug:<bug-id>`.
- `Tracker: github` → `gh issue view <number>` (add `--json …` for raw fields). Issue ids are integers. Issue refs are `Closes #<number>`.

Do **not** run `gh issue …` against a git-bug project, or `git-bug …` against a GitHub one — they'll error or silently look in the wrong place. If the `Tracker:` line isn't in your prompt (older AO build), fall back to looking at the top-level `plugins:` block in `~/.config/fleet/agent-orchestrator.yaml` and picking the first tracker plugin listed there.

## How to record progress

AO tracks your lifecycle through these commands. Run them from the worker shell — they're how AO knows what you're doing.

- `ao acknowledge` — run this once after reading the issue. Confirms you've picked up the task.
- `ao report working` — entering the implementation phase.
- `ao report waiting` / `needs-input` — blocked on something you can't resolve.
- `ao report pr-created --pr-url <url>` — **after** opening the PR (see "Push and open the PR" below).
- `ao report ready-for-review` — after CI is green and the PR is ready for human review.

Do NOT self-report `done` or `terminated`. AO owns those transitions — it observes PR merge / session-kill and writes the terminal state itself.

You can also comment on the issue to leave a trail using the tracker CLI matching your `Tracker: <name>`:

- git-bug → `git-bug bug comment new <bug-id> -m "…"`
- github → `gh issue comment <number> -b "…"`

## Push and open the PR

This is the most-missed step. **The session will stay stuck on `working` on the AO dashboard until a PR is opened and reported.** No PR ⇒ no completion, no matter how thoroughly the code is done.

Run, in order, from the worktree root:

```sh
# 1. Push the agent branch to origin (-u so the upstream is tracked).
git push -u origin "$(git branch --show-current)"

# 2. Open the PR. Use a Conventional-Commits-style title and a body
#    that explains the *why* of the change and references the issue.
gh pr create \
  --base "$(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/origin/@@')" \
  --title  '<type>(<scope>): <short summary>' \
  --body   "Closes <tracker-id>

<why this change is being made — not what the diff says>"

# 3. Capture the URL the PR was opened at and report it back to AO.
PR_URL="$(gh pr view --json url -q .url)"
ao report pr-created --pr-url "$PR_URL"

# 4. Once CI is green, hand the PR off to a human reviewer.
ao report ready-for-review
```

Notes:

- `<tracker-id>` is the issue identifier the spawn prompt gave you, formatted per your `Tracker: <name>`: git-bug uses the human-id (`Closes git-bug:fa8098a`); github uses the issue number with a `#` prefix (`Closes #123`). Don't mix forms — `Closes git-bug:123` against a github project (or `Closes #fa8098a` against git-bug) doesn't auto-link or auto-close on merge.
- If `gh pr create` fails (auth, no remote, branch already has a PR), **do not** call `ao report pr-created`. Instead `ao report needs-input` with a comment describing the failure and stop — a human needs to unblock the credential / remote.
- If you have to push amended commits later, the PR updates automatically; no further `ao report pr-created` needed.

## Branching and commits

- The worktree was created on a branch named `agent/<issue-id>` by AO. Do not rename it — AO matches the worktree to the session by branch name.
- The base branch is the project's default (typically `main`). Resolve it via `git symbolic-ref refs/remotes/origin/HEAD` (shown above) rather than guessing.
- **One PR per issue.** Link the issue id in the PR description (`Closes <tracker-id>` is parsed by GitHub for auto-close on merge).
- **Conventional Commits v1.0.0** on every commit. Allowed types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `chore`, `build`, `ci`, `style`, `revert`. Breaking changes go in the footer (`BREAKING CHANGE: …`).
- **Never bypass git hooks.** No `--no-verify`, no `--no-gpg-sign`.

## Verification gates (mandatory before every commit)

Run the project's verify suite from the worktree root. Find the canonical command by checking, in order:

1. `Makefile` — look for `verify`, `check`, `test` targets.
2. `package.json` `scripts` — `verify`, `check`, `test`, `lint`, `typecheck`.
3. `Cargo.toml` — defaults to `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
4. The project's own `AGENTS.md` / `CONTRIBUTING.md` / `README.md` if any of the above are ambiguous.

Run format, lint, typecheck, and tests in that order. Each is a hard requirement — fix and re-run from the top if any fails.

Rules:

- All gates must pass on the **staged** state, not just the working tree. Re-run after `git add`.
- Lint warnings are errors. No "known warnings" backlog.
- New behaviour requires a new test. A PR that adds untested code is incomplete.
- Do not silence lint rules with file-level disables to pass verification. Either fix the code or change the rule in a separate PR with justification.

## Conventions

Follow the conventions evident in the repo. Concretely:

- Mirror the style of code adjacent to your change — naming, file layout, error model, import grouping.
- Use `git log -p -- <path>` to see how a particular area has been evolving and stay consistent with the recent prevailing style.
- If the repo has its own `AGENTS.md`, `CONTRIBUTING.md`, or style doc at its root, those override anything here for project-specific decisions.

## Do not touch

Without explicit approval in the issue body or via `ao report needs-input`:

- Anything under paths the repo's own docs flag as sensitive (auth, billing, customer-data, secrets, key material). Check `CODEOWNERS`, `SECURITY.md`, and any per-project `AGENTS.md` for the list.
- Database migrations — propose in the PR description, do not write.
- Public API contracts.
- Secrets, env files, anything matching `*.pem` / `*.key` / `.env*`.
- CI configuration (`.github/workflows/*` etc.) — propose changes in the PR description first.

## What "done" means

**You are not done until `ao report ready-for-review` has been called.** Self-reporting completion before opening the PR will leave the session stuck on `working` on the AO dashboard, with a "PR not created" card — this is the most common failure mode.

The full checklist:

- All verification gates pass on the staged code.
- Branch is pushed to origin.
- PR is opened with `gh pr create`, references the issue id, and explains the *why*.
- `ao report pr-created --pr-url <url>` has been called with the real PR URL.
- CI is green on the PR.
- `ao report ready-for-review` has been called.
- Conventional-commit messages on every commit.
- Any new behavior has a test.
- No new runtime dependencies without justification in the PR description.
- No `TODO`/`FIXME`/`XXX` left in code paths the PR touches; if a follow-up is needed, open a new tracker issue and reference its id.
- Diff is focused: no incidental "while I was here" reformat / refactor outside the PR's scope.

## When stuck

- Re-read the issue body. AO supplied it; the answer to "what does done look like?" is often there.
- Leave a tracker comment documenting what's ambiguous using the CLI matching your `Tracker: <name>` (`git-bug bug comment new …` or `gh issue comment …`), then `ao report waiting`. Do not invent acceptance criteria.

---

<!--
Maintaining this file (for fleet contributors, not workers):

The on-disk copy at ~/.config/fleet/templates/AGENTS.md is materialized
from the binary's embedded `include_str!` of templates/AGENTS.md on
every fleet launch. Edits to the on-disk file are overwritten on the
next launch — change `templates/AGENTS.md` in the fleet repo and
rebuild.

These rules must remain project-agnostic. A worker spawned against any
tracker (git-bug, GitHub Issues, whatever) and any language stack must
be able to follow them. Project-specific bits (verify commands, "do not
touch" paths) are deferred to the project's own root AGENTS.md /
CONTRIBUTING.md, which the worker is instructed above to read.
-->
