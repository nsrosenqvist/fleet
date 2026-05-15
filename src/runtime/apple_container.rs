//! `AppleContainer` runtime adapter (macOS 26+).
//!
//! Each container runs in its own Virtualization.framework microVM — the
//! kernel isolation story is provided by the OS, not by fleet. The adapter
//! therefore declares [`Hardening::MicroVm`] and
//! [`NetworkIsolation::MicroVm`] unconditionally, and passes no per-backend
//! run-args to the engine.
//!
//! Image build and container lifecycle flow through the same
//! [`DevcontainerCli`](super::devcontainer_cli::DevcontainerCli) helper the
//! Linux adapters use, with `--docker-path container` selecting Apple's CLI.
//! `stop` and `inspect` shell directly to `container` because the
//! devcontainer CLI doesn't expose those operations as separate commands.

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

/// Adapter that drives Apple Containerization through the devcontainer CLI.
/// Sibling to [`super::podman::PodmanAdapter`] and
/// [`super::docker::DockerAdapter`]; kept as a separate struct so each
/// adapter owns its own capability story.
pub struct AppleContainerAdapter {
    invoker: Arc<dyn ProcessInvoker>,
    cli: DevcontainerCli,
    workspaces: Arc<Mutex<HashMap<ContainerId, PathBuf>>>,
}

impl AppleContainerAdapter {
    pub fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        let cli = DevcontainerCli::new(Arc::clone(&invoker), Engine::AppleContainer);
        Self {
            invoker,
            cli,
            workspaces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn workspace_for(&self, id: &ContainerId) -> Result<PathBuf> {
        let guard = self
            .workspaces
            .lock()
            .map_err(|_| anyhow!("apple-container adapter workspace mutex poisoned"))?;
        let ws = guard
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no such container: {id}"));
        drop(guard);
        ws
    }
}

impl RuntimeAdapter for AppleContainerAdapter {
    fn name(&self) -> &'static str {
        "apple-container"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Per-container microVM isolation is the headline Apple
            // Containerization story; reflect it directly.
            hardening: Hardening::MicroVm,
            // The host user runs `container` as themselves; there is no
            // privileged daemon in front.
            rootless: true,
            network_isolation: NetworkIsolation::MicroVm,
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
            .map_err(|_| anyhow!("apple-container adapter workspace mutex poisoned"))?;
        guard.insert(id.clone(), spec.workspace.clone());
        drop(guard);
        Ok(id)
    }

    fn exec(&self, container: &ContainerId, argv: &[String], opts: ExecOpts) -> Result<ExecHandle> {
        let workspace = self.workspace_for(container)?;
        self.cli.exec(&workspace, argv, opts)
    }

    fn attach_pty(&self, container: &ContainerId, user_argv: &[String]) -> Result<PtyHandle> {
        // Per the plan's open questions: `container exec -it` is still
        // maturing in early Apple Containerization releases; the v1
        // macOS-26 acceptance smoke must exercise this path before we
        // declare PTY attach done. Behaviour-wise it mirrors the
        // podman/docker impls: shell `container exec -it <id> <argv>`
        // with the caller's stdio inherited.
        if user_argv.is_empty() {
            bail!("attach_pty argv must contain at least the program name");
        }
        let cli_args = attach_argv(container, user_argv);
        let exit_code = crate::process::run_interactive("container", &cli_args, &[], &[])?;
        Ok(PtyHandle {
            container: container.clone(),
            exit_code,
        })
    }

    fn stop(&self, container: &ContainerId) -> Result<()> {
        let result = self.invoker.run(
            "container",
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
            .map_err(|_| anyhow!("apple-container adapter workspace mutex poisoned"))?;
        guard.remove(container);
        drop(guard);
        Ok(())
    }

    fn inspect(&self, container: &ContainerId) -> Result<ContainerState> {
        // Apple's `container` CLI uses `inspect <id>` returning JSON, not
        // the docker `--format` template syntax. Ask for JSON and look for
        // a `status` field.
        let stdout = match self.invoker.run(
            "container",
            vec!["inspect".to_string(), container.as_str().to_string()],
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
        Ok(parse_inspect_json(&stdout))
    }
}

/// Build the argv for `container exec -it <id> <cmd…>`. Pure so the
/// shape can be locked in by tests without spawning the real CLI
/// (which doesn't exist on Linux at all).
fn attach_argv(container: &ContainerId, user_argv: &[String]) -> Vec<String> {
    let mut cli_args = Vec::with_capacity(3 + user_argv.len());
    cli_args.push("exec".to_string());
    cli_args.push("-it".to_string());
    cli_args.push(container.as_str().to_string());
    cli_args.extend(user_argv.iter().cloned());
    cli_args
}

/// Same `<repo>/.devcontainer/...` mapping the other adapters use.
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

/// Apple's `container` CLI reports missing containers as `Error: container
/// not found` or, in newer pre-releases, `no such container`. Accept both.
fn is_missing_container_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("container not found") || lower.contains("no such container")
}

/// Parse `container inspect <id>` JSON output. The CLI returns either a
/// single object or a one-element array depending on version; accept both.
/// The status field is what we care about; nested `state.exitCode` carries
/// the exit when the status is `exited`.
fn parse_inspect_json(stdout: &str) -> ContainerState {
    use serde_json::Value;

    let value: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    let object = value.as_ref().and_then(|v| match v {
        Value::Array(a) => a.first(),
        Value::Object(_) => Some(v),
        _ => None,
    });

    let status = object
        .and_then(|v| {
            v.get("status")
                .or_else(|| v.get("Status"))
                .or_else(|| v.pointer("/state/status"))
                .or_else(|| v.pointer("/State/Status"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    let exit_code = object
        .and_then(|v| {
            v.pointer("/state/exitCode")
                .or_else(|| v.pointer("/State/ExitCode"))
        })
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
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
            r#"{ "image": "ghcr.io/example/img:1" }"#,
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
    fn name_is_apple_container() {
        let a = AppleContainerAdapter::new(Arc::new(MockProcessInvoker::new()));
        assert_eq!(a.name(), "apple-container");
    }

    #[test]
    fn capabilities_report_microvm_isolation() {
        let a = AppleContainerAdapter::new(Arc::new(MockProcessInvoker::new()));
        let c = a.capabilities();
        assert_eq!(c.hardening, Hardening::MicroVm);
        assert!(c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::MicroVm);
        assert!(c.can_build_images);
        assert!(c.supports_pty);
    }

    #[test]
    fn ensure_image_invokes_devcontainer_build_with_container_engine() {
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
            "container".to_string(),
        ];
        let stdout = format!(r#"{{"outcome":"success","imageName":"{minted}"}}"#);
        let invoker = invoker_with(vec![("devcontainer", build_args, stdout)]);
        let a = AppleContainerAdapter::new(invoker);
        let id = a.ensure_image(&dc).unwrap();
        assert_eq!(id.as_str(), minted);
    }

    #[test]
    fn start_container_passes_no_extra_run_args() {
        // microVM isolation is OS-provided; we must not inject hardening
        // flags. Test asserts on the exact argv to lock this in.
        let up_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "container".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c-apple-1"}"#.to_string();
        let invoker = invoker_with(vec![("devcontainer", up_args, stdout)]);
        let a = AppleContainerAdapter::new(invoker);
        let dc = sample_dc();
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        assert_eq!(id.as_str(), "c-apple-1");
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
            "container".to_string(),
            "--mount".to_string(),
            "type=bind,source=/repo/.fleet/sessions/s1/artifacts,target=/artifacts".to_string(),
        ];
        let exec_args = vec![
            "exec".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--docker-path".to_string(),
            "container".to_string(),
            "--".to_string(),
            "uname".to_string(),
            "-s".to_string(),
        ];
        let invoker = invoker_with(vec![
            (
                "devcontainer",
                up_args,
                r#"{"outcome":"success","containerId":"c-a"}"#.to_string(),
            ),
            ("devcontainer", exec_args, "Linux\n".to_string()),
        ]);
        let a = AppleContainerAdapter::new(invoker);
        let spec = sample_spec(ImageId::new(DevcontainerCli::mint_image_name(&dc)));
        let id = a.start_container(&spec).unwrap();
        let h = a
            .exec(
                &id,
                &["uname".to_string(), "-s".to_string()],
                ExecOpts::default(),
            )
            .unwrap();
        assert_eq!(h.exit_code, 0);
        assert!(h.stdout.contains("Linux"));
    }

    #[test]
    fn attach_pty_rejects_empty_argv() {
        let a = AppleContainerAdapter::new(Arc::new(MockProcessInvoker::new()));
        let err = a.attach_pty(&ContainerId::new("c"), &[]).unwrap_err();
        assert!(format!("{err}").contains("at least the program name"));
    }

    #[test]
    fn attach_argv_wraps_exec_dash_it_with_container_id() {
        let argv = attach_argv(&ContainerId::new("c-mac"), &["bash".to_string()]);
        assert_eq!(argv, vec!["exec", "-it", "c-mac", "bash"]);
    }

    #[test]
    fn attach_argv_passes_through_multi_arg_commands() {
        let argv = attach_argv(
            &ContainerId::new("c-1"),
            &["sh".to_string(), "-c".to_string(), "uname -a".to_string()],
        );
        assert_eq!(argv, vec!["exec", "-it", "c-1", "sh", "-c", "uname -a"]);
    }

    #[test]
    fn stop_is_idempotent_for_unknown_container() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "`container stop` exited with 1: Error: container not found: nope"
            ))
        });
        let a = AppleContainerAdapter::new(Arc::new(mock));
        a.stop(&ContainerId::new("nope")).unwrap();
    }

    #[test]
    fn inspect_running_container_from_json_array() {
        // Apple's CLI wraps the result in a single-element array — verify
        // both common shapes are accepted.
        let stdout =
            r#"[{"id":"c-run","status":"running","state":{"status":"running","exitCode":0}}]"#;
        let invoker = invoker_with(vec![(
            "container",
            vec!["inspect".to_string(), "c-run".to_string()],
            stdout.to_string(),
        )]);
        let a = AppleContainerAdapter::new(invoker);
        assert_eq!(
            a.inspect(&ContainerId::new("c-run")).unwrap(),
            ContainerState::Running
        );
    }

    #[test]
    fn inspect_exited_container_carries_exit_code() {
        let stdout = r#"{"status":"exited","state":{"status":"exited","exitCode":137}}"#;
        let invoker = invoker_with(vec![(
            "container",
            vec!["inspect".to_string(), "c-done".to_string()],
            stdout.to_string(),
        )]);
        let a = AppleContainerAdapter::new(invoker);
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
                "`container inspect` exited with 1: Error: container not found: ghost"
            ))
        });
        let a = AppleContainerAdapter::new(Arc::new(mock));
        match a.inspect(&ContainerId::new("ghost")).unwrap() {
            ContainerState::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parse_inspect_json_handles_unknown_state_verbatim() {
        match parse_inspect_json(r#"{"status":"frozen"}"#) {
            ContainerState::Unknown(s) => assert_eq!(s, "frozen"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn adapter_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AppleContainerAdapter>();
    }
}
