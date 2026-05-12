//! `agent-orchestrator.yaml` parsing.
//!
//! Strongly-typed view of the subset of AO's config fleet cares about
//! today — `defaults`, `projects` (name / path / branch / tracker), and
//! a tiny shell of `reactions` / `plugins` so we can present + edit
//! them later. Unknown fields are preserved as raw [`serde_yml::Value`]
//! so a round-trip doesn't drop schema-rich keys we haven't modelled
//! yet (e.g. fleet hasn't surfaced `notifiers` in the UI, but losing
//! those entries on save would be obnoxious).
//!
//! Read-only for the moment; the eventual writeback path lives in a
//! sibling commit and uses [`save`] which is currently absent. Comments
//! are not preserved by `serde_yml` — callers planning to write back
//! should confirm with the user before clobbering hand-written notes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The agent fleet's `start` flow optimizes for. Mirrors what AO's
/// own enum is named; we keep it as a free string so unrecognised
/// values from a newer AO release don't crash the parser — they just
/// fall through to `passthrough` in [`Self::auth_mode_hint`].
pub const AGENT_CLAUDE_CODE: &str = "claude-code";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AoConfig {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
    /// Unknown / not-yet-modelled top-level keys (`reactions`,
    /// `plugins`, `notifiers`, …). Round-tripped verbatim so save
    /// doesn't drop them; the config UI will surface model'd
    /// sections in their own panels and leave these alone until we
    /// extend the schema.
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_yml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifiers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    #[serde(rename = "sessionPrefix", default, skip_serializing_if = "Option::is_none")]
    pub session_prefix: Option<String>,
    pub path: PathBuf,
    #[serde(rename = "defaultBranch", default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(rename = "agentRulesFile", default, skip_serializing_if = "Option::is_none")]
    pub agent_rules_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<Tracker>,
    #[serde(rename = "postCreate", default, skip_serializing_if = "Vec::is_empty")]
    pub post_create: Vec<String>,
    /// Other per-project keys we haven't surfaced yet.
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_yml::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tracker {
    pub plugin: String,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_yml::Value>,
}

impl AoConfig {
    /// Where a *new* AO config should land when nothing exists yet.
    /// XDG-style central catalog so the same file drives every fleet
    /// instance the user runs, regardless of which repo they're in.
    pub fn default_xdg_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("fleet").join("agent-orchestrator.yaml"))
    }

    /// Resolve the path fleet should read from, in order of precedence:
    ///
    /// 1. `$XDG_CONFIG_HOME/fleet/agent-orchestrator.yaml` (or
    ///    `~/.config/fleet/…`) — the central catalog. Wins when
    ///    present; lets one yaml drive many per-repo fleet instances.
    /// 2. `<repo_root>/agent-orchestrator.yaml` — the historical
    ///    per-repo location. Kept as a fallback so existing setups
    ///    keep working until users migrate.
    ///
    /// Returns `None` when neither file exists — caller should treat
    /// that as "nothing configured yet" rather than an error.
    pub fn resolve_path(repo_root: &Path) -> Option<PathBuf> {
        if let Some(xdg) = Self::default_xdg_path()
            && xdg.is_file()
        {
            return Some(xdg);
        }
        let repo_path = repo_root.join("agent-orchestrator.yaml");
        if repo_path.is_file() {
            return Some(repo_path);
        }
        None
    }

    /// Parse the on-disk yaml. Returns `Ok(None)` when neither the
    /// XDG nor the repo-local file exists. Anything else (parse error,
    /// IO error) bubbles as `Err` with context. The successful return
    /// includes the *path* the config was loaded from so callers can
    /// save back to the same file.
    pub fn load(repo_root: &Path) -> Result<Option<(PathBuf, Self)>> {
        let Some(path) = Self::resolve_path(repo_root) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let parsed: Self = serde_yml::from_str(&text)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(Some((path, parsed)))
    }

    /// Effective agent name. Looks at the `Self::defaults.agent` field;
    /// returns `None` when neither defaults nor the file set one (AO
    /// falls back to claude-code internally, but fleet treats "not set"
    /// distinctly so the config UI can show "(default)" instead of
    /// pretending the user chose it).
    pub fn agent(&self) -> Option<&str> {
        self.defaults.agent.as_deref()
    }

    /// Find the project whose `path` is an ancestor of (or equal to)
    /// `cwd`. Used by fleet to scope the per-instance UI to one
    /// project based on where the user launched it from.
    ///
    /// When multiple projects could match (nested project paths,
    /// rare in practice), the deepest one wins so a sub-project takes
    /// precedence over its parent. Paths are canonicalised so a
    /// symlinked checkout still matches.
    pub fn project_for_cwd(&self, cwd: &Path) -> Option<&str> {
        let Ok(cwd_real) = cwd.canonicalize() else {
            return None;
        };
        let mut best: Option<(usize, &str)> = None;
        for (key, project) in &self.projects {
            let Ok(p_real) = project.path.canonicalize() else {
                continue;
            };
            if cwd_real.starts_with(&p_real) {
                let depth = p_real.components().count();
                if best.is_none_or(|(d, _)| depth > d) {
                    best = Some((depth, key.as_str()));
                }
            }
        }
        best.map(|(_, k)| k)
    }
}

/// Heuristic mapping from a free-form agent name to the auth flow
/// fleet's `start` should run. Used by [`crate::config::AgentAuthMode::Auto`].
///
/// `claude-code` → `claude-oauth` (writes credentials file, scrubs env).
/// Anything else → `passthrough` (forwards provider env vars, no
/// claude-specific prep). Unset / unparseable AO config also yields
/// `claude-oauth` for backwards compatibility — that's the historical
/// behaviour and keeps existing setups working.
pub fn auth_mode_for_agent(agent: Option<&str>) -> AuthModeHint {
    match agent {
        Some(name) if name == AGENT_CLAUDE_CODE => AuthModeHint::ClaudeOauth,
        Some(_) => AuthModeHint::Passthrough,
        None => AuthModeHint::ClaudeOauth,
    }
}

/// Two-state hint returned by [`auth_mode_for_agent`]. Mirrors the
/// concrete variants of [`crate::config::AgentAuthMode`] minus `Auto`
/// itself, since this function is the *resolver* — it can't return
/// "auto, figure it out yourself."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthModeHint {
    ClaudeOauth,
    Passthrough,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimum_yaml() {
        let yaml = "port: 3000\n";
        let cfg: AoConfig = serde_yml::from_str(yaml).expect("parse");
        assert_eq!(cfg.port, Some(3000));
        assert!(cfg.defaults.agent.is_none());
    }

    #[test]
    fn parses_full_sample() {
        // Modelled on the actual agent-orchestrator.yaml shipped in
        // this repo, including unknown top-level keys (`reactions`,
        // `plugins`) we expect to round-trip via `extra`.
        let yaml = r"
port: 3000
defaults:
  runtime: tmux
  agent: claude-code
  workspace: worktree
  notifiers: []
projects:
  sandbox:
    name: sandbox
    sessionPrefix: sb
    path: /tmp/sandbox
    defaultBranch: main
    tracker:
      plugin: git-bug
reactions:
  approved-and-green:
    auto: false
";
        let cfg: AoConfig = serde_yml::from_str(yaml).expect("parse");
        assert_eq!(cfg.defaults.agent.as_deref(), Some("claude-code"));
        assert_eq!(cfg.defaults.runtime.as_deref(), Some("tmux"));
        let sandbox = cfg.projects.get("sandbox").expect("sandbox project");
        assert_eq!(sandbox.name, "sandbox");
        assert_eq!(sandbox.session_prefix.as_deref(), Some("sb"));
        assert_eq!(sandbox.tracker.as_ref().map(|t| t.plugin.as_str()), Some("git-bug"));
        // Unknown top-level key preserved.
        assert!(cfg.extra.contains_key("reactions"));
    }

    #[test]
    fn auth_mode_hint_claude_code() {
        assert_eq!(auth_mode_for_agent(Some("claude-code")), AuthModeHint::ClaudeOauth);
    }

    #[test]
    fn auth_mode_hint_codex_is_passthrough() {
        assert_eq!(auth_mode_for_agent(Some("codex")), AuthModeHint::Passthrough);
    }

    #[test]
    fn auth_mode_hint_aider_is_passthrough() {
        assert_eq!(auth_mode_for_agent(Some("aider")), AuthModeHint::Passthrough);
    }

    #[test]
    fn auth_mode_hint_unset_defaults_to_claude_oauth() {
        // Backwards compat: a missing or unreadable AO config keeps
        // the historical behaviour for users who haven't migrated.
        assert_eq!(auth_mode_for_agent(None), AuthModeHint::ClaudeOauth);
    }

    #[test]
    fn project_for_cwd_matches_exact_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().join("alpha");
        std::fs::create_dir_all(&project_dir).unwrap();

        let cfg = AoConfig {
            schema: None,
            port: None,
            defaults: Defaults::default(),
            projects: BTreeMap::from([(
                "alpha".to_string(),
                Project {
                    name: "alpha".into(),
                    session_prefix: None,
                    path: project_dir.clone(),
                    default_branch: None,
                    agent_rules_file: None,
                    agent: None,
                    tracker: None,
                    post_create: Vec::new(),
                    extra: BTreeMap::new(),
                },
            )]),
            extra: BTreeMap::new(),
        };
        assert_eq!(cfg.project_for_cwd(&project_dir), Some("alpha"));
    }

    #[test]
    fn project_for_cwd_matches_subdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().join("alpha");
        let sub_dir = project_dir.join("src/inner");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let cfg = AoConfig {
            schema: None,
            port: None,
            defaults: Defaults::default(),
            projects: BTreeMap::from([(
                "alpha".to_string(),
                Project {
                    name: "alpha".into(),
                    session_prefix: None,
                    path: project_dir,
                    default_branch: None,
                    agent_rules_file: None,
                    agent: None,
                    tracker: None,
                    post_create: Vec::new(),
                    extra: BTreeMap::new(),
                },
            )]),
            extra: BTreeMap::new(),
        };
        // cwd is a subdir of the project path — the project still
        // matches, since "you're working inside it" is the right
        // interpretation.
        assert_eq!(cfg.project_for_cwd(&sub_dir), Some("alpha"));
    }

    #[test]
    fn project_for_cwd_returns_none_when_outside() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().join("alpha");
        let unrelated = tmp.path().join("beta");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();

        let cfg = AoConfig {
            schema: None,
            port: None,
            defaults: Defaults::default(),
            projects: BTreeMap::from([(
                "alpha".to_string(),
                Project {
                    name: "alpha".into(),
                    session_prefix: None,
                    path: project_dir,
                    default_branch: None,
                    agent_rules_file: None,
                    agent: None,
                    tracker: None,
                    post_create: Vec::new(),
                    extra: BTreeMap::new(),
                },
            )]),
            extra: BTreeMap::new(),
        };
        assert_eq!(cfg.project_for_cwd(&unrelated), None);
    }

    #[test]
    fn project_for_cwd_picks_deepest_match() {
        // Nested projects: parent + child. cwd inside child should
        // resolve to the child, not the parent.
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let child = parent.join("nested-child");
        std::fs::create_dir_all(&child).unwrap();

        let cfg = AoConfig {
            schema: None,
            port: None,
            defaults: Defaults::default(),
            projects: BTreeMap::from([
                (
                    "parent".to_string(),
                    Project {
                        name: "parent".into(),
                        session_prefix: None,
                        path: parent.clone(),
                        default_branch: None,
                        agent_rules_file: None,
                        agent: None,
                        tracker: None,
                        post_create: Vec::new(),
                        extra: BTreeMap::new(),
                    },
                ),
                (
                    "child".to_string(),
                    Project {
                        name: "child".into(),
                        session_prefix: None,
                        path: child.clone(),
                        default_branch: None,
                        agent_rules_file: None,
                        agent: None,
                        tracker: None,
                        post_create: Vec::new(),
                        extra: BTreeMap::new(),
                    },
                ),
            ]),
            extra: BTreeMap::new(),
        };
        assert_eq!(cfg.project_for_cwd(&child), Some("child"));
    }
}
