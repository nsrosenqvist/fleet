//! Runtime adapters: the boundary between fleet and whatever spawns containers
//! to host agent workloads. The trait below is what the session manager and
//! workflow engine depend on; concrete impls (`local`, future `podman`,
//! `docker`, `apple_container`) live in sibling modules and are interchangeable
//! per the Liskov substitution principle.
//!
//! Module responsibilities (one each, per single-responsibility):
//! - `capabilities` — `Capabilities` value object describing what an adapter
//!   can promise (hardening, rootless, network isolation kind).
//! - `devcontainer` — parsing the subset of `devcontainer.json` fleet itself
//!   needs to reason about. The bulk of the spec (Features, lifecycle hooks,
//!   env merging) is delegated to the underlying devcontainer CLI; we only
//!   model fields fleet directly consumes.
//! - `detect` — host probing: which adapters are available, which is best.
//! - `local` — no-isolation adapter; runs commands on the host with the
//!   workspace as cwd. Useful for fleet-on-fleet development and as a
//!   reference implementation for the trait shape.
//!
//! Container/image IDs are newtypes rather than bare `String`s so that the
//! compiler can stop us from mixing them up at call sites.

pub mod apple_container;
pub mod capabilities;
pub mod detect;
pub mod devcontainer;
pub mod devcontainer_cli;
pub mod docker;
pub mod factory;
pub mod local;
pub mod podman;

pub use capabilities::{Capabilities, Hardening, NetworkIsolation};
pub use devcontainer::Devcontainer;

use anyhow::Result;
use std::path::PathBuf;

/// Stable identifier for a built (or pulled) container image. Adapters mint
/// these; callers treat them as opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageId(String);

impl ImageId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw id. Paired with the [`Display`] impl; no v2
    /// caller uses it directly today, but it's part of the opaque
    /// newtype's public surface.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ImageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identifier for a running (or recently-running) container. Same
/// opaque-newtype contract as [`ImageId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerId(String);

impl ContainerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the caller wants from a freshly-started container. A value object;
/// adapters consume but never mutate it. `image` + `command` are part of
/// the protocol surface — engine adapters route around them today (the
/// devcontainer CLI rebuilds the image itself; the engine's entrypoint
/// covers `command`), but they're kept for use by future no-devcontainer
/// adapters.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ContainerSpec {
    pub image: ImageId,
    /// Bind-mounted into the container at the devcontainer's
    /// `workspaceFolder` (or `/workspace` if unspecified).
    pub workspace: PathBuf,
    /// Bind-mounted at `/artifacts`. Workflow hand-off lives here.
    pub artifacts: PathBuf,
    /// Env injected at start. Secrets land here briefly; the adapter is
    /// responsible for not persisting them (no command-line leakage, no
    /// image-layer baking).
    pub env: Vec<(String, String)>,
    /// Override the image's default command. `None` = use the image entrypoint.
    pub command: Option<Vec<String>>,
    /// Engine-level network to attach the container to. `None` lets the
    /// engine use its default bridge; `Some(name)` pins the container
    /// to that network — used by the egress enforcer to route the
    /// workflow container through a sidecar proxy. Adapters without
    /// network-pinning support (Local, today's Docker) ignore this.
    pub network: Option<String>,
    /// Engine-level DNS server override (`--dns=<ip>`). `None` lets the
    /// engine use its default resolver; `Some(ip)` pins the container's
    /// resolv.conf to that single nameserver — used by the egress
    /// enforcer's DNS-stub sidecar to refuse any lookup that isn't a
    /// pre-resolved allowlist host. Adapters without DNS-pinning
    /// support ignore this.
    pub dns: Option<String>,
    /// Additional bind mounts beyond `workspace` and `artifacts`. Used
    /// by the bridge plumbing to drop the `fleet-tracker` binary into
    /// the container at `/usr/local/bin/fleet-tracker`. Each entry is
    /// rendered into the engine's `--mount type=bind,...` syntax by
    /// adapters that go through the devcontainer CLI; adapters without
    /// container isolation (Local) ignore the field.
    pub extra_mounts: Vec<MountSpec>,
}

/// A single host-to-container bind mount. Read-only is opt-in because
/// it's the safe default for trust-boundary mounts (a poisoned agent
/// shouldn't be able to rewrite `fleet-tracker` underneath itself)
/// but workflows occasionally need read-write side-channels.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct MountSpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

impl MountSpec {
    /// Render as a single `--mount` argument for the devcontainer CLI's
    /// `--mount` flag (which the CLI forwards verbatim to `docker run`
    /// / `podman run`). Path values are embedded literally; callers are
    /// expected to feed in absolute paths the engine can resolve.
    #[must_use]
    pub fn to_mount_arg(&self) -> String {
        let mut out = format!(
            "type=bind,source={},target={}",
            self.host_path.display(),
            self.container_path.display()
        );
        if self.read_only {
            out.push_str(",readonly");
        }
        out
    }
}

/// Per-exec overrides. Empty defaults mean "inherit from the container."
#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    /// Working directory inside the container.
    pub workdir: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Result of a non-interactive `exec`. PTY/streaming variants will be a
/// separate handle type; keeping them apart avoids forcing every caller to
/// reason about streaming when a one-shot `String` is enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecHandle {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Result of an interactive PTY attach. Today's adapters spawn the engine's
/// `exec -it` command with the user's terminal inherited as the PTY, block
/// until the inner process exits, and return the exit code here. A future
/// chunk that grows TUI-side attach will add a master fd field for
/// multiplexing; the current shape is the CLI-only contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyHandle {
    pub container: ContainerId,
    pub exit_code: i32,
}

/// Lifecycle state of a container as reported by the adapter. `Unknown`
/// carries a backend-specific string for surfacing in diagnostics without
/// forcing every adapter to model exotic states (e.g. "Restarting", "Paused").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Exited { code: i32 },
    Dead,
    Unknown(String),
}

/// The runtime port of fleet's hexagonal architecture. Callers depend on this
/// trait; concrete adapters depend on host tooling (`podman`, `docker`,
/// `container`, or nothing for `Local`).
///
/// Trait methods take `&self`; adapters that need mutable bookkeeping use
/// interior mutability (typically `Mutex<HashMap<…>>`) so the trait stays
/// `Send + Sync` for use from the TUI's async refresh thread.
pub trait RuntimeAdapter: Send + Sync {
    /// Short backend name for diagnostics and config (`"podman"`,
    /// `"docker"`, `"apple-container"`, `"local"`).
    fn name(&self) -> &'static str;

    /// What this adapter can promise on the host it's running on.
    fn capabilities(&self) -> Capabilities;

    /// Build or pull the image declared in the devcontainer. Idempotent:
    /// repeat invocations with the same devcontainer return the same
    /// [`ImageId`] without rebuilding.
    fn ensure_image(&self, devcontainer: &Devcontainer) -> Result<ImageId>;

    /// Start a container from a built image; return its id. The container
    /// is running when this returns (or the call errors).
    fn start_container(&self, spec: &ContainerSpec) -> Result<ContainerId>;

    /// One-shot exec inside a running container. Stdout/stderr are captured
    /// in full; interactive use cases go through [`Self::attach_pty`].
    fn exec(&self, container: &ContainerId, argv: &[String], opts: ExecOpts) -> Result<ExecHandle>;

    /// Attach a PTY for an interactive session. The returned handle is
    /// owned by the caller; the adapter does not retain it.
    fn attach_pty(&self, container: &ContainerId, argv: &[String]) -> Result<PtyHandle>;

    /// Stop and remove the container. Idempotent — stopping an already-stopped
    /// container is not an error.
    fn stop(&self, container: &ContainerId) -> Result<()>;

    /// Lifecycle snapshot. Returns [`ContainerState::Unknown`] (not an error)
    /// when the container has never existed under this adapter.
    fn inspect(&self, container: &ContainerId) -> Result<ContainerState>;

    /// Convenience predicate for liveness probing. Default impl derives
    /// from [`Self::inspect`] so adapters that already model state
    /// accurately get the right answer for free; adapters that can
    /// implement a cheaper probe (e.g. `podman ps -q --filter id=…`)
    /// may override.
    ///
    /// Anything other than [`ContainerState::Running`] is treated as
    /// "not alive": `Exited`, `Dead`, and `Unknown` all indicate the
    /// container cannot serve the workload. The session reaper's
    /// primary crash signal is the driver pid, not container state —
    /// fleet sessions tear down their containers per node, so by the
    /// time a session is "stuck", its containers are usually already
    /// gone. Used by [`AdapterStopper`] to skip the engine-level
    /// `stop` call for containers that have already exited (saves an
    /// engine round-trip when many leaks are stale).
    fn is_running(&self, container: &ContainerId) -> Result<bool> {
        Ok(matches!(self.inspect(container)?, ContainerState::Running))
    }
}

/// Bridge between [`RuntimeAdapter`] and [`crate::session::reaper::ContainerStopper`].
/// Lives here (rather than in `session::reaper`) so the session module
/// doesn't have to depend on runtime details.
///
/// Owns the boxed adapter so callers can hand off a one-shot stopper
/// without juggling separate lifetimes: the typical use is to build
/// an adapter, wrap it in a stopper, pass it to `reaper::reap`, and
/// drop everything at the end of the sweep.
///
/// The stopper short-circuits when the engine already considers the
/// container gone — saves an engine round-trip across a list of stale
/// leaks, and lets `Inspect` errors fall through to the actual stop
/// call so transient probe failures don't masquerade as success. Stop
/// errors from the adapter pass through verbatim so the caller can
/// list them as failed-to-stop entries.
pub struct AdapterStopper {
    adapter: Box<dyn RuntimeAdapter>,
}

impl AdapterStopper {
    #[must_use]
    pub fn new(adapter: Box<dyn RuntimeAdapter>) -> Self {
        Self { adapter }
    }
}

impl crate::session::reaper::ContainerStopper for AdapterStopper {
    fn stop(&self, container_id: &str) -> Result<()> {
        let id = ContainerId::new(container_id);
        match self.adapter.is_running(&id) {
            // Already gone — nothing to stop.
            Ok(false) => Ok(()),
            // Running, or we can't tell. Either way attempt the stop;
            // `stop` is contract-idempotent so it's safe to call on a
            // probably-dead container.
            _ => self.adapter.stop(&id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn image_id_round_trips_through_string() {
        let id = ImageId::new("sha256:abc");
        assert_eq!(id.as_str(), "sha256:abc");
        assert_eq!(format!("{id}"), "sha256:abc");
    }

    #[test]
    fn container_id_round_trips_through_string() {
        let id = ContainerId::new("c-42");
        assert_eq!(id.as_str(), "c-42");
        assert_eq!(format!("{id}"), "c-42");
    }

    #[test]
    fn ids_compare_by_value() {
        assert_eq!(ImageId::new("a"), ImageId::new("a"));
        assert_ne!(ImageId::new("a"), ImageId::new("b"));
        assert_eq!(ContainerId::new("c"), ContainerId::new("c"));
        assert_ne!(ContainerId::new("c"), ContainerId::new("d"));
    }

    #[test]
    fn mount_spec_renders_read_only_flag_when_set() {
        let m = MountSpec {
            host_path: PathBuf::from("/host/bin/fleet-tracker"),
            container_path: PathBuf::from("/usr/local/bin/fleet-tracker"),
            read_only: true,
        };
        assert_eq!(
            m.to_mount_arg(),
            "type=bind,source=/host/bin/fleet-tracker,target=/usr/local/bin/fleet-tracker,readonly"
        );
    }

    #[test]
    fn mount_spec_omits_read_only_flag_when_unset() {
        let m = MountSpec {
            host_path: PathBuf::from("/h"),
            container_path: PathBuf::from("/c"),
            read_only: false,
        };
        assert_eq!(m.to_mount_arg(), "type=bind,source=/h,target=/c");
    }

    #[test]
    fn ids_are_hashable_for_use_as_map_keys() {
        use std::collections::HashMap;
        let mut m: HashMap<ContainerId, i32> = HashMap::new();
        m.insert(ContainerId::new("x"), 1);
        assert_eq!(m.get(&ContainerId::new("x")), Some(&1));
        assert_eq!(m.get(&ContainerId::new("y")), None);
    }

    /// Adapter stub for testing the default [`RuntimeAdapter::is_running`]
    /// mapping. Returns whatever [`ContainerState`] the test queues up;
    /// every other trait method is `unimplemented!` because the default
    /// impl only consults `inspect`.
    struct FakeInspectAdapter {
        state: Mutex<ContainerState>,
    }

    impl FakeInspectAdapter {
        fn new(state: ContainerState) -> Self {
            Self {
                state: Mutex::new(state),
            }
        }
    }

    impl RuntimeAdapter for FakeInspectAdapter {
        fn name(&self) -> &'static str {
            "fake-inspect"
        }
        fn capabilities(&self) -> Capabilities {
            unimplemented!("not exercised by is_running tests")
        }
        fn ensure_image(&self, _devcontainer: &Devcontainer) -> Result<ImageId> {
            unimplemented!("not exercised by is_running tests")
        }
        fn start_container(&self, _spec: &ContainerSpec) -> Result<ContainerId> {
            unimplemented!("not exercised by is_running tests")
        }
        fn exec(
            &self,
            _container: &ContainerId,
            _argv: &[String],
            _opts: ExecOpts,
        ) -> Result<ExecHandle> {
            unimplemented!("not exercised by is_running tests")
        }
        fn attach_pty(&self, _container: &ContainerId, _argv: &[String]) -> Result<PtyHandle> {
            unimplemented!("not exercised by is_running tests")
        }
        fn stop(&self, _container: &ContainerId) -> Result<()> {
            unimplemented!("not exercised by is_running tests")
        }
        fn inspect(&self, _container: &ContainerId) -> Result<ContainerState> {
            Ok(self.state.lock().unwrap().clone())
        }
    }

    #[test]
    fn is_running_true_only_for_running_state() {
        let cid = ContainerId::new("c");
        assert!(
            FakeInspectAdapter::new(ContainerState::Running)
                .is_running(&cid)
                .unwrap()
        );
    }

    #[test]
    fn is_running_false_for_exited() {
        let cid = ContainerId::new("c");
        assert!(
            !FakeInspectAdapter::new(ContainerState::Exited { code: 0 })
                .is_running(&cid)
                .unwrap()
        );
        assert!(
            !FakeInspectAdapter::new(ContainerState::Exited { code: 137 })
                .is_running(&cid)
                .unwrap()
        );
    }

    #[test]
    fn is_running_false_for_dead() {
        let cid = ContainerId::new("c");
        assert!(
            !FakeInspectAdapter::new(ContainerState::Dead)
                .is_running(&cid)
                .unwrap()
        );
    }

    #[test]
    fn is_running_false_for_unknown() {
        let cid = ContainerId::new("c");
        assert!(
            !FakeInspectAdapter::new(ContainerState::Unknown("not tracked by engine".to_string()))
                .is_running(&cid)
                .unwrap()
        );
    }

    #[test]
    fn is_running_false_for_created() {
        // Created means the container exists but has not yet started —
        // not "alive" for the reaper's purposes.
        let cid = ContainerId::new("c");
        assert!(
            !FakeInspectAdapter::new(ContainerState::Created)
                .is_running(&cid)
                .unwrap()
        );
    }
}
