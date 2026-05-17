//! `fleet runtime …` — runtime-adapter commands.
//!
//! `runtime doctor` probes the host for container engines (Podman, Apple
//! Container, Docker) plus the devcontainer CLI and prints a
//! human-readable summary of what's usable.
//!
//! `runtime build / up / exec / stop / inspect / attach` drive the chosen
//! adapter end-to-end against the current repo's `.fleet/config.yaml`.
//! They're the manual escape hatch users reach for when something feels
//! off — and the surface fleet developers iterate against when wiring
//! new adapter behaviour before the workflow engine wraps everything.
//!
//! All formatting goes through pure functions (e.g. [`render_doctor`]) so the
//! CLI is a thin wire-up over fully-tested formatters. Entry points hand in a
//! real `ProcessInvoker`; tests hand in mocks.

use anyhow::{Context, Result, anyhow};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::{AdapterChoice, RepoConfig};
use crate::runtime::ContainerSpec;
use crate::runtime::detect::{BackendKind, BackendStatus, ProbeReport, probe};
use crate::runtime::devcontainer::Devcontainer;
use crate::runtime::devcontainer_cli::{DevcontainerCli, Engine};
use crate::runtime::factory::build_adapter;
use crate::runtime::{ContainerId, ContainerState, ExecOpts, RuntimeAdapter};

/// CLI entry point for `fleet runtime doctor`. Probes the host and writes
/// the report to stdout. Returns the exit code: 0 if a usable engine was
/// found, 1 otherwise (so it's scriptable in CI). The probe itself is
/// infallible — bad invoker output is treated as "tool absent" — so this
/// returns a bare `i32`, not `Result`. The dispatcher wraps it in `Ok` to
/// match the surrounding signature.
pub fn run_doctor() -> i32 {
    let invoker = RealProcessInvoker;
    let report = probe(&invoker);
    let rendered = render_doctor(&report);
    println!("{rendered}");
    i32::from(!report.is_usable())
}

/// CLI entry point for `fleet runtime build`. Loads `.fleet/config.yaml`,
/// probes, picks the adapter, builds the image, prints the resulting
/// `ImageId` on a line of its own (scriptable: callers can capture stdout).
pub fn run_build() -> Result<i32> {
    let ctx = Context_::load()?;
    let dc = load_devcontainer(&ctx)?;
    let image = ctx.adapter.ensure_image(&dc)?;
    println!("{image}");
    Ok(0)
}

/// CLI entry point for `fleet runtime up`. Starts (or replaces) the container
/// for the current repo's devcontainer. Prints the container id on stdout
/// and a one-line confirmation on stderr so scripting paths can capture id
/// while users still see what happened.
pub fn run_up() -> Result<i32> {
    let ctx = Context_::load()?;
    let dc = load_devcontainer(&ctx)?;
    let image = ctx.adapter.ensure_image(&dc)?;
    let artifacts = ctx.artifacts_dir()?;
    let spec = ContainerSpec {
        image,
        workspace: ctx.repo_root.clone(),
        artifacts,
        env: Vec::new(),
        command: None,
        network: None,
        dns: None,
        extra_mounts: Vec::new(),
    };
    let id = ctx.adapter.start_container(&spec)?;
    eprintln!(
        "fleet runtime up: started container in {}",
        ctx.repo_root.display()
    );
    println!("{id}");
    Ok(0)
}

/// CLI entry point for `fleet runtime exec -- <argv>`. Bypasses the
/// adapter trait (whose in-memory container→workspace mapping doesn't
/// survive across CLI invocations) and addresses the running container by
/// workspace folder through [`DevcontainerCli`] directly. Returns the exec
/// handle's exit code so the caller can `&&`-chain with other commands.
pub fn run_exec(argv: &[String]) -> Result<i32> {
    if argv.is_empty() {
        return Err(anyhow!(
            "fleet runtime exec: missing command — pass argv after `--`"
        ));
    }
    let ctx = Context_::load()?;
    let engine = engine_for_adapter(&ctx)?;
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let cli = DevcontainerCli::new(invoker, engine);
    let h = cli.exec(&ctx.repo_root, argv, ExecOpts::default())?;
    // Forward stdout to ours; the adapter's exec handle is lossy on stderr
    // and exit code (see DevcontainerCli::exec docs). For manual use that
    // tradeoff is acceptable; richer streaming arrives with the workflow
    // engine.
    print!("{}", h.stdout);
    if !h.stderr.is_empty() {
        eprint!("{}", h.stderr);
    }
    Ok(h.exit_code)
}

/// CLI entry point for `fleet runtime attach <id> [-- argv…]`. Routes
/// through the adapter's `attach_pty`, which spawns the engine's
/// `exec -it` with inherited stdio. Blocks until the inner process exits;
/// returns its exit code as fleet's exit code (so users can pipe-chain
/// `fleet runtime attach … && …`).
///
/// When `argv` is empty, defaults to `bash`. Users can override (`sh`,
/// `python`, etc.) via the trailing argv after `--`.
pub fn run_attach(id: &str, argv: &[String]) -> Result<i32> {
    let ctx = Context_::load()?;
    let resolved: Vec<String> = if argv.is_empty() {
        vec!["bash".to_string()]
    } else {
        argv.to_vec()
    };
    let handle = ctx
        .adapter
        .attach_pty(&ContainerId::new(id), &resolved, ExecOpts::default())?;
    Ok(handle.exit_code)
}

/// CLI entry point for `fleet runtime stop <id>`. Idempotent — stopping
/// an unknown container is not an error, matching the trait contract.
pub fn run_stop(id: &str) -> Result<i32> {
    let ctx = Context_::load()?;
    ctx.adapter.stop(&ContainerId::new(id))?;
    eprintln!("fleet runtime stop: {id} stopped");
    Ok(0)
}

/// CLI entry point for `fleet runtime inspect <id>`. Prints the container
/// state on a single line. Exit code mirrors the state:
///   - 0 for running
///   - 0 for exited{0}
///   - the exit code for exited{n}
///   - 2 for dead / unknown (caller-visible "weird state")
pub fn run_inspect(id: &str) -> Result<i32> {
    let ctx = Context_::load()?;
    let state = ctx.adapter.inspect(&ContainerId::new(id))?;
    let (line, code) = render_inspect(&state);
    println!("{line}");
    Ok(code)
}

/// Stable rendering + exit-code mapping for [`ContainerState`]. Pure so
/// it's unit-testable; the CLI's `run_inspect` is a thin wrapper.
#[must_use]
pub fn render_inspect(state: &ContainerState) -> (String, i32) {
    match state {
        ContainerState::Created => ("created".to_string(), 0),
        ContainerState::Running => ("running".to_string(), 0),
        ContainerState::Exited { code } => (format!("exited {code}"), *code),
        ContainerState::Dead => ("dead".to_string(), 2),
        ContainerState::Unknown(s) => (format!("unknown ({s})"), 2),
    }
}

/// Engine selector mirroring [`build_adapter`]'s post-resolution decision.
/// Used by `run_exec` to construct a [`DevcontainerCli`] directly without
/// re-running the full factory.
fn engine_for_adapter(ctx: &Context_) -> Result<Engine> {
    match ctx.adapter.name() {
        "podman" => Ok(Engine::Podman),
        "docker" => Ok(Engine::Docker),
        "apple-container" => Ok(Engine::AppleContainer),
        "local" => Err(anyhow!(
            "fleet runtime exec: not supported on the `local` adapter — \
             use plain shell since there's no container to enter"
        )),
        other => Err(anyhow!("internal: unrecognised adapter `{other}`")),
    }
}

/// Resolved environment for every `fleet runtime …` command: repo root,
/// loaded config, probe report, and a built adapter. Held in a small
/// struct so the entry points don't repeat the same five lines.
struct Context_ {
    repo_root: PathBuf,
    config: RepoConfig,
    adapter: Box<dyn RuntimeAdapter>,
}

impl Context_ {
    fn load() -> Result<Self> {
        let cwd = std::env::current_dir().context("reading current directory")?;
        let repo_root = repo::fleet_root(&cwd);
        let config = RepoConfig::load(repo_root.join(".fleet/config.yaml"))
            .context("loading .fleet/config.yaml")?;
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        let report = probe(invoker.as_ref());
        let adapter = build_adapter(&config.runtime, &report, invoker)?;
        Ok(Self {
            repo_root,
            config,
            adapter,
        })
    }

    /// Where the adapter should bind-mount as `/artifacts` for manually-driven
    /// containers. Per-repo location (`.fleet/sessions/manual/artifacts/`) so
    /// it survives across CLI invocations and stays in the repo's gitignored
    /// state.
    fn artifacts_dir(&self) -> Result<PathBuf> {
        let path = self.repo_root.join(".fleet/sessions/manual/artifacts");
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(path)
    }
}

fn load_devcontainer(ctx: &Context_) -> Result<Devcontainer> {
    let path = if ctx.config.runtime.devcontainer.is_absolute() {
        ctx.config.runtime.devcontainer.clone()
    } else {
        ctx.repo_root.join(&ctx.config.runtime.devcontainer)
    };
    let dc = Devcontainer::from_path(&path).with_context(|| {
        // The local adapter is fine with a "fake" devcontainer; for the
        // others, missing file is the user's most common stumble — make
        // the hint concrete.
        if matches!(ctx.config.runtime.adapter, AdapterChoice::Local) {
            format!("reading devcontainer at {}", path.display())
        } else {
            format!(
                "reading devcontainer at {} — did you run `fleet init`?",
                path.display()
            )
        }
    })?;
    Ok(dc)
}

/// Pure renderer for the doctor report. Stable wording — tests assert on it.
pub fn render_doctor(report: &ProbeReport) -> String {
    let mut out = String::new();
    out.push_str("fleet runtime doctor\n");
    out.push_str("====================\n\n");

    out.push_str("Engines:\n");
    out.push_str(&render_backend("podman", &report.podman));
    out.push_str(&render_backend("docker", &report.docker));
    out.push_str(&render_backend(
        "container (Apple)",
        &report.apple_container,
    ));

    out.push_str("\nHardening:\n");
    out.push_str(&render_backend("runsc (gVisor)", &report.gvisor));

    out.push_str("\nDevcontainer CLI:\n");
    out.push_str(&render_backend("devcontainer", &report.devcontainer_cli));

    out.push_str("\nTrackers + egress tools:\n");
    out.push_str(&render_backend("git-bug", &report.git_bug));
    out.push_str(&render_backend("tinyproxy", &report.tinyproxy));

    out.push_str("\nOrchestrator:\n");
    out.push_str(&render_backend("tmux", &report.tmux));

    out.push_str("\nRecommended engine: ");
    match report.recommended {
        Some(BackendKind::Podman) => out.push_str("podman\n"),
        Some(BackendKind::Docker) => out.push_str("docker\n"),
        Some(BackendKind::AppleContainer) => out.push_str("container (Apple)\n"),
        // gVisor and DevcontainerCli aren't engines; not reachable here.
        Some(other) => {
            let _ = writeln!(out, "{} (unexpected)", other.program());
        }
        None => out.push_str("none — fleet cannot start a container\n"),
    }

    let hints = report.install_hints();
    if !hints.is_empty() {
        out.push_str("\nNext steps:\n");
        for hint in hints {
            out.push_str("  - ");
            out.push_str(hint);
            out.push('\n');
        }
    }

    out
}

fn render_backend(label: &str, status: &BackendStatus) -> String {
    let mark = if status.present { "✓" } else { "✗" };
    let version = status
        .version
        .as_deref()
        .map_or_else(String::new, |v| format!(" — {v}"));
    let notes = if status.notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", status.notes.join("; "))
    };
    format!("  {mark} {label}{version}{notes}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::detect::BackendStatus;

    fn present(kind: BackendKind, version: &str) -> BackendStatus {
        BackendStatus {
            kind,
            present: true,
            version: Some(version.to_string()),
            notes: Vec::new(),
        }
    }

    fn absent(kind: BackendKind) -> BackendStatus {
        BackendStatus {
            kind,
            present: false,
            version: None,
            notes: Vec::new(),
        }
    }

    fn report_all_present() -> ProbeReport {
        ProbeReport {
            podman: present(BackendKind::Podman, "podman version 5.0.1"),
            docker: present(BackendKind::Docker, "Docker version 27.0.0"),
            apple_container: present(BackendKind::AppleContainer, "container 0.1.0"),
            gvisor: present(BackendKind::GVisor, "runsc 20260301"),
            devcontainer_cli: present(BackendKind::DevcontainerCli, "devcontainer 0.1.12"),
            git_bug: present(BackendKind::GitBug, "git-bug version: 0.10.0"),
            tinyproxy: present(BackendKind::Tinyproxy, "tinyproxy 1.11.1"),
            tmux: present(BackendKind::Tmux, "tmux 3.4"),
            recommended: Some(BackendKind::AppleContainer),
        }
    }

    fn report_nothing_present() -> ProbeReport {
        ProbeReport {
            podman: absent(BackendKind::Podman),
            docker: absent(BackendKind::Docker),
            apple_container: absent(BackendKind::AppleContainer),
            gvisor: absent(BackendKind::GVisor),
            devcontainer_cli: absent(BackendKind::DevcontainerCli),
            git_bug: absent(BackendKind::GitBug),
            tinyproxy: absent(BackendKind::Tinyproxy),
            tmux: absent(BackendKind::Tmux),
            recommended: None,
        }
    }

    #[test]
    fn renders_check_marks_for_present_backends() {
        let rendered = render_doctor(&report_all_present());
        assert!(rendered.contains("✓ podman — podman version 5.0.1"));
        assert!(rendered.contains("✓ docker — Docker version 27.0.0"));
        assert!(rendered.contains("✓ container (Apple) — container 0.1.0"));
        assert!(rendered.contains("✓ runsc (gVisor) — runsc 20260301"));
        assert!(rendered.contains("✓ devcontainer — devcontainer 0.1.12"));
    }

    #[test]
    fn renders_cross_marks_for_absent_backends_without_version() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("✗ podman\n"));
        assert!(rendered.contains("✗ docker\n"));
        assert!(rendered.contains("✗ container (Apple)\n"));
        assert!(rendered.contains("✗ runsc (gVisor)\n"));
        assert!(rendered.contains("✗ devcontainer\n"));
    }

    #[test]
    fn renders_recommended_engine_when_available() {
        let rendered = render_doctor(&report_all_present());
        assert!(rendered.contains("Recommended engine: container (Apple)"));
    }

    #[test]
    fn renders_recommended_none_with_actionable_text() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("Recommended engine: none"));
        assert!(rendered.contains("fleet cannot start a container"));
    }

    #[test]
    fn renders_install_hints_when_present() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("Next steps:"));
        assert!(rendered.contains("no container engine detected"));
        assert!(rendered.contains("devcontainer CLI missing"));
        assert!(rendered.contains("git-bug missing"));
        assert!(rendered.contains("tinyproxy missing"));
    }

    #[test]
    fn renders_tracker_and_egress_tools_section() {
        let rendered = render_doctor(&report_all_present());
        assert!(rendered.contains("Trackers + egress tools:"));
        assert!(rendered.contains("✓ git-bug — git-bug version: 0.10.0"));
        assert!(rendered.contains("✓ tinyproxy — tinyproxy 1.11.1"));
    }

    #[test]
    fn omits_install_hints_section_when_nothing_to_say() {
        let rendered = render_doctor(&report_all_present());
        assert!(!rendered.contains("Next steps:"));
    }

    #[test]
    fn renders_recommendation_when_only_podman_present() {
        let mut r = report_nothing_present();
        r.podman = present(BackendKind::Podman, "podman 5.0.1");
        r.recommended = Some(BackendKind::Podman);
        let rendered = render_doctor(&r);
        assert!(rendered.contains("Recommended engine: podman"));
    }

    #[test]
    fn render_inspect_running_is_exit_zero() {
        let (line, code) = render_inspect(&ContainerState::Running);
        assert_eq!(line, "running");
        assert_eq!(code, 0);
    }

    #[test]
    fn render_inspect_exited_carries_exit_code() {
        let (line, code) = render_inspect(&ContainerState::Exited { code: 137 });
        assert_eq!(line, "exited 137");
        assert_eq!(code, 137);
    }

    #[test]
    fn render_inspect_exited_zero_stays_zero() {
        let (line, code) = render_inspect(&ContainerState::Exited { code: 0 });
        assert_eq!(line, "exited 0");
        assert_eq!(code, 0);
    }

    #[test]
    fn render_inspect_created_is_exit_zero() {
        let (_, code) = render_inspect(&ContainerState::Created);
        assert_eq!(code, 0);
    }

    #[test]
    fn render_inspect_dead_maps_to_exit_two() {
        let (line, code) = render_inspect(&ContainerState::Dead);
        assert_eq!(line, "dead");
        assert_eq!(code, 2);
    }

    #[test]
    fn render_inspect_unknown_carries_engine_text_and_exits_two() {
        let (line, code) = render_inspect(&ContainerState::Unknown("paused".to_string()));
        assert_eq!(line, "unknown (paused)");
        assert_eq!(code, 2);
    }

    #[test]
    fn render_backend_includes_notes_when_nonempty() {
        let mut s = present(BackendKind::Podman, "podman 5.0.1");
        s.notes.push("rootless ready".to_string());
        let line = render_backend("podman", &s);
        assert!(line.contains("(rootless ready)"));
    }
}
