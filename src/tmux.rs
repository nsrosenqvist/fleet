//! Tmux helpers — list sessions, capture pane output. All run via [`Lima`]
//! against the VM, since tmux lives inside the VM.

use anyhow::Result;
use std::path::Path;

use crate::lima::Lima;

/// `tmux list-sessions -F '#{session_name}'` inside the VM, parsed into a
/// vector of session names.
pub fn list_sessions(lima: &Lima, workdir: &Path) -> Result<Vec<String>> {
    let raw = lima.shell(
        workdir,
        vec![
            "tmux".to_string(),
            "list-sessions".to_string(),
            "-F".to_string(),
            "#{session_name}".to_string(),
        ],
    )?;
    Ok(raw
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `tmux capture-pane -p -t <target> -S -<max_lines>` inside the VM.
///
/// Returns the full pane content (most recent `max_lines` lines). The caller
/// decides what to do with it (the TUI renders the last N visible lines).
pub fn capture_pane(lima: &Lima, workdir: &Path, target: &str, max_lines: u32) -> Result<String> {
    lima.shell(
        workdir,
        vec![
            "tmux".to_string(),
            "capture-pane".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            target.to_string(),
            "-S".to_string(),
            format!("-{max_lines}"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::{always, eq};
    use std::sync::Arc;

    #[test]
    fn list_sessions_splits_lines_and_trims_empties() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("limactl"), always())
            .returning(|_, _| Ok("sb-1\nsb-2\n\nsb-3\n".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        let names = list_sessions(&lima, Path::new("/tmp/repo")).expect("ok");
        assert_eq!(names, vec!["sb-1", "sb-2", "sb-3"]);
    }

    #[test]
    fn capture_pane_passes_target_and_scroll() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .withf(|prog, args| {
                prog == "limactl"
                    && args.windows(2).any(|w| w[0] == "-t" && w[1] == "sb-1")
                    && args.windows(2).any(|w| w[0] == "-S" && w[1] == "-200")
            })
            .returning(|_, _| Ok("pane contents".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        let out = capture_pane(&lima, Path::new("/tmp/repo"), "sb-1", 200).expect("ok");
        assert_eq!(out, "pane contents");
    }
}
