//! Startup checks that gate the main event loop.
//!
//! Fleet has no degraded mode — every host-side operation funnels through
//! `limactl shell <vm> …`, so running the TUI when prerequisites are wrong
//! just produces noisy mid-session failures. The preflight runs three
//! checks, each at most one cheap subprocess:
//!
//! 1. Is `limactl` on `$PATH`?
//! 2. Does the `fleet-vm` Lima instance exist?
//! 3. Is it running?
//!
//! Each negative answer maps to one [`Preflight`] variant the terminal
//! layer drives: the missing-host-binary case is a dead-end modal (quit
//! only); the VM cases are actionable modals that suspend the TUI and
//! invoke `limactl start fleet-vm` to remediate, then loop back through
//! preflight.
//!
//! Tmux is intentionally not probed here. Every tmux call in fleet runs
//! through `limactl shell <vm> … tmux …`, so tmux's availability is a
//! property of the Lima guest image, not the host.

use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::RealProcessInvoker;

/// The Lima instance fleet expects. Mirrors `cli::vm::VM_NAME`.
pub(super) const VM_NAME: &str = "fleet-vm";

/// Outcome of [`check`]. The first failure short-circuits — checking VM
/// status when `limactl` itself is missing would just be a confusing
/// "command not found" error wrapped in a missing-VM message.
pub enum Preflight {
    Ok,
    HostBinsMissing(Vec<MissingDep>),
    VmMissing,
    VmStopped,
}

/// One missing host binary, ready to display in the preflight modal.
/// `purpose` answers "why does fleet need this?"; `install` is the one
/// copy-pasteable command for the platform we expect most users on
/// (macOS via Homebrew); `docs` is an optional URL for anyone on a
/// different platform.
pub struct MissingDep {
    pub name: &'static str,
    pub purpose: &'static str,
    pub install: &'static str,
    pub docs: Option<&'static str>,
}

/// Run the preflight probes. See module docs for the order.
pub fn check() -> Preflight {
    check_with(default_has_bin, default_vm_status)
}

/// Inner form with the two probe functions injected — keeps [`check`]
/// trivially testable without mocking subprocess invocations.
fn check_with(
    has_bin: impl Fn(&str) -> bool,
    vm_status: impl FnOnce() -> VmStatus,
) -> Preflight {
    let mut host = Vec::new();
    if !has_bin("limactl") {
        host.push(MissingDep {
            name: "limactl",
            purpose: "drives the Lima VM that hosts the ao orchestrator and tmux sessions",
            install: "brew install lima",
            docs: Some("https://lima-vm.io"),
        });
    }
    if !host.is_empty() {
        return Preflight::HostBinsMissing(host);
    }
    match vm_status() {
        VmStatus::Running => Preflight::Ok,
        VmStatus::Stopped => Preflight::VmStopped,
        VmStatus::Missing => Preflight::VmMissing,
    }
}

fn default_has_bin(name: &str) -> bool {
    // Subprocess to `which` rather than a `which` crate dep: this runs
    // at most three times per fleet session and the shell built-in is
    // faster to reason about than a transitive crate.
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn default_vm_status() -> VmStatus {
    Lima::new(Arc::new(RealProcessInvoker), VM_NAME).status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_missing_short_circuits_vm_check() {
        // If `limactl` is missing the VM probe is meaningless — we'd be
        // about to shell out to a binary we know isn't there. The result
        // must report the host miss without consulting `vm_status`.
        let vm_status = || panic!("vm_status must not run when host check fails");
        let result = check_with(|_| false, vm_status);
        assert!(matches!(result, Preflight::HostBinsMissing(_)));
    }

    #[test]
    fn ok_when_host_and_vm_running() {
        let result = check_with(|_| true, || VmStatus::Running);
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn vm_missing_when_lima_reports_missing() {
        let result = check_with(|_| true, || VmStatus::Missing);
        assert!(matches!(result, Preflight::VmMissing));
    }

    #[test]
    fn vm_stopped_when_lima_reports_stopped() {
        let result = check_with(|_| true, || VmStatus::Stopped);
        assert!(matches!(result, Preflight::VmStopped));
    }

    #[test]
    fn has_bin_finds_sh() {
        assert!(default_has_bin("sh"));
    }

    #[test]
    fn has_bin_misses_obvious_nonsense() {
        assert!(!default_has_bin("definitely-not-a-real-binary-zzz"));
    }
}
