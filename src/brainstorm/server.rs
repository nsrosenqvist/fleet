//! Brainstorm tool server: HTTP localhost listener bound for the
//! duration of a brainstorm session, giving the in-tmux agent
//! access to fleet's plan + tracker + sessions surface through a
//! REST-ish JSON API.
//!
//! Wire-shape mirrors the per-session [`Bridge`](crate::bridge::Bridge):
//! random loopback port, 256-bit bearer token, constant-time auth.
//! The agent receives the URL + token via env vars at tmux-spawn
//! time (`FLEET_BRAINSTORM_URL`, `FLEET_BRAINSTORM_TOKEN`) and uses
//! them on every call.
//!
//! Why a separate server (not the bridge): the bridge is per-
//! workflow-session and per-ticket-scoped. Brainstorm is host-side
//! and unscoped — the agent can read any ticket, create new ones,
//! manage any plan, see any session. Sharing the bridge would have
//! meant relaxing its safety contract; a parallel server with its
//! own life cycle keeps the two stories separate.
//!
//! Plan endpoints (this commit):
//!
//! - `GET  /plan/list`                         → Vec<PlanSummary>
//! - `GET  /plan/show?id=<id>`                 → Plan
//! - `POST /plan/new { name, tickets }`        → { id }
//! - `POST /plan/pause/<id>`
//! - `POST /plan/resume/<id>`
//! - `POST /plan/complete/<id>`
//! - `POST /plan/abandon/<id> { reason? }`
//! - `POST /plan/inject/<id> { ticket, before? }`
//!
//! Tracker / sessions / deps endpoints land in the next commit.
//!
//! Helpers (`JsonResponse`, `mint_token`, `respond`, etc.) are
//! intentionally duplicated from `crate::bridge`'s private surface.
//! A future cleanup commit can extract them into a shared
//! `crate::http_util` module; the duplication keeps this commit
//! self-contained.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::deps::DepsStore;
use crate::plans::store::PlanStore;
use crate::plans::{ClockPlanIdSource, Plan, PlanId, PlanIdSource, PlanItem, PlanState};
use crate::session::SessionId;
use crate::session::now_ms;
use crate::session::store::SessionStore;
use crate::tracker::{Status, Tracker};

/// Token length in bytes; 32 → 256 bits → 64 hex chars.
const TOKEN_BYTES: usize = 32;

/// How often the listener wakes to check the shutdown flag.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// Live handle to a per-brainstorm-session tool server. Listener
/// runs on a dedicated thread; dropping the handle signals
/// shutdown and joins the thread (RAII teardown).
pub struct BrainstormServer {
    join: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    token: String,
    port: u16,
}

impl BrainstormServer {
    /// Start the server bound to `repo_root` with `tracker` as the
    /// host-side tracker handle. The listener derives every store
    /// it needs (plans, deps, sessions) from `repo_root`, so the
    /// caller only has to plumb those two values.
    pub fn start(repo_root: PathBuf, tracker: Arc<dyn Tracker>) -> Result<Self> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| anyhow!("starting brainstorm HTTP server on loopback: {e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow!("brainstorm listener bound to a non-IP address"))?
            .port();
        let token = mint_token().context("minting brainstorm bearer token")?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let join = spawn_listener(
            server,
            token.clone(),
            repo_root,
            tracker,
            Arc::clone(&shutdown),
        );
        Ok(Self {
            join: Some(join),
            shutdown,
            token,
            port,
        })
    }

    /// Loopback URL agents call. Brainstorm runs on the host, not
    /// in a container, so we don't need the bridge's
    /// `host.containers.internal` indirection.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for BrainstormServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Bundle of store handles + the tracker, threaded into each
/// request handler. Built once when the listener thread starts so
/// individual handlers don't have to re-derive paths.
struct ServerCtx {
    plans: PlanStore,
    deps: DepsStore,
    sessions: SessionStore,
    tracker: Arc<dyn Tracker>,
    repo_root: PathBuf,
}

fn spawn_listener(
    server: tiny_http::Server,
    token: String,
    repo_root: PathBuf,
    tracker: Arc<dyn Tracker>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let ctx = ServerCtx {
            plans: PlanStore::for_repo(&repo_root),
            deps: DepsStore::for_repo(&repo_root),
            sessions: SessionStore::for_repo(&repo_root),
            tracker,
            repo_root,
        };
        while !shutdown.load(Ordering::SeqCst) {
            match server.recv_timeout(SHUTDOWN_POLL) {
                Ok(Some(req)) => handle_request(req, &token, &ctx),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(?err, "brainstorm listener recv error; shutting down");
                    break;
                }
            }
        }
    })
}

fn handle_request(mut req: tiny_http::Request, token: &str, ctx: &ServerCtx) {
    if let Err(response) = require_bearer(&req, token) {
        respond(req, &response);
        return;
    }
    let method = req.method().clone();
    let url = req.url().to_string();
    let response = dispatch(&method, &url, &mut req, ctx);
    respond(req, &response);
}

fn dispatch(
    method: &tiny_http::Method,
    url: &str,
    req: &mut tiny_http::Request,
    ctx: &ServerCtx,
) -> JsonResponse {
    let (path, query) = split_path_query(url);
    match (method, path) {
        // ---- plan reads ----
        (tiny_http::Method::Get, "/plan/list") => handle_plan_list(&ctx.plans),
        (tiny_http::Method::Get, "/plan/show") => handle_plan_show(&ctx.plans, query),
        // ---- plan writes ----
        (tiny_http::Method::Post, "/plan/new") => handle_plan_new(&ctx.plans, req),
        (tiny_http::Method::Post, p) if p.starts_with("/plan/pause/") => {
            handle_plan_state_transition(
                &ctx.plans,
                plan_id_from_path(p, "/plan/pause/"),
                PlanState::Paused,
            )
        }
        (tiny_http::Method::Post, p) if p.starts_with("/plan/resume/") => {
            handle_plan_state_transition(
                &ctx.plans,
                plan_id_from_path(p, "/plan/resume/"),
                PlanState::Active,
            )
        }
        (tiny_http::Method::Post, p) if p.starts_with("/plan/complete/") => {
            handle_plan_state_transition(
                &ctx.plans,
                plan_id_from_path(p, "/plan/complete/"),
                PlanState::Completed,
            )
        }
        (tiny_http::Method::Post, p) if p.starts_with("/plan/abandon/") => {
            handle_plan_state_transition(
                &ctx.plans,
                plan_id_from_path(p, "/plan/abandon/"),
                PlanState::Abandoned,
            )
        }
        (tiny_http::Method::Post, p) if p.starts_with("/plan/inject/") => {
            handle_plan_inject(&ctx.plans, plan_id_from_path(p, "/plan/inject/"), req)
        }
        // ---- tracker reads ----
        (tiny_http::Method::Get, "/tracker/list") => handle_tracker_list(ctx),
        (tiny_http::Method::Get, "/tracker/read") => handle_tracker_read(ctx, query),
        // ---- tracker writes ----
        (tiny_http::Method::Post, "/tracker/create") => handle_tracker_create(ctx, req),
        (tiny_http::Method::Post, p) if p.starts_with("/tracker/comment/") => {
            handle_tracker_comment(ctx, last_segment(p, "/tracker/comment/"), req)
        }
        (tiny_http::Method::Post, p) if p.starts_with("/tracker/set-status/") => {
            handle_tracker_set_status(ctx, last_segment(p, "/tracker/set-status/"), req)
        }
        (tiny_http::Method::Post, p) if p.starts_with("/tracker/add-label/") => {
            handle_tracker_add_label(ctx, last_segment(p, "/tracker/add-label/"), req)
        }
        (tiny_http::Method::Post, p) if p.starts_with("/tracker/remove-label/") => {
            handle_tracker_remove_label(ctx, last_segment(p, "/tracker/remove-label/"), req)
        }
        // ---- sessions ----
        (tiny_http::Method::Get, "/sessions/list") => handle_sessions_list(&ctx.sessions),
        (tiny_http::Method::Get, "/sessions/show") => handle_sessions_show(&ctx.sessions, query),
        (tiny_http::Method::Post, p) if p.starts_with("/sessions/unblock/") => {
            handle_sessions_unblock(ctx, last_segment(p, "/sessions/unblock/"), req)
        }
        // ---- deps ----
        (tiny_http::Method::Get, "/deps/list") => handle_deps_list(&ctx.deps),
        (tiny_http::Method::Post, "/deps/remove") => handle_deps_remove(&ctx.deps, req),
        _ => JsonResponse::error(404, "no_route", &format!("no route for {method:?} {path}")),
    }
}

// === Plan handlers ===

#[derive(Serialize)]
struct PlanSummary {
    id: String,
    name: String,
    state: String,
    completed: usize,
    total: usize,
}

fn handle_plan_list(plans: &PlanStore) -> JsonResponse {
    let ids = match plans.list() {
        Ok(ids) => ids,
        Err(err) => return store_error(&err, "list plans"),
    };
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match plans.load(&id) {
            Ok(plan) => {
                let (completed, total) = plan.progress();
                out.push(PlanSummary {
                    id: plan.id.as_str().to_string(),
                    name: plan.name,
                    state: plan_state_word(plan.state).to_string(),
                    completed,
                    total,
                });
            }
            Err(err) => {
                tracing::warn!(plan = %id, error = %err, "brainstorm: skipping unreadable plan in /plan/list");
            }
        }
    }
    JsonResponse::ok(serde_json::json!({ "plans": out }))
}

fn handle_plan_show(plans: &PlanStore, query: Option<&str>) -> JsonResponse {
    let Some(id) = query.and_then(|q| query_value(q, "id")) else {
        return JsonResponse::error(400, "missing_id", "GET /plan/show requires `?id=<plan-id>`");
    };
    match plans.load(&PlanId::new(id)) {
        Ok(plan) => match serde_json::to_value(&plan) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => store_error(&err, "load plan"),
    }
}

#[derive(Deserialize)]
struct PlanNewRequest {
    name: String,
    tickets: Vec<String>,
}

fn handle_plan_new(plans: &PlanStore, req: &mut tiny_http::Request) -> JsonResponse {
    let body: PlanNewRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if body.name.trim().is_empty() {
        return JsonResponse::error(400, "empty_name", "plan `name` must be non-empty");
    }
    if body.tickets.is_empty() {
        return JsonResponse::error(400, "empty_tickets", "plan must list at least one ticket");
    }
    let id = ClockPlanIdSource.mint();
    let plan = Plan::new(id.clone(), body.name, body.tickets, now_ms());
    match plans.create(&plan) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "id": id.as_str() })),
        Err(err) => store_error(&err, "create plan"),
    }
}

fn handle_plan_state_transition(
    plans: &PlanStore,
    plan_id: Option<String>,
    target: PlanState,
) -> JsonResponse {
    let Some(id) = plan_id else {
        return JsonResponse::error(400, "missing_id", "path needs a plan id segment");
    };
    let pid = PlanId::new(id);
    let mut plan = match plans.load(&pid) {
        Ok(p) => p,
        Err(err) => return store_error(&err, "load plan"),
    };
    plan.state = target;
    plan.updated_at_ms = now_ms();
    match plans.save(&plan) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => store_error(&err, "save plan"),
    }
}

#[derive(Deserialize)]
struct PlanInjectRequest {
    ticket: String,
    #[serde(default)]
    before: Option<String>,
}

fn handle_plan_inject(
    plans: &PlanStore,
    plan_id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = plan_id else {
        return JsonResponse::error(400, "missing_id", "path needs a plan id segment");
    };
    let body: PlanInjectRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let pid = PlanId::new(id);
    let mut plan = match plans.load(&pid) {
        Ok(p) => p,
        Err(err) => return store_error(&err, "load plan"),
    };
    let position = match body.before {
        Some(before) => match plan.position_of(&before) {
            Some(p) => p,
            None => {
                return JsonResponse::error(
                    404,
                    "before_not_found",
                    &format!("plan has no item with ticket id `{before}`"),
                );
            }
        },
        None => plan.items.len(),
    };
    plan.items.insert(position, PlanItem::pending(&body.ticket));
    plan.updated_at_ms = now_ms();
    match plans.save(&plan) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => store_error(&err, "save plan"),
    }
}

// === Tracker handlers ===

fn handle_tracker_list(ctx: &ServerCtx) -> JsonResponse {
    match ctx.tracker.list_issues(&ctx.repo_root) {
        Ok(issues) => match serde_json::to_value(&issues) {
            Ok(v) => JsonResponse::ok(serde_json::json!({ "issues": v })),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => tracker_error(&err, "list issues"),
    }
}

fn handle_tracker_read(ctx: &ServerCtx, query: Option<&str>) -> JsonResponse {
    let Some(id) = query.and_then(|q| query_value(q, "id")) else {
        return JsonResponse::error(
            400,
            "missing_id",
            "GET /tracker/read requires `?id=<ticket-id>`",
        );
    };
    match ctx.tracker.read(&ctx.repo_root, &id) {
        Ok(detail) => match serde_json::to_value(&detail) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => tracker_error(&err, "read ticket"),
    }
}

#[derive(Deserialize)]
struct TrackerCreateRequest {
    title: String,
    body: String,
    #[serde(default)]
    labels: Vec<String>,
}

fn handle_tracker_create(ctx: &ServerCtx, req: &mut tiny_http::Request) -> JsonResponse {
    let body: TrackerCreateRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if body.title.trim().is_empty() {
        return JsonResponse::error(400, "empty_title", "tracker create needs a non-empty title");
    }
    match ctx
        .tracker
        .create(&ctx.repo_root, &body.title, &body.body, &body.labels)
    {
        Ok(issue) => match serde_json::to_value(&issue) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => tracker_error(&err, "create ticket"),
    }
}

#[derive(Deserialize)]
struct CommentRequest {
    body: String,
}

fn handle_tracker_comment(
    ctx: &ServerCtx,
    id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = id else {
        return JsonResponse::error(400, "missing_id", "path needs a ticket id segment");
    };
    let body: CommentRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match ctx.tracker.comment(&ctx.repo_root, &id, &body.body) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => tracker_error(&err, "comment"),
    }
}

#[derive(Deserialize)]
struct SetStatusRequest {
    status: String,
}

fn handle_tracker_set_status(
    ctx: &ServerCtx,
    id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = id else {
        return JsonResponse::error(400, "missing_id", "path needs a ticket id segment");
    };
    let body: SetStatusRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let status = match body.status.as_str() {
        "open" => Status::Open,
        "in-progress" => Status::InProgress,
        "closed" => Status::Closed,
        other => {
            return JsonResponse::error(
                400,
                "bad_status",
                &format!("unknown status `{other}` (allowed: open / in-progress / closed)"),
            );
        }
    };
    match ctx.tracker.set_status(&ctx.repo_root, &id, status) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => tracker_error(&err, "set status"),
    }
}

#[derive(Deserialize)]
struct LabelRequest {
    label: String,
}

fn handle_tracker_add_label(
    ctx: &ServerCtx,
    id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = id else {
        return JsonResponse::error(400, "missing_id", "path needs a ticket id segment");
    };
    let body: LabelRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match ctx.tracker.add_label(&ctx.repo_root, &id, &body.label) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => tracker_error(&err, "add label"),
    }
}

fn handle_tracker_remove_label(
    ctx: &ServerCtx,
    id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = id else {
        return JsonResponse::error(400, "missing_id", "path needs a ticket id segment");
    };
    let body: LabelRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match ctx.tracker.remove_label(&ctx.repo_root, &id, &body.label) {
        Ok(()) => JsonResponse::ok(serde_json::json!({ "ok": true })),
        Err(err) => tracker_error(&err, "remove label"),
    }
}

// === Sessions handlers ===

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    workflow: String,
    state: String,
    issue_human_id: Option<String>,
}

fn handle_sessions_list(sessions: &SessionStore) -> JsonResponse {
    let ids = match sessions.list() {
        Ok(ids) => ids,
        Err(err) => return store_error(&err, "list sessions"),
    };
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match sessions.load(&id) {
            Ok(s) => out.push(SessionSummary {
                id: s.id.as_str().to_string(),
                workflow: s.workflow,
                state: format!("{:?}", s.state).to_lowercase(),
                issue_human_id: s.issue.map(|i| i.human_id),
            }),
            Err(err) => {
                tracing::warn!(session = %id, error = %err, "brainstorm: skipping unreadable session")
            }
        }
    }
    JsonResponse::ok(serde_json::json!({ "sessions": out }))
}

fn handle_sessions_show(sessions: &SessionStore, query: Option<&str>) -> JsonResponse {
    let Some(id) = query.and_then(|q| query_value(q, "id")) else {
        return JsonResponse::error(
            400,
            "missing_id",
            "GET /sessions/show requires `?id=<session-id>`",
        );
    };
    match sessions.load(&SessionId::new(id)) {
        Ok(session) => match serde_json::to_value(&session) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => store_error(&err, "load session"),
    }
}

#[derive(Deserialize, Default)]
struct UnblockRequest {
    #[serde(default)]
    reason: Option<String>,
}

fn handle_sessions_unblock(
    ctx: &ServerCtx,
    id: Option<String>,
    req: &mut tiny_http::Request,
) -> JsonResponse {
    let Some(id) = id else {
        return JsonResponse::error(400, "missing_id", "path needs a session id segment");
    };
    // Empty body is acceptable for the no-reason form, so a body-
    // parse failure falls back to the default rather than 400.
    let body: UnblockRequest = read_json(req).unwrap_or_default();
    let session_id = SessionId::new(id);
    let tracker_opt: Option<&dyn Tracker> = if body.reason.is_some() {
        Some(ctx.tracker.as_ref())
    } else {
        None
    };
    match crate::cli::sessions::unblock_session(
        &ctx.sessions,
        &ctx.deps,
        tracker_opt,
        &ctx.repo_root,
        &session_id,
        body.reason.as_deref(),
    ) {
        Ok(outcome) => JsonResponse::ok(serde_json::json!({
            "ticket_id": outcome.ticket_id,
            "edges_removed": outcome.edges_removed,
            "comment_posted": outcome.comment_posted,
        })),
        Err(err) => JsonResponse::error(500, "unblock_failed", &format!("{err:#}")),
    }
}

// === Deps handlers ===

fn handle_deps_list(deps: &DepsStore) -> JsonResponse {
    match deps.load() {
        Ok(doc) => match serde_json::to_value(&doc) {
            Ok(v) => JsonResponse::ok(v),
            Err(err) => JsonResponse::error(500, "serialize_failed", &format!("{err}")),
        },
        Err(err) => store_error(&err, "load deps"),
    }
}

#[derive(Deserialize)]
struct DepsRemoveRequest {
    blocked: String,
    blocked_on: String,
}

fn handle_deps_remove(deps: &DepsStore, req: &mut tiny_http::Request) -> JsonResponse {
    let body: DepsRemoveRequest = match read_json(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let doc = match deps.load() {
        Ok(d) => d,
        Err(err) => return store_error(&err, "load deps"),
    };
    let before = doc.edges.len();
    let mut new_doc = doc;
    new_doc
        .edges
        .retain(|e| !(e.blocked == body.blocked && e.blocked_on == body.blocked_on));
    let removed = before - new_doc.edges.len();
    if removed > 0
        && let Err(err) = deps.save(&new_doc)
    {
        return store_error(&err, "save deps");
    }
    JsonResponse::ok(serde_json::json!({ "removed": removed }))
}

fn tracker_error(err: &anyhow::Error, action: &str) -> JsonResponse {
    JsonResponse::error(500, "tracker_error", &format!("{action}: {err:#}"))
}

// === Path helpers ===

#[must_use]
fn plan_id_from_path(path: &str, prefix: &str) -> Option<String> {
    let stripped = path.strip_prefix(prefix)?;
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

#[must_use]
fn last_segment(path: &str, prefix: &str) -> Option<String> {
    let stripped = path.strip_prefix(prefix)?;
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

#[must_use]
fn plan_state_word(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "active",
        PlanState::Paused => "paused",
        PlanState::Completed => "completed",
        PlanState::Abandoned => "abandoned",
    }
}

// === HTTP helpers (duplicated from bridge; future cleanup) ===

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

fn store_error(err: &anyhow::Error, action: &str) -> JsonResponse {
    JsonResponse::error(500, "store_error", &format!("{action}: {err:#}"))
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
        tracing::warn!(?err, "brainstorm response send failed");
    }
}

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
            "invalid brainstorm bearer token",
        ))
    }
}

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

#[must_use]
fn split_path_query(url: &str) -> (&str, Option<&str>) {
    match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    }
}

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

fn mint_token() -> Result<String> {
    use std::fs::File;
    let mut buf = [0u8; TOKEN_BYTES];
    let mut f = File::open("/dev/urandom").context("opening /dev/urandom for brainstorm token")?;
    f.read_exact(&mut buf)
        .context("reading brainstorm token bytes from /dev/urandom")?;
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(buf.len() * 2);
    for b in buf {
        write!(hex, "{b:02x}").expect("writing to String never fails");
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// Speak HTTP/1.1 to the brainstorm server by hand.
    /// Returns `(status_code, body)`. Borrowed shape from the
    /// bridge tests so the assertions read identically.
    fn http_call(
        url: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        use std::fmt::Write as _;
        let host_port = url.strip_prefix("http://").expect("http url");
        let mut stream = TcpStream::connect(host_port).expect("connect to brainstorm");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
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
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("read status");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .expect("status code")
            .parse()
            .expect("status is integer");
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header");
            if line.trim().is_empty() {
                break;
            }
        }
        let mut body = String::new();
        let _ = reader.read_to_string(&mut body);
        (status, body)
    }

    /// Mock tracker used by every endpoint test. Records calls,
    /// returns canned issues from `list`/`read`/`create`.
    struct MockTracker {
        calls: std::sync::Mutex<Vec<String>>,
        next_create: std::sync::Mutex<Option<crate::tracker::Issue>>,
    }

    impl MockTracker {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_create: std::sync::Mutex::new(None),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Tracker for MockTracker {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn list_issues(&self, _: &std::path::Path) -> Result<Vec<crate::tracker::Issue>> {
            self.calls.lock().unwrap().push("list_issues".into());
            Ok(vec![crate::tracker::Issue {
                id: "gh:42".into(),
                human_id: "42".into(),
                title: "Sample".into(),
                status: "open".into(),
                labels: Vec::new(),
            }])
        }
        fn read(&self, _: &std::path::Path, id: &str) -> Result<crate::tracker::IssueDetail> {
            self.calls.lock().unwrap().push(format!("read({id})"));
            Ok(crate::tracker::IssueDetail {
                issue: crate::tracker::Issue {
                    id: format!("gh:{id}"),
                    human_id: id.to_string(),
                    title: "T".into(),
                    status: "open".into(),
                    labels: Vec::new(),
                },
                body: "body".into(),
                comments: Vec::new(),
            })
        }
        fn create(
            &self,
            _: &std::path::Path,
            title: &str,
            body: &str,
            labels: &[String],
        ) -> Result<crate::tracker::Issue> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create({title:?}, {body:?}, {labels:?})"));
            self.next_create
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no next_create configured"))
        }
        fn comment(&self, _: &std::path::Path, id: &str, body: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("comment({id}, {body:?})"));
            Ok(())
        }
        fn set_status(&self, _: &std::path::Path, id: &str, status: Status) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_status({id}, {status:?})"));
            Ok(())
        }
        fn add_label(&self, _: &std::path::Path, id: &str, label: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("add_label({id}, {label})"));
            Ok(())
        }
        fn remove_label(&self, _: &std::path::Path, id: &str, label: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove_label({id}, {label})"));
            Ok(())
        }
    }

    fn fixture() -> (tempfile::TempDir, BrainstormServer, Arc<MockTracker>) {
        fixture_with_tracker_response(None)
    }

    fn fixture_with_tracker_response(
        next_create: Option<crate::tracker::Issue>,
    ) -> (tempfile::TempDir, BrainstormServer, Arc<MockTracker>) {
        let dir = tempfile::tempdir().unwrap();
        let plans_root = dir.path().join(".fleet/plans");
        std::fs::create_dir_all(&plans_root).unwrap();
        let tracker = Arc::new(MockTracker::new());
        if let Some(issue) = next_create {
            *tracker.next_create.lock().unwrap() = Some(issue);
        }
        let server = BrainstormServer::start(
            dir.path().to_path_buf(),
            Arc::clone(&tracker) as Arc<dyn Tracker>,
        )
        .unwrap();
        (dir, server, tracker)
    }

    #[test]
    fn rejects_request_without_bearer_token() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(&server.url(), "GET", "/plan/list", None, None);
        assert_eq!(status, 401);
        assert!(body.contains("missing_authorization"), "body: {body}");
    }

    #[test]
    fn rejects_request_with_wrong_bearer_token() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/plan/list",
            Some("not-the-right-token"),
            None,
        );
        assert_eq!(status, 401);
        assert!(body.contains("bad_token"), "body: {body}");
    }

    #[test]
    fn plan_list_returns_empty_array_for_fresh_repo() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/plan/list",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"plans\":[]"), "body: {body}");
    }

    #[test]
    fn plan_new_then_list_round_trips() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"Parser refactor","tickets":["42","43"]}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 200, "body: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().expect("id field").to_string();
        assert!(id.starts_with("plan-"), "got id: {id}");

        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/plan/list",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("Parser refactor"), "body: {body}");
        assert!(body.contains("\"completed\":0"), "body: {body}");
        assert!(body.contains("\"total\":2"), "body: {body}");
        assert!(body.contains("\"state\":\"active\""), "body: {body}");
    }

    #[test]
    fn plan_new_rejects_empty_name() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"","tickets":["42"]}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 400, "body: {body}");
        assert!(body.contains("empty_name"), "body: {body}");
    }

    #[test]
    fn plan_new_rejects_empty_tickets() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"x","tickets":[]}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 400);
        assert!(body.contains("empty_tickets"), "body: {body}");
    }

    #[test]
    fn plan_show_loads_a_specific_plan() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"P","tickets":["1","2","3"]}"#;
        let (_, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap();

        let path = format!("/plan/show?id={id}");
        let (status, body) = http_call(&server.url(), "GET", &path, Some(server.token()), None);
        assert_eq!(status, 200);
        assert!(body.contains("\"name\":\"P\""), "body: {body}");
        assert!(body.contains("\"ticket_id\":\"2\""), "body: {body}");
    }

    #[test]
    fn plan_show_errors_when_id_query_param_missing() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/plan/show",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 400);
        assert!(body.contains("missing_id"), "body: {body}");
    }

    #[test]
    fn plan_pause_resume_round_trip_changes_state() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"x","tickets":["42"]}"#;
        let (_, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let pause = format!("/plan/pause/{id}");
        let (status, _) = http_call(&server.url(), "POST", &pause, Some(server.token()), None);
        assert_eq!(status, 200);
        // List should now show paused.
        let (_, body) = http_call(
            &server.url(),
            "GET",
            "/plan/list",
            Some(server.token()),
            None,
        );
        assert!(body.contains("\"state\":\"paused\""), "body: {body}");

        let resume = format!("/plan/resume/{id}");
        let (status, _) = http_call(&server.url(), "POST", &resume, Some(server.token()), None);
        assert_eq!(status, 200);
        let (_, body) = http_call(
            &server.url(),
            "GET",
            "/plan/list",
            Some(server.token()),
            None,
        );
        assert!(body.contains("\"state\":\"active\""), "body: {body}");
    }

    #[test]
    fn plan_inject_appends_by_default() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"x","tickets":["42","43"]}"#;
        let (_, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let path = format!("/plan/inject/{id}");
        let (status, _) = http_call(
            &server.url(),
            "POST",
            &path,
            Some(server.token()),
            Some(r#"{"ticket":"44"}"#),
        );
        assert_eq!(status, 200);
        let show_path = format!("/plan/show?id={id}");
        let (_, body) = http_call(&server.url(), "GET", &show_path, Some(server.token()), None);
        // 44 should be the last item (appended).
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["ticket_id"].as_str(), Some("44"));
    }

    #[test]
    fn plan_inject_inserts_before_named_item() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"x","tickets":["42","43"]}"#;
        let (_, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let path = format!("/plan/inject/{id}");
        let (status, _) = http_call(
            &server.url(),
            "POST",
            &path,
            Some(server.token()),
            Some(r#"{"ticket":"99","before":"43"}"#),
        );
        assert_eq!(status, 200);
        let show_path = format!("/plan/show?id={id}");
        let (_, body) = http_call(&server.url(), "GET", &show_path, Some(server.token()), None);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = v["items"].as_array().unwrap();
        // Order: 42, 99, 43.
        assert_eq!(items[0]["ticket_id"].as_str(), Some("42"));
        assert_eq!(items[1]["ticket_id"].as_str(), Some("99"));
        assert_eq!(items[2]["ticket_id"].as_str(), Some("43"));
    }

    #[test]
    fn plan_inject_errors_when_before_id_not_found() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"name":"x","tickets":["42"]}"#;
        let (_, body) = http_call(
            &server.url(),
            "POST",
            "/plan/new",
            Some(server.token()),
            Some(req),
        );
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let path = format!("/plan/inject/{id}");
        let (status, body) = http_call(
            &server.url(),
            "POST",
            &path,
            Some(server.token()),
            Some(r#"{"ticket":"99","before":"nope"}"#),
        );
        assert_eq!(status, 404, "body: {body}");
        assert!(body.contains("before_not_found"), "body: {body}");
    }

    #[test]
    fn unknown_route_returns_404() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/no/such/route",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 404);
        assert!(body.contains("no_route"), "body: {body}");
    }

    #[test]
    fn split_path_query_handles_no_query() {
        assert_eq!(split_path_query("/foo"), ("/foo", None));
        assert_eq!(split_path_query("/foo?a=b"), ("/foo", Some("a=b")));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn mint_token_returns_64_hex_chars() {
        let t = mint_token().unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn plan_id_from_path_extracts_segment() {
        assert_eq!(
            plan_id_from_path("/plan/pause/plan-1", "/plan/pause/"),
            Some("plan-1".to_string())
        );
        assert_eq!(plan_id_from_path("/plan/pause/", "/plan/pause/"), None);
        assert_eq!(plan_id_from_path("/plan/other", "/plan/pause/"), None);
    }

    // ---- tracker / sessions / deps endpoints --------------------------

    #[test]
    fn tracker_list_returns_canned_issues_from_mock() {
        let (_dir, server, tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/tracker/list",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"human_id\":\"42\""), "body: {body}");
        assert!(tracker.calls().contains(&"list_issues".to_string()));
    }

    #[test]
    fn tracker_read_passes_id_through_to_tracker() {
        let (_dir, server, tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/tracker/read?id=87",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"human_id\":\"87\""), "body: {body}");
        assert!(tracker.calls().iter().any(|c| c == "read(87)"));
    }

    #[test]
    fn tracker_read_errors_when_id_missing() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/tracker/read",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 400);
        assert!(body.contains("missing_id"), "body: {body}");
    }

    #[test]
    fn tracker_create_returns_canned_issue() {
        let issue = crate::tracker::Issue {
            id: "gh:101".into(),
            human_id: "101".into(),
            title: "Fresh".into(),
            status: "open".into(),
            labels: Vec::new(),
        };
        let (_dir, server, tracker) = fixture_with_tracker_response(Some(issue));
        let req = r#"{"title":"Fresh","body":"new ticket","labels":["needs-impl"]}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/tracker/create",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("\"human_id\":\"101\""), "body: {body}");
        let calls = tracker.calls();
        assert!(calls.iter().any(|c| c.starts_with("create(")));
    }

    #[test]
    fn tracker_create_rejects_empty_title() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/tracker/create",
            Some(server.token()),
            Some(r#"{"title":"","body":"x"}"#),
        );
        assert_eq!(status, 400);
        assert!(body.contains("empty_title"), "body: {body}");
    }

    #[test]
    fn tracker_comment_posts_to_the_named_ticket() {
        let (_dir, server, tracker) = fixture();
        let (status, _) = http_call(
            &server.url(),
            "POST",
            "/tracker/comment/87",
            Some(server.token()),
            Some(r#"{"body":"hi from brainstorm"}"#),
        );
        assert_eq!(status, 200);
        assert!(
            tracker
                .calls()
                .iter()
                .any(|c| c.contains("comment(87") && c.contains("hi from brainstorm")),
            "calls: {:?}",
            tracker.calls()
        );
    }

    #[test]
    fn tracker_set_status_maps_recognised_strings() {
        let (_dir, server, tracker) = fixture();
        for (s, marker) in [
            ("open", "Open"),
            ("in-progress", "InProgress"),
            ("closed", "Closed"),
        ] {
            let body = format!(r#"{{"status":"{s}"}}"#);
            let (status, _) = http_call(
                &server.url(),
                "POST",
                "/tracker/set-status/42",
                Some(server.token()),
                Some(&body),
            );
            assert_eq!(status, 200, "{s}");
            assert!(
                tracker
                    .calls()
                    .iter()
                    .any(|c| c.contains(&format!("set_status(42, {marker}"))),
                "calls: {:?}",
                tracker.calls()
            );
        }
    }

    #[test]
    fn tracker_set_status_rejects_unknown_value() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/tracker/set-status/42",
            Some(server.token()),
            Some(r#"{"status":"frozen"}"#),
        );
        assert_eq!(status, 400);
        assert!(body.contains("bad_status"), "body: {body}");
    }

    #[test]
    fn tracker_add_remove_label_round_trip() {
        let (_dir, server, tracker) = fixture();
        let (status, _) = http_call(
            &server.url(),
            "POST",
            "/tracker/add-label/42",
            Some(server.token()),
            Some(r#"{"label":"needs-review"}"#),
        );
        assert_eq!(status, 200);
        let (status, _) = http_call(
            &server.url(),
            "POST",
            "/tracker/remove-label/42",
            Some(server.token()),
            Some(r#"{"label":"needs-review"}"#),
        );
        assert_eq!(status, 200);
        let calls = tracker.calls();
        assert!(calls.iter().any(|c| c == "add_label(42, needs-review)"));
        assert!(calls.iter().any(|c| c == "remove_label(42, needs-review)"));
    }

    #[test]
    fn sessions_list_returns_empty_for_fresh_repo() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/sessions/list",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"sessions\":[]"), "body: {body}");
    }

    #[test]
    fn deps_list_returns_empty_doc_for_fresh_repo() {
        let (_dir, server, _tracker) = fixture();
        let (status, body) = http_call(
            &server.url(),
            "GET",
            "/deps/list",
            Some(server.token()),
            None,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"version\":1"), "body: {body}");
        assert!(body.contains("\"edges\":[]"), "body: {body}");
    }

    #[test]
    fn deps_remove_drops_only_matching_edges() {
        let (dir, server, _tracker) = fixture();
        // Seed two edges on disk.
        let deps_path = dir.path().join(".fleet/deps.json");
        std::fs::create_dir_all(deps_path.parent().unwrap()).unwrap();
        std::fs::write(
            &deps_path,
            r#"{"version":1,"edges":[
                {"blocked":"42","blocked_on":"43","reason":"ticket","created_at_ms":1},
                {"blocked":"42","blocked_on":"44","reason":"ticket","created_at_ms":2}
            ]}"#,
        )
        .unwrap();
        let req = r#"{"blocked":"42","blocked_on":"43"}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/deps/remove",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"removed\":1"), "body: {body}");
        // Round-trip: list now has only the surviving edge.
        let (_, list_body) = http_call(
            &server.url(),
            "GET",
            "/deps/list",
            Some(server.token()),
            None,
        );
        assert!(
            list_body.contains("\"blocked_on\":\"44\""),
            "body: {list_body}"
        );
        assert!(
            !list_body.contains("\"blocked_on\":\"43\""),
            "body: {list_body}"
        );
    }

    #[test]
    fn deps_remove_returns_zero_when_no_matching_edge() {
        let (_dir, server, _tracker) = fixture();
        let req = r#"{"blocked":"999","blocked_on":"888"}"#;
        let (status, body) = http_call(
            &server.url(),
            "POST",
            "/deps/remove",
            Some(server.token()),
            Some(req),
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"removed\":0"), "body: {body}");
    }
}
