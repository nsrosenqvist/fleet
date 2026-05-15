//! Build a concrete [`RuntimeAdapter`] from a parsed [`RuntimeConfig`] and a
//! host [`ProbeReport`].
//!
//! Two-stage resolution: first the adapter *kind* (Auto resolves to the probe's
//! recommendation; explicit choices are pinned regardless of what's installed),
//! then the hardening flag (Auto enables gVisor on Podman when runsc is
//! present; explicit Gvisor outside Podman is an error so the user sees the
//! mistake before they spawn anything).
//!
//! Errors are written in user-facing terms — they're surfaced verbatim through
//! `fleet runtime …` commands and shouldn't leak implementation jargon.

use anyhow::{Result, bail};
use std::path::Path;
use std::sync::Arc;

use super::apple_container::AppleContainerAdapter;
use super::detect::{BackendKind, ProbeReport, probe};
use super::docker::DockerAdapter;
use super::local::LocalAdapter;
use super::podman::PodmanAdapter;
use super::{AdapterStopper, RuntimeAdapter};
use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo_config::{AdapterChoice, HardeningChoice, RepoConfig, RuntimeConfig};
use crate::session::reaper::{ContainerStopper, NoopStopper};

/// Build the runtime adapter for this host given the repo config and a fresh
/// probe report. `invoker` is shared with the constructed adapter; callers
/// typically pass an `Arc<RealProcessInvoker>` in production and a mock in
/// tests.
pub fn build_adapter(
    config: &RuntimeConfig,
    probe: &ProbeReport,
    invoker: Arc<dyn ProcessInvoker>,
) -> Result<Box<dyn RuntimeAdapter>> {
    let kind = resolve_kind(config.adapter, probe)?;

    match kind {
        ResolvedKind::Podman => {
            let use_runsc = resolve_podman_hardening(config.hardening, probe)?;
            Ok(Box::new(PodmanAdapter::new(invoker, use_runsc)))
        }
        ResolvedKind::Docker => {
            forbid_gvisor_outside_podman(config.hardening, "docker")?;
            let rootless = probe_docker_rootless(invoker.as_ref());
            Ok(Box::new(DockerAdapter::new(invoker, rootless)))
        }
        ResolvedKind::AppleContainer => {
            forbid_gvisor_outside_podman(config.hardening, "apple-container")?;
            Ok(Box::new(AppleContainerAdapter::new(invoker)))
        }
        ResolvedKind::Local => {
            forbid_gvisor_outside_podman(config.hardening, "local")?;
            Ok(Box::new(LocalAdapter::new(invoker)))
        }
    }
}

/// Post-resolution adapter kind. Distinct from [`AdapterChoice`] because the
/// `Auto` variant is gone — by this point we've collapsed it against the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedKind {
    Podman,
    Docker,
    AppleContainer,
    Local,
}

fn resolve_kind(choice: AdapterChoice, probe: &ProbeReport) -> Result<ResolvedKind> {
    match choice {
        AdapterChoice::Auto => match probe.recommended {
            Some(BackendKind::Podman) => Ok(ResolvedKind::Podman),
            Some(BackendKind::Docker) => Ok(ResolvedKind::Docker),
            Some(BackendKind::AppleContainer) => Ok(ResolvedKind::AppleContainer),
            // GVisor/DevcontainerCli aren't engines; `recommended` never
            // points at them. The exhaustive arm keeps the compiler happy
            // if the enum grows.
            Some(other) => bail!(
                "internal: probe recommended non-engine backend {other:?}"
            ),
            None => bail!(
                "no container engine detected — run `fleet runtime doctor` for install hints"
            ),
        },
        AdapterChoice::Podman => {
            if !probe.podman.present {
                bail!(
                    "`adapter: podman` requested but podman is not installed — run `fleet runtime doctor`"
                );
            }
            Ok(ResolvedKind::Podman)
        }
        AdapterChoice::Docker => {
            if !probe.docker.present {
                bail!(
                    "`adapter: docker` requested but docker is not installed — run `fleet runtime doctor`"
                );
            }
            Ok(ResolvedKind::Docker)
        }
        AdapterChoice::AppleContainer => {
            if !probe.apple_container.present {
                bail!(
                    "`adapter: apple-container` requested but Apple's `container` CLI is not installed — \
                     requires macOS 26+; run `fleet runtime doctor`"
                );
            }
            Ok(ResolvedKind::AppleContainer)
        }
        AdapterChoice::Local => Ok(ResolvedKind::Local),
    }
}

fn resolve_podman_hardening(choice: HardeningChoice, probe: &ProbeReport) -> Result<bool> {
    match choice {
        HardeningChoice::Auto => Ok(probe.gvisor.present),
        HardeningChoice::Gvisor => {
            if !probe.gvisor.present {
                bail!(
                    "`hardening: gvisor` requested but runsc is not installed — run `fleet runtime doctor` for install hints"
                );
            }
            Ok(true)
        }
        HardeningChoice::None => Ok(false),
    }
}

/// gVisor is a Linux/Podman-only story. Asking for it on Docker, Apple
/// Containerization, or Local is a configuration mistake — fail fast with
/// the actionable message rather than constructing a "rootless docker
/// secretly with no hardening" adapter.
fn forbid_gvisor_outside_podman(choice: HardeningChoice, adapter_name: &str) -> Result<()> {
    if matches!(choice, HardeningChoice::Gvisor) {
        bail!(
            "`hardening: gvisor` is only supported with the podman adapter; \
             chosen adapter is {adapter_name}. Set `hardening: auto` or `none`."
        );
    }
    Ok(())
}

/// Best-effort detection of rootless Docker. Calls
/// `docker info --format {{json .SecurityOptions}}` and looks for the
/// `name=rootless` token. Any error (daemon not running, parse failure)
/// resolves to `false`; the TUI will surface a per-session warning when
/// rootless is false, so a missed-true here is the safer default.
fn probe_docker_rootless(invoker: &dyn ProcessInvoker) -> bool {
    invoker
        .run(
            "docker",
            vec![
                "info".to_string(),
                "--format".to_string(),
                "{{json .SecurityOptions}}".to_string(),
            ],
        )
        .is_ok_and(|stdout| stdout.contains("name=rootless"))
}

/// Build a `ContainerStopper` for the fleet root at `fleet_root`,
/// falling back to [`NoopStopper`] on any failure. The reaper calls
/// this at startup; a misconfigured `.fleet/config.yaml` (or one that
/// references an uninstalled engine) must not prevent the reaper from
/// transitioning crashed sessions out of `Running`. Failures are
/// logged at `warn` so users still see what went wrong if they're
/// investigating.
///
/// The boxed stopper owns its adapter — it's a one-shot built for
/// the sweep, not the long-lived adapter the workflow executor uses.
pub fn build_stopper(fleet_root: &Path) -> Box<dyn ContainerStopper> {
    let config_path = fleet_root.join(".fleet/config.yaml");
    let config = match RepoConfig::load(&config_path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, path = %config_path.display(), "stopper: falling back to noop (config unreadable)");
            return Box::new(NoopStopper);
        }
    };
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let report = probe(invoker.as_ref());
    match build_adapter(&config.runtime, &report, invoker) {
        Ok(adapter) => Box::new(AdapterStopper::new(adapter)),
        Err(err) => {
            tracing::warn!(error = %err, "stopper: falling back to noop (adapter build failed)");
            Box::new(NoopStopper)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::runtime::detect::BackendStatus;
    use crate::runtime::{Hardening, NetworkIsolation};
    use mockall::predicate::eq;

    fn present(kind: BackendKind) -> BackendStatus {
        BackendStatus {
            kind,
            present: true,
            version: Some("v1.0".to_string()),
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

    fn empty_probe() -> ProbeReport {
        ProbeReport {
            podman: absent(BackendKind::Podman),
            docker: absent(BackendKind::Docker),
            apple_container: absent(BackendKind::AppleContainer),
            gvisor: absent(BackendKind::GVisor),
            devcontainer_cli: absent(BackendKind::DevcontainerCli),
            recommended: None,
        }
    }

    fn probe_podman_only() -> ProbeReport {
        let mut p = empty_probe();
        p.podman = present(BackendKind::Podman);
        p.recommended = Some(BackendKind::Podman);
        p
    }

    fn probe_podman_with_runsc() -> ProbeReport {
        let mut p = probe_podman_only();
        p.gvisor = present(BackendKind::GVisor);
        p
    }

    fn probe_docker_only() -> ProbeReport {
        let mut p = empty_probe();
        p.docker = present(BackendKind::Docker);
        p.recommended = Some(BackendKind::Docker);
        p
    }

    fn probe_apple_only() -> ProbeReport {
        let mut p = empty_probe();
        p.apple_container = present(BackendKind::AppleContainer);
        p.recommended = Some(BackendKind::AppleContainer);
        p
    }

    fn noop_invoker() -> Arc<dyn ProcessInvoker> {
        Arc::new(MockProcessInvoker::new())
    }

    fn docker_rootless_invoker() -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("docker"),
                eq(vec![
                    "info".to_string(),
                    "--format".to_string(),
                    "{{json .SecurityOptions}}".to_string(),
                ]),
            )
            .returning(|_, _| Ok(r#"["name=rootless","name=seccomp"]"#.to_string()));
        Arc::new(mock)
    }

    fn docker_rootful_invoker() -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok(r#"["name=seccomp"]"#.to_string()));
        Arc::new(mock)
    }

    fn docker_daemon_down_invoker() -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow::anyhow!("cannot connect to daemon")));
        Arc::new(mock)
    }

    /// Extract the `Err` from a `Result` whose `Ok` type doesn't implement
    /// `Debug` (Box<dyn RuntimeAdapter> doesn't, so `.unwrap_err()` won't
    /// compile against it).
    fn expect_err<T>(r: Result<T>) -> anyhow::Error {
        match r {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn auto_with_podman_recommended_builds_podman() {
        let cfg = RuntimeConfig::default();
        let probe = probe_podman_only();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.name(), "podman");
    }

    #[test]
    fn auto_with_apple_recommended_builds_apple_container() {
        let cfg = RuntimeConfig::default();
        let probe = probe_apple_only();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.name(), "apple-container");
        assert_eq!(a.capabilities().hardening, Hardening::MicroVm);
    }

    #[test]
    fn auto_with_docker_recommended_builds_docker_and_probes_rootless() {
        let cfg = RuntimeConfig::default();
        let probe = probe_docker_only();
        let a = build_adapter(&cfg, &probe, docker_rootless_invoker()).unwrap();
        assert_eq!(a.name(), "docker");
        assert!(a.capabilities().rootless);
    }

    #[test]
    fn auto_with_docker_recommended_reports_rootful_when_not_rootless() {
        let cfg = RuntimeConfig::default();
        let probe = probe_docker_only();
        let a = build_adapter(&cfg, &probe, docker_rootful_invoker()).unwrap();
        assert!(!a.capabilities().rootless);
    }

    #[test]
    fn auto_with_no_engine_returns_actionable_error() {
        let cfg = RuntimeConfig::default();
        let probe = empty_probe();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        let msg = format!("{err}");
        assert!(msg.contains("no container engine"), "msg = {msg}");
        assert!(msg.contains("fleet runtime doctor"), "msg = {msg}");
    }

    #[test]
    fn explicit_podman_when_not_installed_is_an_error() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Podman,
            ..Default::default()
        };
        let probe = empty_probe();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        assert!(format!("{err}").contains("podman is not installed"));
    }

    #[test]
    fn explicit_docker_when_not_installed_is_an_error() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Docker,
            ..Default::default()
        };
        let probe = empty_probe();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        assert!(format!("{err}").contains("docker is not installed"));
    }

    #[test]
    fn explicit_apple_container_when_not_installed_is_an_error() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::AppleContainer,
            ..Default::default()
        };
        let probe = empty_probe();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        let msg = format!("{err}");
        assert!(msg.contains("Apple's `container` CLI is not installed"));
        assert!(msg.contains("macOS 26"));
    }

    #[test]
    fn local_can_be_built_even_when_nothing_is_installed() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Local,
            ..Default::default()
        };
        let probe = empty_probe();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.name(), "local");
        assert_eq!(a.capabilities().network_isolation, NetworkIsolation::None);
    }

    #[test]
    fn hardening_auto_on_podman_uses_gvisor_when_runsc_present() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Podman,
            hardening: HardeningChoice::Auto,
            ..Default::default()
        };
        let probe = probe_podman_with_runsc();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.capabilities().hardening, Hardening::GVisor);
    }

    #[test]
    fn hardening_auto_on_podman_leaves_gvisor_off_when_runsc_missing() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Podman,
            hardening: HardeningChoice::Auto,
            ..Default::default()
        };
        let probe = probe_podman_only();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.capabilities().hardening, Hardening::None);
    }

    #[test]
    fn hardening_gvisor_without_runsc_errors() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Podman,
            hardening: HardeningChoice::Gvisor,
            ..Default::default()
        };
        let probe = probe_podman_only();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        let msg = format!("{err}");
        assert!(msg.contains("runsc is not installed"), "msg = {msg}");
    }

    #[test]
    fn hardening_gvisor_with_docker_is_a_config_error() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Docker,
            hardening: HardeningChoice::Gvisor,
            ..Default::default()
        };
        let probe = probe_docker_only();
        let err = expect_err(build_adapter(&cfg, &probe, docker_rootful_invoker()));
        let msg = format!("{err}");
        assert!(msg.contains("only supported with the podman adapter"), "msg = {msg}");
        assert!(msg.contains("docker"));
    }

    #[test]
    fn hardening_gvisor_with_apple_container_is_a_config_error() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::AppleContainer,
            hardening: HardeningChoice::Gvisor,
            ..Default::default()
        };
        let probe = probe_apple_only();
        let err = expect_err(build_adapter(&cfg, &probe, noop_invoker()));
        let msg = format!("{err}");
        assert!(msg.contains("only supported with the podman adapter"));
    }

    #[test]
    fn hardening_none_overrides_runsc_presence() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Podman,
            hardening: HardeningChoice::None,
            ..Default::default()
        };
        let probe = probe_podman_with_runsc();
        let a = build_adapter(&cfg, &probe, noop_invoker()).unwrap();
        assert_eq!(a.capabilities().hardening, Hardening::None);
    }

    #[test]
    fn docker_rootless_probe_treats_daemon_down_as_rootful() {
        let cfg = RuntimeConfig {
            adapter: AdapterChoice::Docker,
            ..Default::default()
        };
        let probe = probe_docker_only();
        let a = build_adapter(&cfg, &probe, docker_daemon_down_invoker()).unwrap();
        assert!(!a.capabilities().rootless);
    }
}
