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
    /// `git-bug` host binary — needed when `tracker: git-bug` in the repo
    /// config. fleet shells `git-bug bug --format json` directly; missing
    /// binary surfaces as the first `fleet issues list` error today.
    GitBug,
    /// `tinyproxy` host binary — needed by `HostProxyEnforcer` on macOS
    /// (Apple Container / Docker) when `network.policy: allowlist` is in
    /// force. Not used by the Linux Podman path, which runs tinyproxy
    /// inside a sidecar container.
    Tinyproxy,
    /// `tmux` host binary — needed for `fleet brainstorm` interactive
    /// planning sessions. Not used by workflow execution, so its
    /// absence is only a problem when the user reaches for the
    /// brainstorm flow.
    Tmux,
}

impl BackendKind {
    pub const fn program(self) -> &'static str {
        use BackendKind::{
            AppleContainer, DevcontainerCli, Docker, GVisor, GitBug, Podman, Tinyproxy, Tmux,
        };
        match self {
            Podman => "podman",
            Docker => "docker",
            AppleContainer => "container",
            GVisor => "runsc",
            DevcontainerCli => "devcontainer",
            GitBug => "git-bug",
            Tinyproxy => "tinyproxy",
            Tmux => "tmux",
        }
    }

    /// Argv to invoke for the presence-probe. Most tools accept
    /// `--version`; the exceptions:
    /// - `tinyproxy -v` (the long form prints help instead),
    /// - `tmux -V` (capital V; lowercase opens a session),
    /// - `git-bug version` — subcommand, not a flag. `git-bug
    ///   --version` errors with `unknown flag: --version` and
    ///   exits 1, which the probe would otherwise treat as
    ///   "missing".
    pub const fn probe_arg(self) -> &'static str {
        match self {
            Self::Tinyproxy => "-v",
            Self::Tmux => "-V",
            Self::GitBug => "version",
            _ => "--version",
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
    /// `git-bug` binary (host-side, needed when `tracker: git-bug`).
    pub git_bug: BackendStatus,
    /// `tinyproxy` binary (host-side, needed when `HostProxyEnforcer`
    /// is the chosen egress backend — macOS Apple Container / Docker
    /// with `policy: allowlist`).
    pub tinyproxy: BackendStatus,
    /// `tmux` binary (host-side, needed for `fleet brainstorm`).
    pub tmux: BackendStatus,
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
    /// Hints for conditionally-needed tools (git-bug, tinyproxy) always
    /// surface when absent — they're cheap to install if needed and
    /// silent-on-config is worse than slightly noisy.
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
        if !self.git_bug.present {
            hints.push(
                "git-bug missing — `brew install git-bug` (macOS) or \
                 `cargo install git-bug` (anywhere). Only needed when \
                 `tracker: git-bug` in `.fleet/config.yaml`.",
            );
        }
        if !self.tinyproxy.present {
            hints.push(
                "tinyproxy missing — `brew install tinyproxy` (macOS) or \
                 `apt install tinyproxy` (Debian/Ubuntu) / `dnf install tinyproxy` \
                 (Fedora). Only needed on macOS Apple Container / Docker with \
                 `runtime.network.policy: allowlist`; Linux Podman uses a sidecar \
                 container instead.",
            );
        }
        if !self.tmux.present {
            hints.push(
                "tmux missing — `brew install tmux` (macOS) or `apt install tmux` \
                 (Debian/Ubuntu) / `dnf install tmux` (Fedora). Only needed when \
                 you run `fleet brainstorm`; workflow sessions don't use it.",
            );
        }
        hints
    }

    /// OS-tailored variant of [`Self::install_hints`]. Drops the
    /// platform suffixes that don't apply on the current host so
    /// `fleet init`'s first-run bootstrap surface is copy-pasteable.
    /// `os` is `std::env::consts::OS` (`"linux"` / `"macos"`); pass
    /// it explicitly so tests can pin either.
    pub fn install_hints_for_os(&self, os: &str) -> Vec<BootstrapHint> {
        let mut hints = Vec::new();
        if !self.podman.present && !self.docker.present && !self.apple_container.present {
            hints.push(BootstrapHint {
                tool: "container engine".to_string(),
                command: match os {
                    "macos" => {
                        "brew install podman    # or: brew install --cask container (macOS 26+)"
                            .to_string()
                    }
                    "linux" => {
                        "sudo dnf install podman   # or: sudo apt install podman".to_string()
                    }
                    _ => "install Podman, Docker, or Apple Container (macOS 26+)".to_string(),
                },
                note: "fleet cannot start a container without one of these".to_string(),
            });
        }
        if !self.devcontainer_cli.present {
            hints.push(BootstrapHint {
                tool: "devcontainer CLI".to_string(),
                command: "cargo install devcontainer    # or: npm install -g @devcontainers/cli"
                    .to_string(),
                note: "required — fleet builds images via this CLI".to_string(),
            });
        }
        if !self.git_bug.present {
            hints.push(BootstrapHint {
                tool: "git-bug".to_string(),
                command: match os {
                    "macos" => "brew install git-bug".to_string(),
                    _ => "cargo install git-bug".to_string(),
                },
                note: "only needed when `tracker: git-bug` in .fleet/config.yaml".to_string(),
            });
        }
        if !self.tinyproxy.present {
            hints.push(BootstrapHint {
                tool: "tinyproxy".to_string(),
                command: match os {
                    "macos" => "brew install tinyproxy".to_string(),
                    "linux" => {
                        "sudo apt install tinyproxy    # or: sudo dnf install tinyproxy".to_string()
                    }
                    _ => "install tinyproxy from your distro's repos".to_string(),
                },
                note: "only needed on macOS with `policy: allowlist` (Linux uses a sidecar)"
                    .to_string(),
            });
        }
        if !self.tmux.present {
            hints.push(BootstrapHint {
                tool: "tmux".to_string(),
                command: match os {
                    "macos" => "brew install tmux".to_string(),
                    "linux" => "sudo apt install tmux    # or: sudo dnf install tmux".to_string(),
                    _ => "install tmux from your distro's repos".to_string(),
                },
                note: "only needed when running `fleet brainstorm` interactive sessions"
                    .to_string(),
            });
        }
        hints
    }
}

/// One install hint, OS-tailored. Used by `fleet init`'s bootstrap
/// section so the user can copy/paste the install command directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapHint {
    pub tool: String,
    pub command: String,
    pub note: String,
}

/// Probe the host. Pure with respect to the invoker — for tests, hand in a
/// mock that returns canned `--version` output (or errors) per program.
pub fn probe(invoker: &dyn ProcessInvoker) -> ProbeReport {
    let podman = probe_one(invoker, BackendKind::Podman);
    let docker = probe_one(invoker, BackendKind::Docker);
    let apple_container = probe_one(invoker, BackendKind::AppleContainer);
    let gvisor = probe_one(invoker, BackendKind::GVisor);
    let devcontainer_cli = probe_one(invoker, BackendKind::DevcontainerCli);
    let git_bug = probe_one(invoker, BackendKind::GitBug);
    let tinyproxy = probe_one(invoker, BackendKind::Tinyproxy);
    let tmux = probe_one(invoker, BackendKind::Tmux);

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
        git_bug,
        tinyproxy,
        tmux,
        recommended,
    }
}

fn probe_one(invoker: &dyn ProcessInvoker, kind: BackendKind) -> BackendStatus {
    let program = kind.program();
    invoker
        .run(program, vec![kind.probe_arg().to_string()])
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
            // Match on program name only; per-tool argv differs
            // (`tinyproxy -v` vs `--version` for everything else).
            mock.expect_run()
                .with(eq(*prog), mockall::predicate::always())
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
            ("git-bug", "git-bug version: 0.10.0"),
            ("tinyproxy", "tinyproxy 1.11.1"),
            ("tmux", "tmux 3.4"),
        ]));
        assert!(r.install_hints().is_empty());
    }

    #[test]
    fn probe_arg_matches_each_tool_actual_version_invocation() {
        // Regression: git-bug doesn't accept `--version` as a
        // flag — it uses a `version` *subcommand* and exits 1 on
        // the flag form. probe_one treats exit 1 as "missing",
        // so for a long time `fleet runtime doctor` always
        // reported git-bug absent on hosts where it was
        // installed. Pin the per-tool invocation here so the
        // table can't silently regress to a uniform
        // `--version`.
        assert_eq!(BackendKind::Podman.probe_arg(), "--version");
        assert_eq!(BackendKind::Docker.probe_arg(), "--version");
        assert_eq!(BackendKind::AppleContainer.probe_arg(), "--version");
        assert_eq!(BackendKind::GVisor.probe_arg(), "--version");
        assert_eq!(BackendKind::DevcontainerCli.probe_arg(), "--version");
        assert_eq!(BackendKind::Tinyproxy.probe_arg(), "-v");
        assert_eq!(BackendKind::Tmux.probe_arg(), "-V");
        assert_eq!(BackendKind::GitBug.probe_arg(), "version");
    }

    #[test]
    fn probe_invokes_git_bug_with_version_subcommand_not_flag() {
        // Tightly observe the argv `probe` passes to git-bug — a
        // typo on the probe_arg table (or a future refactor that
        // collapses the match) shouldn't silently re-break the
        // probe on every git-bug-using repo.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("git-bug"), eq(vec!["version".to_string()]))
            .returning(|_, _| Ok("git-bug version: v0.8.0".to_string()));
        // Catch-all: any other tool the broader probe queries.
        mock.expect_run()
            .returning(|prog, _| Err(anyhow::anyhow!("no such command: {prog}")));
        let r = probe(&mock);
        assert!(r.git_bug.present);
        assert_eq!(
            r.git_bug.version.as_deref(),
            Some("git-bug version: v0.8.0")
        );
    }

    #[test]
    fn probe_picks_up_git_bug_and_tinyproxy_independently_of_engine() {
        // The two host-tool probes are wholly independent of the engine
        // selection: they appear in the report regardless of whether
        // any container runtime is present.
        let mock = invoker_with(&[
            ("git-bug", "git-bug version: 0.10.0"),
            ("tinyproxy", "tinyproxy 1.11.1"),
        ]);
        let r = probe(&mock);
        assert!(r.git_bug.present);
        assert_eq!(
            r.git_bug.version.as_deref(),
            Some("git-bug version: 0.10.0")
        );
        assert!(r.tinyproxy.present);
        assert_eq!(r.tinyproxy.version.as_deref(), Some("tinyproxy 1.11.1"));
        // No engine present → still recommended=None.
        assert_eq!(r.recommended, None);
    }

    #[test]
    fn install_hints_flag_missing_git_bug() {
        let r = probe(&invoker_with(&[("podman", "podman version 5.0.1")]));
        let hints = r.install_hints();
        assert!(hints.iter().any(|h| h.contains("git-bug missing")));
    }

    #[test]
    fn install_hints_flag_missing_tinyproxy() {
        let r = probe(&invoker_with(&[("podman", "podman version 5.0.1")]));
        let hints = r.install_hints();
        assert!(hints.iter().any(|h| h.contains("tinyproxy missing")));
    }

    #[test]
    fn backend_kind_program_is_the_executable_name() {
        assert_eq!(BackendKind::Podman.program(), "podman");
        assert_eq!(BackendKind::Docker.program(), "docker");
        assert_eq!(BackendKind::AppleContainer.program(), "container");
        assert_eq!(BackendKind::GVisor.program(), "runsc");
        assert_eq!(BackendKind::DevcontainerCli.program(), "devcontainer");
        assert_eq!(BackendKind::GitBug.program(), "git-bug");
        assert_eq!(BackendKind::Tinyproxy.program(), "tinyproxy");
    }

}
