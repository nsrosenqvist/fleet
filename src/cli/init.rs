//! `fleet init` — scaffold `.fleet/` + a minimal `.devcontainer/devcontainer.json`
//! in the current repo.
//!
//! Idempotent: re-running on an already-initialised repo creates only the
//! pieces that are missing. The pure [`perform_init`] does the filesystem
//! work; [`render_init_summary`] turns its [`InitPlan`] into the
//! user-visible report. The thin [`run`] entry point glues the two
//! together against the real cwd.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::process::RealProcessInvoker;
use crate::repo;
use crate::runtime::detect::{BootstrapHint, ProbeReport, probe};

/// What `fleet init` did (or would have done — the structure lets us add
/// a `--dry-run` later without reshaping the call sites).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InitPlan {
    /// Paths created during this run (relative to the fleet root).
    pub created: Vec<PathBuf>,
    /// Paths that already existed and were left untouched (relative to
    /// the fleet root).
    pub already_present: Vec<PathBuf>,
}

impl InitPlan {
    fn record_created(&mut self, path: PathBuf) {
        self.created.push(path);
    }

    fn record_present(&mut self, path: PathBuf) {
        self.already_present.push(path);
    }
}

/// Scaffold the per-repo fleet layout under `root`. Pure with respect to
/// the filesystem under `root`; no other I/O happens here.
///
/// What it writes (only if missing):
/// - `.fleet/` directory
/// - `.fleet/.gitignore` — keeps `sessions/` and `crashed/` out of git
/// - `.fleet/config.yaml` — fleet's per-repo config (commented schema)
/// - `.fleet/workflows/` — empty; user-authored YAML lands here
/// - `.fleet/sessions/` — empty; the session store fills this in
/// - `.devcontainer/` directory
/// - `.devcontainer/devcontainer.json` — minimal Ubuntu base image
/// - `.devcontainer/Dockerfile` — Node.js + `claude-code` CLI layered
///   on top of the base image so the default agent works out of the box
pub fn perform_init(root: &Path) -> Result<InitPlan> {
    let mut plan = InitPlan::default();

    let fleet_dir = root.join(".fleet");
    ensure_dir(&fleet_dir, root, &mut plan)?;
    let workflows_dir = fleet_dir.join("workflows");
    ensure_dir(&workflows_dir, root, &mut plan)?;
    ensure_dir(&fleet_dir.join("sessions"), root, &mut plan)?;
    let prompts_dir = fleet_dir.join("prompts");
    ensure_dir(&prompts_dir, root, &mut plan)?;
    ensure_file(
        &fleet_dir.join(".gitignore"),
        root,
        DEFAULT_FLEET_GITIGNORE,
        &mut plan,
    )?;
    ensure_file(
        &fleet_dir.join("config.yaml"),
        root,
        DEFAULT_FLEET_CONFIG_YAML,
        &mut plan,
    )?;

    // Default workflows + their prompt files. Users can edit or remove
    // any of these without breaking `fleet init` (idempotent: we only
    // write files that don't already exist).
    ensure_file(
        &workflows_dir.join("standard.yaml"),
        root,
        DEFAULT_WORKFLOW_STANDARD,
        &mut plan,
    )?;
    ensure_file(
        &workflows_dir.join("hotfix.yaml"),
        root,
        DEFAULT_WORKFLOW_HOTFIX,
        &mut plan,
    )?;
    ensure_file(
        &workflows_dir.join("review-only.yaml"),
        root,
        DEFAULT_WORKFLOW_REVIEW_ONLY,
        &mut plan,
    )?;
    ensure_file(
        &prompts_dir.join("planner.md"),
        root,
        DEFAULT_PROMPT_PLANNER,
        &mut plan,
    )?;
    ensure_file(
        &prompts_dir.join("implementer.md"),
        root,
        DEFAULT_PROMPT_IMPLEMENTER,
        &mut plan,
    )?;
    ensure_file(
        &prompts_dir.join("reviewer.md"),
        root,
        DEFAULT_PROMPT_REVIEWER,
        &mut plan,
    )?;

    let devcontainer_dir = root.join(".devcontainer");
    ensure_dir(&devcontainer_dir, root, &mut plan)?;
    ensure_file(
        &devcontainer_dir.join("devcontainer.json"),
        root,
        DEFAULT_DEVCONTAINER_JSON,
        &mut plan,
    )?;
    ensure_file(
        &devcontainer_dir.join("Dockerfile"),
        root,
        DEFAULT_DEVCONTAINER_DOCKERFILE,
        &mut plan,
    )?;

    Ok(plan)
}

/// Format the user-visible summary of an [`InitPlan`]. Stable wording —
/// tests assert on the exact text.
#[must_use]
pub fn render_init_summary(root: &Path, plan: &InitPlan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "fleet init: {}", root.display());

    if plan.created.is_empty() && plan.already_present.is_empty() {
        out.push_str("  (nothing to do)\n");
        return out;
    }

    if !plan.created.is_empty() {
        out.push_str("\nCreated:\n");
        for p in &plan.created {
            let _ = writeln!(out, "  + {}", p.display());
        }
    }
    if !plan.already_present.is_empty() {
        out.push_str("\nAlready present (left untouched):\n");
        for p in &plan.already_present {
            let _ = writeln!(out, "  · {}", p.display());
        }
    }
    if !plan.created.is_empty() {
        out.push_str("\nNext steps:\n");
        out.push_str("  - Edit .devcontainer/devcontainer.json to match your project.\n");
        out.push_str("  - Tweak the default prompts under .fleet/prompts/.\n");
        out.push_str("  - Run `fleet runtime doctor` to verify your container engine.\n");
        out.push_str("  - Try `fleet workflow list` and `fleet workflow run review-only`.\n");
    }
    out
}

/// Render the bootstrap section appended to `fleet init`'s output:
/// per-missing-tool, OS-tailored install commands the user can
/// copy-paste. When everything is present, returns an empty string
/// — the section is opt-in based on probe results.
#[must_use]
pub fn render_bootstrap_section(report: &ProbeReport, os: &str) -> String {
    let hints = report.install_hints_for_os(os);
    if hints.is_empty() {
        return String::new();
    }
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("\nBootstrap your environment:\n");
    out.push_str("  (fleet runtime doctor found tools missing for the configurations below)\n");
    for BootstrapHint {
        tool,
        command,
        note,
    } in &hints
    {
        let _ = writeln!(out, "\n  ✗ {tool}");
        let _ = writeln!(out, "      {command}");
        let _ = writeln!(out, "      ({note})");
    }
    out
}

/// CLI entry point. Resolves the fleet root via [`crate::repo::fleet_root`],
/// scaffolds, prints the summary + bootstrap hints, returns exit code 0.
pub fn run() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let plan = perform_init(&root)?;
    print!("{}", render_init_summary(&root, &plan));
    // First-run bootstrap: probe the host and surface OS-tailored
    // install commands for whatever's missing. Silent when nothing
    // needs installing.
    let report = probe(&RealProcessInvoker);
    print!(
        "{}",
        render_bootstrap_section(&report, std::env::consts::OS)
    );
    Ok(0)
}

fn ensure_dir(path: &Path, root: &Path, plan: &mut InitPlan) -> Result<()> {
    let rel = relativise(path, root);
    if path.is_dir() {
        plan.record_present(rel);
    } else {
        fs::create_dir_all(path)
            .with_context(|| format!("creating directory {}", path.display()))?;
        plan.record_created(rel);
    }
    Ok(())
}

fn ensure_file(path: &Path, root: &Path, contents: &str, plan: &mut InitPlan) -> Result<()> {
    let rel = relativise(path, root);
    if path.exists() {
        plan.record_present(rel);
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent of {}", path.display()))?;
        }
        fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
        plan.record_created(rel);
    }
    Ok(())
}

/// Strip the `root` prefix from `path` when possible so the summary
/// shows `.fleet/config.yaml` rather than the absolute path. Falls back
/// to the full path if `path` is somehow not under `root` (it always is
/// in practice, but the guard keeps the function total).
fn relativise(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

/// Default `.fleet/.gitignore`. Keeps per-run state out of version
/// control while still letting users check in `config.yaml` and
/// `workflows/*.yaml`.
const DEFAULT_FLEET_GITIGNORE: &str = "\
# fleet — per-repo runtime state. Keep out of version control by default;
# config.yaml and workflows/ are intended to be checked in.
sessions/
crashed/
plans/
planning/
orchestrator/
spawn-logs/
deps.json
*.tmp
";

/// Test-only accessor for the default config.yaml template. Lets the
/// `repo_config` parser tests assert that the template scaffolded by
/// `fleet init` round-trips to `RepoConfig::default()`.
#[cfg(test)]
#[must_use]
pub fn default_fleet_config_yaml_for_tests() -> &'static str {
    DEFAULT_FLEET_CONFIG_YAML
}

/// Default `.fleet/config.yaml`. Commented and minimal — the full schema
/// is in the v2 plan. Consumers are still being implemented, so we keep
/// only the fields whose meaning is already settled.
const DEFAULT_FLEET_CONFIG_YAML: &str = "\
# fleet — per-repo configuration.
#
# Fields use the v2 schema described in
# /home/niklas/.claude/plans/declarative-wobbling-quasar.md. Values
# below are the defaults applied when keys are missing; uncomment and
# change to override.

runtime:
  # podman | docker | apple-container | local | auto
  # `auto` picks per host on first run (see `fleet runtime doctor`).
  adapter: auto

  # gvisor | none | auto
  # `auto` enables gVisor on Linux when `runsc` is detected; leaves the
  # engine default otherwise.
  hardening: auto

  # Devcontainer that defines the agent environment.
  devcontainer: .devcontainer/devcontainer.json

  # Defense-in-depth `git push` guard. The `fleet-git` shim is
  # bind-mounted into the devcontainer at /usr/local/bin/git and
  # blocks pushes to protected refs. See docs/security.md.
  # git:
  #   # When false, the shim is not mounted at all.
  #   enabled: true
  #   # Refs the shim refuses to push to. Empty = auto-detect the
  #   # remote's default branch via origin/HEAD (with a fallback to
  #   # [main, master] if detection fails). Explicit names ADD to the
  #   # detected set rather than replacing it — the default branch is
  #   # always protected.
  #   protected_branches: []
  #   # Refs the shim WILL allow pushing to. Default empty: push
  #   # nothing from inside the container. Add the session branch
  #   # (or a pattern your workflow needs) when you want preview /
  #   # CI runs to be triggerable from the agent. Exact-name match;
  #   # protected wins on overlap.
  #   allow_push_to: []

# git-bug | github | linear | jira
tracker: git-bug

agents:
  default: claude-code

workflows:
  default: standard

# autonomous:
#   # Bounds + cadence for the supervisor (`fleet autonomous run`,
#   # TUI Shift+A). Defaults shown.
#   # max_parallel: 3
#   # workflow: standard
#   # scan_interval_secs: 10
#   # spawn_cooldown_secs: 2
#   #
#   # Repo-wide label gate. Tickets must carry ALL of these labels to
#   # be picked by the supervisor, and the same set is unioned onto
#   # every ticket fleet creates (via `fleet issues create` or a
#   # `tracker-create` workflow node). Empty (the default) = no
#   # filtering and no creation-stamp.
#   # filter_labels: []

# orchestrator:
#   # Binary fleet exec's inside the tmux pane for `fleet orchestrator`.
#   # Override when your agent runtime is `claude-code`, `aider`, or a
#   # wrapper script. v1 doesn't support multi-word commands.
#   agent: claude
";

/// Default `.devcontainer/devcontainer.json`. Points at a sibling
/// Dockerfile that layers Node + the `claude-code` CLI on top of the
/// Microsoft Ubuntu base so the default `claude-code` agent works
/// out of the box; users who pick a different agent can swap the
/// install line in the Dockerfile.
///
/// `workspaceMount` is set explicitly because the devcontainer CLI's
/// default mount target is `/workspaces/${localWorkspaceFolderBasename}`
/// — for fleet's per-session worktrees that becomes `/workspaces/worktree`,
/// which doesn't match the `workspaceFolder: /workspace` cwd users
/// (and our agent prompts) expect. Pinning the mount keeps them
/// aligned: the bind mounts at /workspace, the agent's cwd is
/// /workspace, agent prompts can reference /workspace and find files.
const DEFAULT_DEVCONTAINER_JSON: &str = "\
{
  \"name\": \"fleet workspace\",
  \"build\": { \"dockerfile\": \"Dockerfile\" },
  \"workspaceMount\": \"source=${localWorkspaceFolder},target=/workspace,type=bind\",
  \"workspaceFolder\": \"/workspace\"
}
";

/// Default `.devcontainer/Dockerfile`. Ships Node.js LTS + the
/// `claude-code` CLI on `/usr/bin/claude` so the default agent
/// trampoline (`exec claude -p …` from `agent::registry`) actually
/// has something to exec. Built once per image, cached across
/// per-session containers — putting the install in `postCreateCommand`
/// would re-incur the ~30 s npm pull every time fleet spawns a
/// session, which autonomous mode does often.
const DEFAULT_DEVCONTAINER_DOCKERFILE: &str = "\
# fleet's default agent image: Ubuntu base + Node.js LTS +
# @anthropic-ai/claude-code on PATH. Swap the npm line for `pipx
# install aider-chat` etc. if you change `runtime.agent` in
# .fleet/config.yaml.
FROM mcr.microsoft.com/devcontainers/base:ubuntu

USER root
RUN curl -fsSL https://deb.nodesource.com/setup_lts.x | bash - \\
 && apt-get install -y --no-install-recommends nodejs \\
 && rm -rf /var/lib/apt/lists/* \\
 && npm install -g @anthropic-ai/claude-code \\
 && npm cache clean --force
";

/// `.fleet/workflows/standard.yaml` — the canonical planner → coder →
/// reviewer flow. Three agent nodes plus a bounded revision cycle and
/// a final `create-pr` step that only fires on an `approve` verdict.
/// The artifact contract chains `plan.md` from `plan` into
/// `implement`'s inputs; the reviewer's `outputs: { decision: … }`
/// feeds the `when:`-gated fork.
const DEFAULT_WORKFLOW_STANDARD: &str = r#"# Default `standard` workflow shipped by `fleet init`.
# Planner -> Coder -> Reviewer -> (Revise loop | Open PR), with a
# `file-dep` branch that fires when the implementer reports
# `outcome: blocked` and recommends a follow-up ticket.
#
# Implementer contract: write `implement.outputs.json` with at least
# {outcome, summary} and — when outcome is `blocked` and a new
# ticket would unblock — a `recommend_ticket: {title, body, labels}`
# payload. See .fleet/prompts/implementer.md.
#
# Reviewer contract: write `review.outputs.json` with a `decision`
# key whose value is `approve` or `changes_requested` (use any other
# string, e.g. `noop`, to skip both fork branches — useful when the
# implementer was blocked and there's no diff worth reviewing).
name: standard
description: Plan -> Code -> Review (+revise loop, +open PR on approve, +file-dep on blocked)
trigger:
  manual: true
  autonomous: true
max_recommended_tickets: 3
nodes:
  - id: plan
    agent: claude-code
    persona: planner
    prompt_file: .fleet/prompts/planner.md
    artifacts:
      out: [plan.md]
  - id: implement
    depends_on: [plan]
    agent: claude-code
    persona: implementer
    prompt_file: .fleet/prompts/implementer.md
    artifacts:
      in: [plan.md]
    outputs:
      outcome: outcome
      blocker: blocker
      recommend_ticket: recommend_ticket
  - id: review
    depends_on: [implement]
    agent: claude-code
    persona: reviewer
    prompt_file: .fleet/prompts/reviewer.md
    outputs:
      decision: decision
  - id: revise
    depends_on: [review]
    when: 'review.decision == "changes_requested"'
    agent: claude-code
    persona: implementer
    prompt_file: .fleet/prompts/implementer.md
    loop_back_to: review
    max_loops: 2
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == "approve"'
    type: create-pr
    title: '${FLEET_ISSUE_TITLE:-fleet change}'
    body: 'Automated PR for issue ${FLEET_ISSUE_HUMAN_ID:-?}'
  - id: file-dep
    depends_on: [implement]
    when: 'implement.outcome == "blocked" && implement.recommend_ticket != ""'
    type: tracker-create
    from: implement.recommend_ticket
    link_parent: true
"#;

/// `.fleet/workflows/hotfix.yaml` — minimal two-step flow for small
/// fixes. Skips the planner pass.
const DEFAULT_WORKFLOW_HOTFIX: &str = "\
# Default `hotfix` workflow shipped by `fleet init`.
# Quick fix — implement then review, no planner pass.
name: hotfix
description: Implement → Review (no planner)
trigger:
  manual: true
nodes:
  - id: implement
    agent: claude-code
    persona: implementer
    prompt_file: .fleet/prompts/implementer.md
  - id: review
    depends_on: [implement]
    agent: claude-code
    persona: reviewer
    prompt_file: .fleet/prompts/reviewer.md
";

/// `.fleet/workflows/review-only.yaml` — runs only the reviewer pass
/// over the current state of the workspace.
const DEFAULT_WORKFLOW_REVIEW_ONLY: &str = "\
# Default `review-only` workflow shipped by `fleet init`.
# Review the workspace's current state without making changes.
name: review-only
description: Reviewer pass only
trigger:
  manual: true
nodes:
  - id: review
    agent: claude-code
    persona: reviewer
    prompt_file: .fleet/prompts/reviewer.md
";

/// Default planner persona prompt. Surfaced to the agent as the
/// `FLEET_PROMPT` env var; users adapt to their team's style.
const DEFAULT_PROMPT_PLANNER: &str = "\
You are the planner.

Read the issue (it's in the environment as FLEET_ISSUE_TITLE and
FLEET_ISSUE_HUMAN_ID) and produce a short implementation plan as
`plan.md` under /artifacts. The plan should cover:

  - what files to touch and why
  - the testing strategy
  - any constraints (perf, compatibility, security)

Do NOT write code yet. Only the plan.
";

/// Default implementer persona prompt.
const DEFAULT_PROMPT_IMPLEMENTER: &str = r#"You are the implementer.

Read /artifacts/plan.md if present and implement the changes in the
workspace. Run the project's tests. Stage commits as you go; don't
push or open a PR.

If you deviate from the plan, note the deviation in your final
response so the reviewer can audit it.

When you finish, write `/artifacts/implement.outputs.json` with the
following shape (every declared key must be present — use `null` to
mean "doesn't apply this run"):

    {
      "outcome": "progress" | "blocked" | "done",
      "summary": "<one-line description of what happened>",
      "blocker": null OR "<why you're stuck, if outcome=blocked>",
      "recommend_ticket": null OR {
        "title": "<short ticket title>",
        "body":  "<paragraph describing the work>",
        "labels": ["<optional>", "<labels>"]
      }
    }

Conventions fleet's workflow engine enforces:

- `outcome` is required and must be one of the three strings above.
- `blocker` is required to be non-empty when `outcome` is `blocked`;
  otherwise set it to `null`.
- `recommend_ticket` is optional even when blocked. Only emit it
  when a NEW follow-up ticket would actually unblock the work —
  do NOT recommend tickets for transient environmental issues
  (flaky CI, temporary API outage, dependency build hiccup). The
  workflow caps recommendations per session (default 3) to keep
  runaway agents from spamming the tracker.

If your outcome is `blocked` and you emit a `recommend_ticket`,
fleet will file the new ticket via the tracker and cross-link both
issues automatically — you don't need to call any CLI yourself.
"#;

/// Default reviewer persona prompt.
const DEFAULT_PROMPT_REVIEWER: &str = r#"You are the reviewer.

Inspect the diff against the workspace's main branch (or the working
tree). Look for:

  - correctness bugs
  - missing test coverage
  - security issues (injection, secret handling, etc.)
  - mismatch with /artifacts/plan.md (if it exists)

Produce two artifacts under /artifacts:

  1. `review.md` — a short human-readable verdict and the specifics.
  2. `review.outputs.json` — a flat JSON object that fleet reads to
     steer the workflow. The standard workflow's `revise` /
     `open_pr` fork keys off `decision`:

         {"decision": "approve"}

     or

         {"decision": "changes_requested"}

     `approve` triggers `create-pr`; `changes_requested` triggers
     the bounded revise loop. Any other value falls through both
     branches — useful when the implementer was blocked and there's
     no real diff worth reviewing (write `{"decision": "noop"}` in
     that case so both forks skip cleanly).

Be concise.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_created(plan: &InitPlan, rel: &str) {
        let target = PathBuf::from(rel);
        assert!(
            plan.created.iter().any(|p| p == &target),
            "expected to create {rel}, got created = {:?}",
            plan.created
        );
    }

    fn assert_present(plan: &InitPlan, rel: &str) {
        let target = PathBuf::from(rel);
        assert!(
            plan.already_present.iter().any(|p| p == &target),
            "expected to leave {rel} alone, got already_present = {:?}",
            plan.already_present
        );
    }

    #[test]
    fn first_run_creates_full_layout() {
        let dir = tempfile::tempdir().unwrap();
        let plan = perform_init(dir.path()).unwrap();
        let root = dir.path();
        assert!(root.join(".fleet").is_dir());
        assert!(root.join(".fleet/workflows").is_dir());
        assert!(root.join(".fleet/sessions").is_dir());
        assert!(root.join(".fleet/.gitignore").is_file());
        assert!(root.join(".fleet/config.yaml").is_file());
        assert!(root.join(".devcontainer").is_dir());
        assert!(root.join(".devcontainer/devcontainer.json").is_file());
        assert!(root.join(".devcontainer/Dockerfile").is_file());

        assert_created(&plan, ".fleet");
        assert_created(&plan, ".fleet/workflows");
        assert_created(&plan, ".fleet/sessions");
        assert_created(&plan, ".fleet/.gitignore");
        assert_created(&plan, ".fleet/config.yaml");
        assert_created(&plan, ".devcontainer");
        assert_created(&plan, ".devcontainer/devcontainer.json");
        assert_created(&plan, ".devcontainer/Dockerfile");
        assert!(plan.already_present.is_empty());
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        perform_init(dir.path()).unwrap();
        let plan = perform_init(dir.path()).unwrap();
        assert!(
            plan.created.is_empty(),
            "second run created = {:?}",
            plan.created
        );
        // Every scaffolded path should show up as "already present".
        for expected in [
            ".fleet",
            ".fleet/workflows",
            ".fleet/sessions",
            ".fleet/.gitignore",
            ".fleet/config.yaml",
            ".devcontainer",
            ".devcontainer/devcontainer.json",
            ".devcontainer/Dockerfile",
        ] {
            assert_present(&plan, expected);
        }
    }

    #[test]
    fn preserves_existing_devcontainer_content() {
        let dir = tempfile::tempdir().unwrap();
        let dc_dir = dir.path().join(".devcontainer");
        fs::create_dir_all(&dc_dir).unwrap();
        let custom = r#"{ "image": "user-authored:latest" }"#;
        fs::write(dc_dir.join("devcontainer.json"), custom).unwrap();

        let plan = perform_init(dir.path()).unwrap();

        let on_disk = fs::read_to_string(dc_dir.join("devcontainer.json")).unwrap();
        assert_eq!(on_disk, custom, "user content must survive init");
        assert_present(&plan, ".devcontainer/devcontainer.json");
    }

    #[test]
    fn preserves_existing_config_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let fd = dir.path().join(".fleet");
        fs::create_dir_all(&fd).unwrap();
        let custom = "runtime:\n  adapter: docker\n";
        fs::write(fd.join("config.yaml"), custom).unwrap();

        perform_init(dir.path()).unwrap();
        let on_disk = fs::read_to_string(fd.join("config.yaml")).unwrap();
        assert_eq!(on_disk, custom);
    }

    #[test]
    fn default_config_yaml_is_loadable_yaml() {
        // The template ships with users; if it grows a syntax error
        // the next `fleet init` plants broken state in every new repo.
        let parsed: serde_yml::Value =
            serde_yml::from_str(DEFAULT_FLEET_CONFIG_YAML).expect("default config.yaml must parse");
        // Spot-check a known key to catch dramatic regressions.
        assert!(parsed.get("runtime").is_some(), "runtime: section missing");
    }

    #[test]
    fn default_workflows_parse_and_validate() {
        use crate::workflow::spec::Workflow;
        use crate::workflow::validate::validate;
        for (name, body) in [
            ("standard", DEFAULT_WORKFLOW_STANDARD),
            ("hotfix", DEFAULT_WORKFLOW_HOTFIX),
            ("review-only", DEFAULT_WORKFLOW_REVIEW_ONLY),
        ] {
            let wf = Workflow::from_str_at(body, format!("/{name}.yaml"))
                .unwrap_or_else(|e| panic!("default workflow `{name}` must parse: {e:#}"));
            validate(&wf)
                .unwrap_or_else(|e| panic!("default workflow `{name}` must validate: {e:#}"));
        }
    }

    #[test]
    fn default_standard_workflow_wires_the_when_outputs_fork() {
        // The shipped `standard.yaml` is the canonical worked example
        // of `outputs:` + `when:` + `loop_back_to`. If a future edit
        // drops any of those, the standard flow silently degrades
        // back to "run every branch unconditionally". Catch it here.
        use crate::workflow::spec::{NodeKind, Workflow};
        let wf = Workflow::from_str_at(DEFAULT_WORKFLOW_STANDARD, "/standard.yaml").unwrap();

        let review = wf.node("review").expect("review node");
        assert_eq!(
            review.outputs.get("decision").map(String::as_str),
            Some("decision"),
            "review must declare outputs: {{ decision: decision }}"
        );

        let revise = wf.node("revise").expect("revise node");
        assert_eq!(
            revise.when.as_deref(),
            Some("review.decision == \"changes_requested\""),
            "revise must gate on changes_requested"
        );
        assert_eq!(revise.loop_back_to.as_deref(), Some("review"));
        assert_eq!(revise.max_loops, Some(2));

        let open_pr = wf.node("open_pr").expect("open_pr node");
        assert_eq!(
            open_pr.when.as_deref(),
            Some("review.decision == \"approve\""),
            "open_pr must gate on approve"
        );
        // Migrated from the legacy `gh pr create` bash hack to the
        // first-class CreatePr NodeKind in stage 5 of the loop-
        // scheduler / PR-aware feature.
        assert!(
            matches!(open_pr.kind, NodeKind::CreatePr { .. }),
            "open_pr should use the CreatePr NodeKind; got {:?}",
            open_pr.kind,
        );
    }

    #[test]
    fn default_standard_workflow_wires_the_blocked_handler_branch() {
        // Phase 2: an implementer outcome of `blocked` should route
        // into a `tracker-create` follow-up. Lock in the exact wiring
        // so a future regression doesn't silently strip the branch.
        use crate::workflow::spec::{NodeKind, Workflow};
        let wf = Workflow::from_str_at(DEFAULT_WORKFLOW_STANDARD, "/standard.yaml").unwrap();

        let implement = wf.node("implement").expect("implement node");
        assert_eq!(
            implement.outputs.get("outcome").map(String::as_str),
            Some("outcome"),
            "implement must declare outputs.outcome to join the convention"
        );
        assert_eq!(
            implement.outputs.get("blocker").map(String::as_str),
            Some("blocker"),
        );
        assert_eq!(
            implement
                .outputs
                .get("recommend_ticket")
                .map(String::as_str),
            Some("recommend_ticket"),
        );

        let file_dep = wf.node("file-dep").expect("file-dep node");
        assert_eq!(
            file_dep.when.as_deref(),
            Some("implement.outcome == \"blocked\" && implement.recommend_ticket != \"\""),
            "file-dep must gate on outcome=blocked AND non-empty recommend_ticket"
        );
        match &file_dep.kind {
            NodeKind::TrackerCreate { from, link_parent } => {
                assert_eq!(from, "implement.recommend_ticket");
                assert!(*link_parent, "link_parent should default to true here");
            }
            other => panic!("expected TrackerCreate, got {other:?}"),
        }

        // The workflow declares the cap explicitly to make the
        // 3-ticket runaway-protection story visible to anyone
        // reading the YAML, even though the default is the same.
        assert_eq!(wf.max_recommended_tickets, Some(3));
    }

    #[test]
    fn default_implementer_prompt_documents_the_outcome_contract() {
        // The implementer is the source of `implement.outputs.json`.
        // The shipped prompt must spell out the contract — the
        // outcome strings, the required-iff-blocked blocker, and
        // the optional recommend_ticket shape — or agents will
        // emit subtly wrong shapes and the workflow's blocked
        // branch becomes a guessing game.
        assert!(
            DEFAULT_PROMPT_IMPLEMENTER.contains("implement.outputs.json"),
            "implementer prompt must mention implement.outputs.json"
        );
        for outcome in ["progress", "blocked", "done"] {
            assert!(
                DEFAULT_PROMPT_IMPLEMENTER.contains(outcome),
                "implementer prompt must document outcome value `{outcome}`"
            );
        }
        assert!(
            DEFAULT_PROMPT_IMPLEMENTER.contains("recommend_ticket"),
            "implementer prompt must document the recommend_ticket shape"
        );
        assert!(
            DEFAULT_PROMPT_IMPLEMENTER.contains("blocker"),
            "implementer prompt must document the blocker field"
        );
    }

    #[test]
    fn default_reviewer_prompt_documents_the_outputs_contract() {
        // The reviewer agent is the source of `review.outputs.json`.
        // The shipped prompt must spell out that contract or the
        // fork-via-when downstream of `review` becomes a guessing game.
        assert!(
            DEFAULT_PROMPT_REVIEWER.contains("review.outputs.json"),
            "reviewer prompt must mention review.outputs.json"
        );
        assert!(
            DEFAULT_PROMPT_REVIEWER.contains("decision")
                && DEFAULT_PROMPT_REVIEWER.contains("approve")
                && DEFAULT_PROMPT_REVIEWER.contains("changes_requested"),
            "reviewer prompt must document the `decision` values"
        );
    }

    #[test]
    fn default_workflows_reference_claude_code_agent() {
        // The default agent registry includes `claude-code` out of the
        // box; the workflows we ship must reference it (otherwise the
        // first `fleet workflow run` errors with an unknown-agent
        // message — terrible first-run UX).
        for (name, body) in [
            ("standard", DEFAULT_WORKFLOW_STANDARD),
            ("hotfix", DEFAULT_WORKFLOW_HOTFIX),
            ("review-only", DEFAULT_WORKFLOW_REVIEW_ONLY),
        ] {
            assert!(
                body.contains("agent: claude-code"),
                "{name} must reference claude-code"
            );
        }
    }

    #[test]
    fn default_prompts_are_nonempty_markdown() {
        for (name, body) in [
            ("planner.md", DEFAULT_PROMPT_PLANNER),
            ("implementer.md", DEFAULT_PROMPT_IMPLEMENTER),
            ("reviewer.md", DEFAULT_PROMPT_REVIEWER),
        ] {
            assert!(!body.trim().is_empty(), "prompt `{name}` must be non-empty");
            // Loose sanity: every default prompt should mention what
            // persona it's for so a user editing it knows immediately.
            let body_lower = body.to_lowercase();
            let role = match name {
                "planner.md" => "planner",
                "implementer.md" => "implementer",
                "reviewer.md" => "reviewer",
                _ => unreachable!(),
            };
            assert!(
                body_lower.contains(role),
                "prompt `{name}` must mention its persona (`{role}`)"
            );
        }
    }

    #[test]
    fn first_run_creates_default_workflows_and_prompts() {
        let dir = tempfile::tempdir().unwrap();
        perform_init(dir.path()).unwrap();
        let root = dir.path();
        assert!(root.join(".fleet/workflows/standard.yaml").is_file());
        assert!(root.join(".fleet/workflows/hotfix.yaml").is_file());
        assert!(root.join(".fleet/workflows/review-only.yaml").is_file());
        assert!(root.join(".fleet/prompts/planner.md").is_file());
        assert!(root.join(".fleet/prompts/implementer.md").is_file());
        assert!(root.join(".fleet/prompts/reviewer.md").is_file());
    }

    #[test]
    fn default_devcontainer_json_is_loadable_json() {
        let parsed: serde_json::Value = serde_json::from_str(DEFAULT_DEVCONTAINER_JSON)
            .expect("default devcontainer.json must parse");
        assert_eq!(
            parsed.get("name").and_then(|v| v.as_str()),
            Some("fleet workspace")
        );
        // The template builds from a sibling Dockerfile rather than
        // pulling a bare base image — the Dockerfile is where the
        // claude-code install layers in.
        assert_eq!(
            parsed
                .pointer("/build/dockerfile")
                .and_then(|v| v.as_str()),
            Some("Dockerfile"),
        );
    }

    #[test]
    fn default_devcontainer_dockerfile_installs_claude_code_on_path() {
        // The shipped Dockerfile is the reason the default
        // `claude-code` agent works out of the box — if this
        // regresses, every new `fleet init` plants a container in
        // which `exec claude` fails with "command not found".
        let dockerfile = DEFAULT_DEVCONTAINER_DOCKERFILE;
        assert!(
            dockerfile.starts_with("# fleet")
                && dockerfile.contains("FROM mcr.microsoft.com/devcontainers/base:ubuntu"),
            "Dockerfile must extend the Ubuntu devcontainer base",
        );
        assert!(
            dockerfile.contains("npm install -g @anthropic-ai/claude-code"),
            "Dockerfile must install the claude-code CLI",
        );
    }

    #[test]
    fn render_summary_lists_created_paths_with_plus_marker() {
        let plan = InitPlan {
            created: vec![PathBuf::from(".fleet"), PathBuf::from(".fleet/config.yaml")],
            already_present: Vec::new(),
        };
        let rendered = render_init_summary(Path::new("/repo"), &plan);
        assert!(rendered.contains("fleet init: /repo"));
        assert!(rendered.contains("+ .fleet\n"));
        assert!(rendered.contains("+ .fleet/config.yaml\n"));
        assert!(!rendered.contains("Already present"));
    }

    #[test]
    fn render_summary_lists_present_paths_with_dot_marker() {
        let plan = InitPlan {
            created: Vec::new(),
            already_present: vec![PathBuf::from(".fleet")],
        };
        let rendered = render_init_summary(Path::new("/repo"), &plan);
        assert!(rendered.contains("Already present"));
        assert!(rendered.contains("· .fleet\n"));
        assert!(!rendered.contains("Created:"));
        assert!(!rendered.contains("Next steps"));
    }

    #[test]
    fn render_summary_handles_empty_plan() {
        let rendered = render_init_summary(Path::new("/r"), &InitPlan::default());
        assert!(rendered.contains("(nothing to do)"));
    }

    #[test]
    fn render_summary_offers_next_steps_only_after_creation() {
        let plan = InitPlan {
            created: vec![PathBuf::from(".fleet")],
            already_present: Vec::new(),
        };
        let rendered = render_init_summary(Path::new("/r"), &plan);
        assert!(rendered.contains("Next steps"));
        assert!(rendered.contains("fleet runtime doctor"));
    }

    #[test]
    fn relativise_strips_root_prefix() {
        let p = Path::new("/r/.fleet/config.yaml");
        let r = Path::new("/r");
        assert_eq!(relativise(p, r), PathBuf::from(".fleet/config.yaml"));
    }

    #[test]
    fn relativise_returns_absolute_when_not_under_root() {
        let p = Path::new("/elsewhere/file");
        let r = Path::new("/r");
        assert_eq!(relativise(p, r), PathBuf::from("/elsewhere/file"));
    }

    // Test helpers for bootstrap-section rendering: pure
    // ProbeReport fixtures, no need for a real probe.
    fn missing(kind: crate::runtime::detect::BackendKind) -> crate::runtime::detect::BackendStatus {
        // Reach into detect's status shape via a value-only path —
        // BackendStatus is `pub`, fields are `pub`. This mirrors the
        // helper in cli::runtime tests; duplicating to keep the test
        // module self-contained.
        crate::runtime::detect::BackendStatus {
            kind,
            present: false,
            version: None,
            notes: Vec::new(),
        }
    }

    fn present_b(
        kind: crate::runtime::detect::BackendKind,
        v: &str,
    ) -> crate::runtime::detect::BackendStatus {
        crate::runtime::detect::BackendStatus {
            kind,
            present: true,
            version: Some(v.to_string()),
            notes: Vec::new(),
        }
    }

    fn report_with_all_missing() -> ProbeReport {
        use crate::runtime::detect::BackendKind as K;
        ProbeReport {
            podman: missing(K::Podman),
            docker: missing(K::Docker),
            apple_container: missing(K::AppleContainer),
            gvisor: missing(K::GVisor),
            devcontainer_cli: missing(K::DevcontainerCli),
            git_bug: missing(K::GitBug),
            tinyproxy: missing(K::Tinyproxy),
            tmux: missing(K::Tmux),
            recommended: None,
        }
    }

    #[test]
    fn bootstrap_section_is_empty_when_everything_present() {
        use crate::runtime::detect::BackendKind as K;
        let r = ProbeReport {
            podman: present_b(K::Podman, "5.0.1"),
            docker: missing(K::Docker),
            apple_container: missing(K::AppleContainer),
            gvisor: missing(K::GVisor),
            devcontainer_cli: present_b(K::DevcontainerCli, "0.1.12"),
            git_bug: present_b(K::GitBug, "0.10.0"),
            tinyproxy: present_b(K::Tinyproxy, "1.11.1"),
            tmux: present_b(K::Tmux, "3.4"),
            recommended: Some(K::Podman),
        };
        assert_eq!(render_bootstrap_section(&r, "linux"), String::new());
    }

    #[test]
    fn bootstrap_section_macos_emits_brew_commands() {
        let r = report_with_all_missing();
        let s = render_bootstrap_section(&r, "macos");
        assert!(s.contains("Bootstrap your environment:"));
        assert!(s.contains("brew install podman"));
        assert!(s.contains("brew install git-bug"));
        assert!(s.contains("brew install tinyproxy"));
        // Linux-only commands should NOT appear on macOS.
        assert!(!s.contains("apt install tinyproxy"));
        assert!(!s.contains("dnf install podman"));
    }

    #[test]
    fn bootstrap_section_linux_emits_apt_dnf_commands() {
        let r = report_with_all_missing();
        let s = render_bootstrap_section(&r, "linux");
        assert!(s.contains("sudo dnf install podman"));
        assert!(s.contains("sudo apt install tinyproxy"));
        // macOS-only commands should NOT appear on Linux.
        assert!(!s.contains("brew install podman"));
        assert!(!s.contains("brew install tinyproxy"));
    }

    #[test]
    fn bootstrap_section_each_hint_carries_tool_command_and_note() {
        let r = report_with_all_missing();
        let s = render_bootstrap_section(&r, "linux");
        // Three-line shape per hint: `✗ tool`, `    command`, `    (note)`.
        assert!(s.contains("✗ container engine"));
        assert!(s.contains("(fleet cannot start a container without one of these)"));
        assert!(s.contains("✗ devcontainer CLI"));
        assert!(s.contains("(required — fleet builds images via this CLI)"));
        assert!(s.contains("✗ git-bug"));
        assert!(s.contains("(only needed when `tracker: git-bug`"));
        assert!(s.contains("✗ tinyproxy"));
        assert!(s.contains("(only needed on macOS with `policy: allowlist`"));
    }

    #[test]
    fn bootstrap_section_devcontainer_cli_command_is_os_agnostic() {
        // cargo install / npm install -g work on any OS; the hint is
        // the same regardless of platform.
        let r = report_with_all_missing();
        let linux = render_bootstrap_section(&r, "linux");
        let mac = render_bootstrap_section(&r, "macos");
        for s in [&linux, &mac] {
            assert!(s.contains("cargo install devcontainer"));
        }
    }
}
