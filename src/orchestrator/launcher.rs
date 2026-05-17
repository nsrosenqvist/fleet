//! Per-agent argv construction for `fleet orchestrator`.
//!
//! Fleet writes a fully-rendered system prompt to disk at session
//! spawn time (see [`super::prompt`]), but each supported agent
//! takes that prompt through a different CLI surface:
//!
//! - **Claude Code** (`claude`, `claude-code`) — `--append-system-prompt <body>`
//!   keeps the agent's built-in scaffolding and appends fleet's
//!   role + tool docs. Fleet additionally pre-assigns a UUID via
//!   `--session-id` at spawn so a respawn after a dead pane can
//!   `--resume <uuid>` the exact same conversation.
//! - **Aider** (`aider`) — no `--system-prompt` flag exists; the
//!   idiomatic path is `--read <file>` against a read-only
//!   conventions doc, so we point it at the prompt file we already
//!   wrote. The chat history file is pinned to the orchestrator
//!   session dir (`<dir>/aider-chat-history.md`) so respawn can
//!   `--restore-chat-history` from a deterministic location
//!   instead of relying on the repo-root default.
//! - **Codex** (`codex`) — `-c developer_instructions=<body>`
//!   appends developer-role instructions; chosen over
//!   `model_instructions_file` (which *replaces* codex's built-in
//!   prompt) so the agent's default safety scaffolding stays in
//!   place. Resume after a dead pane uses `codex resume --last`
//!   (codex doesn't take a pre-assigned id at spawn, and the
//!   single-session-per-repo design means "last" maps to "this").
//! - **Anything else** — fleet doesn't know how to wire the prompt
//!   in. We spawn the binary bare and warn on stderr so the user
//!   can either configure their agent to read
//!   `$FLEET_ORCHESTRATOR_PROMPT` themselves or switch to a
//!   supported agent. Resume just re-spawns fresh.
//!
//! Pure: each [`AgentLauncher`] method is a function of inputs
//! only, so tests pin the exact argv per agent without spawning
//! anything.

use std::path::{Path, PathBuf};

/// Strategy for turning the orchestrator `agent` name + the rendered
/// prompt into the argv passed to `tmux new-session -- …`.
pub trait AgentLauncher {
    /// Build the argv for a fresh spawn with no pre-assigned agent
    /// session id. `agent` is the resolved binary name (the caller
    /// has already done the basename match); `prompt_path` is the
    /// on-disk system prompt; `prompt_body` is the same content in
    /// memory.
    fn argv(&self, agent: &str, prompt_path: &Path, prompt_body: &str) -> Vec<String>;

    /// Mint a fresh agent-session id at spawn time, when the agent
    /// supports pre-assigning one (claude: `--session-id <uuid>`).
    /// Returns `None` for agents that don't (aider, codex, bare) —
    /// those recover continuity through other mechanisms on resume.
    /// Default: no id minted.
    fn mint_session_id(&self) -> Option<String> {
        None
    }

    /// Build the spawn argv when a pre-minted session id is to be
    /// baked into the agent's invocation. Default: pass through to
    /// [`Self::argv`] (the id is dropped — which is correct for any
    /// launcher whose [`Self::mint_session_id`] returns `None`).
    fn argv_with_session_id(
        &self,
        agent: &str,
        prompt_path: &Path,
        prompt_body: &str,
        session_id: &str,
    ) -> Vec<String> {
        let _ = session_id;
        self.argv(agent, prompt_path, prompt_body)
    }

    /// Build the argv to *resume* an existing orchestrator whose
    /// previous agent process is dead (e.g. Ctrl+C inside the tmux
    /// pane). `prior_session_id` is whatever
    /// [`Self::mint_session_id`] returned at original spawn —
    /// claude uses it for `--resume <uuid>`; aider/codex ignore it
    /// (they recover history via their own mechanisms).
    ///
    /// Default: same argv as a fresh spawn (history is lost; the
    /// system prompt is re-injected). Override for agents that
    /// support continuity.
    fn resume_argv(
        &self,
        agent: &str,
        prompt_path: &Path,
        prompt_body: &str,
        prior_session_id: Option<&str>,
    ) -> Vec<String> {
        let _ = prior_session_id;
        self.argv(agent, prompt_path, prompt_body)
    }

    /// Optional stderr warning printed at spawn time. Used by
    /// [`BareLauncher`] to flag that the prompt won't auto-deliver
    /// for unrecognised agents.
    fn unsupported_warning(&self) -> Option<String> {
        None
    }
}

/// `claude` / `claude-code` — `--append-system-prompt <body>`,
/// optionally `--session-id <uuid>` at first spawn so a later
/// respawn can `--resume <uuid>`.
pub struct ClaudeLauncher;

impl AgentLauncher for ClaudeLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "--append-system-prompt".to_string(),
            prompt_body.to_string(),
        ]
    }

    fn mint_session_id(&self) -> Option<String> {
        Some(uuid::Uuid::new_v4().to_string())
    }

    fn argv_with_session_id(
        &self,
        agent: &str,
        _prompt_path: &Path,
        prompt_body: &str,
        session_id: &str,
    ) -> Vec<String> {
        // `--session-id` ahead of `--append-system-prompt` matches
        // the order the CLI doc lists them; claude itself accepts
        // either order, but keeping it stable makes the argv easier
        // to eyeball.
        vec![
            agent.to_string(),
            "--session-id".to_string(),
            session_id.to_string(),
            "--append-system-prompt".to_string(),
            prompt_body.to_string(),
        ]
    }

    fn resume_argv(
        &self,
        agent: &str,
        _prompt_path: &Path,
        _prompt_body: &str,
        prior_session_id: Option<&str>,
    ) -> Vec<String> {
        // On resume the session already has the appended system
        // prompt baked in, so we deliberately drop
        // `--append-system-prompt` to avoid double-injection.
        //
        // Some(id) → `--resume <id>` for deterministic continuity.
        // None → `--continue` resumes the most recent conversation
        // in cwd, which is the orchestrator's prior session in the
        // single-session-per-repo design. (Reachable when meta
        // predates the `--session-id` capture.)
        prior_session_id.map_or_else(
            || vec![agent.to_string(), "--continue".to_string()],
            |id| vec![agent.to_string(), "--resume".to_string(), id.to_string()],
        )
    }
}

/// `aider` — `--read <prompt_path>` for the system prompt (aider
/// has no inline flag) plus `--chat-history-file <path>` pinned to
/// the orchestrator session dir so a respawn can
/// `--restore-chat-history` from a deterministic location.
pub struct AiderLauncher;

impl AgentLauncher for AiderLauncher {
    fn argv(&self, agent: &str, prompt_path: &Path, _prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "--chat-history-file".to_string(),
            aider_history_path(prompt_path).display().to_string(),
            "--read".to_string(),
            prompt_path.display().to_string(),
        ]
    }

    fn resume_argv(
        &self,
        agent: &str,
        prompt_path: &Path,
        _prompt_body: &str,
        _prior_session_id: Option<&str>,
    ) -> Vec<String> {
        vec![
            agent.to_string(),
            "--chat-history-file".to_string(),
            aider_history_path(prompt_path).display().to_string(),
            "--restore-chat-history".to_string(),
            "--read".to_string(),
            prompt_path.display().to_string(),
        ]
    }
}

/// Aider chat history lives alongside the orchestrator prompt so it's
/// scoped to this orchestrator and a respawn can find it without
/// guessing about the repo-root default. Pure helper — derives from
/// `prompt_path`'s parent so the launcher trait doesn't need a
/// separate session-dir argument.
#[must_use]
fn aider_history_path(prompt_path: &Path) -> PathBuf {
    let dir = prompt_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    dir.join("aider-chat-history.md")
}

/// `codex` — `-c developer_instructions=<body>` for append
/// semantics. The replace-the-builtin alternative
/// (`model_instructions_file`) is deliberately not used: keeping
/// codex's own safety scaffolding is less surprising. Resume uses
/// `codex resume --last`: codex doesn't take a pre-assigned session
/// id at spawn, and in fleet's single-session-per-repo design
/// "last" reliably maps to "this orchestrator".
pub struct CodexLauncher;

impl AgentLauncher for CodexLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, prompt_body: &str) -> Vec<String> {
        vec![
            agent.to_string(),
            "-c".to_string(),
            format!("developer_instructions={prompt_body}"),
        ]
    }

    fn resume_argv(
        &self,
        agent: &str,
        _prompt_path: &Path,
        _prompt_body: &str,
        _prior_session_id: Option<&str>,
    ) -> Vec<String> {
        // The developer_instructions injected at first spawn are
        // already part of the persisted session, so we don't re-pass
        // them here; `resume --last` rehydrates everything.
        vec![
            agent.to_string(),
            "resume".to_string(),
            "--last".to_string(),
        ]
    }
}

/// Fallback for agents fleet doesn't know how to wire. Spawns the
/// binary with no args and warns that the rendered prompt won't be
/// auto-delivered — the user can still wire their agent to read
/// `$FLEET_ORCHESTRATOR_PROMPT`.
pub struct BareLauncher;

impl AgentLauncher for BareLauncher {
    fn argv(&self, agent: &str, _prompt_path: &Path, _prompt_body: &str) -> Vec<String> {
        vec![agent.to_string()]
    }

    fn unsupported_warning(&self) -> Option<String> {
        Some(
            "fleet doesn't know how to inject the orchestrator system prompt for this agent. \
             Configure the agent to read $FLEET_ORCHESTRATOR_PROMPT itself, or use claude / \
             claude-code / aider / codex."
                .to_string(),
        )
    }
}

/// Dispatch on the agent's basename. Strips any directory prefix
/// so a `orchestrator.agent: /opt/claude/bin/claude` still resolves
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
    fn aider_launcher_passes_prompt_path_via_read_flag_with_pinned_history_file() {
        let argv = AiderLauncher.argv("aider", &prompt_path(), "ignored body");
        assert_eq!(
            argv,
            vec![
                "aider".to_string(),
                "--chat-history-file".to_string(),
                "/repo/.fleet/planning/b-1/aider-chat-history.md".to_string(),
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
        assert!(warn.contains("FLEET_ORCHESTRATOR_PROMPT"), "got: {warn}");
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
        assert_eq!(argv[1], "--chat-history-file");

        let argv = launcher_for("codex").argv("codex", &prompt_path(), "X");
        assert_eq!(argv[1], "-c");

        let argv = launcher_for("totally-unknown").argv("totally-unknown", &prompt_path(), "X");
        assert_eq!(argv, vec!["totally-unknown".to_string()]);
    }

    #[test]
    fn launcher_for_strips_directory_prefix_before_matching() {
        // `orchestrator.agent` is documented as a bare program name,
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

    #[test]
    fn claude_launcher_mints_a_valid_uuid() {
        // Claude is the only launcher that mints — the value gets
        // passed straight to `claude --session-id <uuid>` which
        // rejects anything that isn't a UUID.
        let id = ClaudeLauncher.mint_session_id().unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "not a uuid: {id}");
    }

    #[test]
    fn aider_codex_bare_do_not_mint_session_ids() {
        // They recover continuity through other mechanisms (chat
        // history file, `resume --last`, nothing), so there's no id
        // to pre-assign.
        assert!(AiderLauncher.mint_session_id().is_none());
        assert!(CodexLauncher.mint_session_id().is_none());
        assert!(BareLauncher.mint_session_id().is_none());
    }

    #[test]
    fn claude_argv_with_session_id_prepends_session_id_flag() {
        let argv = ClaudeLauncher.argv_with_session_id(
            "claude",
            &prompt_path(),
            "BODY",
            "11111111-2222-3333-4444-555555555555",
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--session-id".to_string(),
                "11111111-2222-3333-4444-555555555555".to_string(),
                "--append-system-prompt".to_string(),
                "BODY".to_string(),
            ]
        );
    }

    #[test]
    fn non_claude_argv_with_session_id_ignores_the_id() {
        // For aider/codex/bare, the default impl drops the id —
        // their mint_session_id returns None, so this branch is
        // only hit if a caller mis-uses the API. Safer to behave
        // identically to a plain spawn than to inject a meaningless
        // flag.
        let aider = AiderLauncher.argv_with_session_id("aider", &prompt_path(), "B", "irrelevant");
        assert_eq!(aider, AiderLauncher.argv("aider", &prompt_path(), "B"));
        let codex = CodexLauncher.argv_with_session_id("codex", &prompt_path(), "B", "irrelevant");
        assert_eq!(codex, CodexLauncher.argv("codex", &prompt_path(), "B"));
        let bare = BareLauncher.argv_with_session_id("x", &prompt_path(), "B", "irrelevant");
        assert_eq!(bare, BareLauncher.argv("x", &prompt_path(), "B"));
    }

    #[test]
    fn claude_resume_argv_with_known_session_id_uses_resume_flag() {
        let argv = ClaudeLauncher.resume_argv(
            "claude",
            &prompt_path(),
            "BODY (ignored on resume)",
            Some("11111111-2222-3333-4444-555555555555"),
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--resume".to_string(),
                "11111111-2222-3333-4444-555555555555".to_string(),
            ]
        );
    }

    #[test]
    fn claude_resume_argv_without_id_falls_back_to_continue() {
        // Legacy meta (created before agent_session_id was a field)
        // has None; `--continue` resumes the most recent claude
        // session in cwd, which under the single-session-per-repo
        // design is the orchestrator's prior session.
        let argv = ClaudeLauncher.resume_argv("claude", &prompt_path(), "BODY", None);
        assert_eq!(argv, vec!["claude".to_string(), "--continue".to_string()]);
    }

    #[test]
    fn aider_resume_argv_adds_restore_chat_history_to_normal_argv() {
        let argv = AiderLauncher.resume_argv("aider", &prompt_path(), "ignored", None);
        assert_eq!(
            argv,
            vec![
                "aider".to_string(),
                "--chat-history-file".to_string(),
                "/repo/.fleet/planning/b-1/aider-chat-history.md".to_string(),
                "--restore-chat-history".to_string(),
                "--read".to_string(),
                "/repo/.fleet/planning/b-1/prompt.md".to_string(),
            ]
        );
    }

    #[test]
    fn codex_resume_argv_uses_resume_last_subcommand() {
        // codex can't take a pre-assigned id at spawn, and the
        // developer_instructions are already in the persisted
        // session, so resume is just `codex resume --last`.
        let argv = CodexLauncher.resume_argv("codex", &prompt_path(), "ignored", None);
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "resume".to_string(),
                "--last".to_string(),
            ]
        );
    }

    #[test]
    fn bare_resume_argv_falls_back_to_fresh_spawn() {
        // Unknown agents have no continuity mechanism fleet can
        // wire — the safest default is "spawn again, same way" and
        // let the user re-type whatever they had in flight.
        let argv = BareLauncher.resume_argv("custom-agent", &prompt_path(), "BODY", None);
        assert_eq!(
            argv,
            BareLauncher.argv("custom-agent", &prompt_path(), "BODY")
        );
    }

    #[test]
    fn aider_history_path_lives_next_to_prompt_file() {
        // The launcher derives history from `prompt_path.parent()`
        // so callers don't have to thread a separate session-dir
        // argument through the trait.
        let p = aider_history_path(Path::new("/repo/.fleet/planning/b-9/prompt.md"));
        assert_eq!(
            p,
            PathBuf::from("/repo/.fleet/planning/b-9/aider-chat-history.md")
        );
    }
}
