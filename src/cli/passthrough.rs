//! Generic passthrough to `ao` inside the VM. Used for `status`, `doctor`,
//! `session …`, `plugin …`, `stop`. No secret needed.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::{CommandSpec, RealProcessInvoker, run_interactive_spec};

/// Pass `ao <args…>` to AO inside the VM. AO discovers its config from
/// cwd only, so we always run from the canonical XDG dir — the same
/// place fleet's spawn / status / refresh paths use — so the user
/// gets the registered projects regardless of which directory fleet
/// was launched from.
pub fn run(_repo_root: &Path, ao_args: &[String]) -> Result<i32> {
    let spec = build_spec(ao_args)?;
    run_interactive_spec(&spec)
}

/// Pass `ao <prefix> <args…>` (e.g. `ao session ls`).
pub fn run_with_prefix(repo_root: &Path, prefix: &str, args: &[String]) -> Result<i32> {
    let mut full = vec![prefix.to_string()];
    full.extend(args.iter().cloned());
    run(repo_root, &full)
}

/// Build the `limactl shell --workdir <dir> fleet-vm ao <args…>` spec
/// without spawning. Used by the in-TUI background path
/// ([`crate::tui::ao_task::AoTask`]) which pipes stdio rather than
/// inheriting it. No env vars — passthrough operations like
/// `session kill` / `stop` don't read host environment.
pub fn build_spec(ao_args: &[String]) -> Result<CommandSpec> {
    let invoker = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    match lima.status() {
        VmStatus::Running => {}
        VmStatus::Stopped => bail!("Lima VM `{}` is stopped.", lima.vm_name()),
        VmStatus::Missing => bail!("Lima VM `{}` not found.", lima.vm_name()),
    }
    let workdir = crate::ao::config::AoConfig::workdir()
        .context("no $HOME / $XDG_CONFIG_HOME — can't resolve AO workdir")?;
    // Materialize the worker AGENTS.md to its XDG location before any
    // `ao` call. Non-fatal — pass-through commands like `ao status`
    // don't depend on the worker rules file existing, but spawn flows
    // chained off these (`ao session ls` -> attach -> follow-up spawn)
    // do, so syncing here keeps the file fresh on every entry point.
    if let Err(e) = crate::templates_sync::ensure_worker_agents_md() {
        tracing::warn!(error = ?e, "failed to materialize worker AGENTS.md");
    }
    let mut args = vec![
        "shell".to_string(),
        // `--preserve-env` + `LIMA_SHELLENV_ALLOW=AO_GLOBAL_CONFIG`
        // below forwards fleet's config path to AO inside the guest.
        // Without this, `ao stop` / `ao session ls` / etc. resolve
        // `getGlobalConfigPath()` to AO's upstream default
        // (~/.agent-orchestrator/config.yaml) which doesn't exist on
        // fleet-managed installs, and the supervisor's reconcile
        // silently bails on missing-config. Mirror in
        // `cli::spawn::build_claude_oauth_spec` /
        // `build_passthrough_spec` keeps every host→guest entrypoint
        // consistent.
        "--preserve-env".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        lima.vm_name().to_string(),
        "ao".to_string(),
    ];
    args.extend(ao_args.iter().cloned());
    let ao_global_config = crate::ao::config::AoConfig::default_xdg_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(CommandSpec {
        program: "limactl".to_string(),
        args,
        env_set: vec![
            ("AO_GLOBAL_CONFIG".to_string(), ao_global_config),
            (
                "LIMA_SHELLENV_ALLOW".to_string(),
                "AO_GLOBAL_CONFIG".to_string(),
            ),
        ],
        env_unset: Vec::new(),
    })
}

/// Convenience wrapper around [`build_spec`] for the `<prefix> <args>`
/// pattern (mirrors [`run_with_prefix`] but returns a spec).
pub fn build_spec_with_prefix(prefix: &str, args: &[String]) -> Result<CommandSpec> {
    let mut full = vec![prefix.to_string()];
    full.extend(args.iter().cloned());
    build_spec(&full)
}
