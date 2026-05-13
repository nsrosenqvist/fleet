//! Generic passthrough to `ao` inside the VM. Used for `status`, `doctor`,
//! `session …`, `plugin …`, `stop`. No secret needed.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::{RealProcessInvoker, run_interactive};

/// Pass `ao <args…>` to AO inside the VM. AO discovers its config from
/// cwd only, so we always run from the canonical XDG dir — the same
/// place fleet's spawn / status / refresh paths use — so the user
/// gets the registered projects regardless of which directory fleet
/// was launched from.
pub fn run(_repo_root: &Path, ao_args: &[String]) -> Result<i32> {
    let invoker = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    match lima.status() {
        VmStatus::Running => {}
        VmStatus::Stopped => bail!("Lima VM `{}` is stopped.", lima.vm_name()),
        VmStatus::Missing => bail!("Lima VM `{}` not found.", lima.vm_name()),
    }
    let workdir = crate::ao::config::AoConfig::workdir()
        .context("no $HOME / $XDG_CONFIG_HOME — can't resolve AO workdir")?;
    let mut argv = vec![
        "shell".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        lima.vm_name().to_string(),
        "ao".to_string(),
    ];
    argv.extend(ao_args.iter().cloned());
    run_interactive("limactl", &argv, &[], &[])
}

/// Pass `ao <prefix> <args…>` (e.g. `ao session ls`).
pub fn run_with_prefix(repo_root: &Path, prefix: &str, args: &[String]) -> Result<i32> {
    let mut full = vec![prefix.to_string()];
    full.extend(args.iter().cloned());
    run(repo_root, &full)
}
