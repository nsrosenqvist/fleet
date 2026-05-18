//! `fleet-pr` — in-container client for fleet's per-session bridge,
//! PR side.
//!
//! The fleet workflow executor starts a per-session HTTP bridge on
//! the host and exposes its URL + bearer token to the agent container
//! via `FLEET_BRIDGE_URL` and `FLEET_BRIDGE_TOKEN`. This binary is
//! what the agent invokes to read PR data — `fleet-pr read`,
//! `fleet-pr checks`, `fleet-pr check-logs <name>`.
//!
//! Read-only by design. Write operations (`pr-comment`, `create-pr`)
//! go through workflow nodes, not the bridge — the agent's authority
//! is intentionally limited to inspecting the PR it is working on.
//! When `--number <N>` is omitted, every command defaults to the
//! session's bound PR (set by the scheduler dispatcher in
//! `run_for_pr`).
//!
//! Same hand-rolled HTTP/1.1 shape as `fleet-tracker`; the bind-
//! mounted image stays small and the dep graph stays shallow.

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// Per-request timeout. Same rationale as fleet-tracker: the bridge
/// is on loopback / the host bridge gateway, so anything slower than
/// this means the bridge is dead or unhealthy and the agent should
/// fail fast instead of hanging.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(
    name = "fleet-pr",
    about = "In-container client for the fleet bridge — PR reads. Writes go via workflow nodes.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read a PR (description, head/base refs, labels, draft state).
    /// Defaults to the session's bound PR; pass `--number` for any
    /// other PR.
    Read {
        #[arg(long)]
        number: Option<u32>,
    },
    /// Read the CI checks rollup for a PR (status / failed / pending).
    Checks {
        #[arg(long)]
        number: Option<u32>,
    },
    /// Fetch the failed-step logs for a specific check on a PR.
    /// `check` is the check name (e.g. `build`, `lint`); the bridge
    /// resolves it to a GitHub Actions run id and shells
    /// `gh run view --log-failed`.
    CheckLogs {
        check: String,
        #[arg(long)]
        number: Option<u32>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("fleet-pr: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<u8, String> {
    let bridge_url = env::var("FLEET_BRIDGE_URL").map_err(|_| {
        "FLEET_BRIDGE_URL not set — is this binary running inside a fleet workflow container?"
            .to_string()
    })?;
    let token = env::var("FLEET_BRIDGE_TOKEN")
        .map_err(|_| "FLEET_BRIDGE_TOKEN not set — bridge auth missing".to_string())?;

    let path = match cli.cmd {
        Cmd::Read { number } => path_with_number("/pr/read", number, None),
        Cmd::Checks { number } => path_with_number("/pr/checks", number, None),
        Cmd::CheckLogs { check, number } => {
            path_with_number("/pr/check-logs", number, Some(("check", check)))
        }
    };

    let (status, response_body) = http_call(&bridge_url, "GET", &path, &token, None)
        .map_err(|e| format!("calling bridge: {e}"))?;

    // Stdout = response body (JSON or plain text), so agents can pipe
    // into jq. Human notes go to stderr like fleet-tracker.
    print!("{response_body}");
    if (200..300).contains(&status) {
        Ok(0)
    } else {
        eprintln!("fleet-pr: HTTP {status}");
        let code = match status {
            400..=499 => 2,
            500..=599 => 3,
            _ => 4,
        };
        Ok(code)
    }
}

/// Build a path with optional `?number=N` and an optional extra
/// query parameter. Encodes nothing — both values are constrained
/// (numeric or a check name with no `&` / `=` in practice). Tests
/// pin the format so the bridge route can rely on the shape.
fn path_with_number(
    base: &str,
    number: Option<u32>,
    extra: Option<(&str, String)>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = number {
        parts.push(format!("number={n}"));
    }
    if let Some((k, v)) = extra {
        parts.push(format!("{k}={v}"));
    }
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", parts.join("&"))
    }
}

/// Same minimal HTTP/1.1 client as fleet-tracker. Identical shape so
/// any future change to the wire format can be tracked across both
/// binaries.
fn http_call(
    url: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    use std::fmt::Write as _;
    let host_port = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("FLEET_BRIDGE_URL must start with http://, got {url:?}"))?;
    let mut stream = TcpStream::connect(host_port).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok();
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).ok();

    let body_bytes = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
    writeln!(req, "Authorization: Bearer {token}\r").expect("write to String");
    if !body_bytes.is_empty() {
        writeln!(
            req,
            "Content-Type: application/json\r\nContent-Length: {}\r",
            body_bytes.len()
        )
        .expect("write to String");
    }
    req.push_str("\r\n");
    req.push_str(body_bytes);

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("read status: {e}"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?
        .parse()
        .map_err(|e| format!("non-integer status: {e}"))?;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("read header: {e}"))?
            == 0
        {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut body = String::new();
    reader
        .read_to_string(&mut body)
        .map_err(|e| format!("read body: {e}"))?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_with_no_number_and_no_extra_uses_base() {
        assert_eq!(path_with_number("/pr/read", None, None), "/pr/read");
    }

    #[test]
    fn path_with_number_appends_query_string() {
        assert_eq!(
            path_with_number("/pr/checks", Some(42), None),
            "/pr/checks?number=42"
        );
    }

    #[test]
    fn path_with_number_and_extra_chains_with_ampersand() {
        assert_eq!(
            path_with_number("/pr/check-logs", Some(42), Some(("check", "build".into()))),
            "/pr/check-logs?number=42&check=build"
        );
    }

    #[test]
    fn path_with_extra_only_omits_number_segment() {
        assert_eq!(
            path_with_number("/pr/check-logs", None, Some(("check", "lint".into()))),
            "/pr/check-logs?check=lint"
        );
    }
}
