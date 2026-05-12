//! Hard-dependency check that runs before the main event loop.
//!
//! Fleet has no degraded mode — every host-side operation funnels through
//! `limactl shell <vm> …`, so a missing `limactl` binary means nothing
//! works. Surfacing that as a one-line flash mid-session (the way the old
//! code did) leaves the user staring at a TUI they can't drive, with the
//! root cause one buried error away. Probe once at startup; if anything
//! fails, hand control to the preflight modal in [`crate::tui::ui`].
//!
//! Tmux is intentionally not probed here. Every fleet call to tmux goes
//! through `limactl shell … tmux …`, so tmux's availability is a property
//! of the Lima guest image, not the host — preflighting it on the host
//! would gate on the wrong machine.

use std::process::{Command, Stdio};

/// One missing dependency, ready to display in the preflight modal.
/// `purpose` answers "why does fleet need this?"; `install_hint` is the
/// one-liner the user can copy-paste.
pub struct MissingDep {
    pub name: &'static str,
    pub purpose: &'static str,
    pub install_hint: &'static str,
}

/// Run the preflight probes. Returns an empty vec on success.
pub fn check() -> Vec<MissingDep> {
    let mut out = Vec::new();
    if !has_bin("limactl") {
        out.push(MissingDep {
            name: "limactl",
            purpose: "drives the Lima VM that hosts the ao orchestrator and tmux sessions",
            install_hint: "brew install lima   (macOS)   |   see https://lima-vm.io for other platforms",
        });
    }
    out
}

/// `which <name>` — true when the binary resolves on `$PATH`. Subprocess
/// rather than a `which` crate dep: this runs once at startup and the
/// shell built-in is faster to reason about than a transitive crate.
fn has_bin(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_bin_finds_sh() {
        // `sh` is POSIX-required on every supported platform; if this
        // fails the test runner itself is broken.
        assert!(has_bin("sh"));
    }

    #[test]
    fn has_bin_misses_obvious_nonsense() {
        assert!(!has_bin("definitely-not-a-real-binary-zzz"));
    }
}
