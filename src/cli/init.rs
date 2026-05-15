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

use crate::repo;

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

/// CLI entry point. Resolves the fleet root via [`crate::repo::fleet_root`],
/// scaffolds, prints the summary, returns exit code 0.
pub fn run() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let plan = perform_init(&root)?;
    print!("{}", render_init_summary(&root, &plan));
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
        fs::write(path, contents)
            .with_context(|| format!("writing {}", path.display()))?;
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

# git-bug | github | linear | jira
tracker: git-bug

agents:
  default: claude-code

workflows:
  default: standard
";

/// Default `.devcontainer/devcontainer.json`. Minimal so it always
/// builds; the user is expected to edit it for their project.
const DEFAULT_DEVCONTAINER_JSON: &str = "\
{
  \"name\": \"fleet workspace\",
  \"image\": \"mcr.microsoft.com/devcontainers/base:ubuntu\",
  \"workspaceFolder\": \"/workspace\"
}
";

/// `.fleet/workflows/standard.yaml` — the canonical planner → coder →
/// reviewer flow. Three agent nodes with persona + `prompt_file` wired;
/// the artifact contract chains `plan.md` from `plan` into `implement`'s
/// inputs. Review opens unbounded for now — `loop_back_to` + gates land
/// in the executor in subsequent commits.
const DEFAULT_WORKFLOW_STANDARD: &str = "\
# Default `standard` workflow shipped by `fleet init`.
# Planner → Coder → Reviewer. Edit freely.
name: standard
description: Plan → Code → Review
trigger:
  manual: true
  autonomous: true
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
  - id: review
    depends_on: [implement]
    agent: claude-code
    persona: reviewer
    prompt_file: .fleet/prompts/reviewer.md
";

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
const DEFAULT_PROMPT_IMPLEMENTER: &str = "\
You are the implementer.

Read /artifacts/plan.md if present and implement the changes in the
workspace. Run the project's tests. Stage commits as you go; don't
push or open a PR.

If you deviate from the plan, note the deviation in your final
response so the reviewer can audit it.
";

/// Default reviewer persona prompt.
const DEFAULT_PROMPT_REVIEWER: &str = "\
You are the reviewer.

Inspect the diff against the workspace's main branch (or the working
tree). Look for:

  - correctness bugs
  - missing test coverage
  - security issues (injection, secret handling, etc.)
  - mismatch with /artifacts/plan.md (if it exists)

Produce `review.md` under /artifacts with a short verdict
(approve / changes_requested) and the specifics. Be concise.
";

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

        assert_created(&plan, ".fleet");
        assert_created(&plan, ".fleet/workflows");
        assert_created(&plan, ".fleet/sessions");
        assert_created(&plan, ".fleet/.gitignore");
        assert_created(&plan, ".fleet/config.yaml");
        assert_created(&plan, ".devcontainer");
        assert_created(&plan, ".devcontainer/devcontainer.json");
        assert!(plan.already_present.is_empty());
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        perform_init(dir.path()).unwrap();
        let plan = perform_init(dir.path()).unwrap();
        assert!(plan.created.is_empty(), "second run created = {:?}", plan.created);
        // Every scaffolded path should show up as "already present".
        for expected in [
            ".fleet",
            ".fleet/workflows",
            ".fleet/sessions",
            ".fleet/.gitignore",
            ".fleet/config.yaml",
            ".devcontainer",
            ".devcontainer/devcontainer.json",
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
        assert_eq!(parsed.get("name").and_then(|v| v.as_str()), Some("fleet workspace"));
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
}
