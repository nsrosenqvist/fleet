//! Invoke `ao` subcommands via `limactl shell` and parse their JSON output.

use anyhow::{Context, Result, bail};
use std::path::Path;

use super::state::{AoResponse, SessionInfo};
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
}

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
