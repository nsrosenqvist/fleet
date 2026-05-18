//! Per-session HTTP bridge that gives an in-container agent scoped
//! write access to the tracker.
//!
//! The bridge runs on the host (not inside the container), bound to a
//! random loopback port. The fleet executor starts a bridge before
//! each agent node, threads the URL + bearer token into the
//! container's env as `FLEET_BRIDGE_URL` / `FLEET_BRIDGE_TOKEN`, then
//! tears down the bridge when the node ends.
//!
//! Threat model: the bridge is the only new write authority reaching
//! out of the container. Mitigations:
//!
//! - **Bearer token.** 256 bits of entropy from `/dev/urandom`, fresh
//!   per session. Every request must carry `Authorization: Bearer
//!   <token>`; mismatches → 401. Constant-time comparison avoids
//!   timing leaks.
//! - **Per-issue write scoping.** Writes always target the session's
//!   bound ticket; the body never carries a target id. A session with
//!   no bound ticket can't write at all.
//! - **Unscoped reads.** `GET /read` and `GET /list` deliberately
//!   allow any ticket id — agents often need context from related
//!   tickets and the blast-radius of an unwanted read is much lower
//!   than an unwanted write.
//!
//! Routes (per `recursive-careful-orchestrator.md` § Bridge server):
//!
//! - `POST /comment        { body }`         → comments on bound ticket
//! - `POST /set-status     { status }`       → sets bound ticket status
//! - `POST /add-label      { label }`        → adds label to bound
//! - `POST /remove-label   { label }`        → removes label from bound
//! - `GET  /read[?id=<id>]`                  → `IssueDetail` (default: bound)
//! - `GET  /list`                            → Vec<Issue>
//!
//! Lifecycle: [`Bridge`] owns the listener thread and a shutdown
//! atomic. Dropping a `Bridge` flips the flag and joins the thread,
//! so callers don't have to remember to call `stop` — `?` early-exit
//! cleans up via Drop. Tests rely on this for deterministic teardown.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::session::Session;
use crate::tracker::{Status, Tracker};

/// Hostname the bridge URL uses when piped into the container env.
/// Podman + Docker both resolve this name to the host's bridge
/// gateway, so the in-container agent reaches the loopback listener
/// without the host having to know the container's egress IP. The
/// constant is exposed so the executor can also inject it as
/// `NO_PROXY` (otherwise `HTTP_PROXY` would route the bridge request
/// through tinyproxy, which doesn't speak the bridge's loopback
/// auth).
pub const BRIDGE_HOST: &str = "host.containers.internal";

/// Token length in bytes; 32 bytes = 256 bits, rendered as 64 hex
/// chars. Plenty for a per-session secret whose lifetime is the
/// agent node's run.
const TOKEN_BYTES: usize = 32;

/// How often the listener thread wakes to check the shutdown flag.
/// Short enough that `Drop` returns promptly when a test tears the
/// bridge down; long enough that idle bridges don't busy-spin.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// Live handle to a per-session bridge HTTP server.
///
/// The listener runs on a dedicated thread; dropping the handle
/// signals shutdown and joins the thread, so callers can rely on
/// RAII for cleanup. `url_for_container` is what the executor pipes
/// to the in-container agent; `loopback_url` is what tests speak to
/// directly (the listener actually binds to `127.0.0.1`).
pub struct Bridge {
    join: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    token: String,
    port: u16,
}

impl Bridge {
    /// Start the bridge for `session`. The tracker handle is shared
    /// with the listener thread; the bridge holds it for the
    /// listener's lifetime. `code_host` is optional — when `None`,
    /// the `/pr/*` routes return 404 with a clear "no code host
    /// configured" message.
    pub fn start(
        session: &Session,
        tracker: Arc<dyn Tracker>,
        code_host: Option<Arc<dyn crate::code_host::CodeHost>>,
        repo_root: PathBuf,
    ) -> Result<Self> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| anyhow!("starting bridge HTTP server on loopback: {e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow!("bridge listener bound to a non-IP address"))?
            .port();
        let token = mint_token().context("minting bridge bearer token")?;
        let bound_ticket = session.issue.as_ref().map(|i| i.human_id.clone());
        let bound_pr = session.pr.as_ref().map(|p| p.number);
        let shutdown = Arc::new(AtomicBool::new(false));
        let join = spawn_listener(
            server,
            token.clone(),
            bound_ticket,
            bound_pr,
            tracker,
            code_host,
            repo_root,
            Arc::clone(&shutdown),
        );
        Ok(Self {
            join: Some(join),
            shutdown,
            token,
            port,
        })
    }

    /// URL the in-container agent uses to reach the bridge. Built
    /// against [`BRIDGE_HOST`], not localhost — see the constant's
    /// doc comment.
    pub fn url_for_container(&self) -> String {
        format!("http://{BRIDGE_HOST}:{}", self.port)
    }

    /// URL the host-side loopback uses. Tests connect here directly;
    /// production callers only need [`Self::url_for_container`].
    #[allow(dead_code)]
    pub fn loopback_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            // Listener wakes from recv_timeout within SHUTDOWN_POLL.
            // Join is best-effort; a poisoned worker thread isn't
            // recoverable at drop time so the error is discarded.
            let _ = h.join();
        }
    }
}

/// Spawn the listener thread. Borrows the parameters needed to
/// service requests; returns the join handle so [`Bridge`] can
/// shut it down on drop.
#[allow(clippy::too_many_arguments)]
fn spawn_listener(
    server: tiny_http::Server,
    token: String,
    bound_ticket: Option<String>,
    bound_pr: Option<u32>,
    tracker: Arc<dyn Tracker>,
    code_host: Option<Arc<dyn crate::code_host::CodeHost>>,
    repo_root: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            match server.recv_timeout(SHUTDOWN_POLL) {
                Ok(Some(req)) => {
                    handle_request(
                        req,
                        &token,
                        bound_ticket.as_deref(),
                        bound_pr,
                        &*tracker,
                        code_host.as_deref(),
                        &repo_root,
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(?err, "bridge listener recv error; shutting down");
                    break;
                }
            }
        }
    })
}

/// Route + dispatch a single request. Response failures are logged
/// rather than propagated because `tiny_http`'s `send_response` errors
/// only occur when the client has already gone away; there's no
/// caller-actionable recovery.
#[allow(clippy::too_many_arguments)]
fn handle_request(
    mut req: tiny_http::Request,
    token: &str,
    bound_ticket: Option<&str>,
    bound_pr: Option<u32>,
    tracker: &dyn Tracker,
    code_host: Option<&dyn crate::code_host::CodeHost>,
    repo_root: &std::path::Path,
) {
    if let Err(response) = require_bearer(&req, token) {
        respond(req, &response);
        return;
    }
    let method = req.method().clone();
    let url = req.url().to_string();
    let response = dispatch(
        &method,
        &url,
        &mut req,
        bound_ticket,
        bound_pr,
        tracker,
        code_host,
        repo_root,
    );
    respond(req, &response);
}

/// Inspect `Authorization: Bearer <token>` and reject on mismatch.
/// Constant-time string compare avoids the textbook timing side
/// channel even though a port-local attacker has fewer interesting
/// options than a remote one.
fn require_bearer(req: &tiny_http::Request, expected: &str) -> Result<(), JsonResponse> {
    let header = req
        .headers()
        .iter()
        .find(|h| {
            h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("authorization")
        })
        .map(|h| h.value.as_str().to_string());
    let Some(value) = header else {
        return Err(JsonResponse::error(
            401,
            "missing_authorization",
            "Authorization header required",
        ));
    };
    let trimmed = value.strip_prefix("Bearer ").unwrap_or(&value);
    if constant_time_eq(trimmed.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(JsonResponse::error(
            401,
            "bad_token",
            "invalid bridge bearer token",
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    method: &tiny_http::Method,
    url: &str,
    req: &mut tiny_http::Request,
    bound_ticket: Option<&str>,
    bound_pr: Option<u32>,
    tracker: &dyn Tracker,
    code_host: Option<&dyn crate::code_host::CodeHost>,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let (path, query) = split_path_query(url);
    match (method, path) {
        (tiny_http::Method::Post, "/comment") => with_bound(bound_ticket, |bound| {
            handle_comment(req, bound, tracker, repo_root)
        }),
        (tiny_http::Method::Post, "/set-status") => with_bound(bound_ticket, |bound| {
            handle_set_status(req, bound, tracker, repo_root)
        }),
        (tiny_http::Method::Post, "/add-label") => with_bound(bound_ticket, |bound| {
            handle_add_label(req, bound, tracker, repo_root)
        }),
        (tiny_http::Method::Post, "/remove-label") => with_bound(bound_ticket, |bound| {
            handle_remove_label(req, bound, tracker, repo_root)
        }),
        (tiny_http::Method::Get, "/read") => handle_read(query, bound_ticket, tracker, repo_root),
        (tiny_http::Method::Get, "/list") => handle_list(tracker, repo_root),
        (tiny_http::Method::Get, "/pr/read") => with_code_host(code_host, |host| {
            handle_pr_read(query, bound_pr, host, repo_root)
        }),
        (tiny_http::Method::Get, "/pr/checks") => with_code_host(code_host, |host| {
            handle_pr_checks(query, bound_pr, host, repo_root)
        }),
        (tiny_http::Method::Get, "/pr/check-logs") => with_code_host(code_host, |host| {
            handle_pr_check_logs(query, bound_pr, host, repo_root)
        }),
        _ => JsonResponse::error(404, "no_route", &format!("no route for {method} {path}")),
    }
}

/// Helper: a PR route can only fire when the bridge has a code host
/// configured. Without one we return 404 — there is no useful action
/// to take. Mirrors `with_bound` for tracker writes.
fn with_code_host(
    host: Option<&dyn crate::code_host::CodeHost>,
    f: impl FnOnce(&dyn crate::code_host::CodeHost) -> JsonResponse,
) -> JsonResponse {
    host.map_or_else(
        || {
            JsonResponse::error(
                404,
                "no_code_host",
                "this bridge was started without a code host; PR routes unavailable",
            )
        },
        f,
    )
}

/// Helper: a write route can only fire when the session has a bound
/// ticket. Without one we return 400 — distinct from 403 (which we'd
/// reserve for a write attempt targeting a *different* ticket if the
/// body ever carried an id; it doesn't today).
fn with_bound(bound: Option<&str>, f: impl FnOnce(&str) -> JsonResponse) -> JsonResponse {
    bound.map_or_else(
        || {
            JsonResponse::error(
                400,
                "no_bound_ticket",
                "this session has no bound ticket; writes are scoped to one",
            )
        },
        f,
    )
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
}

fn handle_comment(
    req: &mut tiny_http::Request,
    bound: &str,
    tracker: &dyn Tracker,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let body = match read_json::<CommentBody>(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match tracker.comment(repo_root, bound, &body.body) {
        Ok(()) => JsonResponse::ok(serde_json::json!({"ok": true})),
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct StatusBody {
    status: Status,
}

fn handle_set_status(
    req: &mut tiny_http::Request,
    bound: &str,
    tracker: &dyn Tracker,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let body = match read_json::<StatusBody>(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match tracker.set_status(repo_root, bound, body.status) {
        Ok(()) => JsonResponse::ok(serde_json::json!({"ok": true})),
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct LabelBody {
    label: String,
}

fn handle_add_label(
    req: &mut tiny_http::Request,
    bound: &str,
    tracker: &dyn Tracker,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let body = match read_json::<LabelBody>(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match tracker.add_label(repo_root, bound, &body.label) {
        Ok(()) => JsonResponse::ok(serde_json::json!({"ok": true})),
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

fn handle_remove_label(
    req: &mut tiny_http::Request,
    bound: &str,
    tracker: &dyn Tracker,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let body = match read_json::<LabelBody>(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match tracker.remove_label(repo_root, bound, &body.label) {
        Ok(()) => JsonResponse::ok(serde_json::json!({"ok": true})),
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

fn handle_read(
    query: Option<&str>,
    bound_ticket: Option<&str>,
    tracker: &dyn Tracker,
    repo_root: &std::path::Path,
) -> JsonResponse {
    // ?id=<id> overrides; otherwise default to the bound ticket.
    let requested = query
        .and_then(|q| query_value(q, "id"))
        .or_else(|| bound_ticket.map(str::to_string));
    let Some(id) = requested else {
        return JsonResponse::error(
            400,
            "no_target",
            "no ?id= query parameter and no bound ticket to default to",
        );
    };
    match tracker.read(repo_root, &id) {
        Ok(detail) => match serde_json::to_value(&detail) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => {
                JsonResponse::error(500, "serde_error", &format!("encoding IssueDetail: {err}"))
            }
        },
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

fn handle_list(tracker: &dyn Tracker, repo_root: &std::path::Path) -> JsonResponse {
    match tracker.list_issues(repo_root) {
        Ok(issues) => match serde_json::to_value(&issues) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => {
                JsonResponse::error(500, "serde_error", &format!("encoding Issue list: {err}"))
            }
        },
        Err(err) => JsonResponse::error(500, "tracker_error", &format!("{err:#}")),
    }
}

/// Resolve which PR number a `/pr/*` route should target. `?number=N`
/// in the query wins; otherwise default to the session's bound PR.
/// Returns `None` when neither is available so the handler can
/// surface 400.
fn resolve_pr_target(query: Option<&str>, bound_pr: Option<u32>) -> Option<u32> {
    query
        .and_then(|q| query_value(q, "number"))
        .and_then(|s| s.parse::<u32>().ok())
        .or(bound_pr)
}

fn handle_pr_read(
    query: Option<&str>,
    bound_pr: Option<u32>,
    host: &dyn crate::code_host::CodeHost,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let Some(number) = resolve_pr_target(query, bound_pr) else {
        return JsonResponse::error(
            400,
            "no_target",
            "no ?number= query parameter and no bound PR to default to",
        );
    };
    match host.read_pr(repo_root, number) {
        Ok(detail) => match serde_json::to_value(&detail) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => {
                JsonResponse::error(500, "serde_error", &format!("encoding PrDetail: {err}"))
            }
        },
        Err(err) => JsonResponse::error(500, "code_host_error", &format!("{err:#}")),
    }
}

fn handle_pr_checks(
    query: Option<&str>,
    bound_pr: Option<u32>,
    host: &dyn crate::code_host::CodeHost,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let Some(number) = resolve_pr_target(query, bound_pr) else {
        return JsonResponse::error(
            400,
            "no_target",
            "no ?number= query parameter and no bound PR to default to",
        );
    };
    match host.pr_checks(repo_root, number) {
        Ok(summary) => match serde_json::to_value(&summary) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => {
                JsonResponse::error(500, "serde_error", &format!("encoding ChecksSummary: {err}"))
            }
        },
        Err(err) => JsonResponse::error(500, "code_host_error", &format!("{err:#}")),
    }
}

fn handle_pr_check_logs(
    query: Option<&str>,
    bound_pr: Option<u32>,
    host: &dyn crate::code_host::CodeHost,
    repo_root: &std::path::Path,
) -> JsonResponse {
    let Some(number) = resolve_pr_target(query, bound_pr) else {
        return JsonResponse::error(
            400,
            "no_target",
            "no ?number= query parameter and no bound PR to default to",
        );
    };
    let Some(check) = query.and_then(|q| query_value(q, "check")) else {
        return JsonResponse::error(
            400,
            "no_check",
            "missing required ?check=<name> query parameter",
        );
    };
    match host.pr_check_logs(repo_root, number, &check) {
        Ok(logs) => JsonResponse::ok(serde_json::json!({ "logs": logs })),
        Err(err) => JsonResponse::error(500, "code_host_error", &format!("{err:#}")),
    }
}

/// JSON body + status code; rendered to a `tiny_http` response by
/// [`respond`].
struct JsonResponse {
    status: u16,
    body: serde_json::Value,
}

impl JsonResponse {
    fn ok(body: serde_json::Value) -> Self {
        Self { status: 200, body }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({"error": {"code": code, "message": message}}),
        }
    }
}

fn respond(req: tiny_http::Request, r: &JsonResponse) {
    let body = r.body.to_string();
    let resp = tiny_http::Response::from_string(body)
        .with_status_code(r.status)
        .with_header(
            "Content-Type: application/json"
                .parse::<tiny_http::Header>()
                .expect("static header parses"),
        );
    if let Err(err) = req.respond(resp) {
        tracing::warn!(?err, "bridge response send failed");
    }
}

/// Read the request body and deserialise as `T`. Bounded by the
/// reader's content-length; `tiny_http` already buffers small bodies
/// in memory.
fn read_json<T: for<'de> Deserialize<'de>>(
    req: &mut tiny_http::Request,
) -> Result<T, JsonResponse> {
    let mut buf = String::new();
    if let Err(err) = req.as_reader().read_to_string(&mut buf) {
        return Err(JsonResponse::error(
            400,
            "body_read_failed",
            &format!("{err}"),
        ));
    }
    serde_json::from_str(&buf).map_err(|err| {
        JsonResponse::error(400, "body_parse_failed", &format!("invalid JSON: {err}"))
    })
}

/// Split `/path?key=value` into `("/path", Some("key=value"))`.
/// Pure for unit testing.
#[must_use]
fn split_path_query(url: &str) -> (&str, Option<&str>) {
    match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    }
}

/// Single-key form parser. Sufficient for the bridge's `?id=<id>`
/// surface; not a general-purpose URL decoder. Pure for unit
/// testing.
#[must_use]
fn query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Constant-time byte slice comparison. Returns false on length
/// mismatch without leaking length via early exit timing.
#[must_use]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Generate a fresh 256-bit bearer token rendered as 64 hex chars.
/// Reads from `/dev/urandom` (POSIX); fleet targets Linux + macOS,
/// both of which expose it.
fn mint_token() -> Result<String> {
    use std::fs::File;
    let mut buf = [0u8; TOKEN_BYTES];
    let mut f = File::open("/dev/urandom").context("opening /dev/urandom for bridge token")?;
    f.read_exact(&mut buf)
        .context("reading bridge token bytes from /dev/urandom")?;
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(buf.len() * 2);
    for b in buf {
        write!(hex, "{b:02x}").expect("writing to String never fails");
    }
    Ok(hex)
}

/// Serialisable view of `Issue` for tests / integration boundaries.
/// Kept inline here rather than in the tracker module because it's
/// the wire shape on `/list`; if the bridge ever grows a richer JSON
/// envelope the wrapper goes here.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ListResponseItem {
    pub id: String,
    pub human_id: String,
    pub title: String,
    pub status: String,
    pub labels: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{IssueContext, SessionId};
    use crate::tracker::{Issue, IssueDetail};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Mock tracker: records every call, returns canned data so tests
    /// can drive the bridge end-to-end without shelling to git-bug / gh.
    struct MockTracker {
        calls: Mutex<Vec<String>>,
        list_response: Mutex<Vec<Issue>>,
        read_response: Mutex<Option<IssueDetail>>,
        fail_writes: Mutex<bool>,
    }

    impl MockTracker {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                list_response: Mutex::new(Vec::new()),
                read_response: Mutex::new(None),
                fail_writes: Mutex::new(false),
            }
        }

        fn record(&self, c: String) {
            self.calls.lock().unwrap().push(c);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Tracker for MockTracker {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn list_issues(&self, _: &std::path::Path) -> Result<Vec<Issue>> {
            self.record("list".into());
            Ok(self.list_response.lock().unwrap().clone())
        }

        fn read(&self, _: &std::path::Path, id: &str) -> Result<IssueDetail> {
            self.record(format!("read({id})"));
            self.read_response
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no read_response set"))
        }

        fn comment(&self, _: &std::path::Path, id: &str, body: &str) -> Result<()> {
            self.record(format!("comment({id}, {body})"));
            if *self.fail_writes.lock().unwrap() {
                Err(anyhow!("forced write failure"))
            } else {
                Ok(())
            }
        }

        fn set_status(&self, _: &std::path::Path, id: &str, status: Status) -> Result<()> {
            self.record(format!("set_status({id}, {status:?})"));
            Ok(())
        }

        fn add_label(&self, _: &std::path::Path, id: &str, label: &str) -> Result<()> {
            self.record(format!("add_label({id}, {label})"));
            Ok(())
        }

        fn remove_label(&self, _: &std::path::Path, id: &str, label: &str) -> Result<()> {
            self.record(format!("remove_label({id}, {label})"));
            Ok(())
        }
    }

    fn sample_session(human_id: Option<&str>) -> Session {
        let mut s = Session::new(SessionId::new("s-test"), "wf", 0);
        if let Some(h) = human_id {
            s.issue = Some(IssueContext {
                id: format!("gh:{h}"),
                human_id: h.into(),
                title: "T".into(),
                labels: Vec::new(),
            });
        }
        s
    }

    fn session_with_pr(number: u32) -> Session {
        let mut s = sample_session(None);
        s.pr = Some(crate::session::PrContext {
            number,
            human_id: format!("pr:{number}"),
            title: "Fix x".into(),
            head_ref: "feat/x".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            url: format!("https://github.com/o/r/pull/{number}"),
        });
        s
    }

    /// Mock code host: records calls + returns canned data. Mirror
    /// of `MockTracker` for the PR side; same minimal-surface ethos.
    struct MockCodeHost {
        calls: Mutex<Vec<String>>,
        detail: Mutex<Option<crate::code_host::PrDetail>>,
        summary: Mutex<Option<crate::code_host::ChecksSummary>>,
        logs: Mutex<Option<String>>,
    }

    impl MockCodeHost {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                detail: Mutex::new(None),
                summary: Mutex::new(None),
                logs: Mutex::new(None),
            }
        }

        fn record(&self, c: String) {
            self.calls.lock().unwrap().push(c);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn with_detail(self, d: crate::code_host::PrDetail) -> Self {
            *self.detail.lock().unwrap() = Some(d);
            self
        }

        fn with_summary(self, s: crate::code_host::ChecksSummary) -> Self {
            *self.summary.lock().unwrap() = Some(s);
            self
        }

        fn with_logs(self, s: &str) -> Self {
            *self.logs.lock().unwrap() = Some(s.to_string());
            self
        }
    }

    impl crate::code_host::CodeHost for MockCodeHost {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn read_pr(&self, _: &std::path::Path, n: u32) -> Result<crate::code_host::PrDetail> {
            self.record(format!("read_pr({n})"));
            self.detail
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no detail set"))
        }
        fn pr_checks(
            &self,
            _: &std::path::Path,
            n: u32,
        ) -> Result<crate::code_host::ChecksSummary> {
            self.record(format!("pr_checks({n})"));
            self.summary
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no summary set"))
        }
        fn pr_check_logs(&self, _: &std::path::Path, n: u32, check: &str) -> Result<String> {
            self.record(format!("pr_check_logs({n}, {check})"));
            self.logs
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no logs set"))
        }
    }

    /// Speak HTTP/1.1 to the bridge by hand. Returns
    /// `(status_code, body)`. Avoids pulling in an HTTP-client dep.
    fn http_call(
        url: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        use std::fmt::Write as _;
        // Strip "http://" and split into "host:port".
        let host_port = url.strip_prefix("http://").expect("http url");
        let mut stream = TcpStream::connect(host_port).expect("connect to bridge");
        let body_bytes = body.unwrap_or("");
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
        if let Some(t) = token {
            writeln!(req, "Authorization: Bearer {t}\r").expect("write to String");
        }
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
        stream.write_all(req.as_bytes()).expect("write request");
        stream.flush().ok();

        let mut reader = BufReader::new(stream);
        // Parse the status line.
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("read status");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .expect("status code present")
            .parse()
            .expect("status is integer");
        // Skip headers until blank line.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header");
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
        }
        // Body to EOF (Connection: close).
        let mut body = String::new();
        reader.read_to_string(&mut body).expect("read body");
        (status, body)
    }

    #[test]
    fn split_path_query_handles_no_query() {
        assert_eq!(split_path_query("/read"), ("/read", None));
        assert_eq!(split_path_query("/read?id=42"), ("/read", Some("id=42")));
    }

    #[test]
    fn query_value_finds_key_or_none() {
        assert_eq!(query_value("id=42&x=1", "id"), Some("42".to_string()));
        assert_eq!(query_value("x=1&id=42", "id"), Some("42".to_string()));
        assert_eq!(query_value("x=1", "id"), None);
    }

    #[test]
    fn constant_time_eq_distinguishes_lengths_and_content() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn mint_token_returns_64_hex_chars() {
        let t = mint_token().unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Two consecutive mints differ; otherwise /dev/urandom isn't.
        let t2 = mint_token().unwrap();
        assert_ne!(t, t2);
    }

    #[test]
    fn bridge_url_for_container_uses_host_containers_internal() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = sample_session(Some("42"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        assert!(
            bridge
                .url_for_container()
                .starts_with("http://host.containers.internal:")
        );
        assert!(bridge.loopback_url().starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn bridge_rejects_request_without_bearer_token() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = sample_session(Some("42"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, _) = http_call(&bridge.loopback_url(), "GET", "/list", None, None);
        assert_eq!(status, 401);
    }

    #[test]
    fn bridge_rejects_wrong_bearer_token() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = sample_session(Some("42"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/list",
            Some("not-the-real-token"),
            None,
        );
        assert_eq!(status, 401);
        assert!(body.contains("bad_token"));
    }

    #[test]
    fn comment_writes_against_bound_ticket_only() {
        let mock = Arc::new(MockTracker::new());
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("42"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "POST",
            "/comment",
            Some(bridge.token()),
            Some(r#"{"body":"hello"}"#),
        );
        assert_eq!(status, 200, "body={body}");
        let calls = mock.calls();
        assert_eq!(calls, vec!["comment(42, hello)".to_string()]);
    }

    #[test]
    fn write_against_session_without_bound_ticket_returns_400() {
        let mock = Arc::new(MockTracker::new());
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(None);
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "POST",
            "/comment",
            Some(bridge.token()),
            Some(r#"{"body":"hi"}"#),
        );
        assert_eq!(status, 400);
        assert!(body.contains("no_bound_ticket"));
        // Tracker was never touched.
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn set_status_decodes_kebab_case_payload() {
        let mock = Arc::new(MockTracker::new());
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("7"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, _) = http_call(
            &bridge.loopback_url(),
            "POST",
            "/set-status",
            Some(bridge.token()),
            Some(r#"{"status":"in-progress"}"#),
        );
        assert_eq!(status, 200);
        assert_eq!(mock.calls(), vec!["set_status(7, InProgress)".to_string()]);
    }

    #[test]
    fn read_defaults_to_bound_ticket_when_no_query_id() {
        let mock = Arc::new(MockTracker::new());
        {
            let mut detail = mock.read_response.lock().unwrap();
            *detail = Some(IssueDetail {
                issue: Issue {
                    id: "gh:5".into(),
                    human_id: "5".into(),
                    title: "T".into(),
                    status: "open".into(),
                    labels: vec![],
                },
                body: "body".into(),
                comments: vec![],
            });
        }
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("5"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/read",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200, "body={body}");
        assert_eq!(mock.calls(), vec!["read(5)".to_string()]);
        assert!(body.contains("\"human_id\":\"5\""));
    }

    #[test]
    fn read_with_id_query_overrides_bound_ticket() {
        let mock = Arc::new(MockTracker::new());
        {
            let mut detail = mock.read_response.lock().unwrap();
            *detail = Some(IssueDetail {
                issue: Issue {
                    id: "gh:9".into(),
                    human_id: "9".into(),
                    title: "T".into(),
                    status: "open".into(),
                    labels: vec![],
                },
                body: String::new(),
                comments: vec![],
            });
        }
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("5"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, _) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/read?id=9",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200);
        // Crucially: read was for 9, not for the bound 5 — reads are
        // unscoped so the agent can pull related-ticket context.
        assert_eq!(mock.calls(), vec!["read(9)".to_string()]);
    }

    #[test]
    fn list_route_returns_serialised_issues() {
        let mock = Arc::new(MockTracker::new());
        {
            let mut list = mock.list_response.lock().unwrap();
            *list = vec![Issue {
                id: "gh:1".into(),
                human_id: "1".into(),
                title: "first".into(),
                status: "open".into(),
                labels: vec!["bug".into()],
            }];
        }
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("1"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/list",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"human_id\":\"1\""));
        assert!(body.contains("\"labels\":[\"bug\"]"));
    }

    #[test]
    fn unknown_route_returns_404() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = sample_session(Some("1"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, _) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/no-such-route",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 404);
    }

    #[test]
    fn tracker_failure_surfaces_as_500() {
        let mock = Arc::new(MockTracker::new());
        *mock.fail_writes.lock().unwrap() = true;
        let tracker: Arc<dyn Tracker> = Arc::clone(&mock) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(Some("1"));
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "POST",
            "/comment",
            Some(bridge.token()),
            Some(r#"{"body":"hi"}"#),
        );
        assert_eq!(status, 500);
        assert!(body.contains("tracker_error"));
    }

    #[test]
    fn pr_route_returns_404_when_no_code_host_configured() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/read",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 404, "body={body}");
        assert!(body.contains("no_code_host"), "got: {body}");
    }

    #[test]
    fn pr_read_route_defaults_to_bound_pr_when_query_omits_number() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let detail = crate::code_host::PrDetail {
            summary: crate::code_host::PrSummary {
                number: 42,
                title: "Fix x".into(),
                head_ref: "feat/x".into(),
                head_sha: "abc".into(),
                base_ref: "main".into(),
                state: crate::code_host::PrState::Open,
                draft: false,
                url: "https://github.com/o/r/pull/42".into(),
                labels: vec![],
            },
            body: "details".into(),
            author: "alice".into(),
        };
        let host = Arc::new(MockCodeHost::new().with_detail(detail));
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/read",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200, "body={body}");
        assert!(body.contains("\"number\":42"), "got: {body}");
        assert_eq!(host.calls(), vec!["read_pr(42)".to_string()]);
    }

    #[test]
    fn pr_read_route_with_explicit_number_overrides_bound_pr() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let detail = crate::code_host::PrDetail {
            summary: crate::code_host::PrSummary {
                number: 7,
                title: "Other PR".into(),
                head_ref: "feat/y".into(),
                head_sha: "def".into(),
                base_ref: "main".into(),
                state: crate::code_host::PrState::Open,
                draft: false,
                url: "https://github.com/o/r/pull/7".into(),
                labels: vec![],
            },
            body: "x".into(),
            author: "bob".into(),
        };
        let host = Arc::new(MockCodeHost::new().with_detail(detail));
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, _) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/read?number=7",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200);
        assert_eq!(host.calls(), vec!["read_pr(7)".to_string()]);
    }

    #[test]
    fn pr_route_400_when_neither_bound_pr_nor_query_number() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let host = Arc::new(MockCodeHost::new());
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = sample_session(None);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/read",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 400);
        assert!(body.contains("no_target"));
    }

    #[test]
    fn pr_checks_route_serialises_summary() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let summary = crate::code_host::ChecksSummary {
            checks: vec![crate::code_host::Check {
                name: "build".into(),
                conclusion: crate::code_host::CheckConclusion::Failure,
                details_url: None,
                run_id: None,
            }],
            has_failures: true,
            failed_count: 1,
            pending_count: 0,
        };
        let host = Arc::new(MockCodeHost::new().with_summary(summary));
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/checks",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200, "body={body}");
        assert!(body.contains("\"has_failures\":true"), "got: {body}");
        assert!(body.contains("\"failed_count\":1"), "got: {body}");
    }

    #[test]
    fn pr_check_logs_route_proxies_to_code_host() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let host = Arc::new(MockCodeHost::new().with_logs("failed: foo\nbar"));
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/check-logs?check=build",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 200, "body={body}");
        assert!(body.contains("failed: foo"), "got: {body}");
        assert_eq!(host.calls(), vec!["pr_check_logs(42, build)".to_string()]);
    }

    #[test]
    fn pr_check_logs_route_400_without_check_parameter() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let host = Arc::new(MockCodeHost::new());
        let host_arc: Arc<dyn crate::code_host::CodeHost> = Arc::clone(&host) as _;
        let dir = tempdir().unwrap();
        let session = session_with_pr(42);
        let bridge =
            Bridge::start(&session, tracker, Some(host_arc), dir.path().to_path_buf()).unwrap();
        let (status, body) = http_call(
            &bridge.loopback_url(),
            "GET",
            "/pr/check-logs",
            Some(bridge.token()),
            None,
        );
        assert_eq!(status, 400);
        assert!(body.contains("no_check"), "got: {body}");
    }

    #[test]
    fn dropping_bridge_stops_listener_promptly() {
        let tracker: Arc<dyn Tracker> = Arc::new(MockTracker::new());
        let dir = tempdir().unwrap();
        let session = sample_session(Some("1"));
        let url;
        {
            let bridge = Bridge::start(&session, tracker, None, dir.path().to_path_buf()).unwrap();
            url = bridge.loopback_url();
        }
        // After drop, connecting to the listener's port should fail.
        let host_port = url.strip_prefix("http://").unwrap();
        // Give the kernel a moment to release the port; the shutdown
        // poll is 50ms.
        std::thread::sleep(Duration::from_millis(200));
        let result =
            TcpStream::connect_timeout(&host_port.parse().unwrap(), Duration::from_millis(200));
        assert!(result.is_err(), "expected connect to fail after drop");
    }
}
