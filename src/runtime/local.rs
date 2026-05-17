//! `Local` runtime adapter: no isolation, runs commands directly on the host.
//!
//! This is the simplest concrete [`RuntimeAdapter`]. Useful as a reference for
//! the trait shape, for fleet-on-fleet development, and as a deterministic
//! test target (image/container IDs are minted from a monotonic counter, so
//! tests don't need access to a real container engine).
//!
//! It is **never** auto-selected — the plan calls for explicit opt-in. The
//! caller decides; this module just provides the implementation.
//!
//! State is held in an `Arc<Mutex<HashMap>>` so the adapter is `Send + Sync`
//! without making the trait methods take `&mut self`. The mutex is local to
//! the adapter; lock scopes are intentionally small (one `HashMap` operation
//! per scope) to keep contention non-existent in practice.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{
    Capabilities, ContainerId, ContainerSpec, ContainerState, Devcontainer, ExecHandle, ExecOpts,
    Hardening, ImageId, NetworkIsolation, PtyHandle, RuntimeAdapter,
};
use crate::process::ProcessInvoker;

/// Adapter that runs commands on the host with no isolation. Each "container"
/// is just a logical handle: the workspace path, the env, and a current state.
pub struct LocalAdapter {
    invoker: Arc<dyn ProcessInvoker>,
    state: Arc<Mutex<State>>,
}

/// Per-adapter mutable state. Held behind a `Mutex` so [`LocalAdapter`] can
/// claim `Send + Sync` and live in the TUI's thread-shared `App`.
#[derive(Default)]
struct State {
    next_id: u64,
    containers: HashMap<ContainerId, LocalContainer>,
}

#[derive(Debug, Clone)]
struct LocalContainer {
    workspace: PathBuf,
    state: ContainerState,
    /// Env from the `ContainerSpec`; prepended to every `exec` call so
    /// downstream code that respects `HTTP_PROXY` / `HTTPS_PROXY` sees
    /// the same env it would inside a real container. Local doesn't
    /// have a notion of "container env"; this is the closest analogue.
    env: Vec<(String, String)>,
}

impl LocalAdapter {
    pub fn new(invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self {
            invoker,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn mint_container_id(state: &mut State) -> ContainerId {
        state.next_id += 1;
        ContainerId::new(format!("local-{}", state.next_id))
    }

    fn with_container<R>(
        &self,
        id: &ContainerId,
        f: impl FnOnce(&LocalContainer) -> R,
    ) -> Result<R> {
        let guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("local adapter state mutex poisoned"))?;
        let result = guard
            .containers
            .get(id)
            .map(f)
            .ok_or_else(|| anyhow!("no such container: {id}"));
        drop(guard);
        result
    }
}

impl RuntimeAdapter for LocalAdapter {
    fn name(&self) -> &'static str {
        "local"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardening: Hardening::None,
            // The host *user* runs the commands; there's no privileged
            // engine in between. Calling this "rootless" matches the
            // doctor view's other adapters.
            rootless: true,
            network_isolation: NetworkIsolation::None,
            // Local cannot build OCI images; the workflow node's `image`
            // field is purely informational under this adapter.
            can_build_images: false,
            // Pure-stdio exec only. PTY attach is a future addition;
            // callers should check `supports_pty` before requesting it.
            supports_pty: false,
        }
    }

    /// No image to build — return a stable synthetic id derived from the
    /// devcontainer's source path so repeated calls on the same input
    /// return the same id (the trait's idempotency contract).
    fn ensure_image(&self, devcontainer: &Devcontainer) -> Result<ImageId> {
        Ok(ImageId::new(format!(
            "local:{}",
            devcontainer.source_path.display()
        )))
    }

    fn start_container(&self, spec: &ContainerSpec) -> Result<ContainerId> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("local adapter state mutex poisoned"))?;
        let id = Self::mint_container_id(&mut guard);
        guard.containers.insert(
            id.clone(),
            LocalContainer {
                workspace: spec.workspace.clone(),
                state: ContainerState::Running,
                env: spec.env.clone(),
            },
        );
        drop(guard);
        Ok(id)
    }

    fn exec(
        &self,
        container: &ContainerId,
        argv: &[String],
        _opts: ExecOpts,
    ) -> Result<ExecHandle> {
        if argv.is_empty() {
            bail!("exec argv must contain at least the program name");
        }
        // Snapshot the workspace path + env under the lock, then
        // release before running the (potentially long) subprocess.
        let (workspace, env) = self.with_container(container, |c| {
            if c.state == ContainerState::Running {
                Ok((c.workspace.clone(), c.env.clone()))
            } else {
                Err(anyhow!("container {container} is not running"))
            }
        })??;

        // `Local` has no notion of "inside the container" — the workspace
        // path is just the cwd we'd cd to before running. The current
        // `ProcessInvoker` trait doesn't expose a cwd parameter; for now
        // we shell through `sh -c "cd <ws> && KEY=VAL ... <argv...>"`,
        // exporting any container-level env via the standard shell
        // env-prefix syntax so callers that depend on HTTP_PROXY etc.
        // see them. When the trait grows a structured runner, this
        // collapses naturally.
        let program = &argv[0];
        let rest = argv[1..].join(" ");
        let env_prefix = if env.is_empty() {
            String::new()
        } else {
            let mut parts: Vec<String> = env
                .iter()
                .map(|(k, v)| format!("{k}={}", shell_escape(std::path::Path::new(v))))
                .collect();
            parts.push(String::new());
            parts.join(" ")
        };
        let script = format!(
            "cd {} && {env_prefix}{program} {rest}",
            shell_escape(&workspace)
        );

        match self.invoker.run("sh", vec!["-c".to_string(), script]) {
            Ok(stdout) => Ok(ExecHandle {
                stdout,
                stderr: String::new(),
                exit_code: 0,
            }),
            Err(err) => Ok(ExecHandle {
                stdout: String::new(),
                stderr: format!("{err:#}"),
                exit_code: 1,
            }),
        }
    }

    fn attach_pty(
        &self,
        container: &ContainerId,
        argv: &[String],
        opts: ExecOpts,
    ) -> Result<PtyHandle> {
        // "PTY attach" for Local has no container to enter — we run
        // the agent directly with the caller's stdio inherited so it
        // can render its TUI in the current terminal (the tmux pane,
        // when invoked under `fleet workflow run --detached`).
        // Differs from `exec` only in stdio handling: exec captures
        // output; attach_pty streams it. The per-call `opts.env`
        // overlays the container-level env so callers (e.g. the
        // workflow executor) can pass freshly-resolved secrets at
        // attach time without baking them into the container record.
        if argv.is_empty() {
            bail!("attach_pty argv must contain at least the program name");
        }
        let (workspace, container_env) = self.with_container(container, |c| {
            if c.state == ContainerState::Running {
                Ok((c.workspace.clone(), c.env.clone()))
            } else {
                Err(anyhow!("container {container} is not running"))
            }
        })??;
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&workspace)
            .envs(container_env)
            .envs(opts.env)
            .status()
            .with_context(|| format!("running `{}` with inherited stdio", argv[0]))?;
        Ok(PtyHandle {
            container: container.clone(),
            exit_code: status.code().unwrap_or(-1),
        })
    }

    fn stop(&self, container: &ContainerId) -> Result<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("local adapter state mutex poisoned"))?;
        if let Some(c) = guard.containers.get_mut(container) {
            c.state = ContainerState::Exited { code: 0 };
        }
        drop(guard);
        // Idempotent: stopping an unknown container is not an error.
        Ok(())
    }

    fn inspect(&self, container: &ContainerId) -> Result<ContainerState> {
        let guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("local adapter state mutex poisoned"))?;
        let state = guard.containers.get(container).map_or_else(
            || ContainerState::Unknown("not tracked by adapter".to_string()),
            |c| c.state.clone(),
        );
        drop(guard);
        Ok(state)
    }
}

/// Minimal POSIX-shell escaping for paths we splice into `sh -c`. Wraps in
/// single quotes and escapes any embedded single quote — sufficient for
/// arbitrary filenames.
fn shell_escape(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if s.is_empty() {
        return "''".to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::{always, eq};

    fn invoker_returning(stdout: &'static str) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |_, _| Ok(stdout.to_string()));
        Arc::new(mock)
    }

    fn invoker_strict_eq(
        prog: &'static str,
        args: Vec<String>,
        stdout: &'static str,
    ) -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq(prog), eq(args))
            .returning(move |_, _| Ok(stdout.to_string()));
        Arc::new(mock)
    }

    fn invoker_always_errors() -> Arc<dyn ProcessInvoker> {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Err(anyhow!("nope")));
        Arc::new(mock)
    }

    fn sample_devcontainer() -> Devcontainer {
        Devcontainer::from_str_at(
            r#"{ "image": "x" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap()
    }

    fn sample_spec(image: ImageId) -> ContainerSpec {
        ContainerSpec {
            image,
            workspace: PathBuf::from("/tmp/ws"),
            artifacts: PathBuf::from("/tmp/art"),
            env: vec![],
            command: None,
            network: None,
            dns: None,
            extra_mounts: vec![],
        }
    }

    #[test]
    fn capabilities_promise_nothing() {
        let a = LocalAdapter::new(invoker_returning(""));
        let c = a.capabilities();
        assert_eq!(c.hardening, Hardening::None);
        assert!(c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::None);
        assert!(!c.can_build_images);
        assert!(!c.supports_pty);
    }

    #[test]
    fn name_is_local() {
        let a = LocalAdapter::new(invoker_returning(""));
        assert_eq!(a.name(), "local");
    }

    #[test]
    fn ensure_image_is_idempotent_for_same_devcontainer() {
        let a = LocalAdapter::new(invoker_returning(""));
        let dc = sample_devcontainer();
        let id1 = a.ensure_image(&dc).unwrap();
        let id2 = a.ensure_image(&dc).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn ensure_image_differs_per_source_path() {
        let a = LocalAdapter::new(invoker_returning(""));
        let dc1 = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/a/devcontainer.json").unwrap();
        let dc2 = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/b/devcontainer.json").unwrap();
        assert_ne!(a.ensure_image(&dc1).unwrap(), a.ensure_image(&dc2).unwrap());
    }

    #[test]
    fn start_container_mints_distinct_ids() {
        let a = LocalAdapter::new(invoker_returning(""));
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let c1 = a.start_container(&sample_spec(img.clone())).unwrap();
        let c2 = a.start_container(&sample_spec(img)).unwrap();
        assert_ne!(c1, c2);
    }

    #[test]
    fn freshly_started_container_inspects_as_running() {
        let a = LocalAdapter::new(invoker_returning(""));
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        assert_eq!(a.inspect(&id).unwrap(), ContainerState::Running);
    }

    #[test]
    fn stop_transitions_to_exited() {
        let a = LocalAdapter::new(invoker_returning(""));
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        a.stop(&id).unwrap();
        assert_eq!(a.inspect(&id).unwrap(), ContainerState::Exited { code: 0 });
    }

    #[test]
    fn stop_is_idempotent_for_unknown_container() {
        let a = LocalAdapter::new(invoker_returning(""));
        // Never created; stop must not error.
        a.stop(&ContainerId::new("nope")).unwrap();
    }

    #[test]
    fn inspect_unknown_container_returns_unknown_state() {
        let a = LocalAdapter::new(invoker_returning(""));
        match a.inspect(&ContainerId::new("nope")).unwrap() {
            ContainerState::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn exec_requires_nonempty_argv() {
        let a = LocalAdapter::new(invoker_returning(""));
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        let err = a.exec(&id, &[], ExecOpts::default()).unwrap_err();
        assert!(format!("{err}").contains("at least the program name"));
    }

    #[test]
    fn exec_rejects_unknown_container() {
        let a = LocalAdapter::new(invoker_returning(""));
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
    fn exec_rejects_stopped_container() {
        let a = LocalAdapter::new(invoker_returning(""));
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        a.stop(&id).unwrap();
        let err = a
            .exec(&id, &["true".to_string()], ExecOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("not running"));
    }

    #[test]
    fn exec_runs_program_with_workspace_as_cwd() {
        let invoker = invoker_strict_eq(
            "sh",
            vec!["-c".to_string(), "cd '/tmp/ws' && pwd ".to_string()],
            "/tmp/ws",
        );
        let a = LocalAdapter::new(invoker);
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        let h = a
            .exec(&id, &["pwd".to_string()], ExecOpts::default())
            .unwrap();
        assert_eq!(h.stdout, "/tmp/ws");
        assert_eq!(h.exit_code, 0);
    }

    #[test]
    fn exec_propagates_invoker_error_into_handle() {
        let a = LocalAdapter::new(invoker_always_errors());
        let img = a.ensure_image(&sample_devcontainer()).unwrap();
        let id = a.start_container(&sample_spec(img)).unwrap();
        let h = a
            .exec(&id, &["false".to_string()], ExecOpts::default())
            .unwrap();
        assert_eq!(h.exit_code, 1);
        assert!(h.stderr.contains("nope"));
    }

    #[test]
    fn attach_pty_rejects_empty_argv() {
        // The contract for both `local` and the container adapters
        // is the same: empty argv is a usage error.
        let a = LocalAdapter::new(invoker_returning(""));
        let err = a
            .attach_pty(&ContainerId::new("any"), &[], ExecOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("attach_pty argv"));
    }

    #[test]
    fn attach_pty_rejects_unknown_container() {
        // Mirrors `exec` — addressing a container we never started
        // is a clear caller bug, not a fall-through.
        let a = LocalAdapter::new(invoker_returning(""));
        let err = a
            .attach_pty(
                &ContainerId::new("ghost"),
                &["bash".to_string()],
                ExecOpts::default(),
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not found") || msg.contains("ghost"),
            "got: {msg}"
        );
    }

    #[test]
    fn adapter_is_send_sync() {
        // Compile-time check.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalAdapter>();
    }

    #[test]
    fn shell_escape_handles_single_quotes() {
        let p = std::path::Path::new("/tmp/o'reilly");
        assert_eq!(shell_escape(p), "'/tmp/o'\\''reilly'");
    }

    #[test]
    fn shell_escape_wraps_plain_path() {
        let p = std::path::Path::new("/tmp/ws");
        assert_eq!(shell_escape(p), "'/tmp/ws'");
    }
}
