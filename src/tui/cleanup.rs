//! Periodic crash-session reaper.
//!
//! Background: AO never reclaims session records on its own. Each `ao spawn`
//! mints a fresh `<prefix>-N` and writes a JSON state file + a worktree to
//! disk. Manual kills mark the record `terminated/manually_killed` and stop
//! the tmux pane, but the JSON, worktree, and host-side `.git/worktrees/`
//! pointer all linger. Over weeks of use the litter compounds.
//!
//! The user-visible flow is `ao session cleanup`, but that has two
//! limitations fleet needs to work around:
//!
//! 1. It also reaps *crashed* sessions whose runtime died unexpectedly —
//!    discarding the tmux scrollback and any uncommitted work in the worktree
//!    along with the record. There's no preservation step.
//! 2. It runs only when invoked, so accumulation continues silently.
//!
//! This module fixes both. The refresh thread fires [`sweep`] once on TUI
//! startup and every 30 minutes thereafter. For each crashed-runtime session
//! (`runtime.state == "missing"` with reason ≠ `manual_kill_requested`,
//! terminated > 10 min ago to give the user time to investigate), the script
//! preserves a forensic capture to `<host-repo>/.fleet/crashed-sessions/<id>/`
//! — the session JSON, tmux scrollback, `git status` / `git diff` of the
//! worktree, and a `meta.txt` with timestamps — before invoking `ao session
//! kill <id>` to release the disk. Manual kills are intentionally skipped:
//! you already know what was there.
//!
//! The whole sweep runs as a single `bash -c` invocation inside the VM
//! (driven via `limactl shell`) — keeping the host↔VM roundtrips bounded to
//! one per project per tick, and letting the script use `node` for reliable
//! JSON parsing instead of stitching grep/sed/awk.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

const VM_NAME: &str = "fleet-vm";

/// Bash script run inside the VM by [`sweep`]. Receives three positional args:
///
/// 1. `$PROJECT_KEY` — the AO project key (e.g. `fleet`), used as the
///    `projects/<key>/` subdir name on the VM-side AO state directory.
/// 2. `$HOST_REPO` — host-side absolute path to the project repo, as
///    referenced by `project.path` in `agent-orchestrator.yaml`. The VM
///    bind-mounts the host home, so the same path resolves inside the VM
///    and writes here land on the host's filesystem.
/// 3. `$MIN_AGE_SEC` — minimum seconds since a session was terminated
///    before it's eligible. Gives the user a window to attach to a crashed
///    runtime before fleet reaps it.
///
/// The script is intentionally idempotent and tolerant of missing pieces
/// (no sessions dir → exit 0; no candidates → exit 0; tmux pane already
/// gone → captured as a placeholder line). All status output goes to
/// stdout; only genuine failures emit on stderr.
const SWEEP_SCRIPT: &str = r#"set -e
PROJECT_KEY="$1"
HOST_REPO="$2"
MIN_AGE_SEC="$3"

SESSIONS_DIR="$HOME/.agent-orchestrator/projects/$PROJECT_KEY/sessions"
WORKTREES_DIR="$HOME/.agent-orchestrator/projects/$PROJECT_KEY/worktrees"
LOG_BASE="$HOST_REPO/.fleet/crashed-sessions"

# Nothing to do if AO hasn't been initialised for this project on this VM.
if [ ! -d "$SESSIONS_DIR" ]; then
    exit 0
fi

# Lazily provision the log directory + .gitignore. The gitignore makes the
# captured artefacts invisible to git status without forcing the user to
# edit their project-level .gitignore. They can commit `.fleet/.gitignore`
# itself if they want to share the convention with their team.
ensure_log_base() {
    if [ -d "$LOG_BASE" ]; then
        return 0
    fi
    mkdir -p "$LOG_BASE"
    printf '*\n!.gitignore\n' > "$HOST_REPO/.fleet/.gitignore"
}

# Decide whether a session record is a crash candidate. Exits 0 if yes,
# non-zero if no. Reasons to skip: session is alive, runtime was killed by
# the user (not a crash), or the termination is too recent.
is_crash_candidate() {
    node --no-warnings -e '
        const fs = require("fs");
        const j = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
        const rt = j.lifecycle && j.lifecycle.runtime;
        const sess = j.lifecycle && j.lifecycle.session;
        if (!rt || !sess) process.exit(2);
        // Worker only; orchestrators get reaped through their own
        // restart path (see purge_stale_orchestrator_phase).
        if (sess.kind === "orchestrator") process.exit(2);
        if (rt.state !== "missing") process.exit(2);
        if (rt.reason === "manual_kill_requested") process.exit(2);
        if (sess.reason === "manually_killed") process.exit(2);
        const tAt = sess.terminatedAt ? Date.parse(sess.terminatedAt) : null;
        if (!tAt) process.exit(2);
        const ageSec = (Date.now() - tAt) / 1000;
        if (ageSec < parseFloat(process.argv[2])) process.exit(2);
        process.exit(0);
    ' "$1" "$MIN_AGE_SEC" 2>/dev/null
}

reaped=0
for sf in "$SESSIONS_DIR"/*.json; do
    [ -f "$sf" ] || continue
    sid=$(basename "$sf" .json)

    if ! is_crash_candidate "$sf"; then
        continue
    fi

    ensure_log_base
    LOG_DIR="$LOG_BASE/$sid"
    mkdir -p "$LOG_DIR"
    echo "fleet: capturing crashed session $sid"

    # 1. AO lifecycle record at the moment of capture.
    cp "$sf" "$LOG_DIR/session.json"

    # 2. tmux pane scrollback. -S -500 fetches the last 500 lines from
    #    history; -p prints to stdout. The pane is usually gone by the
    #    time we get here (that's the definition of "crashed runtime"),
    #    so write a placeholder if capture fails so the captured dir
    #    isn't misleadingly empty.
    if ! tmux capture-pane -t "$sid" -p -S -500 > "$LOG_DIR/pane.log" 2>/dev/null; then
        echo "(tmux pane $sid already gone — pane scrollback unavailable)" > "$LOG_DIR/pane.log"
    fi

    # 3. Uncommitted work in the worktree, if it still exists. We don't
    #    want to drop a crashed session that wrote useful changes without
    #    surfacing them.
    WT="$WORKTREES_DIR/$sid"
    if [ -d "$WT" ]; then
        git -C "$WT" status --short > "$LOG_DIR/git-status.txt" 2>&1 || true
        git -C "$WT" diff > "$LOG_DIR/git-diff.txt" 2>&1 || true
    fi

    # 4. Human-readable summary for grep'ability.
    {
        echo "capturedAt=$(date -Iseconds)"
        node --no-warnings -e '
            const j = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
            const sess = (j.lifecycle && j.lifecycle.session) || {};
            const rt = (j.lifecycle && j.lifecycle.runtime) || {};
            console.log("sessionId=" + (j.runtimeHandle && j.runtimeHandle.id || ""));
            console.log("branch=" + (j.branch || ""));
            console.log("issue=" + (j.issue || ""));
            console.log("agent=" + (j.agent || ""));
            console.log("terminatedAt=" + (sess.terminatedAt || ""));
            console.log("sessionReason=" + (sess.reason || ""));
            console.log("runtimeReason=" + (rt.reason || ""));
        ' "$sf"
    } > "$LOG_DIR/meta.txt"

    # 5. Reap. Tolerate non-zero — the worktree might be in an odd state
    #    and we'd rather keep the captured artefacts than abort the sweep.
    if ! ao session kill "$sid" > "$LOG_DIR/kill.log" 2>&1; then
        echo "fleet: ao session kill $sid failed (see kill.log)"
    fi
    reaped=$((reaped + 1))
done

if [ "$reaped" -gt 0 ]; then
    echo "fleet: sweep reaped $reaped crashed session(s)"
fi
"#;

/// Run one crash-sweep for `project_key`, capturing artefacts for any
/// session whose runtime died unexpectedly more than `min_age_secs`
/// seconds ago and then `ao session kill`ing it. Idempotent and silent
/// when there's nothing to reap.
///
/// Pipes stdout/stderr through to the caller's logger via captured
/// output; a non-zero exit returns an error containing the trimmed
/// stderr so the refresh thread can surface it through the normal
/// error channel.
pub fn sweep(host_repo: &Path, ao_workdir: &Path, project_key: &str, min_age_secs: u64) -> Result<()> {
    let args = [
        "shell".to_string(),
        "--workdir".to_string(),
        ao_workdir.display().to_string(),
        VM_NAME.to_string(),
        "bash".to_string(),
        "-s".to_string(),
        "--".to_string(),
        project_key.to_string(),
        host_repo.display().to_string(),
        min_age_secs.to_string(),
    ];
    let mut child = Command::new("limactl")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "spawn `limactl shell … bash -s` for crash sweep")?;
    // The script body is fed on stdin so we don't have to shell-quote the
    // heredoc-style bash; `-s` makes bash read the script from stdin and
    // pass the trailing argv as $1.. positional parameters.
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        stdin
            .write_all(SWEEP_SCRIPT.as_bytes())
            .context("feed cleanup script to bash stdin")?;
    }
    let out = child
        .wait_with_output()
        .context("wait on cleanup subprocess")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().map_or_else(|| "signal".into(), |c| c.to_string());
        anyhow::bail!("crash sweep exited {code}: {stderr}");
    }
    Ok(())
}
