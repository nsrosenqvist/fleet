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

pub mod capabilities;
pub mod detect;
pub mod devcontainer;
pub mod devcontainer_cli;
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
/// adapters consume but never mutate it.
#[derive(Debug, Clone)]
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

/// Marker for an in-progress interactive PTY attach. The real fields land
/// when adapter impls grow PTY support; for now this is an opaque handle the
/// trait method can return so call sites can compile against the final
/// signature.
#[derive(Debug)]
pub struct PtyHandle {
    pub container: ContainerId,
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
    fn exec(
        &self,
        container: &ContainerId,
        argv: &[String],
        opts: ExecOpts,
    ) -> Result<ExecHandle>;

    /// Attach a PTY for an interactive session. The returned handle is
    /// owned by the caller; the adapter does not retain it.
    fn attach_pty(&self, container: &ContainerId, argv: &[String]) -> Result<PtyHandle>;

    /// Stop and remove the container. Idempotent — stopping an already-stopped
    /// container is not an error.
    fn stop(&self, container: &ContainerId) -> Result<()>;

    /// Lifecycle snapshot. Returns [`ContainerState::Unknown`] (not an error)
    /// when the container has never existed under this adapter.
    fn inspect(&self, container: &ContainerId) -> Result<ContainerState>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ids_are_hashable_for_use_as_map_keys() {
        use std::collections::HashMap;
        let mut m: HashMap<ContainerId, i32> = HashMap::new();
        m.insert(ContainerId::new("x"), 1);
        assert_eq!(m.get(&ContainerId::new("x")), Some(&1));
        assert_eq!(m.get(&ContainerId::new("y")), None);
    }
}
