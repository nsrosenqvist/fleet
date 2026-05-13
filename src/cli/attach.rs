//! `fleet attach <session>` — drop the user into a session's tmux pane.
//!
//! Bypasses AO's terminal plugin (which can default to opening a web URL or
//! mishandle TTY when invoked through pipelines that capture stdio). We
//! `tmux attach` directly through `limactl shell`, inheriting the calling
//! shell's TTY.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::lima::{Lima, VmStatus};
use crate::process::{RealProcessInvoker, run_interactive};

pub fn run(_repo_root: &Path, session: &str) -> Result<i32> {
    let invoker = Arc::new(RealProcessInvoker);
    let lima = Lima::new(invoker, "fleet-vm");
    match lima.status() {
        VmStatus::Running => {}
        VmStatus::Stopped => bail!("Lima VM `{}` is stopped.", lima.vm_name()),
        VmStatus::Missing => bail!("Lima VM `{}` not found.", lima.vm_name()),
    }
    // tmux addresses sessions through its socket, so workdir doesn't
    // change the attach result — but we run from the canonical AO
    // config dir for consistency with every other fleet `limactl
    // shell` call and to avoid a path that might not exist in the VM.
    let workdir = crate::ao::config::AoConfig::workdir()
        .context("no $HOME / $XDG_CONFIG_HOME — can't resolve AO workdir")?;
    let argv = vec![
        "shell".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        lima.vm_name().to_string(),
        "tmux".to_string(),
        "attach".to_string(),
        "-t".to_string(),
        session.to_string(),
    ];
    // The VM doesn't carry terminfo for newer host terminals (xterm-ghostty,
    // wezterm, etc.); tmux refuses to attach. Force xterm-256color for the
    // limactl shell so the in-VM tmux client succeeds. Minor rendering
    // capability loss vs the host TERM (acceptable).
    run_interactive("limactl", &argv, &[("TERM", "xterm-256color")], &[])
}
