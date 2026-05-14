//! Streaming subprocess wrapper for `limactl start --tty=false fleet-vm`.
//!
//! Running the bring-up suspended (the previous shape) worked but dropped
//! the user from the TUI into ~10 minutes of scrolling cloud-init output.
//! Worse, the interactive template picker stole keystrokes from anyone
//! who tried to move on, so a typed-too-soon arrow key could land on the
//! wrong template choice.
//!
//! This module replaces that with a piped-child + line-buffered reader
//! pattern: limactl runs non-interactively (`--tty=false` accepts the
//! default template silently); stdout and stderr drain into a bounded
//! ring buffer; the TUI redraws a modal each tick that shows the latest
//! status. When the child exits the modal closes and `settle_preflight`
//! re-runs to confirm the VM is actually up.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::preflight::VM_NAME;

/// Lima template embedded at compile time. Source of truth lives at
/// `templates/fleet-vm.yaml`; `include_str!` lets cargo track changes
/// and rebuilds the binary when the yaml is edited.
const FLEET_VM_TEMPLATE: &str = include_str!("../../templates/fleet-vm.yaml");

/// Whether the bring-up needs to create a new instance from the
/// template, or just start an existing stopped instance. Determined by
/// `preflight::check` upstream — keeps the subprocess args here
/// straightforward instead of inspecting Lima state again.
#[derive(Clone, Copy)]
pub(super) enum BringUpMode {
    Create,
    StartExisting,
}

/// How many tail lines we keep for the modal. 12 fits comfortably in a
/// modal sized to typical terminals while still showing enough scrollback
/// to read the most recent cloud-init step.
const TAIL_BUDGET: usize = 12;

/// One progress event from the child reader thread.
enum BringUpEvent {
    Line(String),
    Exited(Result<i32>),
}

/// Handle to an in-flight VM bring-up. Owns the child + its reader
/// threads via a single `JoinHandle`; the public surface is intentionally
/// small (tick + done + tail) so the renderer stays decoupled from the
/// subprocess plumbing.
pub(super) struct BringUp {
    rx: Receiver<BringUpEvent>,
    join: Option<JoinHandle<()>>,
    tail: VecDeque<String>,
    started_at: Instant,
    /// Final exit status — `None` while still running. `Some(Ok(code))`
    /// for a clean exit (regardless of code; non-zero is still "finished",
    /// just unsuccessful); `Some(Err)` for a spawn/wait failure.
    finished: Option<Result<i32>>,
}

impl BringUp {
    /// Spawn the child and wire up the reader threads. Caller drives the
    /// modal via [`Self::tick`] + [`Self::is_finished`].
    ///
    /// In [`BringUpMode::Create`], the embedded template is written to a
    /// tempfile and passed as `limactl start --name=fleet-vm <yaml>` so
    /// Lima provisions the instance with Node + ao + claude-code on first
    /// boot. Lima copies the resolved config into `~/.lima/fleet-vm/`
    /// during create, so the tempfile is only needed for that one call —
    /// the supervisor thread deletes it after the child exits.
    ///
    /// In [`BringUpMode::StartExisting`], we just `limactl start
    /// fleet-vm`. Lima reads the existing on-disk config.
    pub(super) fn spawn(mode: BringUpMode) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        // Lima refuses to start a VM whose `mounts:` reference a host
        // path that doesn't exist. fleet's template mounts
        // `~/.agent-orchestrator/` (where AO writes worktrees), so we
        // make sure it exists on the host before either Create or
        // StartExisting hands off to `limactl start`. Same idea for
        // `~/.config/fleet/` though it always exists by the time we
        // get here (the AO yaml is in there).
        ensure_host_mount_targets()?;

        // `--tty=false` accepts default prompts silently. Without it,
        // Lima asks "Proceed / Open editor / Choose template" on create
        // — interactive and the wrong shape for an embedded bring-up.
        //
        // `--timeout=30m` raises Lima's outer probe-and-wait ceiling
        // (default 10 min) so the `cloud-init status --wait` probe in
        // templates/fleet-vm.yaml has enough headroom: cloud-init does
        // `npm install -g @aoagents/ao @anthropic-ai/claude-code` plus
        // apt + NodeSource + git-bug curl, which routinely takes
        // 12-15 min on a constrained host before ao lands on PATH.
        let mut args: Vec<String> =
            vec!["start".into(), "--tty=false".into(), "--timeout=30m".into()];
        let template_tmp: Option<PathBuf> = match mode {
            BringUpMode::Create => {
                let path = write_template_tmpfile()?;
                args.push(format!("--name={VM_NAME}"));
                args.push(path.display().to_string());
                Some(path)
            }
            BringUpMode::StartExisting => {
                args.push(VM_NAME.into());
                None
            }
        };

        let mut child = Command::new("limactl")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn `limactl {}`", args.join(" ")))?;
        let stdout = child.stdout.take().context("limactl child has no stdout")?;
        let stderr = child.stderr.take().context("limactl child has no stderr")?;

        let join = thread::spawn(move || drive_child(child, stdout, stderr, &tx, template_tmp));

        Ok(Self {
            rx,
            join: Some(join),
            tail: VecDeque::with_capacity(TAIL_BUDGET),
            started_at: Instant::now(),
            finished: None,
        })
    }

    /// Drain queued events. Call between draws — non-blocking.
    pub(super) fn tick(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(BringUpEvent::Line(line)) => self.push_tail(line),
                Ok(BringUpEvent::Exited(res)) => {
                    self.finished = Some(res);
                    // Reap the supervisor thread so its handle doesn't
                    // dangle past the modal — it's already done, this is
                    // just a tidy `join`.
                    if let Some(join) = self.join.take() {
                        let _ = join.join();
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    /// Last N output lines, oldest first. Used by the renderer.
    pub(super) fn tail_lines(&self) -> impl Iterator<Item = &str> {
        self.tail.iter().map(String::as_str)
    }

    pub(super) fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Final exit status. Caller should only consult this once
    /// [`Self::is_finished`] is true; returns `None` while running.
    pub(super) fn outcome(&self) -> Option<&Result<i32>> {
        self.finished.as_ref()
    }

    fn push_tail(&mut self, line: String) {
        if self.tail.len() == TAIL_BUDGET {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }
}

/// Supervisor thread: spawn per-stream reader threads, wait for the
/// child, then send the final `Exited` event. Owns the `Child` so a drop
/// at TUI shutdown reliably reaps the subprocess. If a `template_tmp`
/// path was passed it gets unlinked after the child exits — Lima has
/// already copied the resolved config into `~/.lima/fleet-vm/` by then.
fn drive_child(
    mut child: Child,
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
    tx: &Sender<BringUpEvent>,
    template_tmp: Option<PathBuf>,
) {
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();
    let h_out = thread::spawn(move || forward_lines(stdout, &tx_stdout));
    let h_err = thread::spawn(move || forward_lines(stderr, &tx_stderr));

    let result: Result<i32> = match child.wait() {
        Ok(status) => Ok(status.code().unwrap_or(-1)),
        Err(e) => Err(anyhow::anyhow!("wait failed: {e}")),
    };

    // Reader threads finish naturally when the pipes close on child exit.
    let _ = h_out.join();
    let _ = h_err.join();

    if let Some(path) = template_tmp {
        let _ = std::fs::remove_file(path);
    }

    let _ = tx.send(BringUpEvent::Exited(result));
}

/// Write the embedded template to a unique tempfile under the system
/// temp dir, with project-source mounts auto-derived from the AO
/// config spliced into the `mounts:` block. Including the pid avoids
/// collisions if (for some reason) two fleet processes try to bring
/// the VM up at the same time.
///
/// The splice replaces the `# __FLEET_EXTRA_MOUNTS__ … __FLEET_EXTRA_MOUNTS_END__`
/// marker block in the embedded template with one yaml entry per
/// project/plugin source path. When no extra mounts are derived the
/// marker is replaced with an empty line, so the resulting yaml has
/// only the two base mounts.
fn write_template_tmpfile() -> Result<PathBuf> {
    let extras = derive_extra_mounts();
    let rendered = compose_template(FLEET_VM_TEMPLATE, &extras);
    let path = std::env::temp_dir().join(format!("fleet-vm-{}.yaml", std::process::id()));
    std::fs::write(&path, rendered)
        .with_context(|| format!("write lima template to {}", path.display()))?;
    Ok(path)
}

/// A host path that needs to be visible inside the VM (with the same
/// absolute path on both sides) so AO can `git worktree add` against
/// the host repo and load local plugins. Auto-derived from the AO
/// catalog by [`derive_extra_mounts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExtraMount {
    pub path: PathBuf,
    pub writable: bool,
}

/// Read the AO catalog (`~/.config/fleet/agent-orchestrator.yaml`)
/// and collect:
///
/// - Every `projects.*.path` — AO needs `git worktree add` against
///   the host repo's `.git/`, which only works if the repo is
///   visible inside the VM. Writable, since `git worktree add`
///   creates worktree admin files in `.git/worktrees/`.
/// - Every `plugins[].path` whose `source: local` — AO loads local
///   plugin packages directly from the host source tree (e.g.
///   `packages/tracker-git-bug/`). Read-only is sufficient; AO
///   doesn't write into plugin source.
///
/// Returns an empty `Vec` when the AO config doesn't exist or
/// doesn't parse — fleet's other code paths will surface that as
/// the underlying error.
fn derive_extra_mounts() -> Vec<ExtraMount> {
    let Ok(Some((_, ao))) = crate::ao::config::AoConfig::load() else {
        return Vec::new();
    };
    let mut out: Vec<ExtraMount> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for project in ao.projects.values() {
        if seen.insert(project.path.clone()) {
            out.push(ExtraMount {
                path: project.path.clone(),
                writable: true,
            });
        }
    }
    for plugin in &ao.plugins {
        let Some(source) = plugin
            .extra
            .get("source")
            .and_then(serde_yml::Value::as_str)
        else {
            continue;
        };
        if source != "local" {
            continue;
        }
        let Some(path_str) = plugin.extra.get("path").and_then(serde_yml::Value::as_str) else {
            continue;
        };
        let path = PathBuf::from(path_str);
        if seen.insert(path.clone()) {
            out.push(ExtraMount {
                path,
                writable: false,
            });
        }
    }
    out
}

/// Splice the rendered yaml fragment between
/// `# __FLEET_EXTRA_MOUNTS__` and `# __FLEET_EXTRA_MOUNTS_END__` in
/// the embedded template. Pure string operation so tests can exercise
/// the rendering without involving the filesystem.
pub(super) fn compose_template(template: &str, extras: &[ExtraMount]) -> String {
    let fragment = render_mounts_fragment(extras);
    let mut out = String::with_capacity(template.len() + fragment.len());
    let begin_marker = "# __FLEET_EXTRA_MOUNTS__";
    let end_marker = "# __FLEET_EXTRA_MOUNTS_END__";
    if let Some(begin) = template.find(begin_marker)
        && let Some(end_rel) = template[begin..].find(end_marker)
    {
        let begin_line_start = template[..begin].rfind('\n').map_or(0, |n| n + 1);
        let after_end = begin + end_rel + end_marker.len();
        let after_end_line = template[after_end..]
            .find('\n')
            .map_or(template.len(), |n| after_end + n + 1);
        out.push_str(&template[..begin_line_start]);
        out.push_str(&fragment);
        out.push_str(&template[after_end_line..]);
        return out;
    }
    // No marker present (someone edited the template) — return the
    // original verbatim. Fleet still boots with the two base mounts;
    // project worktrees will fail until the user re-introduces the
    // marker or hand-edits the saved lima.yaml.
    template.to_string()
}

fn render_mounts_fragment(extras: &[ExtraMount]) -> String {
    use std::fmt::Write;
    if extras.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for m in extras {
        let _ = writeln!(
            out,
            "  - location: \"{path}\"\n    writable: {writable}",
            path = m.path.display(),
            writable = m.writable
        );
    }
    out
}

/// Make sure the host directories fleet's Lima template bind-mounts
/// actually exist before `limactl start` runs. Lima refuses to start
/// otherwise — mount targets are validated up front. Covers both the
/// base mounts (`~/.agent-orchestrator`, `~/.config/fleet`) and every
/// project-source path derived by [`derive_extra_mounts`]. The
/// project paths must already exist (you can't bring up a worktree
/// against a repo that doesn't exist), so absence there is a hard
/// error rather than `mkdir -p`.
fn ensure_host_mount_targets() -> Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        anyhow::bail!("$HOME is not set; cannot resolve bind-mount targets for fleet-vm");
    };
    let ao_dir = home.join(".agent-orchestrator");
    std::fs::create_dir_all(&ao_dir)
        .with_context(|| format!("mkdir {} for AO worktrees", ao_dir.display()))?;
    let fleet_cfg = home.join(".config").join("fleet");
    std::fs::create_dir_all(&fleet_cfg)
        .with_context(|| format!("mkdir {} for fleet config mount", fleet_cfg.display()))?;
    for extra in derive_extra_mounts() {
        if !extra.path.is_dir() {
            anyhow::bail!(
                "project / plugin path `{}` does not exist on the host. \
                 agent-orchestrator.yaml lists it but Lima can't bind-mount \
                 a missing path. Either create the directory or remove the \
                 entry from agent-orchestrator.yaml.",
                extra.path.display()
            );
        }
    }
    Ok(())
}

/// Read `reader` line-by-line, push each into the channel. Silent on
/// IO error — the supervisor already knows the child exited (or will),
/// and one log line lost is less noise than an error-line on top of the
/// genuine output.
fn forward_lines(reader: impl Read, tx: &Sender<BringUpEvent>) {
    let buf = BufReader::new(reader);
    for line in buf.lines().map_while(Result::ok) {
        if tx.send(BringUpEvent::Line(line)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TEMPLATE: &str = r#"# header
mounts:
  - location: "~/.agent-orchestrator"
    mountPoint: "/home/aoworker/.agent-orchestrator"
    writable: true
  - location: "~/.config/fleet"
    writable: false
  # __FLEET_EXTRA_MOUNTS__ — extras
  # __FLEET_EXTRA_MOUNTS_END__
provision: []
"#;

    #[test]
    fn compose_template_with_no_extras_drops_marker_lines() {
        let out = compose_template(SAMPLE_TEMPLATE, &[]);
        assert!(!out.contains("__FLEET_EXTRA_MOUNTS__"));
        assert!(!out.contains("__FLEET_EXTRA_MOUNTS_END__"));
        // Base mounts survive.
        assert!(out.contains("~/.agent-orchestrator"));
        assert!(out.contains("~/.config/fleet"));
        // No extras appended after the markers were removed.
        assert!(
            !out.contains("- location: \"/home/"),
            "no project mount should be present: {out}"
        );
    }

    #[test]
    fn compose_template_inserts_one_entry_per_extra() {
        let extras = vec![
            ExtraMount {
                path: PathBuf::from("/home/niklas/Code/fleet"),
                writable: true,
            },
            ExtraMount {
                path: PathBuf::from("/home/niklas/Code/fleet/packages/tracker-git-bug"),
                writable: false,
            },
        ];
        let out = compose_template(SAMPLE_TEMPLATE, &extras);
        assert!(out.contains("- location: \"/home/niklas/Code/fleet\""));
        assert!(out.contains("    writable: true"));
        assert!(out.contains("- location: \"/home/niklas/Code/fleet/packages/tracker-git-bug\""));
        assert!(out.contains("    writable: false"));
        // The output is still valid YAML structurally — `provision:`
        // line comes after the mounts, on its own line.
        assert!(out.contains("\nprovision: []\n"));
    }

    #[test]
    fn compose_template_without_marker_returns_template_verbatim() {
        let template = "mounts:\n  - location: a\nprovision: []\n";
        let extras = vec![ExtraMount {
            path: PathBuf::from("/foo"),
            writable: true,
        }];
        let out = compose_template(template, &extras);
        assert_eq!(out, template);
    }
}
