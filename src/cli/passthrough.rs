//! Generic passthrough to `ao` inside the VM. Used for `status`, `doctor`,
//! `session …`, `plugin …`, `stop`. No secret needed.

use anyhow::{Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::{RealProcessInvoker, run_interactive};

/// Pass `ao <args…>` to AO inside the VM.
pub fn run(repo_root: &Path, ao_args: &[String]) -> Result<i32> {
    let invoker = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    match lima.status() {
        VmStatus::Running => {}
        VmStatus::Stopped => bail!("Lima VM `{}` is stopped.", lima.vm_name()),
        VmStatus::Missing => bail!("Lima VM `{}` not found.", lima.vm_name()),
    }
    let mut argv = vec![
        "shell".to_string(),
        "--workdir".to_string(),
        repo_root.display().to_string(),
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
