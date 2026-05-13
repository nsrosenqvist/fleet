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

/// `tmux capture-pane -p -e -t <target> -S -<max_lines>` inside the VM.
///
/// Returns the full pane content (most recent `max_lines` lines), with
/// ANSI escape sequences for text/background attributes preserved via
/// `-e`. The caller decides what to do with it (the TUI parses ANSI to
/// styled spans and renders the last N visible lines).
pub fn capture_pane(lima: &Lima, workdir: &Path, target: &str, max_lines: u32) -> Result<String> {
    lima.shell(
        workdir,
        vec![
            "tmux".to_string(),
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-t".to_string(),
            target.to_string(),
            "-S".to_string(),
            format!("-{max_lines}"),
        ],
    )
}

/// Pin the target session's window to the given dimensions. Used after
/// `tmux attach` returns: while attached, tmux resizes the session to
/// the host terminal's width and retains that size on detach, so the
/// next `capture-pane` returns lines wider than fleet's output pane —
/// which breaks Claude Code's TUI rendering inside the panel. Calling
/// this restores a size that matches the panel.
///
/// Bundles `set-option window-size manual` (so tmux honours the resize
/// when no client is currently attached) and `resize-window` into one
/// `bash -c` to keep this a single ~250 ms `limactl shell` round-trip.
/// Errors are swallowed inside the script with `|| true` — if the
/// session has gone away between the attach and now, we'd rather quietly
/// no-op than poison the UI with a flash on every detach.
pub fn resize_window(
    lima: &Lima,
    workdir: &Path,
    target: &str,
    width: u16,
    height: u16,
) -> Result<()> {
    let s = crate::cli::spawn::shell_quote_single(target);
    let script = format!(
        "tmux set-option -t {s} window-size manual >/dev/null 2>&1 || true; \
         tmux resize-window -t {s} -x {width} -y {height} >/dev/null 2>&1 || true"
    );
    lima.shell(
        workdir,
        vec!["bash".to_string(), "-c".to_string(), script],
    )
    .map(|_| ())
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
                    // `-e` preserves ANSI escapes so the UI can render
                    // Claude Code's colored output instead of stripping it.
                    && args.iter().any(|a| a == "-e")
            })
            .returning(|_, _| Ok("pane contents".to_string()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        let out = capture_pane(&lima, Path::new("/tmp/repo"), "sb-1", 200).expect("ok");
        assert_eq!(out, "pane contents");
    }

    #[test]
    fn resize_window_pins_size_and_sets_manual_mode() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .withf(|prog, args| {
                if prog != "limactl" {
                    return false;
                }
                // The whole tmux pipeline rides through `bash -c <script>`;
                // the last argv slot holds the script we want to assert on.
                let script = args.last().map_or("", String::as_str);
                script.contains("window-size manual")
                    && script.contains("resize-window -t 'sb-1' -x 120 -y 40")
            })
            .returning(|_, _| Ok(String::new()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        resize_window(&lima, Path::new("/tmp/repo"), "sb-1", 120, 40).expect("ok");
    }

    #[test]
    fn resize_window_quotes_pathological_session_name() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .withf(|_, args| {
                // Pathological name must land inside single quotes so it
                // can't escape the surrounding bash. `shell_quote_single`
                // turns `fl-4; rm -rf /` into `'fl-4; rm -rf /'`.
                let script = args.last().map_or("", String::as_str);
                script.contains("'fl-4; rm -rf /'")
                    && !script.contains(" fl-4; rm -rf / ")
            })
            .returning(|_, _| Ok(String::new()));
        let lima = Lima::new(Arc::new(mock), "fleet-vm");
        resize_window(&lima, Path::new("/tmp/repo"), "fl-4; rm -rf /", 80, 24).expect("ok");
    }
}
