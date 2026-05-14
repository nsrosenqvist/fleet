//! One-time migration for existing `fleet-vm` instances that predate
//! the idempotent-provisioning fix in `templates/fleet-vm.yaml`.
//!
//! Background: Lima re-runs every `provision:` script on every boot,
//! and the original template's system-mode block did the full apt +
//! Node 20 setup + `npm install -g @aoagents/ao @anthropic-ai/claude-code`
//! unconditionally. The new template short-circuits via a marker file
//! (`/var/lib/fleet/.system-provisioned-vN`), but template edits only
//! affect *new* VMs — Lima writes the resolved config to
//! `~/.lima/fleet-vm/lima.yaml` once at create-time and uses that
//! cached copy on every subsequent start. So existing users would
//! never see the idempotency fix unless they recreate the VM (10-min
//! cloud-init).
//!
//! What this module does, on every TUI launch:
//!
//! 1. Reads `~/.lima/fleet-vm/lima.yaml`. Bails silently if the file
//!    is missing (no VM yet — preflight handles that path).
//! 2. Skips if the marker pattern is already present (already migrated
//!    or new VM created from the patched template).
//! 3. Otherwise, injects the marker-file gate at the top of the
//!    system-mode script. Conservative match — only patches when the
//!    script looks like the shipped pattern; bails on heavily
//!    customized configs.
//! 4. Best-effort: if the VM is currently Running and `ao` is on PATH,
//!    drops the marker file inside the VM via `limactl shell` so the
//!    *next* boot skips re-provisioning immediately (rather than
//!    re-running the install once more and only being fast on the
//!    boot after that).
//!
//! Errors are logged but never block TUI startup — the worst case is
//! that the migration didn't take and the user keeps paying the
//! re-provisioning cost.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Marker substring used to detect "already migrated". Stays stable
/// across `-vN` version bumps in the template so the migration only
/// fires once per VM regardless of which marker generation the
/// template currently writes.
const ALREADY_MIGRATED_NEEDLE: &str = "PROVISION_MARKER=";

/// Exact pattern we match in the cached lima.yaml. Conservative on
/// purpose — anyone who hand-edited their provision script will fall
/// off this match and skip migration rather than getting a corrupted
/// rewrite.
const ORIGINAL_SCRIPT_HEAD: &str = "      set -eux\n\n      export DEBIAN_FRONTEND=";

/// What we inject between `set -eux` and the rest of the install body.
/// Indented to match the YAML block scalar's content column (6 spaces).
const MARKER_INJECTION: &str = "      set -eux

      # fleet auto-migration: idempotency gate added retroactively
      # so this script doesn't re-run apt + npm install on every VM
      # boot (Lima's per-boot provision re-run model). See
      # tui::vm_migrate for the migration logic; bump the marker
      # suffix in templates/fleet-vm.yaml to force a re-run.
      PROVISION_MARKER=\"/var/lib/fleet/.system-provisioned-v1\"
      if [ -f \"$PROVISION_MARKER\" ]; then
          exit 0
      fi

      export DEBIAN_FRONTEND=";

/// Run the migration. Idempotent; cheap when nothing needs doing.
/// Errors are logged at `warn` and swallowed.
pub(super) fn run() {
    let Some(path) = lima_yaml_path() else {
        return;
    };
    if !path.is_file() {
        return;
    }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = ?e, path = %path.display(), "vm-migrate: read failed");
            return;
        }
    };
    if content.contains(ALREADY_MIGRATED_NEEDLE) {
        return;
    }
    if !content.contains(ORIGINAL_SCRIPT_HEAD) {
        // Customized config or an unfamiliar template — don't risk
        // rewriting blindly. The user can recreate the VM if they
        // want the new idempotent provisioning.
        tracing::info!(
            path = %path.display(),
            "vm-migrate: cached lima.yaml doesn't match the shipped pattern; \
             skipping (recreate the VM to pick up the new template)"
        );
        return;
    }
    let patched = content.replacen(ORIGINAL_SCRIPT_HEAD, MARKER_INJECTION, 1);
    if let Err(e) = fs::write(&path, patched) {
        tracing::warn!(error = ?e, path = %path.display(), "vm-migrate: write failed");
        return;
    }
    tracing::info!(path = %path.display(), "vm-migrate: patched cached lima.yaml with idempotency marker");

    drop_marker_if_provisioned();
}

/// Best-effort: drop the marker file inside the VM so the next boot
/// short-circuits immediately. Skips silently if the VM is stopped
/// (next boot still runs the patched script, which will run the
/// install once then create the marker itself) or if `ao` isn't on
/// PATH (means provisioning hasn't actually succeeded yet — let it
/// run on the next boot).
fn drop_marker_if_provisioned() {
    let ao_present = run_cmd(
        "limactl",
        &[
            "shell",
            "--workdir",
            "/tmp",
            "fleet-vm",
            "command",
            "-v",
            "ao",
        ],
    )
    .is_ok_and(|out| !out.trim().is_empty());
    if !ao_present {
        tracing::info!(
            "vm-migrate: VM stopped or ao not on PATH yet — letting the next boot's \
             provision script create the marker itself"
        );
        return;
    }
    let result = run_cmd(
        "limactl",
        &[
            "shell",
            "--workdir",
            "/tmp",
            "fleet-vm",
            "sudo",
            "bash",
            "-c",
            "mkdir -p /var/lib/fleet && touch /var/lib/fleet/.system-provisioned-v1",
        ],
    );
    match result {
        Ok(_) => tracing::info!("vm-migrate: dropped provisioned marker inside VM"),
        Err(e) => tracing::warn!(error = ?e, "vm-migrate: marker drop failed"),
    }
}

fn lima_yaml_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".lima/fleet-vm/lima.yaml"))
}

/// Capture-output subprocess helper local to this module. We don't
/// reuse `crate::process::ProcessInvoker` because this runs before
/// the App is constructed and the trait machinery isn't worth pulling
/// in for two one-shot calls.
fn run_cmd(program: &str, args: &[&str]) -> std::io::Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "{program} {} exited {:?}: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL_SCRIPT: &str = r"provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux

      export DEBIAN_FRONTEND=noninteractive
      apt-get update
      apt-get install -y nodejs
";

    #[test]
    fn original_script_matches_the_needle() {
        // Sanity: the constant we use to detect the original shape
        // actually appears in a representative original script.
        assert!(ORIGINAL_SCRIPT.contains(ORIGINAL_SCRIPT_HEAD));
    }

    #[test]
    fn patched_content_contains_marker_check() {
        let patched = ORIGINAL_SCRIPT.replacen(ORIGINAL_SCRIPT_HEAD, MARKER_INJECTION, 1);
        assert!(patched.contains(ALREADY_MIGRATED_NEEDLE));
        assert!(patched.contains("set -eux"));
        assert!(patched.contains("export DEBIAN_FRONTEND="));
        assert!(patched.contains("exit 0"));
    }

    #[test]
    fn already_migrated_check_short_circuits_replacen() {
        // If the marker is already in the file, we never run replacen.
        // Asserting the run() flow directly is tricky without I/O, so
        // verify the predicate the function relies on.
        let patched = ORIGINAL_SCRIPT.replacen(ORIGINAL_SCRIPT_HEAD, MARKER_INJECTION, 1);
        assert!(patched.contains(ALREADY_MIGRATED_NEEDLE));
    }

    #[test]
    fn customized_script_falls_off_the_pattern() {
        // If a user edited the provision script (e.g. changed
        // `apt update` to `apt-get update -y`), the head pattern
        // won't match and we leave the file alone.
        let customized = ORIGINAL_SCRIPT.replace("set -eux", "set -euxo pipefail");
        assert!(!customized.contains(ORIGINAL_SCRIPT_HEAD));
    }
}
