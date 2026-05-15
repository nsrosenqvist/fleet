//! `Docker` runtime adapter.
//!
//! Cross-platform fallback. Image build and container lifecycle flow through
//! the same [`DevcontainerCli`](super::devcontainer_cli::DevcontainerCli) the
//! Podman adapter uses; the only difference is the engine flag
//! (`--docker-path docker`) and that no per-backend hardening is layered on
//! top (no gVisor; the spec defers to Docker's own seccomp/AppArmor profile).
//!
//! Rootless mode is reported per-construction — the caller probes
//! `docker info` and tells the adapter what it found. If rootless can't be
//! negotiated this is the weakest configuration we ship and is documented as
//! such; the TUI surfaces it every session, per the plan's "warn every
//! session, not silently" rule.

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

/// Adapter that drives Docker through the devcontainer CLI. Sibling to
/// [`super::podman::PodmanAdapter`]; kept as a separate struct (rather than a
/// generic over `Engine`) so each adapter owns its own capability story.
pub struct DockerAdapter {
    invoker: Arc<dyn ProcessInvoker>,
    cli: DevcontainerCli,
    rootless: bool,
    workspaces: Arc<Mutex<HashMap<ContainerId, PathBuf>>>,
}

impl DockerAdapter {
    /// Construct an adapter. `rootless` is a host fact obtained by the caller
    /// from `docker info` (rootless engines report `Name: rootless` in their
    /// security options); passing it in here keeps the adapter unit-testable.
    pub fn new(invoker: Arc<dyn ProcessInvoker>, rootless: bool) -> Self {
        let cli = DevcontainerCli::new(Arc::clone(&invoker), Engine::Docker);
        Self {
            invoker,
            cli,
            rootless,
            workspaces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn workspace_for(&self, id: &ContainerId) -> Result<PathBuf> {
        let guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("docker adapter workspace mutex poisoned"))?;
        let ws = guard
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no such container: {id}"));
        drop(guard);
        ws
    }
}

impl RuntimeAdapter for DockerAdapter {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Docker on its own provides no kernel-isolation upgrade beyond
            // namespaces + seccomp. The plan reserves gVisor for the Podman
            // path; we don't try to detect runsc-under-docker today.
            hardening: Hardening::None,
            rootless: self.rootless,
            network_isolation: NetworkIsolation::Namespaces,
            can_build_images: true,
            supports_pty: true,
        }
    }

    fn ensure_image(&self, devcontainer: &Devcontainer) -> Result<ImageId> {
        let workspace = workspace_from_devcontainer(devcontainer);
        self.cli.build(&workspace, devcontainer)
    }

    fn start_container(&self, spec: &ContainerSpec) -> Result<ContainerId> {
        let req = UpRequest {
            workspace: &spec.workspace,
            config: None,
            artifacts: &spec.artifacts,
            env: &spec.env,
            extra_run_args: &[],
        };
        let id = self.cli.up(&req)?;
        let mut guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("docker adapter workspace mutex poisoned"))?;
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
        let exit_code = crate::process::run_interactive("docker", &cli_args, &[], &[])?;
        Ok(PtyHandle {
            container: container.clone(),
            exit_code,
        })
    }

    fn stop(&self, container: &ContainerId) -> Result<()> {
        let result = self.invoker.run(
            "docker",
            vec!["stop".to_string(), container.as_str().to_string()],
        );
        if let Err(err) = result {
            let msg = format!("{err:#}");
            if !is_missing_container_error(&msg) {
                return Err(err);
            }
        }
        let mut guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("docker adapter workspace mutex poisoned"))?;
        guard.remove(container);
        drop(guard);
        Ok(())
    }

    fn inspect(&self, container: &ContainerId) -> Result<ContainerState> {
        let stdout = match self.invoker.run(
            "docker",
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

/// Build the argv for `docker exec -it <id> <cmd…>`. Pure so the shape
/// can be locked in by tests without spawning real docker.
fn attach_argv(container: &ContainerId, user_argv: &[String]) -> Vec<String> {
    let mut cli_args = Vec::with_capacity(3 + user_argv.len());
    cli_args.push("exec".to_string());
    cli_args.push("-it".to_string());
    cli_args.push(container.as_str().to_string());
    cli_args.extend(user_argv.iter().cloned());
    cli_args
}

/// Same heuristic as [`super::podman`] for mapping a parsed devcontainer file
/// back to its workspace root. Duplicated rather than shared via a free
/// function in `mod.rs` because in the medium term each adapter is likely to
/// grow its own quirks (Apple Containerization may not use the same mount
/// strategy at all), and the boundary stays cleaner with one heuristic per
/// adapter file.
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

/// Docker's "no such container" wording differs slightly from podman's
/// (capital N, "Error: No such container:"). Detect both forms so behaviour
/// remains correct as users upgrade or downgrade their engine.
fn is_missing_container_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("no such container") || lower.contains("error: no such object")
}

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
            r#"{ "image": "node:20" }"#,
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
        }
    }

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
    fn name_is_docker() {
        let a = DockerAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        assert_eq!(a.name(), "docker");
    }

    #[test]
    fn capabilities_reflect_rootful_construction() {
        let a = DockerAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        let c = a.capabilities();
        assert_eq!(c.hardening, Hardening::None);
        assert!(!c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::Namespaces);
        assert!(c.can_build_images);
        assert!(c.supports_pty);
    }

    #[test]
    fn capabilities_reflect_rootless_construction() {
        let a = DockerAdapter::new(Arc::new(MockProcessInvoker::new()), true);
        assert!(a.capabilities().rootless);
    }

    #[test]
    fn ensure_image_invokes_devcontainer_build_with_docker_engine() {
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
            "docker".to_string(),
        ];
        let stdout = format!(r#"{{"outcome":"success","imageName":"{minted}"}}"#);
        let invoker = invoker_with(vec![("devcontainer", build_args, stdout)]);
        let a = DockerAdapter::new(invoker, false);
        let id = a.ensure_image(&dc).unwrap();
        assert_eq!(id.as_str(), minted);
    }

    #[test]
    fn start_container_passes_no_extra_run_args() {
        // Docker adapter must never inject `--runtime=runsc` — that's the
        // Podman path. Test asserts on the exact argv to lock this in.
        let up_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "docker".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-docker-1"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args, stdout)]);
        let a = DockerAdapter::new(invoker, true);
        let dc = sample_dc();
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-docker-1");
    }

    #[test]
    fn exec_uses_recorded_workspace_for_started_container() {
        let dc = sample_dc();
        let up_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "docker".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
        ];
        let exec_args = vec![
            "exec".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--docker-path".to_string(),
            "docker".to_string(),
            "--".to_string(),
            "pwd".to_string(),
        ];
        let invoker = invoker_with(vec![
            (
                "devcontainer",
                up_args,
                r#"{"outcome":"success","containerId":"c-d"}"#.to_string(),
            ),
            ("devcontainer", exec_args, "/workspaces/repo\n".to_string()),
        ]);
        let a = DockerAdapter::new(invoker, false);
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        let h = a
            .exec(&id, &["pwd".to_string()], ExecOpts::default())
            .unwrap();
        assert_eq!(h.exit_code, 0);
        assert!(h.stdout.contains("/workspaces/repo"));
    }

    #[test]
    fn exec_rejects_unknown_container() {
        let a = DockerAdapter::new(Arc::new(MockProcessInvoker::new()), false);
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
        let a = DockerAdapter::new(Arc::new(MockProcessInvoker::new()), false);
        let err = a.attach_pty(&ContainerId::new("c"), &[]).unwrap_err();
        assert!(format!("{err}").contains("at least the program name"));
    }

    #[test]
    fn attach_argv_wraps_exec_dash_it_with_container_id() {
        let argv = attach_argv(&ContainerId::new("c-7"), &["bash".to_string()]);
        assert_eq!(argv, vec!["exec", "-it", "c-7", "bash"]);
    }

    #[test]
    fn attach_argv_passes_through_multi_arg_commands() {
        let argv = attach_argv(
            &ContainerId::new("c-1"),
            &["sh".to_string(), "-c".to_string(), "ls -la".to_string()],
        );
        assert_eq!(argv, vec!["exec", "-it", "c-1", "sh", "-c", "ls -la"]);
    }

    #[test]
    fn stop_calls_docker_stop_and_clears_workspace_mapping() {
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
                    "docker".to_string(),
                    "--mount".to_string(),
                    "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts"
                        .to_string(),
                ],
                r#"{"outcome":"success","containerId":"c-stop"}"#.to_string(),
            ),
            (
                "docker",
                vec!["stop".to_string(), "c-stop".to_string()],
                "c-stop".to_string(),
            ),
        ]);
        let a = DockerAdapter::new(invoker, false);
        let id = a
            .start_container(&sample_spec(ImageId::new(
                DevcontainerCli::mint_image_name(&dc),
            )))
            .unwrap();
        a.stop(&id).unwrap();
        let err = a
            .exec(&id, &["true".to_string()], ExecOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("no such container"));
    }

    #[test]
    fn stop_is_idempotent_for_unknown_container() {
        // Docker uses capital N: `Error: No such container: nope`. Detect
        // both cases — the matcher is case-insensitive on purpose.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "`docker stop` exited with 1: Error: No such container: nope"
            ))
        });
        let a = DockerAdapter::new(Arc::new(mock), false);
        a.stop(&ContainerId::new("nope")).unwrap();
    }

    #[test]
    fn stop_propagates_unexpected_errors() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Err(anyhow!("daemon not running")));
        let a = DockerAdapter::new(Arc::new(mock), false);
        let err = a.stop(&ContainerId::new("c")).unwrap_err();
        assert!(format!("{err}").contains("daemon not running"));
    }

    #[test]
    fn inspect_running_container() {
        let invoker = invoker_with(vec![(
            "docker",
            vec![
                "inspect".to_string(),
                "--format".to_string(),
                "{{.State.Status}} {{.State.ExitCode}}".to_string(),
                "c-run".to_string(),
            ],
            "running 0".to_string(),
        )]);
        let a = DockerAdapter::new(invoker, false);
        assert_eq!(
            a.inspect(&ContainerId::new("c-run")).unwrap(),
            ContainerState::Running
        );
    }

    #[test]
    fn inspect_exited_container_carries_exit_code() {
        let invoker = invoker_with(vec![(
            "docker",
            vec![
                "inspect".to_string(),
                "--format".to_string(),
                "{{.State.Status}} {{.State.ExitCode}}".to_string(),
                "c-done".to_string(),
            ],
            "exited 2".to_string(),
        )]);
        let a = DockerAdapter::new(invoker, false);
        assert_eq!(
            a.inspect(&ContainerId::new("c-done")).unwrap(),
            ContainerState::Exited { code: 2 }
        );
    }

    #[test]
    fn inspect_unknown_container_returns_unknown_state() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "`docker inspect` exited with 1: Error: No such container: ghost"
            ))
        });
        let a = DockerAdapter::new(Arc::new(mock), false);
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
    fn adapter_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DockerAdapter>();
    }
}
