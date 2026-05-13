//! Startup checks that gate the main event loop.
//!
//! Fleet has no degraded mode — every host-side operation funnels through
//! `limactl shell <vm> …`, so running the TUI when prerequisites are wrong
//! just produces noisy mid-session failures. Phases:
//!
//! 1. `limactl` on `$PATH` (hard fail — nothing else works without it).
//! 2. `fleet-vm` Lima instance exists.
//! 3. The instance is running.
//! 4. `defaults.workspace: worktree` is set in the AO yaml. **Hard
//!    fail** — without it, AO may run an agent against the host
//!    checkout's working tree, which is the exact scenario fleet
//!    exists to prevent. Surfaced as a dead-end modal until the user
//!    edits the yaml.
//! 5. Tracker tools (`git-bug` inside the VM, `gh` on the host) match the
//!    plugins configured in `agent-orchestrator.yaml`. **Advisory** — the
//!    rest of the TUI works without these; only the spawn picker breaks
//!    for that one project. Surfaced as a continue-able warning so the
//!    user can fix the AO yaml or install the tool without blocking
//!    everything else.
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

/// Outcome of [`check`]. Hard failures (`HostBinsMissing`, `VmMissing`,
/// `VmStopped`, `WorkspaceUnsafe`) short-circuit later phases.
/// `TrackerWarnings` only arrives after the hard phases pass — the
/// spawn picker is broken but the rest of the TUI works, so the modal
/// is continue-able.
pub enum Preflight {
    Ok,
    HostBinsMissing(Vec<MissingDep>),
    VmMissing,
    VmStopped,
    /// `defaults.workspace` in the AO yaml is missing or set to
    /// something other than `"worktree"`. `current` carries the
    /// observed value (or `None` when the key is absent) so the
    /// modal can show exactly what's wrong.
    WorkspaceUnsafe {
        current: Option<String>,
    },
    TrackerWarnings(Vec<MissingTracker>),
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

/// One tracker plugin whose required in-VM tool isn't installed.
/// `used_by` lists the project keys that configure this plugin so
/// the user can see exactly which projects' spawn pickers will
/// break. Install commands live in [`install_command`] so callers
/// (the modal's auto-install path) and template provisioning stay
/// in one place.
pub struct MissingTracker {
    pub plugin: String,
    pub tool: &'static str,
    pub used_by: Vec<String>,
}

/// Run the preflight probes. See module docs for ordering. The AO yaml
/// is read from the canonical XDG path; the launch repo doesn't enter
/// into the check.
pub fn check() -> Preflight {
    check_with(default_has_bin, default_vm_status, default_in_vm)
}

/// Inner form with probe functions injected so tests can drive any
/// branch without spawning subprocesses.
fn check_with(
    has_bin: impl Fn(&str) -> bool,
    vm_status: impl FnOnce() -> VmStatus,
    in_vm: impl Fn(&str) -> bool,
) -> Preflight {
    // Phase 1: host bins.
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

    // Phase 2 + 3: VM lifecycle.
    match vm_status() {
        VmStatus::Missing => return Preflight::VmMissing,
        VmStatus::Stopped => return Preflight::VmStopped,
        VmStatus::Running => {}
    }

    // Phase 4 + 5: AO yaml-driven checks. Loading is best-effort — if
    // it doesn't parse / doesn't exist, we skip both checks; the
    // spawn flow will surface the underlying error later.
    let ao = crate::ao::config::AoConfig::load()
        .ok()
        .flatten()
        .map(|(_, cfg)| cfg);
    if let Some(cfg) = ao {
        // Phase 4: workspace isolation gate. Hard fail — fleet exists
        // to keep agents off the host checkout, so anything other
        // than `worktree` blocks the TUI until the user fixes it.
        if !cfg.defaults.workspace_is_worktree() {
            return Preflight::WorkspaceUnsafe {
                current: cfg.defaults.workspace,
            };
        }
        // Phase 5: tracker tools.
        let warnings = check_trackers(&cfg, &has_bin, &in_vm);
        if !warnings.is_empty() {
            return Preflight::TrackerWarnings(warnings);
        }
    }

    Preflight::Ok
}

/// Group project tracker plugins by name (so multiple projects sharing
/// a plugin produce one row), probe the required tool for each, and
/// return a `MissingTracker` per missing plugin. Unknown plugins are
/// skipped — fleet doesn't know what tool to look for and the
/// per-project spawn picker will surface the actual error.
fn check_trackers(
    cfg: &crate::ao::config::AoConfig,
    has_bin: &impl Fn(&str) -> bool,
    in_vm: &impl Fn(&str) -> bool,
) -> Vec<MissingTracker> {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, project) in &cfg.projects {
        if let Some(tracker) = &project.tracker {
            grouped
                .entry(tracker.plugin.clone())
                .or_default()
                .push(key.clone());
        }
    }

    let mut missing = Vec::new();
    for (plugin, used_by) in grouped {
        let tools: &[&'static str] = match plugin.as_str() {
            "git-bug" => &["git-bug"],
            // github needs gh (issue list, used by the spawn picker)
            // *and* gh-dash (the TUI preview triggered by `t`).
            "github" => &["gh", "gh-dash"],
            _ => continue, // Unknown plugin; the per-action error
                           // surfaces the actual failure later.
        };
        for tool in tools {
            if in_vm(tool) {
                continue;
            }
            missing.push(MissingTracker {
                plugin: plugin.clone(),
                tool,
                used_by: used_by.clone(),
            });
        }
        let _ = has_bin; // Probe signature is kept for parity / tests.
    }
    missing
}

/// In-VM install command for a tracker tool. Run via `limactl shell
/// fleet-vm -- bash -c '...'` from the preflight modal's auto-install
/// action. Mirrors the provisioning steps in `templates/fleet-vm.yaml`
/// so an existing VM (created before fleet's template included these)
/// converges on the same state without a rebuild.
///
/// `set -ex` at the top echoes each command before running it
/// (otherwise the user sees a silent pause during long downloads /
/// apt updates) and aborts on first failure. `curl -fSL` (no `s`)
/// shows the progress bar to a TTY.
pub fn install_command(tool: &str) -> Option<&'static str> {
    match tool {
        // gh extension — needs `gh` already installed. The
        // preflight probes in declaration order (gh before gh-dash),
        // so by the time the install runs, gh is present (either
        // pre-existed or just installed).
        "gh-dash" => Some(
            r"set -ex
gh extension install dlvhdr/gh-dash
gh dash --version",
        ),
        "git-bug" => Some(
            r#"set -ex
arch="$(dpkg --print-architecture)"
case "$arch" in
  amd64) gb=amd64 ;;
  arm64) gb=arm64 ;;
  *)     gb="$arch" ;;
esac
sudo curl -fSL "https://github.com/git-bug/git-bug/releases/latest/download/git-bug_linux_${gb}" -o /usr/local/bin/git-bug
sudo chmod +x /usr/local/bin/git-bug
git-bug --version"#,
        ),
        "gh" => Some(
            r#"set -ex
if ! sudo apt-get install -y gh; then
  sudo mkdir -p -m 755 /etc/apt/keyrings
  curl -fSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo dd of=/etc/apt/keyrings/githubcli-archive-keyring.gpg
  sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list
  sudo apt-get update
  sudo apt-get install -y gh
fi
gh --version"#,
        ),
        _ => None,
    }
}

fn default_has_bin(name: &str) -> bool {
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

/// Probe for a binary inside the running guest. `gh-dash` is a gh
/// extension (sub-command), not a standalone binary, so `command -v`
/// doesn't find it — use `gh dash --version` which exits 0 when the
/// extension is installed. Everything else goes through plain
/// `command -v`. Cheap (~300ms cold, faster while the VM is hot)
/// so we run it per tracker tool at startup rather than caching.
fn default_in_vm(name: &str) -> bool {
    let (cmd, args): (&str, &[&str]) = match name {
        "gh-dash" => ("gh", &["dash", "--version"]),
        _ => ("command", &["-v", name]),
    };
    Command::new("limactl")
        .args(["shell", VM_NAME, cmd])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Create a tempdir laid out as an `$XDG_CONFIG_HOME` root —
    /// i.e. with a `fleet/` subdir ready to receive the AO yaml.
    /// Returns the *base* path (the value to set `XDG_CONFIG_HOME`
    /// to); the yaml itself goes inside `<base>/fleet/`.
    fn tmp_xdg() -> PathBuf {
        let base = tempfile::tempdir().expect("tempdir").keep();
        std::fs::create_dir_all(base.join("fleet")).expect("mkdir fleet");
        base
    }

    /// Write an `agent-orchestrator.yaml` into `<xdg>/fleet/` so the
    /// `AoConfig` loader picks it up via `default_xdg_path`. Pair
    /// with [`with_isolated_xdg`] to actually scope the load to the
    /// tempdir.
    fn write_ao_yaml(xdg: &Path, body: &str) {
        std::fs::write(xdg.join("fleet").join("agent-orchestrator.yaml"), body)
            .expect("write agent-orchestrator.yaml");
    }

    /// Point `XDG_CONFIG_HOME` at `xdg` while `body` runs and restore
    /// before return. Holds [`crate::test_env::xdg_lock`] so parallel
    /// XDG-mutating tests don't clobber each other — the env var is
    /// process-global and cargo runs unit tests in parallel by
    /// default.
    fn with_isolated_xdg<R>(xdg: &Path, body: impl FnOnce() -> R) -> R {
        let guard = crate::test_env::xdg_lock();
        // SAFETY: env mutation is process-global; restored before return.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg) };
        let result = body();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        drop(guard);
        result
    }

    #[test]
    fn host_missing_short_circuits_vm_check() {
        let vm_status = || panic!("vm_status must not run when host check fails");
        let in_vm = |_: &str| panic!("in_vm must not run when host check fails");
        let result = check_with(|_| false, vm_status, in_vm);
        assert!(matches!(result, Preflight::HostBinsMissing(_)));
    }

    #[test]
    fn ok_when_host_and_vm_running_with_no_ao_yaml() {
        // No yaml in the isolated XDG → both workspace + tracker
        // phases are no-ops → Ok. Without the isolation, this test
        // would read whatever AO yaml the developer has in their
        // real ~/.config/fleet/ and trip the new workspace gate.
        let xdg = tmp_xdg();
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| true)
        });
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn vm_missing_when_lima_reports_missing() {
        let result = check_with(|_| true, || VmStatus::Missing, |_| true);
        assert!(matches!(result, Preflight::VmMissing));
    }

    #[test]
    fn vm_stopped_when_lima_reports_stopped() {
        let result = check_with(|_| true, || VmStatus::Stopped, |_| true);
        assert!(matches!(result, Preflight::VmStopped));
    }

    #[test]
    fn tracker_warning_when_git_bug_not_in_vm() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: worktree
projects:
  sandbox:
    name: sandbox
    path: /tmp/sandbox
    tracker:
      plugin: git-bug
",
        );
        // Override XDG so the loader doesn't pick up the dev's real
        // ~/.config/fleet/agent-orchestrator.yaml.
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |tool| tool != "git-bug")
        });
        match result {
            Preflight::TrackerWarnings(w) => {
                assert_eq!(w.len(), 1);
                assert_eq!(w[0].plugin, "git-bug");
                assert_eq!(w[0].tool, "git-bug");
                assert_eq!(w[0].used_by, vec!["sandbox".to_string()]);
            }
            other => panic!(
                "expected TrackerWarnings, got something else: {:?}",
                other_kind(&other)
            ),
        }
    }

    #[test]
    fn tracker_warning_when_gh_not_in_vm() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: worktree
projects:
  webapp:
    name: webapp
    path: /tmp/webapp
    tracker:
      plugin: github
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(
                |_| true,
                || VmStatus::Running,
                |tool| tool != "gh", // gh is what's missing in the guest
            )
        });
        match result {
            Preflight::TrackerWarnings(w) => {
                assert_eq!(w.len(), 1);
                assert_eq!(w[0].plugin, "github");
                assert_eq!(w[0].tool, "gh");
            }
            _ => panic!("expected TrackerWarnings"),
        }
    }

    #[test]
    fn install_command_covers_known_tools() {
        assert!(install_command("git-bug").is_some());
        assert!(install_command("gh").is_some());
        assert!(install_command("linear").is_none());
    }

    #[test]
    fn tracker_warning_skipped_when_tool_available() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: worktree
projects:
  sandbox:
    name: sandbox
    path: /tmp/sandbox
    tracker:
      plugin: git-bug
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| true)
        });
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn unknown_tracker_plugin_is_silently_skipped() {
        // Unknown plugin name → fleet doesn't know what tool to probe,
        // so it skips. The spawn picker will surface a clearer error
        // later if/when the user actually tries to spawn.
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: worktree
projects:
  webapp:
    name: webapp
    path: /tmp/webapp
    tracker:
      plugin: linear
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| false)
        });
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn has_bin_finds_sh() {
        assert!(default_has_bin("sh"));
    }

    #[test]
    fn has_bin_misses_obvious_nonsense() {
        assert!(!default_has_bin("definitely-not-a-real-binary-zzz"));
    }

    fn other_kind(p: &Preflight) -> &'static str {
        match p {
            Preflight::Ok => "Ok",
            Preflight::HostBinsMissing(_) => "HostBinsMissing",
            Preflight::VmMissing => "VmMissing",
            Preflight::VmStopped => "VmStopped",
            Preflight::WorkspaceUnsafe { .. } => "WorkspaceUnsafe",
            Preflight::TrackerWarnings(_) => "TrackerWarnings",
        }
    }

    #[test]
    fn workspace_unsafe_when_default_missing() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
projects:
  sandbox:
    name: sandbox
    path: /tmp/sandbox
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| true)
        });
        match result {
            Preflight::WorkspaceUnsafe { current } => assert!(current.is_none()),
            other => panic!("expected WorkspaceUnsafe, got {}", other_kind(&other)),
        }
    }

    #[test]
    fn workspace_unsafe_when_wrong_value() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: docker
projects:
  sandbox:
    name: sandbox
    path: /tmp/sandbox
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| true)
        });
        match result {
            Preflight::WorkspaceUnsafe { current } => {
                assert_eq!(current.as_deref(), Some("docker"));
            }
            other => panic!("expected WorkspaceUnsafe, got {}", other_kind(&other)),
        }
    }

    #[test]
    fn workspace_ok_when_worktree_set() {
        let xdg = tmp_xdg();
        write_ao_yaml(
            &xdg,
            r"
defaults:
  workspace: worktree
projects:
  sandbox:
    name: sandbox
    path: /tmp/sandbox
",
        );
        let result = with_isolated_xdg(&xdg, || {
            check_with(|_| true, || VmStatus::Running, |_| true)
        });
        assert!(matches!(result, Preflight::Ok));
    }
}
