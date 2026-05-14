//! `fleet runtime …` — v2-era runtime-adapter commands.
//!
//! `runtime doctor` probes the host for container engines and prints a
//! human-readable summary. It does **not** touch AO or Lima; this is the
//! diagnostic the user runs when their pipeline is the new devcontainer +
//! Podman/Apple Container stack.
//!
//! All formatting goes through a pure function ([`render_doctor`]) so the CLI
//! is a thin wire-up over a fully-tested formatter. The `run_doctor` entry
//! point hands in a real `ProcessInvoker`; tests hand in mocks.

use std::fmt::Write as _;

use crate::process::RealProcessInvoker;
use crate::runtime::detect::{BackendKind, BackendStatus, ProbeReport, probe};

/// CLI entry point for `fleet runtime doctor`. Probes the host and writes
/// the report to stdout. Returns the exit code: 0 if a usable engine was
/// found, 1 otherwise (so it's scriptable in CI). The probe itself is
/// infallible — bad invoker output is treated as "tool absent" — so this
/// returns a bare `i32`, not `Result`. The dispatcher wraps it in `Ok` to
/// match the surrounding signature.
pub fn run_doctor() -> i32 {
    let invoker = RealProcessInvoker;
    let report = probe(&invoker);
    let rendered = render_doctor(&report);
    println!("{rendered}");
    i32::from(!report.is_usable())
}

/// Pure renderer for the doctor report. Stable wording — tests assert on it.
pub fn render_doctor(report: &ProbeReport) -> String {
    let mut out = String::new();
    out.push_str("fleet runtime doctor\n");
    out.push_str("====================\n\n");

    out.push_str("Engines:\n");
    out.push_str(&render_backend("podman", &report.podman));
    out.push_str(&render_backend("docker", &report.docker));
    out.push_str(&render_backend("container (Apple)", &report.apple_container));

    out.push_str("\nHardening:\n");
    out.push_str(&render_backend("runsc (gVisor)", &report.gvisor));

    out.push_str("\nDevcontainer CLI:\n");
    out.push_str(&render_backend("devcontainer", &report.devcontainer_cli));

    out.push_str("\nRecommended engine: ");
    match report.recommended {
        Some(BackendKind::Podman) => out.push_str("podman\n"),
        Some(BackendKind::Docker) => out.push_str("docker\n"),
        Some(BackendKind::AppleContainer) => out.push_str("container (Apple)\n"),
        // gVisor and DevcontainerCli aren't engines; not reachable here.
        Some(other) => {
            let _ = writeln!(out, "{} (unexpected)", other.program());
        }
        None => out.push_str("none — fleet cannot start a container\n"),
    }

    let hints = report.install_hints();
    if !hints.is_empty() {
        out.push_str("\nNext steps:\n");
        for hint in hints {
            out.push_str("  - ");
            out.push_str(hint);
            out.push('\n');
        }
    }

    out
}

fn render_backend(label: &str, status: &BackendStatus) -> String {
    let mark = if status.present { "✓" } else { "✗" };
    let version = status
        .version
        .as_deref()
        .map_or_else(String::new, |v| format!(" — {v}"));
    let notes = if status.notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", status.notes.join("; "))
    };
    format!("  {mark} {label}{version}{notes}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::detect::BackendStatus;

    fn present(kind: BackendKind, version: &str) -> BackendStatus {
        BackendStatus {
            kind,
            present: true,
            version: Some(version.to_string()),
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

    fn report_all_present() -> ProbeReport {
        ProbeReport {
            podman: present(BackendKind::Podman, "podman version 5.0.1"),
            docker: present(BackendKind::Docker, "Docker version 27.0.0"),
            apple_container: present(BackendKind::AppleContainer, "container 0.1.0"),
            gvisor: present(BackendKind::GVisor, "runsc 20260301"),
            devcontainer_cli: present(BackendKind::DevcontainerCli, "devcontainer 0.1.12"),
            recommended: Some(BackendKind::AppleContainer),
        }
    }

    fn report_nothing_present() -> ProbeReport {
        ProbeReport {
            podman: absent(BackendKind::Podman),
            docker: absent(BackendKind::Docker),
            apple_container: absent(BackendKind::AppleContainer),
            gvisor: absent(BackendKind::GVisor),
            devcontainer_cli: absent(BackendKind::DevcontainerCli),
            recommended: None,
        }
    }

    #[test]
    fn renders_check_marks_for_present_backends() {
        let rendered = render_doctor(&report_all_present());
        assert!(rendered.contains("✓ podman — podman version 5.0.1"));
        assert!(rendered.contains("✓ docker — Docker version 27.0.0"));
        assert!(rendered.contains("✓ container (Apple) — container 0.1.0"));
        assert!(rendered.contains("✓ runsc (gVisor) — runsc 20260301"));
        assert!(rendered.contains("✓ devcontainer — devcontainer 0.1.12"));
    }

    #[test]
    fn renders_cross_marks_for_absent_backends_without_version() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("✗ podman\n"));
        assert!(rendered.contains("✗ docker\n"));
        assert!(rendered.contains("✗ container (Apple)\n"));
        assert!(rendered.contains("✗ runsc (gVisor)\n"));
        assert!(rendered.contains("✗ devcontainer\n"));
    }

    #[test]
    fn renders_recommended_engine_when_available() {
        let rendered = render_doctor(&report_all_present());
        assert!(rendered.contains("Recommended engine: container (Apple)"));
    }

    #[test]
    fn renders_recommended_none_with_actionable_text() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("Recommended engine: none"));
        assert!(rendered.contains("fleet cannot start a container"));
    }

    #[test]
    fn renders_install_hints_when_present() {
        let rendered = render_doctor(&report_nothing_present());
        assert!(rendered.contains("Next steps:"));
        assert!(rendered.contains("no container engine detected"));
        assert!(rendered.contains("devcontainer CLI missing"));
    }

    #[test]
    fn omits_install_hints_section_when_nothing_to_say() {
        let rendered = render_doctor(&report_all_present());
        assert!(!rendered.contains("Next steps:"));
    }

    #[test]
    fn renders_recommendation_when_only_podman_present() {
        let mut r = report_nothing_present();
        r.podman = present(BackendKind::Podman, "podman 5.0.1");
        r.recommended = Some(BackendKind::Podman);
        let rendered = render_doctor(&r);
        assert!(rendered.contains("Recommended engine: podman"));
    }

    #[test]
    fn render_backend_includes_notes_when_nonempty() {
        let mut s = present(BackendKind::Podman, "podman 5.0.1");
        s.notes.push("rootless ready".to_string());
        let line = render_backend("podman", &s);
        assert!(line.contains("(rootless ready)"));
    }
}
