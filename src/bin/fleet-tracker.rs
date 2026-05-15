//! `fleet-tracker` — in-container client for fleet's per-session bridge.
//!
//! The fleet workflow executor starts a per-session HTTP bridge on the
//! host and exposes its URL + bearer token to the agent container via
//! `FLEET_BRIDGE_URL` and `FLEET_BRIDGE_TOKEN`. This binary is what
//! the agent invokes to write to its bound ticket — `fleet-tracker
//! comment "…"`, `fleet-tracker status closed`, etc.
//!
//! Writes are scoped to the session's bound ticket by the bridge;
//! `fleet-tracker` deliberately exposes no `id` flag on write
//! subcommands so a poisoned agent can't even ask to write elsewhere.
//! Reads default to the bound ticket but accept an optional id
//! argument for cross-ticket context-gathering.
//!
//! The binary is intentionally self-contained: it speaks HTTP/1.1 by
//! hand over `TcpStream` rather than depending on a client crate, so
//! the bind-mounted image stays small and the dependency graph stays
//! shallow. Two crate deps (clap, `serde_json`) cover argv + JSON.

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// Network timeout per request. The bridge is on loopback (or the
/// host's bridge gateway, one hop away); anything slower than this
/// indicates the bridge is gone or unhealthy, and we prefer a fast
/// failure to a hung agent.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(
    name = "fleet-tracker",
    about = "In-container client for the fleet bridge. Writes are scoped to the session's bound ticket.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Append a comment to the session's bound ticket.
    Comment {
        /// Body text. Pass `-` to read from stdin.
        body: String,
    },
    /// Change the bound ticket's status. STATUS is open / in-progress / closed.
    Status {
        #[arg(value_parser = ["open", "in-progress", "closed"])]
        status: String,
    },
    /// Add a label to the bound ticket.
    AddLabel { label: String },
    /// Remove a label from the bound ticket.
    RemoveLabel { label: String },
    /// Read a ticket. Defaults to the bound ticket; pass an id to
    /// pull a different one (reads are unscoped — see the bridge's
    /// security model).
    Read {
        /// Optional ticket id. Omit to read the session's bound ticket.
        id: Option<String>,
    },
    /// List all open + closed tickets the tracker knows about.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("fleet-tracker: {err}");
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

    let (method, path, body) = match cli.cmd {
        Cmd::Comment { body } => {
            let body_text = if body == "-" {
                read_stdin().map_err(|e| format!("reading stdin for comment body: {e}"))?
            } else {
                body
            };
            (
                "POST",
                "/comment".to_string(),
                Some(serde_json::json!({"body": body_text}).to_string()),
            )
        }
        Cmd::Status { status } => (
            "POST",
            "/set-status".to_string(),
            Some(serde_json::json!({"status": status}).to_string()),
        ),
        Cmd::AddLabel { label } => (
            "POST",
            "/add-label".to_string(),
            Some(serde_json::json!({"label": label}).to_string()),
        ),
        Cmd::RemoveLabel { label } => (
            "POST",
            "/remove-label".to_string(),
            Some(serde_json::json!({"label": label}).to_string()),
        ),
        Cmd::Read { id } => {
            let path = id.map_or_else(|| "/read".to_string(), |i| format!("/read?id={i}"));
            ("GET", path, None)
        }
        Cmd::List => ("GET", "/list".to_string(), None),
    };

    let (status, response_body) = http_call(&bridge_url, method, &path, &token, body.as_deref())
        .map_err(|e| format!("calling bridge: {e}"))?;

    // Print whatever the bridge sent back, success or failure. Stdout
    // is reserved for response bodies (so agents can pipe to jq);
    // human notes go to stderr.
    print!("{response_body}");
    if (200..300).contains(&status) {
        Ok(0)
    } else {
        eprintln!("fleet-tracker: HTTP {status}");
        // Map HTTP error class → process exit code. 4xx → 2 (caller
        // error: bad payload, auth, scope); 5xx → 3 (server error:
        // tracker shell-out failed). Anything else → 4.
        let code = match status {
            400..=499 => 2,
            500..=599 => 3,
            _ => 4,
        };
        Ok(code)
    }
}

/// Hand-rolled HTTP/1.1 client. The bridge speaks bare HTTP on
/// loopback or the host bridge — no TLS, no chunked transfer — so a
/// 60-line client is sufficient.
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
    // Skip headers up to the blank line.
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

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}
