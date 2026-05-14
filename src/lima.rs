//! Lima VM helpers. Wraps `limactl` subprocess invocations so we can:
//!
//! - Probe whether the `fleet-vm` VM is running.
//! - Run arbitrary commands inside the VM via `limactl shell`.
//!
//! All subprocess calls go through [`ProcessInvoker`], so tests can drive
//! these helpers without a real Lima install.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::process::ProcessInvoker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    Running,
    Stopped,
    Missing,
}

pub struct Lima {
    invoker: Arc<dyn ProcessInvoker>,
    vm_name: String,
}

impl Lima {
    pub fn new(invoker: Arc<dyn ProcessInvoker>, vm_name: impl Into<String>) -> Self {
        Self {
            invoker,
            vm_name: vm_name.into(),
        }
    }

    pub fn vm_name(&self) -> &str {
        &self.vm_name
    }

    /// Probe the VM's current status. Returns `Missing` if `limactl list`
    /// returns nothing for this name; `Running` only if status is exactly
    /// `"Running"`; otherwise `Stopped`.
    pub fn status(&self) -> VmStatus {
        let out = self.invoker.run(
            "limactl",
            vec![
                "list".to_string(),
                self.vm_name.clone(),
                "--format".to_string(),
                "{{.Status}}".to_string(),
            ],
        );
        match out {
            Ok(s) if s.trim() == "Running" => VmStatus::Running,
            Ok(s) if s.trim().is_empty() => VmStatus::Missing,
            Ok(_) => VmStatus::Stopped,
            Err(_) => VmStatus::Missing,
        }
    }

    /// Run a command inside the VM via `limactl shell --workdir <wd> <vm> <argv...>`.
    /// `argv` is the in-VM command (e.g. `["ao", "status", "--json"]`).
    ///
    /// The command runs as the `lima` user (Lima's default). For
    /// commands that need to touch AO state, tmux sessions, or
    /// anything else under `aoworker`'s $HOME, see
    /// [`Self::shell_as_aoworker`].
    pub fn shell(&self, workdir: &Path, mut argv: Vec<String>) -> Result<String> {
        let mut cmd = vec![
            "shell".to_string(),
            "--workdir".to_string(),
            workdir.display().to_string(),
            self.vm_name.clone(),
        ];
        cmd.append(&mut argv);
        self.invoker.run("limactl", cmd)
    }

    /// Run a command inside the VM as the `aoworker` user. Goes
    /// through `sudo -u aoworker --preserve-env=TERM` — the sudoers
    /// drop-in provisioned by `templates/fleet-vm.yaml` allows
    /// `lima ALL=(aoworker) NOPASSWD: /bin/bash, /usr/bin/tmux` only,
    /// so `argv[0]` must be one of those (or a path-resolved alias
    /// that's still authorized — fleet's callers stick to the bare
    /// names).
    ///
    /// Use for: tmux helpers, `ao` CLI calls, and any node/bash that
    /// reads `~/.agent-orchestrator/projects/…` (since the bind
    /// mount lands at aoworker's $HOME, lima sees `/home/aoworker/…`
    /// but its own $HOME doesn't contain `.agent-orchestrator`).
    pub fn shell_as_aoworker(&self, workdir: &Path, argv: Vec<String>) -> Result<String> {
        let mut wrapped = vec![
            "shell".to_string(),
            "--workdir".to_string(),
            workdir.display().to_string(),
            self.vm_name.clone(),
            "sudo".to_string(),
            "-u".to_string(),
            "aoworker".to_string(),
            "--preserve-env=TERM".to_string(),
        ];
        wrapped.extend(argv);
        self.invoker.run("limactl", wrapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::eq;

    fn argv(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn status_running_when_limactl_returns_running() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("limactl"),
                eq(argv(&["list", "fleet-vm", "--format", "{{.Status}}"])),
            )
            .returning(|_, _| Ok("Running".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        assert_eq!(lima.status(), VmStatus::Running);
    }

    #[test]
    fn status_stopped_when_other_value() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok("Stopped".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        assert_eq!(lima.status(), VmStatus::Stopped);
    }

    #[test]
    fn status_missing_when_limactl_errors() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| anyhow::bail!("no such instance"));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        assert_eq!(lima.status(), VmStatus::Missing);
    }

    #[test]
    fn shell_assembles_workdir_and_argv() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("limactl"),
                eq(argv(&[
                    "shell",
                    "--workdir",
                    "/tmp/repo",
                    "fleet-vm",
                    "ao",
                    "status",
                ])),
            )
            .returning(|_, _| Ok("ok".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        let out = lima
            .shell(
                Path::new("/tmp/repo"),
                vec!["ao".to_string(), "status".to_string()],
            )
            .expect("ok");
        assert_eq!(out, "ok");
    }
}
