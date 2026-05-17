//! Per-agent argv construction for `fleet brainstorm`.
//!
//! Fleet writes a fully-rendered system prompt to disk at session
//! spawn time (see [`super::prompt`]), but each supported agent
//! takes that prompt through a different CLI surface:
//!
//! - **Claude Code** (`claude`, `claude-code`) — `--append-system-prompt <body>`
//!   keeps the agent's built-in scaffolding and appends fleet's
//!   role + tool docs.
//! - **Aider** (`aider`) — no `--system-prompt` flag exists; the
//!   idiomatic path is `--read <file>` against a read-only
//!   conventions doc, so we point it at the prompt file we already
//!   wrote.
//! - **Codex** (`codex`) — `-c developer_instructions=<body>`
//!   appends developer-role instructions; chosen over
//!   `model_instructions_file` (which *replaces* codex's built-in
//!   prompt) so the agent's default safety scaffolding stays in
//!   place.
//! - **Anything else** — fleet doesn't know how to wire the prompt
//!   in. We spawn the binary bare and warn on stderr so the user
//!   can either configure their agent to read
//!   `$FLEET_BRAINSTORM_PROMPT` themselves or switch to a
//!   supported agent.
//!
//! Pure: each [`AgentLauncher::argv`] is a function of inputs only,
//! so tests pin the exact argv per agent without spawning anything.

use std::path::Path;

/// Strategy for turning the brainstorm `agent` name + the rendered
/// prompt into the argv passed to `tmux new-session -- …`.
pub trait AgentLauncher {
    /// Build the argv. `agent` is the resolved binary name (the
    /// caller has already done the basename match); `prompt_path`
    /// is the on-disk system prompt; `prompt_body` is the same
    /// content in memory.
    fn argv(&self, agent: &str, prompt_path: &Path, prompt_body: &str) -> Vec<String>;

    /// Optional stderr warning printed at spawn time. Used by
    /// [`BareLauncher`] to flag that the prompt won't auto-deliver
    /// for unrecognised agents.
    fn unsupported_warning(&self) -> Option<String> {
        None
    }
}

/// `claude` / `claude-code` — `--append-system-prompt <body>`.
pub struct ClaudeLauncher;

impl AgentLauncher for ClaudeLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "--append-system-prompt".to_string(),
            prompt_body.to_string(),
        ]
    }
}

/// `aider` — `--read <prompt_path>`. Aider has no inline
/// system-prompt flag, so we lean on the file fleet already wrote.
pub struct AiderLauncher;

impl AgentLauncher for AiderLauncher {
    fn argv(&self, agent: &str, prompt_path: &Path, _prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "--read".to_string(),
            prompt_path.display().to_string(),
        ]
    }
}

/// `codex` — `-c developer_instructions=<body>` for append
/// semantics. The replace-the-builtin alternative
/// (`model_instructions_file`) is deliberately not used: keeping
/// codex's own safety scaffolding is less surprising.
pub struct CodexLauncher;

impl AgentLauncher for CodexLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "-c".to_string(),
            format!("developer_instructions={prompt_body}"),
        ]
    }
}

/// Fallback for agents fleet doesn't know how to wire. Spawns the
/// binary with no args and warns that the rendered prompt won't be
/// auto-delivered — the user can still wire their agent to read
/// `$FLEET_BRAINSTORM_PROMPT`.
pub struct BareLauncher;

impl AgentLauncher for BareLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, _prompt_body: &str) -> Vec<String> {
        vec![agent.to_string()]
    }

    fn unsupported_warning(&self) -> Option<String> {
        Some(
            "fleet doesn't know how to inject the brainstorm system prompt for this agent. \
             Configure the agent to read $FLEET_BRAINSTORM_PROMPT itself, or use claude / \
             claude-code / aider / codex."
                .to_string(),
        )
    }
}

/// Dispatch on the agent's basename. Strips any directory prefix
/// so a `brainstorm.agent: /opt/claude/bin/claude` still resolves
/// to [`ClaudeLauncher`].
#[must_use]
pub fn launcher_for(agent: &str) -> Box<dyn AgentLauncher> {
    match agent_basename(agent) {
        "claude" | "claude-code" => Box::new(ClaudeLauncher),
        "aider" => Box::new(AiderLauncher),
        "codex" => Box::new(CodexLauncher),
        _ => Box::new(BareLauncher),
    }
}

/// Last path component of `agent`, with no extension trimming
/// (Windows isn't a target). Pure helper exposed for tests.
#[must_use]
fn agent_basename(agent: &str) -> &str {
    agent
        .rsplit(['/', std::path::MAIN_SEPARATOR])
        .next()
        .unwrap_or(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn prompt_path() -> PathBuf {
        PathBuf::from("/repo/.fleet/planning/b-1/prompt.md")
    }

    #[test]
    fn claude_launcher_uses_append_system_prompt_with_inline_body() {
        let argv = ClaudeLauncher.argv("claude", &prompt_path(), "PROMPT BODY");
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--append-system-prompt".to_string(),
                "PROMPT BODY".to_string(),
            ]
        );
    }

    #[test]
    fn claude_launcher_preserves_caller_supplied_binary_name() {
        // The dispatch resolves `claude-code` to ClaudeLauncher, but
        // the binary name passed in is what actually exec's — make
        // sure we don't accidentally rewrite it to `claude`.
        let argv = ClaudeLauncher.argv("claude-code", &prompt_path(), "BODY");
        assert_eq!(argv[0], "claude-code");
    }

    #[test]
    fn aider_launcher_passes_prompt_path_via_read_flag() {
        let argv = AiderLauncher.argv("aider", &prompt_path(), "ignored body");
        assert_eq!(
            argv,
            vec![
                "aider".to_string(),
                "--read".to_string(),
                "/repo/.fleet/planning/b-1/prompt.md".to_string(),
            ]
        );
    }

    #[test]
    fn codex_launcher_emits_developer_instructions_override() {
        let argv = CodexLauncher.argv("codex", &prompt_path(), "PROMPT BODY");
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "-c".to_string(),
                "developer_instructions=PROMPT BODY".to_string(),
            ]
        );
    }

    #[test]
    fn bare_launcher_spawns_just_the_binary_and_warns() {
        let argv = BareLauncher.argv("my-custom-agent", &prompt_path(), "BODY");
        assert_eq!(argv, vec!["my-custom-agent".to_string()]);
        let warn = BareLauncher.unsupported_warning().unwrap();
        assert!(warn.contains("FLEET_BRAINSTORM_PROMPT"), "got: {warn}");
    }

    #[test]
    fn supported_launchers_produce_no_warning() {
        assert!(ClaudeLauncher.unsupported_warning().is_none());
        assert!(AiderLauncher.unsupported_warning().is_none());
        assert!(CodexLauncher.unsupported_warning().is_none());
    }

    #[test]
    fn launcher_for_dispatches_on_bare_name() {
        let argv = launcher_for("claude").argv("claude", &prompt_path(), "X");
        assert_eq!(argv[1], "--append-system-prompt");

        let argv = launcher_for("claude-code").argv("claude-code", &prompt_path(), "X");
        assert_eq!(argv[1], "--append-system-prompt");

        let argv = launcher_for("aider").argv("aider", &prompt_path(), "X");
        assert_eq!(argv[1], "--read");

        let argv = launcher_for("codex").argv("codex", &prompt_path(), "X");
        assert_eq!(argv[1], "-c");

        let argv = launcher_for("totally-unknown").argv("totally-unknown", &prompt_path(), "X");
        assert_eq!(argv, vec!["totally-unknown".to_string()]);
    }

    #[test]
    fn launcher_for_strips_directory_prefix_before_matching() {
        // `brainstorm.agent` is documented as a bare program name,
        // but a user pinning an absolute path shouldn't silently
        // fall through to BareLauncher.
        let argv = launcher_for("/opt/claude/bin/claude").argv(
            "/opt/claude/bin/claude",
            &prompt_path(),
            "BODY",
        );
        assert_eq!(argv[1], "--append-system-prompt");
        assert_eq!(argv[0], "/opt/claude/bin/claude");
    }

    #[test]
    fn agent_basename_handles_plain_name_and_paths() {
        assert_eq!(agent_basename("claude"), "claude");
        assert_eq!(agent_basename("/usr/local/bin/claude"), "claude");
        assert_eq!(agent_basename("./bin/aider"), "aider");
    }
}
