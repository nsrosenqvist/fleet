//! `Podman` runtime adapter.
//!
//! Image build and container lifecycle are delegated to the
//! [`DevcontainerCli`](super::devcontainer_cli::DevcontainerCli) helper —
//! Podman is docker-CLI compatible, so the same helper that drives Docker
//! drives Podman with `--docker-path podman`. This adapter layers per-backend
//! hardening on top: when gVisor's `runsc` is detected on the host, every
//! `up` adds `--runtime=runsc` through the engine's run-args passthrough.
//!
//! Operations the devcontainer CLI doesn't model — `stop`, `inspect`, future
//! `attach_pty` — shell directly to `podman`. The trait's idempotency rules
//! (`stop` on an unknown container is not an error; `inspect` on an unknown
//! container returns [`ContainerState::Unknown`]) are honoured by translating
//! the engine's error wording into the same closed enum.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::devcontainer_cli::{DevcontainerCli, Engine, UpRequest};
use super::{
    Capabilities, ContainerId, ContainerSpec, ContainerState, Devcontainer, ExecHandle, ExecOpts,
    Hardening, ImageId, NetworkIsolation, PtyHandle, RuntimeAdapter,
};
use crate::process::ProcessInvoker;

/// Adapter that drives rootless Podman through the devcontainer CLI, with
/// optional gVisor hardening.
pub struct PodmanAdapter {
    invoker: Arc<dyn ProcessInvoker>,
    cli: DevcontainerCli,
    has_runsc: bool,
    /// Per-container workspace lookup, populated on `start_container` and
    /// consumed by `exec` (the devcontainer CLI identifies the container by
    /// its workspace folder, not by id).
    workspaces: Arc<Mutex<HashMap<ContainerId, PathBuf>>>,
}

impl PodmanAdapter {
    /// Construct an adapter. `has_runsc` is a host fact obtained by the caller
    /// from [`super::detect::probe`]; passing it in (rather than re-probing
    /// here) keeps the adapter unit-testable without touching the host.
    pub fn new(invoker: Arc<dyn ProcessInvoker>, has_runsc: bool) -> Self {
        let cli = DevcontainerCli::new(Arc::clone(&invoker), Engine::Podman);
        Self {
            invoker,
            cli,
            has_runsc,
            workspaces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn extra_run_args(&self) -> Vec<String> {
        if self.has_runsc {
            vec!["--runtime=runsc".to_string()]
        } else {
            Vec::new()
        }
    }

    fn workspace_for(&self, id: &ContainerId) -> Result<PathBuf> {
        let guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("podman adapter workspace mutex poisoned"))?;
        let ws = guard
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no such container: {id}"));
        drop(guard);
        ws
    }
}

impl RuntimeAdapter for PodmanAdapter {
    fn name(&self) -> &'static str {
        "podman"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardening: if self.has_runsc {
                Hardening::GVisor
            } else {
                Hardening::None
            },
            rootless: true,
            network_isolation: NetworkIsolation::Namespaces,
            can_build_images: true,
            // PTY is supported by `podman exec -it`; the wiring lives in a
            // later chunk, so today `attach_pty` bails NYI even though the
            // capability is true.
            supports_pty: true,
        }
    }

    fn ensure_image(&self, devcontainer: &Devcontainer) -> Result<ImageId> {
        let workspace = workspace_from_devcontainer(devcontainer);
        self.cli.build(&workspace, devcontainer)
    }

    fn start_container(&self, spec: &ContainerSpec) -> Result<ContainerId> {
        // Egress enforcement: when the executor passes a network name,
        // pin the workflow container to that network via podman's
        // `--network=<name>`. Combined with the proxy sidecar attached
        // to the same `--internal` network, this is what closes the
        // outbound path — the container can only reach the proxy.
        let mut extra = self.extra_run_args();
        if let Some(network) = &spec.network {
            extra.push(format!("--network={network}"));
        }
        // DNS stub: when present, pin the container's resolver to the
        // stub sidecar's IP. Combined with the stub returning NXDOMAIN
        // for anything outside the pre-resolved allowlist, this closes
        // the DNS-exfiltration gap aardvark-dns would otherwise leave
        // open (queries forwarded to the host's resolver before failing
        // at the IP layer leak the query content).
        if let Some(dns_ip) = &spec.dns {
            extra.push(format!("--dns={dns_ip}"));
        }
        let req = UpRequest {
            workspace: &spec.workspace,
            config: None,
            artifacts: &spec.artifacts,
            env: &spec.env,
            extra_run_args: &extra,
            extra_mounts: &spec.extra_mounts,
        };
        let id = self.cli.up(&req)?;
        let mut guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("podman adapter workspace mutex poisoned"))?;
        guard.insert(id.clone(), spec.workspace.clone());
        drop(guard);
        Ok(id)
    }

    fn exec(&self, container: &ContainerId, argv: &[String], opts: ExecOpts) -> Result<ExecHandle> {
        let workspace = self.workspace_for(container)?;
        self.cli.exec(&workspace, argv, opts)
    }

    fn attach_pty(&self, container: &ContainerId, user_argv: &[String]) -> Result<PtyHandle> {
        if user_argv.is_empty() {
            bail!("attach_pty argv must contain at least the program name");
        }
        let cli_args = attach_argv(container, user_argv);
        // Inherits the caller's stdio so the terminal *is* the PTY —
        // podman wires the inner process's stdio to the engine's PTY
        // when `-it` is set. Blocks until the inner exits.
        let exit_code = crate::process::run_interactive("podman", &cli_args, &[], &[])?;
        Ok(PtyHandle {
            container: container.clone(),
            exit_code,
        })
    }

    fn stop(&self, container: &ContainerId) -> Result<()> {
        // `podman stop` is graceful by default; we don't follow up with
        // `podman rm` because the devcontainer CLI's
        // `--remove-existing-container` handles cleanup at next `up`.
        let result = self.invoker.run(
            "podman",
            vec!["stop".to_string(), container.as_str().to_string()],
        );
        // Idempotent: missing-container errors are not failures.
        if let Err(err) = result {
            let msg = format!("{err:#}");
            if !is_missing_container_error(&msg) {
                return Err(err);
            }
        }
        let mut guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("podman adapter workspace mutex poisoned"))?;
        guard.remove(container);
        drop(guard);
        Ok(())
    }

    fn inspect(&self, container: &ContainerId) -> Result<ContainerState> {
        let stdout = match self.invoker.run(
            "podman",
            vec![
                "inspect".to_string(),
                "--format".to_string(),
                "{{.State.Status}} {{.State.ExitCode}}".to_string(),
                container.as_str().to_string(),
            ],
        ) {
            Ok(s) => s,
            Err(err) => {
                let msg = format!("{err:#}");
                if is_missing_container_error(&msg) {
                    return Ok(ContainerState::Unknown("not tracked by engine".to_string()));
                }
                return Err(err);
            }
        };
        Ok(parse_inspect_status(&stdout))
    }
}

/// Build the argv for `podman exec -it <id> <cmd…>`. Pure so the shape can
/// be locked in by tests without spawning a real podman subprocess.
fn attach_argv(container: &ContainerId, user_argv: &[String]) -> Vec<String> {
    let mut cli_args = Vec::with_capacity(3 + user_argv.len());
    cli_args.push("exec".to_string());
    cli_args.push("-it".to_string());
    cli_args.push(container.as_str().to_string());
    cli_args.extend(user_argv.iter().cloned());
    cli_args
}

/// Heuristic mapping from a parsed devcontainer file location to the repo root
/// that should be bind-mounted as `/workspaces/<repo>`. Walks past any
/// trailing `.devcontainer` segment (covers both `<repo>/.devcontainer/...`
/// and the multi-config `<repo>/.devcontainer/<name>/...` layout).
fn workspace_from_devcontainer(dc: &Devcontainer) -> PathBuf {
    let mut current = dc
        .source_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    loop {
        if current.file_name().and_then(|n| n.to_str()) == Some(".devcontainer") {
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
                continue;
            }
        }
        // Multi-config: `<repo>/.devcontainer/<name>/devcontainer.json` — the
        // parent of `current` is `.devcontainer`. Hop two levels.
        let parent_named_devcontainer = current
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some(".devcontainer");
        if parent_named_devcontainer {
            if let Some(grand) = current.parent().and_then(Path::parent) {
                current = grand.to_path_buf();
                continue;
            }
        }
        break;
    }
    current
}

/// Podman's "no such container" wording, factored out so both `stop` and
/// `inspect` can branch on it.
fn is_missing_container_error(msg: &str) -> bool {
    msg.contains("no such container")
        || msg.contains("no container with name or ID")
        || msg.contains("Error: no such object")
}

/// Parse `<status> <exit-code>` (the format we ask `podman inspect` for) into
/// [`ContainerState`]. Unknown statuses are surfaced verbatim so the doctor
/// view can show whatever the engine reported.
fn parse_inspect_status(stdout: &str) -> ContainerState {
    let trimmed = stdout.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let status = parts.next().unwrap_or("").to_ascii_lowercase();
    let exit_code = parts
        .next()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    match status.as_str() {
        "created" => ContainerState::Created,
        "running" => ContainerState::Running,
        "exited" | "stopped" => ContainerState::Exited { code: exit_code },
        "dead" => ContainerState::Dead,
        "" => ContainerState::Unknown("empty status".to_string()),
        other => ContainerState::Unknown(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    fn sample_dc() -> Devcontainer {
        Devcontainer::from_str_at(
            r#"{ "image": "mcr.microsoft.com/devcontainers/rust:1" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap()
    }

    fn sample_spec(image: ImageId) -> ContainerSpec {
        ContainerSpec {
            image,
            workspace: PathBuf::from("/repo"),
            artifacts: PathBuf::from("/repo/.fleet/sessions/s1/artifacts"),
            env: vec![],
            command: None,
            network: None,
            dns: None,
            extra_mounts: vec![],
        }
    }

    /// Canned invoker: each (program, args) tuple returns a fixed stdout;
    /// anything unmatched returns an error.
    fn invoker_with(
        responses: Vec<(&'static str, Vec<String>, String)>,
    ) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        for (prog, args, out) in responses {
            mock.expect_run()
                .with(eq(prog), eq(args))
                .returning(move |_, _| Ok(out.clone()));
        }
        mock.expect_run()
            .returning(|prog, args| Err(anyhow!("unexpected call: {prog} {args:?}")));
        Arc::new(mock)
    }

    #[test]
    fn name_is_podman() {
        let a = PodmanAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        assert_eq!(a.name(), "podman");
    }

    #[test]
    fn capabilities_without_runsc_report_no_hardening() {
        let a = PodmanAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        let c = a.capabilities();
        assert_eq!(c.hardening, Hardening::None);
        assert!(c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::Namespaces);
        assert!(c.can_build_images);
        assert!(c.supports_pty);
    }

    #[test]
    fn capabilities_with_runsc_report_gvisor() {
        let a = PodmanAdapter::new(Arc::new(MockProcessInvoker::new()), true);
        assert_eq!(a.capabilities().hardening, Hardening::GVisor);
    }

    #[test]
    fn ensure_image_invokes_devcontainer_build_with_podman_engine() {
        let dc = sample_dc();
        let minted = DevcontainerCli::mint_image_name(&dc);
        let build_args = vec![
            "build".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--config".to_string(),
            "/repo/.devcontainer/devcontainer.json".to_string(),
            "--image-name".to_string(),
            minted.clone(),
            "--docker-path".to_string(),
            "podman".to_string(),
        ];
        let stdout = format!(r#"{{"outcome":"success","imageName":"{minted}"}}"#);
        let invoker = invoker_with(vec![("devcontainer", build_args, stdout)]);
        let a = PodmanAdapter::new(invoker, false);
        let id = a.ensure_image(&dc).unwrap();
        assert_eq!(id.as_str(), minted);
    }

    #[test]
    fn start_container_appends_runsc_run_args_when_runsc_present() {
        let dc = sample_dc();
        let up_args_with_runsc = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
            "--run-args".to_string(),
            "--runtime=runsc".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-podman-1"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args_with_runsc, stdout)]);
        let a = PodmanAdapter::new(invoker, true);
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-podman-1");
    }

    #[test]
    fn start_container_appends_network_flag_when_spec_has_network() {
        // Egress enforcement: when the executor passes spec.network,
        // the Podman adapter must add `--network=<name>` to the
        // devcontainer CLI's `--run-args` chain so the container
        // joins that network (and only that network).
        let dc = sample_dc();
        let up_args_with_net = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
            "--run-args".to_string(),
            "--network=fleet-s-egress".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-net-1"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args_with_net, stdout)]);
        let a = PodmanAdapter::new(invoker, false);
        let mut spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        spec.network = Some("fleet-s-egress".to_string());
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-net-1");
    }

    #[test]
    fn start_container_appends_dns_flag_when_spec_has_dns() {
        // DNS stub: when the executor passes spec.dns (the DNS sidecar's
        // IP), the Podman adapter must add `--dns=<ip>` to the
        // devcontainer CLI's `--run-args` chain so the container's
        // resolv.conf points only at the stub.
        let dc = sample_dc();
        let up_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
            "--run-args".to_string(),
            "--network=fleet-s-egress".to_string(),
            "--run-args".to_string(),
            "--dns=10.89.0.5".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-dns-1"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args, stdout)]);
        let a = PodmanAdapter::new(invoker, false);
        let mut spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        spec.network = Some("fleet-s-egress".to_string());
        spec.dns = Some("10.89.0.5".to_string());
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-dns-1");
    }

    #[test]
    fn start_container_combines_runsc_and_network_flags() {
        // Both hardening and egress active: the run-args list grows
        // to include both. Order matters for the matcher; runsc comes
        // first because it's set up by the adapter's constructor.
        let dc = sample_dc();
        let up_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
            "--run-args".to_string(),
            "--runtime=runsc".to_string(),
            "--run-args".to_string(),
            "--network=fleet-s-x".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-combo"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args, stdout)]);
        let a = PodmanAdapter::new(invoker, true);
        let mut spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        spec.network = Some("fleet-s-x".to_string());
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-combo");
    }

    #[test]
    fn start_container_omits_runsc_run_args_when_runsc_absent() {
        let up_args_no_runsc = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-2"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args_no_runsc, stdout)]);
        let a = PodmanAdapter::new(invoker, false);
        let dc = sample_dc();
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-2");
    }

    #[test]
    fn exec_uses_recorded_workspace_for_started_container() {
        let dc = sample_dc();
        let up_stdout = r#"{"outcome":"success","containerId":"c-x"}"#.to_string();
        let exec_args = vec![
            "exec".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "hi".to_string(),
        ];
        let invoker = invoker_with(vec![
            (
                "devcontainer",
                vec![
                    "up".to_string(),
                    "--workspace-folder".to_string(),
                    "/repo".to_string(),
                    "--remove-existing-container".to_string(),
                    "--docker-path".to_string(),
                    "podman".to_string(),
                    "--mount".to_string(),
                    "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts"
                        .to_string(),
                ],
                up_stdout,
            ),
            ("devcontainer", exec_args, "hi\n".to_string()),
        ]);
        let a = PodmanAdapter::new(invoker, false);
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        let h = a
            .exec(
                &id,
                &["echo".to_string(), "hi".to_string()],
                ExecOpts::default(),
            )
            .unwrap();
        assert_eq!(h.exit_code, 0);
        assert_eq!(h.stdout, "hi\n");
    }

    #[test]
    fn exec_rejects_unknown_container() {
        let a = PodmanAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        let err = a
            .exec(
                &ContainerId::new("nope"),
                &["true".to_string()],
                ExecOpts::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("no such container"));
    }

    #[test]
    fn attach_pty_rejects_empty_argv() {
        // The actual subprocess spawn inherits stdio and isn't unit-
        // testable here; argv-shape coverage lives in the
        // `attach_argv_*` tests below. Empty argv has to be caught up
        // front so callers don't get a cryptic engine error.
        let a = PodmanAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        let err = a.attach_pty(&ContainerId::new("c"), &[]).unwrap_err();
        assert!(format!("{err}").contains("at least the program name"));
    }

    #[test]
    fn attach_argv_wraps_exec_dash_it_with_container_id() {
        let argv = attach_argv(&ContainerId::new("c-123"), &["bash".to_string()]);
        assert_eq!(argv, vec!["exec", "-it", "c-123", "bash"]);
    }

    #[test]
    fn attach_argv_passes_through_multi_arg_commands() {
        let argv = attach_argv(
            &ContainerId::new("c-1"),
            &["sh".to_string(), "-c".to_string(), "echo hi".to_string()],
        );
        assert_eq!(argv, vec!["exec", "-it", "c-1", "sh", "-c", "echo hi"]);
    }

    #[test]
    fn stop_calls_podman_stop_and_clears_workspace_mapping() {
        let dc = sample_dc();
        let invoker = invoker_with(vec![
            (
                "devcontainer",
                vec![
                    "up".to_string(),
                    "--workspace-folder".to_string(),
                    "/repo".to_string(),
                    "--remove-existing-container".to_string(),
                    "--docker-path".to_string(),
                    "podman".to_string(),
                    "--mount".to_string(),
                    "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts"
                        .to_string(),
                ],
                r#"{"outcome":"success","containerId":"c-stop"}"#.to_string(),
            ),
            (
                "podman",
                vec!["stop".to_string(), "c-stop".to_string()],
                "c-stop".to_string(),
            ),
        ]);
        let a = PodmanAdapter::new(invoker, false);
        let id = a
            .start_container(&sample_spec(ImageId::new(
                DevcontainerCli::mint_image_name(&dc),
            )))
            .unwrap();
        a.stop(&id).unwrap();
        // After stop, the workspace mapping must be gone — subsequent exec
        // surfaces as "no such container" rather than racing into a stale
        // workspace lookup.
        let err = a
            .exec(&id, &["true".to_string()], ExecOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("no such container"));
    }

    #[test]
    fn stop_is_idempotent_for_unknown_container() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "`podman stop` exited with 125: Error: no such container nope"
            ))
        });
        let a = PodmanAdapter::new(Arc::new(mock), false);
        a.stop(&ContainerId::new("nope")).unwrap();
    }

    #[test]
    fn stop_propagates_unexpected_errors() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("kernel panic")));
        let a = PodmanAdapter::new(Arc::new(mock), false);
        let err = a.stop(&ContainerId::new("c")).unwrap_err();
        assert!(format!("{err}").contains("kernel panic"));
    }

    #[test]
    fn inspect_running_container() {
        let invoker = invoker_with(vec![(
            "podman",
            vec![
                "inspect".to_string(),
                "--format".to_string(),
                "{{.State.Status}} {{.State.ExitCode}}".to_string(),
                "c-run".to_string(),
            ],
            "running 0".to_string(),
        )]);
        let a = PodmanAdapter::new(invoker, false);
        assert_eq!(
            a.inspect(&ContainerId::new("c-run")).unwrap(),
            ContainerState::Running
        );
    }

    #[test]
    fn inspect_exited_container_carries_exit_code() {
        let invoker = invoker_with(vec![(
            "podman",
            vec![
                "inspect".to_string(),
                "--format".to_string(),
                "{{.State.Status}} {{.State.ExitCode}}".to_string(),
                "c-done".to_string(),
            ],
            "exited 137".to_string(),
        )]);
        let a = PodmanAdapter::new(invoker, false);
        assert_eq!(
            a.inspect(&ContainerId::new("c-done")).unwrap(),
            ContainerState::Exited { code: 137 }
        );
    }

    #[test]
    fn inspect_unknown_container_returns_unknown_state() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "`podman inspect` exited with 125: Error: no such container ghost"
            ))
        });
        let a = PodmanAdapter::new(Arc::new(mock), false);
        match a.inspect(&ContainerId::new("ghost")).unwrap() {
            ContainerState::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn workspace_from_devcontainer_handles_canonical_layout() {
        let dc =
            Devcontainer::from_str_at(r#"{"image":"x"}"#, "/repo/.devcontainer/devcontainer.json")
                .unwrap();
        assert_eq!(workspace_from_devcontainer(&dc), PathBuf::from("/repo"));
    }

    #[test]
    fn workspace_from_devcontainer_handles_multi_config_layout() {
        let dc = Devcontainer::from_str_at(
            r#"{"image":"x"}"#,
            "/repo/.devcontainer/rust/devcontainer.json",
        )
        .unwrap();
        assert_eq!(workspace_from_devcontainer(&dc), PathBuf::from("/repo"));
    }

    #[test]
    fn workspace_from_devcontainer_handles_root_level_file() {
        // `.devcontainer.json` (no directory) lives at the repo root.
        let dc = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/repo/.devcontainer.json").unwrap();
        assert_eq!(workspace_from_devcontainer(&dc), PathBuf::from("/repo"));
    }

    #[test]
    fn adapter_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PodmanAdapter>();
    }

    #[test]
    fn parse_inspect_status_unknown_value_surfaces_verbatim() {
        match parse_inspect_status("paused 0") {
            ContainerState::Unknown(s) => assert_eq!(s, "paused"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
