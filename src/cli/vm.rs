//! Lima VM lifecycle: `vm-shell`, `vm-status`, `vm-start`, `vm-stop`.
//! Distinct from AO lifecycle (`fleet start`/`stop`).

use anyhow::Result;
use std::path::Path;

use crate::process::run_interactive;

const VM_NAME: &str = "fleet-vm";

pub fn run_shell(repo_root: &Path) -> Result<i32> {
    run_interactive(
        "limactl",
        &[
            "shell".to_string(),
            "--workdir".to_string(),
            repo_root.display().to_string(),
            VM_NAME.to_string(),
        ],
        &[],
        &[],
    )
}

pub fn run_status() -> Result<i32> {
    run_interactive(
        "limactl",
        &["list".to_string(), VM_NAME.to_string()],
        &[],
        &[],
    )
}

pub fn run_start() -> Result<i32> {
    run_interactive(
        "limactl",
        &["start".to_string(), VM_NAME.to_string()],
        &[],
        &[],
    )
}

pub fn run_stop() -> Result<i32> {
    run_interactive(
        "limactl",
        &["stop".to_string(), VM_NAME.to_string()],
        &[],
        &[],
    )
}
