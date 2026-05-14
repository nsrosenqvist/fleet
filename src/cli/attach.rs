//! `fleet attach <session>` — drop the user into a session's tmux pane.
//!
//! Bypasses AO's terminal plugin (which can default to opening a web URL or
//! mishandle TTY when invoked through pipelines that capture stdio). We
//! `tmux attach` directly through `limactl shell`, inheriting the calling
//! shell's TTY.
//!
//! Before the attach we inject a fleet-flavoured status bar onto the target
//! session (purple session name + `ctrl+b d to detach` hint) so the user
//! always knows how to get back to the management TUI. The options are set
//! per-session so we don't leak into any other tmux state the user keeps in
//! the VM.

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
    // Set-option calls + attach packed into a single `bash -c` so the
    // attach handshake stays a single `limactl shell` round-trip
    // (each one is ~200-400ms cold; doing 6 of them serially is a
    // visible stall when dropping into a session).
    //
    // The whole script runs as `aoworker` because the tmux server AO
    // anchors is aoworker-owned (see Phase 5 — `src/cli/spawn.rs::
    // build_aoworker_argv`). lima-owned tmux can't see those sessions.
    // The sudoers drop-in provisioned by templates/fleet-vm.yaml
    // allows `lima ALL=(aoworker) NOPASSWD: /bin/bash, /usr/bin/tmux`
    // and preserves the TERM env so xterm-256color survives the hop.
    let script = build_attach_script(session);
    let argv = vec![
        "shell".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        lima.vm_name().to_string(),
        "sudo".to_string(),
        "-u".to_string(),
        "aoworker".to_string(),
        "--preserve-env=TERM".to_string(),
        "bash".to_string(),
        "-c".to_string(),
        script,
    ];
    // The VM doesn't carry terminfo for newer host terminals (xterm-ghostty,
    // wezterm, etc.); tmux refuses to attach. Force xterm-256color for the
    // limactl shell so the in-VM tmux client succeeds. Minor rendering
    // capability loss vs the host TERM (acceptable).
    run_interactive("limactl", &argv, &[("TERM", "xterm-256color")], &[])
}

/// Bash one-liner that customises the target session's status bar
/// and then attaches. `set-option -t <session>` is per-session, so
/// other tmux sessions in the VM (e.g. the user's own scratch shell)
/// keep whatever they had configured. We tolerate set-option errors
/// with `|| true` — if tmux is older / the session has gone away
/// between selection and attach, the attach itself will fail with a
/// useful error rather than the status-option setup masking it.
///
/// Status bar:
///   - Left:   ` <session-name>  │  ctrl+b d to detach `
///   - Window list rides to the right of `status-left` via tmux's
///     built-in `status-window-format`.
///   - Colour: fleet purple (`colour141`) for the session name +
///     active-window marker; muted gray for everything else.
fn build_attach_script(session: &str) -> String {
    let s = crate::cli::spawn::shell_quote_single(session);
    let status_left =
        " #[fg=colour141,bold]#S#[default] #[fg=brightblack]│#[default] ctrl+b d to detach ";
    let q_status_left = crate::cli::spawn::shell_quote_single(status_left);
    // `window-size latest` flips the session out of the `manual` pin
    // applied by [`crate::tmux::resize_window`] (used to keep the
    // capture-pane width aligned with fleet's output panel). With
    // `latest`, tmux resizes the window to the attaching client's
    // terminal size, so the user sees the session at full width
    // instead of cropped to the panel. The post-detach handler in
    // `attach_selected` re-pins via `resize_window` for the panel.
    // AO's tmux conf ships with `status off`, so the styling below
    // would be invisible without explicitly turning the bar back on
    // for this session.
    format!(
        "set -e
tmux set-option -t {s} status on >/dev/null 2>&1 || true
tmux set-option -t {s} status-style 'bg=default,fg=colour250' >/dev/null 2>&1 || true
tmux set-option -t {s} status-left {q_status_left} >/dev/null 2>&1 || true
tmux set-option -t {s} status-left-length 60 >/dev/null 2>&1 || true
tmux set-option -t {s} window-status-current-style 'fg=colour141,bold' >/dev/null 2>&1 || true
tmux set-option -t {s} window-status-style 'fg=colour250' >/dev/null 2>&1 || true
tmux set-option -t {s} window-size latest >/dev/null 2>&1 || true
exec tmux attach -t {s}
"
    )
}

#[cfg(test)]
mod tests {
    use super::build_attach_script;

    #[test]
    fn script_quotes_session_name_safely() {
        // A pathological session name with shell metachars must be
        // single-quoted so it can't escape into the surrounding bash.
        let script = build_attach_script("fl-4; rm -rf /");
        // The dangerous tail is wrapped inside single quotes, never
        // a bare token.
        assert!(script.contains("'fl-4; rm -rf /'"));
        assert!(!script.contains(" fl-4; rm -rf / "));
    }

    #[test]
    fn script_ends_with_exec_attach() {
        // Using `exec` so tmux replaces the bash wrapper — keeps the
        // process tree clean and lets tmux own the TTY for the
        // duration of the attach.
        let script = build_attach_script("fl-4");
        assert!(script.contains("exec tmux attach -t 'fl-4'"));
    }

    #[test]
    fn script_sets_status_left_with_detach_hint() {
        let script = build_attach_script("fl-4");
        assert!(script.contains("ctrl+b d to detach"));
    }

    #[test]
    fn script_turns_status_bar_on() {
        // AO's tmux conf disables the status bar by default. Without
        // an explicit `status on` the styled status-left would never
        // render.
        let script = build_attach_script("fl-4");
        assert!(script.contains("set-option -t 'fl-4' status on"));
    }

    #[test]
    fn script_switches_window_size_to_latest_before_attach() {
        // The session is normally pinned to `window-size manual` so the
        // capture-pane width matches the output panel. Without flipping
        // back to `latest`, the attaching client would see the window
        // at the panel's narrow width instead of their full terminal.
        let script = build_attach_script("fl-4");
        let ws_idx = script
            .find("window-size latest")
            .expect("attach script should switch window-size to latest");
        let attach_idx = script
            .find("exec tmux attach")
            .expect("attach script should exec tmux attach");
        assert!(
            ws_idx < attach_idx,
            "window-size latest must be set before the attach"
        );
    }
}
