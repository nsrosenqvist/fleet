//! Pre-flight reachability check for the configured container runtime.
//!
//! `fleet runtime doctor` answers "is the binary installed?" — but
//! that's not enough: a Docker install with the daemon stopped passes
//! the doctor and then every `devcontainer build` errors with
//! `Cannot connect to the Docker daemon`. The TUI's spawn paths
//! (autonomous, scheduler, spawn picker) need to catch that *before*
//! firing a detached subprocess that's guaranteed to fail.
//!
//! [`check_engine_reachable`] resolves the adapter the same way
//! [`crate::runtime::factory::build_adapter`] does, then runs the
//! engine's canonical "talk to the daemon" command (`docker info` /
//! `podman info`). Returns a `Result<(), String>` with a
//! user-facing message — including a "what to do next" hint — so
//! callers can flash it through the status bar verbatim.
//!
//! The Local adapter has no daemon → always Ok. Apple Container's
//! daemon model is different (per-container microVMs); we treat it
//! as always-Ok for now and refine when we have macOS smoke
//! coverage.

use crate::process::ProcessInvoker;
use crate::repo_config::RuntimeConfig;
use crate::runtime::detect::ProbeReport;
use crate::runtime::factory::{ResolvedKind, resolve_kind};

/// Check that the configured engine's daemon is reachable. Cheap
/// (one shell call); safe to call before every spawn dispatch.
///
/// `Ok(())` → good to spawn. `Err(msg)` → flash `msg` and bail; the
/// message already names the engine and suggests how to start it.
pub fn check_engine_reachable(
    invoker: &dyn ProcessInvoker,
    config: &RuntimeConfig,
    probe: &ProbeReport,
) -> Result<(), String> {
    let kind = resolve_kind(config.adapter, probe)
        .map_err(|e| format!("runtime adapter selection failed: {e:#}"))?;
    match kind {
        ResolvedKind::Docker => check_via_info(invoker, "docker"),
        ResolvedKind::Podman => check_via_info(invoker, "podman"),
        // No daemon on these — Local runs directly on the host;
        // Apple Container is per-container, no central socket.
        ResolvedKind::Local | ResolvedKind::AppleContainer => Ok(()),
    }
}

fn check_via_info(invoker: &dyn ProcessInvoker, engine: &str) -> Result<(), String> {
    // `info` is the canonical "are you up?" verb for both Docker and
    // Podman. We discard the verbose output — exit status is the
    // signal. Use the `--format` flag to keep the work minimal on
    // the engine side.
    let args = vec!["info".to_string(), "--format".to_string(), "{{.ID}}".to_string()];
    match invoker.run(engine, args) {
        Ok(_) => Ok(()),
        Err(err) => {
            let hint = engine_hint(engine);
            Err(format!(
                "{engine} daemon not reachable — {hint}\n  underlying error: {err:#}"
            ))
        }
    }
}

fn engine_hint(engine: &str) -> &'static str {
    match engine {
        "docker" => "start Docker Desktop, or `systemctl --user start docker-desktop` on Fedora/Ubuntu",
        "podman" => "check `podman info`; on macOS run `podman machine start`",
        _ => "verify the engine is installed and the daemon is running",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::repo_config::{AdapterChoice, HardeningChoice, NetworkConfig, RuntimeConfig};
    use crate::runtime::detect::{BackendKind, BackendStatus, ProbeReport};
    use mockall::predicate::eq;
    use std::path::PathBuf;

    fn runtime_with(adapter: AdapterChoice) -> RuntimeConfig {
        RuntimeConfig {
            adapter,
            hardening: HardeningChoice::None,
            devcontainer: PathBuf::from(".devcontainer/devcontainer.json"),
            network: NetworkConfig::default(),
            ..RuntimeConfig::default()
        }
    }

    fn absent(kind: BackendKind) -> BackendStatus {
        BackendStatus {
            kind,
            present: false,
            version: None,
            notes: vec![],
        }
    }

    fn probe_with_docker() -> ProbeReport {
        ProbeReport {
            podman: absent(BackendKind::Podman),
            docker: BackendStatus {
                kind: BackendKind::Docker,
                present: true,
                version: Some("Docker version 29.0.0".to_string()),
                notes: vec![],
            },
            apple_container: absent(BackendKind::AppleContainer),
            gvisor: absent(BackendKind::GVisor),
            devcontainer_cli: absent(BackendKind::DevcontainerCli),
            git_bug: absent(BackendKind::GitBug),
            tinyproxy: absent(BackendKind::Tinyproxy),
            tmux: absent(BackendKind::Tmux),
            recommended: Some(BackendKind::Docker),
        }
    }

    #[test]
    fn local_adapter_skips_daemon_check() {
        let mock = MockProcessInvoker::new(); // no calls expected
        let config = runtime_with(AdapterChoice::Local);
        let probe = probe_with_docker();
        assert!(check_engine_reachable(&mock, &config, &probe).is_ok());
    }

    #[test]
    fn docker_reachable_returns_ok() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("docker"), eq(vec!["info".into(), "--format".into(), "{{.ID}}".into()]))
            .returning(|_, _| Ok("docker-host-id-123".to_string()));
        let config = runtime_with(AdapterChoice::Docker);
        let probe = probe_with_docker();
        check_engine_reachable(&mock, &config, &probe).unwrap();
    }

    #[test]
    fn docker_down_returns_err_naming_engine_and_hint() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("docker"), eq(vec!["info".into(), "--format".into(), "{{.ID}}".into()]))
            .returning(|_, _| Err(anyhow::anyhow!("Cannot connect to the Docker daemon at unix:///var/run/docker.sock")));
        let config = runtime_with(AdapterChoice::Docker);
        let probe = probe_with_docker();
        let err = check_engine_reachable(&mock, &config, &probe).unwrap_err();
        assert!(err.contains("docker daemon not reachable"), "got: {err}");
        assert!(err.contains("Docker Desktop"), "should hint how to fix; got: {err}");
        assert!(err.contains("Cannot connect"), "should surface underlying err; got: {err}");
    }

    #[test]
    fn auto_adapter_uses_probe_recommended_engine() {
        let mut mock = MockProcessInvoker::new();
        // Probe recommends Docker; with Auto, health-check shells docker.
        mock.expect_run()
            .with(eq("docker"), eq(vec!["info".into(), "--format".into(), "{{.ID}}".into()]))
            .returning(|_, _| Ok("ok".into()));
        let config = runtime_with(AdapterChoice::Auto);
        let probe = probe_with_docker();
        check_engine_reachable(&mock, &config, &probe).unwrap();
    }
}
