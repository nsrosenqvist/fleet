//! End-to-end smoke for the `fleet-tracker` binary against a real
//! `tiny_http` listener pretending to be the bridge.
//!
//! Unit tests in `src/bridge/mod.rs` exercise the bridge from the
//! host side with a hand-rolled HTTP client; this file flips the
//! direction — a fake bridge stands up, fleet-tracker is invoked as
//! a subprocess (the actual built binary, via `CARGO_BIN_EXE_*`), and
//! we assert what it sent on the wire and what exit code it
//! returned. Together they verify the contract from both ends.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// One captured request the fake bridge received.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

/// Start a minimal HTTP server on loopback that records every
/// request and returns a fixed response. Returns the URL plus a
/// shared handle to the captured-requests log. The server thread
/// runs until the test process exits — there's no `JoinHandle` dance
/// because tests are short-lived and `tiny_http` drops cleanly when
/// the binding goes out of scope at process teardown.
fn start_fake_bridge(
    status: u16,
    response_body: &'static str,
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    // tiny_http binds directly to port 0; ask the server for the
    // resolved port. Avoids the bind-then-rebind race that hit us
    // under cargo test's default parallelism.
    let server = tiny_http::Server::http("127.0.0.1:0").expect("start tiny_http server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("server bound to an IP")
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let captured = Arc::new(Mutex::new(Vec::<Captured>::new()));
    let captured_for_thread = Arc::clone(&captured);
    thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let method = req.method().as_str().to_string();
            let path = req.url().to_string();
            let auth = req
                .headers()
                .iter()
                .find(|h| {
                    h.field
                        .as_str()
                        .as_str()
                        .eq_ignore_ascii_case("authorization")
                })
                .map(|h| h.value.as_str().to_string());
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).ok();
            captured_for_thread.lock().unwrap().push(Captured {
                method,
                path,
                auth,
                body,
            });
            let resp = tiny_http::Response::from_string(response_body).with_status_code(status);
            let _ = req.respond(resp);
        }
    });
    (url, captured)
}

/// Invoke the built `fleet-tracker` binary with env + args, capture
/// its exit + stdout/stderr. Cargo plumbs the binary path via
/// `CARGO_BIN_EXE_<name>` for tests of bins in the same crate.
fn run_fleet_tracker(url: &str, token: &str, args: &[&str]) -> (i32, String, String) {
    let exe = env!("CARGO_BIN_EXE_fleet-tracker");
    let output = Command::new(exe)
        .args(args)
        .env("FLEET_BRIDGE_URL", url)
        .env("FLEET_BRIDGE_TOKEN", token)
        // Don't inherit a system-wide HTTP_PROXY — would route the
        // request through it and break the test on hosts where one
        // is set (e.g. workplace machines).
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .output()
        .expect("spawning fleet-tracker");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Wait for the fake bridge to record at least `n` captured
/// requests. Bounded by a short timeout so a misrouted request fails
/// the test rather than hanging CI.
fn wait_for_capture(captured: &Mutex<Vec<Captured>>, n: usize) -> Vec<Captured> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = captured.lock().unwrap();
            if guard.len() >= n {
                return guard.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {n} captured request(s)"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn comment_subcommand_posts_to_comment_route_with_bearer() {
    let (url, captured) = start_fake_bridge(200, r#"{"ok":true}"#);
    let (code, _stdout, _stderr) = run_fleet_tracker(&url, "tok-abc", &["comment", "hello world"]);
    assert_eq!(code, 0);
    let reqs = wait_for_capture(&captured, 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/comment");
    assert_eq!(reqs[0].auth.as_deref(), Some("Bearer tok-abc"));
    assert!(
        reqs[0].body.contains("\"body\":\"hello world\""),
        "body = {}",
        reqs[0].body
    );
}

#[test]
fn status_subcommand_renders_kebab_case() {
    let (url, captured) = start_fake_bridge(200, r#"{"ok":true}"#);
    let (code, _, _) = run_fleet_tracker(&url, "t", &["status", "in-progress"]);
    assert_eq!(code, 0);
    let reqs = wait_for_capture(&captured, 1);
    assert_eq!(reqs[0].path, "/set-status");
    assert!(
        reqs[0].body.contains("\"status\":\"in-progress\""),
        "body = {}",
        reqs[0].body
    );
}

#[test]
fn read_without_id_hits_bare_read_route() {
    let (url, captured) = start_fake_bridge(200, r#"{"issue":{"id":"gh:1","human_id":"1"}}"#);
    let (code, stdout, _) = run_fleet_tracker(&url, "t", &["read"]);
    assert_eq!(code, 0);
    let reqs = wait_for_capture(&captured, 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/read");
    // Response body lands on the caller's stdout for jq-piping.
    assert!(stdout.contains("\"human_id\":\"1\""));
}

#[test]
fn read_with_id_appends_query_param() {
    let (url, captured) = start_fake_bridge(200, "{}");
    let (_code, _, _) = run_fleet_tracker(&url, "t", &["read", "42"]);
    let reqs = wait_for_capture(&captured, 1);
    assert_eq!(reqs[0].path, "/read?id=42");
}

#[test]
fn list_returns_response_body_to_stdout() {
    let (url, captured) = start_fake_bridge(200, r#"[{"id":"gh:1","human_id":"1","title":"x"}]"#);
    let (code, stdout, _) = run_fleet_tracker(&url, "t", &["list"]);
    assert_eq!(code, 0);
    let reqs = wait_for_capture(&captured, 1);
    assert_eq!(reqs[0].path, "/list");
    assert!(stdout.contains("\"human_id\":\"1\""));
}

#[test]
fn http_400_maps_to_exit_code_2() {
    let (url, _) = start_fake_bridge(400, r#"{"error":"bad input"}"#);
    let (code, _, _) = run_fleet_tracker(&url, "t", &["comment", "hi"]);
    assert_eq!(code, 2);
}

#[test]
fn http_500_maps_to_exit_code_3() {
    let (url, _) = start_fake_bridge(500, r#"{"error":"tracker shellout failed"}"#);
    let (code, _, _) = run_fleet_tracker(&url, "t", &["comment", "hi"]);
    assert_eq!(code, 3);
}

#[test]
fn missing_bridge_url_env_yields_exit_code_1() {
    let exe = env!("CARGO_BIN_EXE_fleet-tracker");
    let output = Command::new(exe)
        .args(["list"])
        .env_remove("FLEET_BRIDGE_URL")
        .env_remove("FLEET_BRIDGE_TOKEN")
        .output()
        .expect("spawning fleet-tracker");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("FLEET_BRIDGE_URL not set"),
        "stderr = {stderr}"
    );
}
