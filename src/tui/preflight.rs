//! Startup checks that gate the main event loop.
//!
//! Fleet's TUI is usable without AO running (empty sidebar, Shift+S to
//! bring the stack up). Preflight catches the things that *can't* be
//! fixed from inside the TUI:
//!
//! 1. `limactl` on `$PATH` (hard fail — nothing else works without it).
//! 2. `fleet-vm` Lima instance *exists*. Missing means we need to
//!    provision from the embedded template — a ~10-minute cloud-init
//!    flow with a streaming progress modal, so it stays a startup gate.
//!    *Stopped* is no longer a preflight failure: Shift+S handles VM
//!    start + AO start as a single `AoTask` chain, so the modal would
//!    just duplicate work the keybind does one keystroke later.
//! 3. `defaults.workspace: worktree` is set in the AO yaml. **Hard
//!    fail** — without it, AO may run an agent against the host
//!    checkout's working tree, which is the exact scenario fleet
//!    exists to prevent. Surfaced as a dead-end modal until the user
//!    edits the yaml. Local-file check, runs even with a stopped VM.
//! 4. The running VM's saved Lima config matches the current template's
//!    mount layout. **Hard fail** when a stale VM still has the host
//!    home bind-mounted writable: that defeats fleet's filesystem
//!    sandbox by exposing every host dotfile (`~/.ssh`, `~/.gnupg`,
//!    host `~/.claude`, …) to agents. The user is sent to a
//!    "rebuild required" modal pointing at `docs/sandbox.md`. Only
//!    fires when the VM is running (the saved config is meaningful
//!    only post-create); stopped VMs pass through and revalidate
//!    after Shift+S brings the stack up.
//! 5. Tracker tools (`git-bug` inside the VM, `gh` on the host) match
//!    the plugins configured in `agent-orchestrator.yaml`. **Advisory**
//!    — the rest of the TUI works without these; only the spawn picker
//!    breaks for that one project. Only fires when the VM is running
//!    (the check probes the guest); for a launch-with-stopped-VM the
//!    user discovers missing trackers via the spawn picker error.
//!
//! Tmux is intentionally not probed here. Every tmux call in fleet runs
//! through `limactl shell <vm> … tmux …`, so tmux's availability is a
//! property of the Lima guest image, not the host.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::RealProcessInvoker;

/// The Lima instance fleet expects. Mirrors `cli::vm::VM_NAME`.
pub(super) const VM_NAME: &str = "fleet-vm";

/// Outcome of [`check`]. Hard failures (`HostBinsMissing`, `VmMissing`,
/// `WorkspaceUnsafe`) short-circuit later phases. `TrackerWarnings`
/// only arrives after the hard phases pass — the spawn picker is
/// broken but the rest of the TUI works, so the modal is continue-able.
///
/// A *stopped* VM is intentionally not a preflight failure: Shift+S
/// inside the TUI brings VM + AO up together, so blocking startup on
/// a yes/no modal that does the same thing one keystroke later is
/// just noise. The TUI launches with empty sidebar + "AO down" badge
/// and the user starts the stack when they're ready.
pub enum Preflight {
    Ok,
    HostBinsMissing(Vec<MissingDep>),
    VmMissing,
    /// `defaults.workspace` in the AO yaml is missing or set to
    /// something other than `"worktree"`. `current` carries the
    /// observed value (or `None` when the key is absent) so the
    /// modal can show exactly what's wrong.
    WorkspaceUnsafe {
        current: Option<String>,
    },
    /// The running VM was created on an older fleet template whose
    /// `mounts:` block bind-mounted the whole host home writable.
    /// fleet's current model expects only `~/.agent-orchestrator/`
    /// writable + `~/.config/fleet/` read-only; anything else is
    /// treated as stale because it gives agents access to host
    /// dotfiles, SSH keys, GPG keyrings, etc. Modal sends the user
    /// at `limactl delete fleet-vm` + a return to fleet's bring-up
    /// flow; `~/.agent-orchestrator/` lives host-side already so
    /// worktrees survive the rebuild.
    VmMountsStale {
        /// The Lima yaml path we read, for inclusion in the modal
        /// body. `None` when we couldn't resolve `$HOME` at all
        /// (extreme edge case; the lookup falls back to the
        /// canonical `~/.lima/fleet-vm/lima.yaml` text).
        lima_yaml: Option<PathBuf>,
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
    check_with(
        default_has_bin,
        default_vm_status,
        default_in_vm,
        default_vm_mounts,
    )
}

/// Inner form with probe functions injected so tests can drive any
/// branch without spawning subprocesses.
fn check_with(
    has_bin: impl Fn(&str) -> bool,
    vm_status: impl FnOnce() -> VmStatus,
    in_vm: impl Fn(&str) -> bool,
    vm_mounts: impl FnOnce() -> VmMountsLayout,
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

    // Phase 2: VM existence. Missing means we need to provision from
    // the embedded template — that's a ~10-minute cloud-init flow with
    // a streaming progress modal, so it stays a startup gate.
    // *Stopped* falls through: Shift+S inside the TUI starts the VM
    // and AO together, no point making the user click through a modal
    // that does the same thing.
    let vm = vm_status();
    if matches!(vm, VmStatus::Missing) {
        return Preflight::VmMissing;
    }

    // Mount-layout drift. Only meaningful once Lima has resolved the
    // template into ~/.lima/fleet-vm/lima.yaml, which it does the first
    // time the VM starts — so we only probe when the VM is running.
    // Stopped VMs pass through and revalidate after Shift+S.
    if matches!(vm, VmStatus::Running) {
        let mounts = vm_mounts();
        if mounts.kind == VmMountsKind::Stale {
            return Preflight::VmMountsStale {
                lima_yaml: mounts.path,
            };
        }
    }

    // Materialize the worker AGENTS.md to its XDG path before any AO
    // call. Non-fatal: if the XDG dir can't be resolved or the write
    // fails, log and continue — workers may still receive rules via a
    // per-project override.
    if let Err(e) = crate::templates_sync::ensure_worker_agents_md() {
        tracing::warn!(error = ?e, "failed to materialize worker AGENTS.md");
    }

    // Phase 3 + 4: AO yaml-driven checks. Loading is best-effort — if
    // it doesn't parse / doesn't exist, we skip both checks; the
    // spawn flow will surface the underlying error later.
    let ao = crate::ao::config::AoConfig::load()
        .ok()
        .flatten()
        .map(|(_, cfg)| cfg);
    if let Some(cfg) = ao {
        // Phase 3: workspace isolation gate. Hard fail — fleet exists
        // to keep agents off the host checkout, so anything other
        // than `worktree` blocks the TUI until the user fixes it.
        // Reads the local yaml, so VM state doesn't matter.
        if !cfg.defaults.workspace_is_worktree() {
            return Preflight::WorkspaceUnsafe {
                current: cfg.defaults.workspace,
            };
        }
        // Phase 4: tracker tools. Probes the guest via `limactl shell
        // command -v <tool>`, so it can only run with a running VM.
        // When the VM is stopped at launch, fleet skips this check —
        // trackers live on the VM disk and survive `limactl stop`, so
        // missing them after a clean shutdown is rare. A user in that
        // edge case (e.g. VM created from a template predating
        // tracker provisioning) sees the failure via the spawn
        // picker's per-action error and can fix it via `fleet
        // vm-shell` or by restarting fleet.
        if matches!(vm, VmStatus::Running) {
            let warnings = check_trackers(&cfg, &has_bin, &in_vm);
            if !warnings.is_empty() {
                return Preflight::TrackerWarnings(warnings);
            }
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
    // Resolve the effective tracker per project. `tracker_plugin_for`
    // already handles both schemas — per-project `tracker:` block
    // (older) and top-level `plugins:` list (newer). Projects with no
    // recognised tracker silently drop off the install check.
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in cfg.projects.keys() {
        if let Some(plugin) = cfg.tracker_plugin_for(key) {
            grouped.entry(plugin).or_default().push(key.clone());
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
/// fleet-vm -- bash -c '<script>'` from the preflight modal's auto-
/// install action. Mirrors the provisioning steps in
/// `templates/fleet-vm.yaml` so an existing VM (created before fleet's
/// template included these) converges on the same state without a
/// rebuild.
///
/// `set -ex` at the top echoes each command before running it
/// (otherwise the user sees a silent pause during long downloads /
/// apt updates) and aborts on first failure. `curl -fSL` (no `s`)
/// shows the progress bar to a TTY.
///
/// Scripts run as the `lima` user — Lima 2.x's `limactl shell` has no
/// `--user` flag, so we use the default user and shell-out to `sudo`
/// for the system-scope steps (apt-install, /usr/local/bin writes).
/// That depends on the lima user having sudo, which Lima's cloud-init
/// grants by default and fleet's template does not currently revoke
/// (see [`templates/fleet-vm.yaml`](../../../templates/fleet-vm.yaml)
/// and docs/sandbox.md → "Sudo lockdown" for the deferred lockdown).
pub fn install_command(tool: &str) -> Option<&'static str> {
    match tool {
        // gh extension — needs `gh` already installed. The preflight
        // probes in declaration order (gh before gh-dash), so by the
        // time the install runs, gh is present (either pre-existed or
        // just installed). `gh extension install` writes to
        // $HOME/.local/share/gh/extensions and reads the lima user's
        // gh auth (forwarded via GH_TOKEN at fleet start time).
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

/// Result of inspecting the running VM's mount layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VmMountsLayout {
    pub kind: VmMountsKind,
    /// The Lima yaml path we read. Surfaced in the modal so the user
    /// can grep / inspect the offending file.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VmMountsKind {
    /// Mounts match the current template — `~/.agent-orchestrator/`
    /// writable and `~/.config/fleet/` read-only, or any forward-
    /// compatible superset of those. Passes preflight.
    Current,
    /// The host home itself is bind-mounted (the layout from before
    /// Phase 1's filesystem isolation). Hard fail.
    Stale,
    /// We couldn't determine the layout — yaml missing, parse error,
    /// `$HOME` unset, etc. Treat as `Current` to avoid false positives
    /// blocking startup. The spawn flow's other guards (worktree
    /// workspace, etc.) still apply.
    Unknown,
}

fn default_vm_mounts() -> VmMountsLayout {
    let Some(path) = lima_yaml_path() else {
        return VmMountsLayout {
            kind: VmMountsKind::Unknown,
            path: None,
        };
    };
    let kind =
        std::fs::read_to_string(&path).map_or(VmMountsKind::Unknown, |text| classify_mounts(&text));
    VmMountsLayout {
        kind,
        path: Some(path),
    }
}

/// Canonical location Lima writes the resolved instance yaml after
/// `limactl start --name fleet-vm <template>`. Returns `None` only
/// when `$HOME` is unset — the path itself isn't checked for
/// existence here; the read-stage handles the missing-file case.
fn lima_yaml_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".lima")
            .join(VM_NAME)
            .join("lima.yaml"),
    )
}

/// Inspect a Lima-resolved instance yaml and classify its `mounts:`
/// block. Pure function over the yaml text so tests can exercise both
/// outcomes without a real Lima install.
///
/// `Stale` ⇔ any mount whose `location:` is the host home itself
/// (i.e. ends with the literal `$HOME` value Lima resolved to, with
/// no trailing path component). `Current` is the residual.
fn classify_mounts(yaml_text: &str) -> VmMountsKind {
    let Ok(value): Result<serde_yml::Value, _> = serde_yml::from_str(yaml_text) else {
        return VmMountsKind::Unknown;
    };
    let Some(mounts) = value.get("mounts").and_then(serde_yml::Value::as_sequence) else {
        // No `mounts:` at all — definitely not the current layout,
        // but also not the stale-home-mount problem. Treat as Unknown
        // so we don't spuriously block startup on a VM whose template
        // skipped mounts entirely (unlikely but possible).
        return VmMountsKind::Unknown;
    };
    let Ok(home) = std::env::var("HOME") else {
        return VmMountsKind::Unknown;
    };
    let home_norm = home.trim_end_matches('/');
    for mount in mounts {
        let Some(loc) = mount.get("location").and_then(serde_yml::Value::as_str) else {
            continue;
        };
        let loc_norm = loc.trim_end_matches('/');
        if loc_norm == home_norm || loc_norm == "~" {
            return VmMountsKind::Stale;
        }
    }
    VmMountsKind::Current
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

    /// Convenience: the "current template" mounts layout. Used by
    /// almost every preflight test that doesn't specifically care
    /// about mount drift detection.
    fn current_mounts() -> VmMountsLayout {
        VmMountsLayout {
            kind: VmMountsKind::Current,
            path: None,
        }
    }

    fn stale_mounts() -> VmMountsLayout {
        VmMountsLayout {
            kind: VmMountsKind::Stale,
            path: Some(PathBuf::from("/fake/lima.yaml")),
        }
    }

    #[test]
    fn host_missing_short_circuits_vm_check() {
        let vm_status = || panic!("vm_status must not run when host check fails");
        let in_vm = |_: &str| panic!("in_vm must not run when host check fails");
        let mounts = || panic!("mounts probe must not run when host check fails");
        let result = check_with(|_| false, vm_status, in_vm, mounts);
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
            check_with(|_| true, || VmStatus::Running, |_| true, current_mounts)
        });
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn vm_missing_when_lima_reports_missing() {
        let result = check_with(
            |_| true,
            || VmStatus::Missing,
            |_| true,
            || panic!("mounts probe must not run when VM is missing"),
        );
        assert!(matches!(result, Preflight::VmMissing));
    }

    #[test]
    fn vm_stopped_falls_through_to_ok() {
        // VmStopped used to be a hard gate with its own modal; now
        // Shift+S inside the TUI handles starting VM + AO together,
        // so the preflight just lets the TUI launch and skips the
        // tracker check (which needs a running guest).
        let xdg = tmp_xdg();
        let result = with_isolated_xdg(&xdg, || {
            check_with(
                |_| true,
                || VmStatus::Stopped,
                |_| panic!("tracker probe must not run when VM is stopped"),
                || panic!("mounts probe must not run when VM is stopped"),
            )
        });
        assert!(matches!(result, Preflight::Ok));
    }

    #[test]
    fn vm_stopped_with_workspace_unsafe_still_blocks() {
        // Workspace check is local (reads the AO yaml), so it fires
        // even when the VM isn't running.
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
            check_with(
                |_| true,
                || VmStatus::Stopped,
                |_| true,
                || panic!("mounts probe must not run when VM is stopped"),
            )
        });
        assert!(matches!(result, Preflight::WorkspaceUnsafe { .. }));
    }

    #[test]
    fn vm_mounts_stale_blocks_before_workspace_check() {
        // Even when the AO yaml is fine, a VM with the stale
        // host-home mount layout has to be rebuilt first.
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
            check_with(|_| true, || VmStatus::Running, |_| true, stale_mounts)
        });
        match result {
            Preflight::VmMountsStale { lima_yaml } => {
                assert_eq!(lima_yaml, Some(PathBuf::from("/fake/lima.yaml")));
            }
            other => panic!("expected VmMountsStale, got {}", other_kind(&other)),
        }
    }

    #[test]
    fn classify_mounts_current_when_only_subdirs_mounted() {
        let yaml = r#"
mounts:
  - location: "~/.agent-orchestrator"
    writable: true
  - location: "~/.config/fleet"
    writable: false
"#;
        assert_eq!(classify_mounts(yaml), VmMountsKind::Current);
    }

    #[test]
    fn classify_mounts_stale_when_home_bind_mounted() {
        // Lima resolves `~` to the user's HOME before saving the
        // instance yaml, so test against the resolved form.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let yaml = format!("mounts:\n  - location: {home}\n    writable: true\n");
        assert_eq!(classify_mounts(&yaml), VmMountsKind::Stale);
    }

    #[test]
    fn classify_mounts_stale_when_tilde_bind_mounted() {
        // Defensive: a yaml that hasn't been resolved yet (just-loaded
        // template, no fleet-vm install yet) still has `~`. We treat
        // the literal tilde as a home mount too.
        let yaml = r#"
mounts:
  - location: "~"
    writable: true
"#;
        assert_eq!(classify_mounts(yaml), VmMountsKind::Stale);
    }

    #[test]
    fn classify_mounts_unknown_when_no_mounts_block() {
        let yaml = "cpus: 4\nmemory: 4GiB\n";
        assert_eq!(classify_mounts(yaml), VmMountsKind::Unknown);
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
            check_with(
                |_| true,
                || VmStatus::Running,
                |tool| tool != "git-bug",
                current_mounts,
            )
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
                current_mounts,
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
    fn install_command_uses_sudo_for_system_scope_steps() {
        // The system-scope installs (apt-install, /usr/local/bin
        // writes) shell out to sudo since Lima 2.x's `limactl shell`
        // can't pick a user. If a future Lima exposes `--user root`
        // and we drop the lima user from sudo, this assertion is
        // what we'll have to invert.
        let s = install_command("git-bug").expect("git-bug script");
        assert!(s.contains("sudo "), "expected sudo in: {s}");
        let s = install_command("gh").expect("gh script");
        assert!(s.contains("sudo "), "expected sudo in: {s}");
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
            check_with(|_| true, || VmStatus::Running, |_| true, current_mounts)
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
            check_with(|_| true, || VmStatus::Running, |_| false, current_mounts)
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
            Preflight::WorkspaceUnsafe { .. } => "WorkspaceUnsafe",
            Preflight::VmMountsStale { .. } => "VmMountsStale",
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
            check_with(|_| true, || VmStatus::Running, |_| true, current_mounts)
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
            check_with(|_| true, || VmStatus::Running, |_| true, current_mounts)
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
            check_with(|_| true, || VmStatus::Running, |_| true, current_mounts)
        });
        assert!(matches!(result, Preflight::Ok));
    }
}
