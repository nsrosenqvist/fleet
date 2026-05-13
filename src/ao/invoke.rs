//! Invoke `ao` subcommands via `limactl shell` and parse their JSON output.

use anyhow::{Context, Result, bail};
use std::path::Path;

use std::collections::HashMap;

use super::state::{AoResponse, EventInfo, EventsResponse, SessionInfo, SessionMeta};
use crate::lima::Lima;

pub struct Ao<'a> {
    lima: &'a Lima,
    workdir: &'a Path,
}

impl<'a> Ao<'a> {
    pub fn new(lima: &'a Lima, workdir: &'a Path) -> Self {
        Self { lima, workdir }
    }

    /// `ao status --json` — all sessions, with live activity hints.
    pub fn status(&self) -> Result<AoResponse<SessionInfo>> {
        let raw = self.lima.shell(
            self.workdir,
            vec!["ao".to_string(), "status".to_string(), "--json".to_string()],
        )?;
        parse_response(&raw).context("parsing ao status --json")
    }

    /// `ao session ls --json` — session metadata including worktree paths.
    pub fn session_ls(&self) -> Result<AoResponse<SessionInfo>> {
        let raw = self.lima.shell(
            self.workdir,
            vec![
                "ao".to_string(),
                "session".to_string(),
                "ls".to_string(),
                "--json".to_string(),
            ],
        )?;
        parse_response(&raw).context("parsing ao session ls --json")
    }

    /// `ao session ls --all --json` — same as [`Self::session_ls`] but
    /// includes orchestrator sessions (`role: "orchestrator"`) alongside
    /// workers. Required for fleet's grouped sidebar; the plain
    /// `ao status --json` omits orchestrators.
    pub fn session_ls_all(&self) -> Result<AoResponse<SessionInfo>> {
        let raw = self.lima.shell(
            self.workdir,
            vec![
                "ao".to_string(),
                "session".to_string(),
                "ls".to_string(),
                "--all".to_string(),
                "--json".to_string(),
            ],
        )?;
        parse_response(&raw).context("parsing ao session ls --all --json")
    }

    /// Per-session lifecycle metadata read in bulk from the VM's AO
    /// state directory. Returns a map keyed by session id (matches
    /// [`SessionInfo::id`]) containing the fields the sidebar wants
    /// to badge on: agent's self-reported state, lifecycle session
    /// state, runtime state + reason.
    ///
    /// These live in per-session JSON files (one per session) under
    /// `~/.agent-orchestrator/projects/<project>/sessions/` inside
    /// the VM. The host can't read them directly — Lima doesn't
    /// expose the guest's `$HOME` — so we run a small node script
    /// inside the VM that walks every project's `sessions/` dir and
    /// emits a single JSON map. One limactl roundtrip regardless of
    /// session count.
    ///
    /// Tolerant of missing pieces: an empty / missing
    /// `.agent-orchestrator/projects` directory returns an empty map
    /// (a fresh VM with no AO state yet). Sessions whose JSON fails
    /// to parse are skipped silently.
    pub fn session_meta_bulk(&self) -> Result<HashMap<String, SessionMeta>> {
        let raw = self.lima.shell(
            self.workdir,
            vec!["node".to_string(), "-e".to_string(), META_NODE_SCRIPT.to_string()],
        )?;
        let start = find_json_start(&raw).with_context(|| {
            format!(
                "no JSON object in session_meta_bulk output: {}",
                error_preview(&raw)
            )
        })?;
        let map: HashMap<String, SessionMeta> = serde_json::from_str(&raw[start..])
            .with_context(|| {
                format!(
                    "parsing session_meta_bulk JSON: {}",
                    error_preview(&raw[start..])
                )
            })?;
        Ok(map)
    }

    /// `ao events list --since <window> -n <limit> --json` — recent
    /// activity events (spawns, kills, lifecycle transitions, CI
    /// failures, review events). Drives the bottom-pane ticker;
    /// caller picks a sensible time window since the log is
    /// otherwise unbounded.
    pub fn events_list(&self, since: &str, limit: u32) -> Result<Vec<EventInfo>> {
        let raw = self.lima.shell(
            self.workdir,
            vec![
                "ao".to_string(),
                "events".to_string(),
                "list".to_string(),
                "--since".to_string(),
                since.to_string(),
                "-n".to_string(),
                limit.to_string(),
                "--json".to_string(),
            ],
        )?;
        let start = find_json_start(&raw).with_context(|| {
            format!(
                "no JSON object found in ao events output: {}",
                error_preview(&raw)
            )
        })?;
        let parsed: EventsResponse = serde_json::from_str(&raw[start..]).with_context(|| {
            format!(
                "parsing ao events list --json: {}",
                error_preview(&raw[start..])
            )
        })?;
        Ok(parsed.events)
    }
}

/// Node script (passed via `node -e`) that walks every per-project
/// `sessions/` directory under `~/.agent-orchestrator/projects/`,
/// reads each session JSON, and prints a single JSON map keyed by
/// session id. Each value carries the subset of lifecycle fields the
/// sidebar badges on.
///
/// Single quotes (`'`) are avoided because the script is sent via
/// `bash -c "node -e '...'"` further upstream in some flows; the
/// double-quoted JS form keeps that escape-free. The script is also
/// tolerant of missing dirs (empty AO state → empty map) and bad
/// JSON (skip that session silently — one corrupt record shouldn't
/// blank every badge).
const META_NODE_SCRIPT: &str = r#"
const fs = require("fs");
const path = require("path");
const root = process.env.HOME + "/.agent-orchestrator/projects";
const out = {};
try {
  for (const proj of fs.readdirSync(root)) {
    const sessDir = path.join(root, proj, "sessions");
    let entries;
    try { entries = fs.readdirSync(sessDir); } catch (e) { continue; }
    for (const f of entries) {
      if (!f.endsWith(".json")) continue;
      const sid = f.slice(0, -5);
      try {
        const j = JSON.parse(fs.readFileSync(path.join(sessDir, f), "utf8"));
        const sess = (j.lifecycle && j.lifecycle.session) || {};
        const rt = (j.lifecycle && j.lifecycle.runtime) || {};
        out[sid] = {
          agentReportedState: j.agentReportedState || null,
          sessionState: sess.state || null,
          sessionReason: sess.reason || null,
          runtimeState: rt.state || null,
          runtimeReason: rt.reason || null,
        };
      } catch (e) { /* skip unreadable session */ }
    }
  }
} catch (e) { /* no projects dir yet */ }
process.stdout.write(JSON.stringify(out));
"#;

/// AO prefixes `--json` output with one or more notifier-warning lines like
/// `[notifier-discord] No webhookUrl configured.` (plus indented continuation
/// lines). The JSON body starts on its own line with `{` or `[`. Find that
/// line — careful not to mistake the `[` in `[notifier-...]` for a JSON array.
fn parse_response(raw: &str) -> Result<AoResponse<SessionInfo>> {
    let start = find_json_start(raw)
        .with_context(|| format!("no JSON object found in ao output: {}", error_preview(raw)))?;
    let json = &raw[start..];
    let parsed: AoResponse<SessionInfo> = serde_json::from_str(json)
        .with_context(|| format!("parsing JSON: {}", error_preview(json)))?;
    if parsed.data.is_empty() && !json.contains("\"data\"") {
        bail!(
            "ao response had no `data` key — unexpected shape: {}",
            error_preview(json)
        );
    }
    Ok(parsed)
}

/// Trim raw AO output for inclusion in an error message. The status
/// bar shows errors as a single line; a 500-line AO banner dump
/// pushes the actual cause off-screen. Keep the first line (where
/// the real signal — "No config found", a JSON-shape mismatch — lives)
/// plus a length hint when truncation occurred.
fn error_preview(raw: &str) -> String {
    const MAX_CHARS: usize = 160;
    let first_line = raw.lines().next().unwrap_or("").trim();
    let trimmed: String = first_line.chars().take(MAX_CHARS).collect();
    if first_line.chars().count() > MAX_CHARS || raw.len() > first_line.len() {
        format!("{trimmed}… ({} bytes total)", raw.len())
    } else {
        trimmed
    }
}

/// Return the byte offset of the first character that starts a JSON body.
/// A "JSON line" is one whose first non-whitespace character is `{` or `[`
/// AND which is not a `[notifier-...]` log line.
fn find_json_start(raw: &str) -> Option<usize> {
    let mut byte_pos = 0;
    for line in raw.lines() {
        let trimmed_start = line.trim_start();
        let leading_ws = line.len() - trimmed_start.len();
        let line_len_with_nl = line.len() + 1; // +1 for the consumed '\n'
        let candidate = trimmed_start.starts_with('{')
            || (trimmed_start.starts_with('[') && !trimmed_start.starts_with("[notifier"));
        if candidate {
            return Some(byte_pos + leading_ws);
        }
        byte_pos += line_len_with_nl;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::always;
    use std::sync::Arc;

    fn ao_with_canned(stdout: &'static str) -> (Lima, &'static str) {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |_, _| Ok(stdout.to_string()));
        (Lima::new(Arc::new(mock), "fleet-vm"), stdout)
    }

    #[test]
    fn status_strips_notifier_preamble() {
        let canned = r#"[notifier-discord] No webhookUrl configured.
[notifier-slack] No webhookUrl configured — notifications will be no-ops
{
  "data": [
    { "name": "sb-1", "status": "working", "issue": "fa8098a" }
  ],
  "meta": { "hiddenTerminatedCount": 0 }
}"#;
        let (lima, _) = ao_with_canned(canned);
        let ao = Ao::new(&lima, Path::new("/tmp/repo"));
        let r = ao.status().expect("ok");
        assert_eq!(r.data.len(), 1);
        assert_eq!(r.data[0].id.as_deref(), Some("sb-1"));
        assert_eq!(r.data[0].issue_id.as_deref(), Some("fa8098a"));
    }

    #[test]
    fn status_errors_when_no_json() {
        let canned = "[notifier-discord] error\n[notifier-slack] error\n";
        let (lima, _) = ao_with_canned(canned);
        let ao = Ao::new(&lima, Path::new("/tmp/repo"));
        let err = ao.status().expect_err("should fail");
        let msg = format!("{err:?}");
        assert!(msg.contains("no JSON"), "unexpected: {msg}");
    }
}
