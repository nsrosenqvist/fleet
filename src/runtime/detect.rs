//! Host probing: which container runtimes are installed, what their versions
//! are, and which adapter fleet should default to.
//!
//! All probing flows through [`ProcessInvoker`] (not `std::process::Command`
//! directly) so the recommendation logic is unit-testable without installing
//! any of these tools. A missing binary surfaces as an `Err` from the invoker;
//! we treat that as "not present" rather than propagating.

use crate::process::ProcessInvoker;

/// What we probed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Linux primary; works on macOS via `podman machine`.
    Podman,
    /// Cross-platform fallback.
    Docker,
    /// Apple Containerization (`container` CLI, macOS 26+).
    AppleContainer,
    /// gVisor's `runsc` binary — a *hardening* option, not a primary engine.
    GVisor,
    /// The Rust `devcontainer` CLI (preferred). MS's Node `devcontainer` is
    /// the fallback, detected separately.
    DevcontainerCli,
}

impl BackendKind {
    pub const fn program(self) -> &'static str {
        use BackendKind::{AppleContainer, DevcontainerCli, Docker, GVisor, Podman};
        match self {
            Podman => "podman",
            Docker => "docker",
            AppleContainer => "container",
            GVisor => "runsc",
            DevcontainerCli => "devcontainer",
        }
    }
}

/// Per-backend probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub kind: BackendKind,
    pub present: bool,
    /// First line of the tool's `--version` output, trimmed. `None` when not
    /// present or when version parsing didn't yield anything sensible.
    pub version: Option<String>,
    /// Free-text remarks for the doctor view (e.g. "rootless ready",
    /// "machine not initialised"). Empty when nothing notable.
    pub notes: Vec<String>,
}

impl BackendStatus {
    fn missing(kind: BackendKind) -> Self {
        Self {
            kind,
            present: false,
            version: None,
            notes: Vec::new(),
        }
    }
}

/// Aggregated probe results plus a chosen default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub podman: BackendStatus,
    pub docker: BackendStatus,
    pub apple_container: BackendStatus,
    pub gvisor: BackendStatus,
    pub devcontainer_cli: BackendStatus,
    /// Recommended *engine* (not hardening or devcontainer CLI). `None`
    /// when nothing usable was found — `fleet doctor` surfaces install hints
    /// in that case.
    pub recommended: Option<BackendKind>,
}

impl ProbeReport {
    /// Whether fleet has enough installed to start a container at all.
    pub fn is_usable(&self) -> bool {
        self.recommended.is_some()
    }

    /// Install hints for missing tools, in priority order. Kept here (rather
    /// than in the TUI) so unit tests can assert on hint stability.
    pub fn install_hints(&self) -> Vec<&'static str> {
        let mut hints = Vec::new();
        if !self.podman.present && !self.docker.present && !self.apple_container.present {
            hints.push(
                "no container engine detected — install Podman \
                 (`brew install podman` on macOS, `dnf install podman` on Fedora) \
                 or Apple's `container` CLI on macOS 26+.",
            );
        }
        if !self.devcontainer_cli.present {
            hints.push(
                "devcontainer CLI missing — `cargo install devcontainer` \
                 (Rust) or `npm install -g @devcontainers/cli` (Node fallback).",
            );
        }
        hints
    }
}

/// Probe the host. Pure with respect to the invoker — for tests, hand in a
/// mock that returns canned `--version` output (or errors) per program.
pub fn probe(invoker: &dyn ProcessInvoker) -> ProbeReport {
    let podman = probe_one(invoker, BackendKind::Podman);
    let docker = probe_one(invoker, BackendKind::Docker);
    let apple_container = probe_one(invoker, BackendKind::AppleContainer);
    let gvisor = probe_one(invoker, BackendKind::GVisor);
    let devcontainer_cli = probe_one(invoker, BackendKind::DevcontainerCli);

    // Preference order: Apple Container (strongest isolation, native on
    // macOS 26+) > Podman (rootless + optional gVisor on Linux) > Docker
    // (last resort fallback). We do *not* probe the OS — Apple Container
    // simply won't be present off macOS so the ordering self-resolves.
    let recommended = [&apple_container, &podman, &docker]
        .into_iter()
        .find(|s| s.present)
        .map(|s| s.kind);

    ProbeReport {
        podman,
        docker,
        apple_container,
        gvisor,
        devcontainer_cli,
        recommended,
    }
}

fn probe_one(invoker: &dyn ProcessInvoker, kind: BackendKind) -> BackendStatus {
    let program = kind.program();
    invoker
        .run(program, vec!["--version".to_string()])
        .map_or_else(
            |_| BackendStatus::missing(kind),
            |stdout| BackendStatus {
                kind,
                present: true,
                version: stdout.lines().next().map(str::trim).map(String::from),
                notes: Vec::new(),
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    /// Build a mock invoker where each (program, response) pair canned out.
    /// Anything not listed returns an error (= "binary not found").
    fn invoker_with(present: &[(&'static str, &'static str)]) -> MockProcessInvoker {
        let mut mock = MockProcessInvoker::new();
        for (prog, out) in present {
            let out_owned = (*out).to_string();
            mock.expect_run()
                .with(eq(*prog), eq(vec!["--version".to_string()]))
                .returning(move |_, _| Ok(out_owned.clone()));
        }
        // Everything else: error.
        mock.expect_run()
            .returning(|prog, _| Err(anyhow::anyhow!("no such command: {prog}")));
        mock
    }

    #[test]
    fn probe_marks_present_backends_with_first_line_version() {
        let mock = invoker_with(&[
            ("podman", "podman version 5.0.1"),
            ("docker", "Docker version 27.0.0, build abc"),
        ]);
        let r = probe(&mock);
        assert!(r.podman.present);
        assert_eq!(r.podman.version.as_deref(), Some("podman version 5.0.1"));
        assert!(r.docker.present);
        assert_eq!(
            r.docker.version.as_deref(),
            Some("Docker version 27.0.0, build abc")
        );
    }

    #[test]
    fn probe_marks_missing_backends_absent() {
        let mock = invoker_with(&[("podman", "podman version 5.0.1")]);
        let r = probe(&mock);
        assert!(!r.docker.present);
        assert_eq!(r.docker.version, None);
        assert!(!r.apple_container.present);
        assert!(!r.gvisor.present);
    }

    #[test]
    fn probe_recommends_apple_container_when_available() {
        let mock = invoker_with(&[
            ("container", "container version 0.1.0"),
            ("podman", "podman version 5.0.1"),
            ("docker", "Docker version 27.0.0"),
        ]);
        let r = probe(&mock);
        assert_eq!(r.recommended, Some(BackendKind::AppleContainer));
    }

    #[test]
    fn probe_falls_back_to_podman_when_no_apple_container() {
        let mock = invoker_with(&[
            ("podman", "podman version 5.0.1"),
            ("docker", "Docker version 27.0.0"),
        ]);
        let r = probe(&mock);
        assert_eq!(r.recommended, Some(BackendKind::Podman));
    }

    #[test]
    fn probe_falls_back_to_docker_as_last_resort() {
        let mock = invoker_with(&[("docker", "Docker version 27.0.0")]);
        let r = probe(&mock);
        assert_eq!(r.recommended, Some(BackendKind::Docker));
    }

    #[test]
    fn probe_returns_none_recommended_when_nothing_present() {
        let mock = invoker_with(&[]);
        let r = probe(&mock);
        assert_eq!(r.recommended, None);
        assert!(!r.is_usable());
    }

    #[test]
    fn probe_detects_gvisor_separately_from_engine() {
        // gVisor is hardening, not an engine — its presence shouldn't
        // affect `recommended`, but should be reflected in the status.
        let mock = invoker_with(&[
            ("podman", "podman version 5.0.1"),
            ("runsc", "runsc version 20260301.0"),
        ]);
        let r = probe(&mock);
        assert!(r.gvisor.present);
        assert_eq!(r.recommended, Some(BackendKind::Podman));
    }

    #[test]
    fn install_hints_flag_missing_engine() {
        let r = probe(&invoker_with(&[]));
        let hints = r.install_hints();
        assert!(hints.iter().any(|h| h.contains("no container engine")));
    }

    #[test]
    fn install_hints_flag_missing_devcontainer_cli() {
        let r = probe(&invoker_with(&[("podman", "podman version 5.0.1")]));
        let hints = r.install_hints();
        assert!(hints.iter().any(|h| h.contains("devcontainer CLI missing")));
    }

    #[test]
    fn install_hints_silent_when_everything_present() {
        let r = probe(&invoker_with(&[
            ("podman", "podman version 5.0.1"),
            ("devcontainer", "devcontainer 0.1.12"),
        ]));
        assert!(r.install_hints().is_empty());
    }

    #[test]
    fn backend_kind_program_is_the_executable_name() {
        assert_eq!(BackendKind::Podman.program(), "podman");
        assert_eq!(BackendKind::Docker.program(), "docker");
        assert_eq!(BackendKind::AppleContainer.program(), "container");
        assert_eq!(BackendKind::GVisor.program(), "runsc");
        assert_eq!(BackendKind::DevcontainerCli.program(), "devcontainer");
    }
}
