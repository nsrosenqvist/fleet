//! Adapter capability descriptors.
//!
//! A [`Capabilities`] value is what an adapter reports about *itself on this
//! host* — not a static property of the backend type. Whether gVisor is
//! available, whether the engine can run rootless, what kind of network
//! boundary the containers actually have: all of these depend on host
//! configuration and must be re-probed.
//!
//! This module owns the shape of those answers (a value object) so that the
//! TUI can render a single `fleet doctor` view without caring whether the
//! information came from `podman info --format json` or Apple's `container
//! system status`.

/// Kernel-isolation flavour an adapter provides per-container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hardening {
    /// No extra hardening beyond what the engine itself provides.
    #[default]
    None,
    /// User-space syscall layer (Linux only, via `runsc`).
    GVisor,
    /// Per-container microVM (Apple Containerization; Firecracker in future).
    MicroVm,
}

/// Network-boundary flavour. Drives how fleet's egress proxy plugs in:
/// `Namespaces` containers get a sidecar proxy in the same network namespace,
/// `MicroVm` containers get a host-side proxy bound to the VM's interface,
/// `None` means the adapter cannot constrain egress (Local).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkIsolation {
    #[default]
    None,
    Namespaces,
    MicroVm,
}

/// What an adapter promises on this host. Snapshot, not contract: adapters
/// re-evaluate on each session so a missing `runsc` after restart shows up
/// as `Hardening::None` immediately.
///
/// `Default` produces the safest-to-assume answer (no isolation guarantees);
/// callers should treat `Default::default()` as "nothing promised."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub hardening: Hardening,
    pub rootless: bool,
    pub network_isolation: NetworkIsolation,
    /// Whether the adapter can build OCI images from a Dockerfile/devcontainer.
    /// Some adapters (`Local`) only execute; image build is a no-op for them.
    pub can_build_images: bool,
    /// Whether the adapter offers attachable PTYs. Today: every adapter
    /// except `Local`; modelled explicitly so callers can fail fast instead
    /// of catching a runtime "not implemented" error.
    pub supports_pty: bool,
}

impl Capabilities {
    /// Maximum-safety preset: microVM, rootless, microvm-network, builds + PTY.
    /// Used as a target by `fleet doctor` to highlight where adapters fall
    /// short.
    pub const fn ideal() -> Self {
        Self {
            hardening: Hardening::MicroVm,
            rootless: true,
            network_isolation: NetworkIsolation::MicroVm,
            can_build_images: true,
            supports_pty: true,
        }
    }

    /// Human-readable description suitable for the TUI's doctor view.
    /// Stable wording — UI tests assert on these strings.
    pub fn describe(self) -> String {
        let hardening = match self.hardening {
            Hardening::None => "no extra hardening",
            Hardening::GVisor => "gVisor (runsc)",
            Hardening::MicroVm => "microVM-per-container",
        };
        let rootless = if self.rootless { "rootless" } else { "rootful" };
        let net = match self.network_isolation {
            NetworkIsolation::None => "shared host network",
            NetworkIsolation::Namespaces => "namespaced network",
            NetworkIsolation::MicroVm => "microVM network",
        };
        let build = if self.can_build_images {
            "builds images"
        } else {
            "exec-only"
        };
        let pty = if self.supports_pty { "PTY" } else { "no PTY" };
        format!("{hardening}, {rootless}, {net}, {build}, {pty}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_promises_nothing() {
        let c = Capabilities::default();
        assert_eq!(c.hardening, Hardening::None);
        assert!(!c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::None);
        assert!(!c.can_build_images);
        assert!(!c.supports_pty);
    }

    #[test]
    fn ideal_is_strongest_combination() {
        let c = Capabilities::ideal();
        assert_eq!(c.hardening, Hardening::MicroVm);
        assert!(c.rootless);
        assert_eq!(c.network_isolation, NetworkIsolation::MicroVm);
        assert!(c.can_build_images);
        assert!(c.supports_pty);
    }

    #[test]
    fn describe_default_reads_as_unhardened() {
        assert_eq!(
            Capabilities::default().describe(),
            "no extra hardening, rootful, shared host network, exec-only, no PTY"
        );
    }

    #[test]
    fn describe_ideal_reads_as_strongest() {
        assert_eq!(
            Capabilities::ideal().describe(),
            "microVM-per-container, rootless, microVM network, builds images, PTY"
        );
    }

    #[test]
    fn describe_renders_gvisor_with_namespaces() {
        let c = Capabilities {
            hardening: Hardening::GVisor,
            rootless: true,
            network_isolation: NetworkIsolation::Namespaces,
            can_build_images: true,
            supports_pty: true,
        };
        assert_eq!(
            c.describe(),
            "gVisor (runsc), rootless, namespaced network, builds images, PTY"
        );
    }

    #[test]
    fn capabilities_are_copy_value_objects() {
        // Compile-time check: confirms `Copy` impl survives future edits.
        const _: fn() = || {
            fn assert_copy<T: Copy>() {}
            assert_copy::<Capabilities>();
            assert_copy::<Hardening>();
            assert_copy::<NetworkIsolation>();
        };
    }
}
