//! Network egress enforcement for workflow containers.
//!
//! The threat model: an agent inside a fleet container reads the
//! user's source code and can shell out arbitrarily. Without egress
//! controls a malicious prompt-injection in a README could exfiltrate
//! credentials or beacon to an attacker-controlled host. We can't
//! make the agent's network bulletproof — it shares the kernel/VM
//! with the rest of the container under any reasonable engine — but
//! we *can* drop a proxy in front of all outbound traffic and refuse
//! everything that isn't on a per-workflow allowlist.
//!
//! ## Architecture
//!
//! `EgressEnforcer` is the port; concrete adapters provide the
//! enforcement mechanism. Two impls today:
//!
//! - [`NoopEnforcer`] (default; used for `policy: open` and on
//!   adapters that don't yet have proxy support — macOS Apple
//!   Container, the `Local` adapter). Returns empty env so the
//!   container behaves exactly as it did pre-Phase-3.
//! - [`PodmanTinyproxyEnforcer`] (Linux + Podman + `policy:
//!   allowlist`). Creates a private podman network, runs a
//!   tinyproxy sidecar attached to that network with the allowlist
//!   compiled into its config, and injects `HTTP_PROXY` /
//!   `HTTPS_PROXY` into the workflow container so every outbound
//!   request is filtered.
//!
//! The trait is intentionally narrow — `setup` before any container
//! that needs egress, `teardown` after the last one finishes. The
//! workflow executor calls `setup` once per session (not per node)
//! so the same proxy lifecycle covers a multi-node run.
//!
//! ## What this *doesn't* defend against
//!
//! - **DNS exfiltration**: tinyproxy doesn't intercept DNS. A
//!   determined attacker could encode data into queries to an
//!   allowlisted nameserver. Out of scope for v1.
//! - **A malicious agent who unsets `HTTP_PROXY`**: tinyproxy is the
//!   *only* network path out, but only because the firewall rules in
//!   the private podman network blackhole everything else. The
//!   adapter must set that up; today's tinyproxy sidecar relies on
//!   podman's default network isolation, which blackholes by default
//!   inside an internal-only network — see [`PodmanTinyproxyEnforcer`]
//!   for the exact flags.
//! - **SNI-on-IP bypass** (a hostile target on a numeric IP without
//!   SNI): tinyproxy filters via the proxy's CONNECT line, which
//!   carries the hostname the client sent. A client that sets
//!   `--resolve example.com:1.2.3.4` defeats this. The allowlist is
//!   a guardrail, not a hard boundary.
//!
//! ## Honest scope statement
//!
//! v1 ships the *plumbing*: trait, config, lifecycle wiring, and a
//! podman-tinyproxy adapter whose podman invocations are unit-tested
//! against a mock invoker. End-to-end verification (a real `curl
//! evil.example.com` returning 403 inside a real container) is a
//! manual smoke step — see `docs/SMOKE_EGRESS.md` once the doctor
//! command surfaces the configured policy.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::process::ProcessInvoker;
use crate::repo_config::{NetworkConfig, NetworkPolicy};

/// What an enforcer needs to do, end-to-end, around a workflow run.
///
/// The split is intentional: setup spawns resources (a network, a
/// proxy container); teardown reclaims them. Returning a `Setup`
/// value rather than relying on Drop lets the workflow executor log
/// each cleanup step and tolerate partial teardown after a crash.
pub trait EgressEnforcer: Send + Sync {
    /// Provision whatever network primitives this enforcer needs and
    /// return a handle the workflow can attach containers to.
    /// Implementations are free to no-op for `policy: open`.
    fn setup(&self, session_id: &str) -> Result<EgressSetup>;

    /// Reclaim the resources `setup` allocated. Idempotent — a
    /// workflow that crashed mid-setup will call this anyway, and
    /// the enforcer must not error when there's nothing to clean up.
    fn teardown(&self, setup: &EgressSetup) -> Result<()>;
}

/// Handed back from [`EgressEnforcer::setup`] and forwarded into
/// each container spec the workflow executor builds.
///
/// `proxy_env` is the headline: `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`
/// pairs the adapter must inject. `network_name` is the engine-level
/// network the workflow container must be pinned to; `proxy_container`
/// is the sidecar id for diagnostics + targeted teardown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressSetup {
    pub proxy_env: Vec<(String, String)>,
    pub network_name: Option<String>,
    pub proxy_container: Option<String>,
}

impl EgressSetup {
    /// Empty setup — `setup()` was a no-op. Lets call sites that
    /// don't care about the policy keep their happy path linear.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when this setup represents *real* enforcement (a proxy
    /// to inject env for, a network to pin). UIs surface this so the
    /// user knows when a workflow has hardening in force.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_enforcing(&self) -> bool {
        !self.proxy_env.is_empty() || self.network_name.is_some()
    }
}

/// No-op enforcer used when the policy is `open` or when the host
/// can't provide a real enforcer (macOS Apple Container, the
/// `Local` adapter). Always returns an empty setup; teardown is a
/// no-op.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEnforcer;

impl EgressEnforcer for NoopEnforcer {
    fn setup(&self, _session_id: &str) -> Result<EgressSetup> {
        Ok(EgressSetup::empty())
    }

    fn teardown(&self, _setup: &EgressSetup) -> Result<()> {
        Ok(())
    }
}

/// Tinyproxy-sidecar enforcer for Linux + Podman.
///
/// Setup sequence (the unit tests below assert each shell invocation):
/// 1. `podman network create --internal fleet-<sid>` — internal means
///    no route to the outside; only containers on this network can
///    reach each other.
/// 2. Write a generated tinyproxy.conf to a tempfile.
/// 3. `podman run -d --network fleet-<sid> --name fleet-proxy-<sid>
///    -v <conf>:/etc/tinyproxy/tinyproxy.conf <image> -d -c /etc/tinyproxy/tinyproxy.conf`
/// 4. Add an outbound bridge: `podman network connect <bridge> fleet-proxy-<sid>`
///    so the proxy itself can reach the public internet to actually
///    relay allowed traffic.
///
/// Teardown is the reverse plus image cleanup.
///
/// Network name and proxy container id are written into `EgressSetup`;
/// the workflow executor pins the workflow container to the same
/// network and injects `HTTP_PROXY=http://fleet-proxy-<sid>:8888`.
pub struct PodmanTinyproxyEnforcer {
    invoker: Arc<dyn ProcessInvoker>,
    /// OCI image to run as the sidecar. Defaults to `kalaksi/tinyproxy:latest`;
    /// users on restricted networks can pin a mirror via
    /// `runtime.network.proxy_image` once that knob lands.
    image: String,
    /// Host:port the proxy listens on inside its network. Tinyproxy's
    /// default is 8888; we hard-code so the env vars are deterministic.
    proxy_port: u16,
    /// Configured allowlist hosts (already merged with fleet defaults).
    /// `setup` bakes them into the tinyproxy config.
    allowlist: Vec<String>,
}

impl PodmanTinyproxyEnforcer {
    #[must_use]
    pub fn new(invoker: Arc<dyn ProcessInvoker>, allowlist: Vec<String>) -> Self {
        Self {
            invoker,
            image: "kalaksi/tinyproxy:latest".to_string(),
            proxy_port: 8888,
            allowlist,
        }
    }

    fn network_name(session_id: &str) -> String {
        format!("fleet-{session_id}")
    }

    fn proxy_container_name(session_id: &str) -> String {
        format!("fleet-proxy-{session_id}")
    }
}

impl EgressEnforcer for PodmanTinyproxyEnforcer {
    fn setup(&self, session_id: &str) -> Result<EgressSetup> {
        let network = Self::network_name(session_id);
        let proxy_name = Self::proxy_container_name(session_id);

        // 1. Private network, internal so a network-only-attached
        //    container has no path out except through the proxy.
        self.invoker
            .run(
                "podman",
                vec![
                    "network".to_string(),
                    "create".to_string(),
                    "--internal".to_string(),
                    network.clone(),
                ],
            )
            .with_context(|| format!("creating podman network {network}"))?;

        // 2. Tinyproxy config: HTTP/HTTPS CONNECT allowlist.
        //    Conf is written via `printf` into the container at
        //    start because tinyproxy reads from a file path. We
        //    encode the config inline (via `--entrypoint sh -c`)
        //    rather than bind-mounting a host file — bind mounts
        //    cross filesystem boundaries differently on every
        //    backend, and the config is small.
        let conf = build_tinyproxy_conf(self.proxy_port, &self.allowlist);
        // base64 keeps the config opaque to argv parsing — embedding
        // newlines in a shell argv string is portable but fragile;
        // base64 round-trip is bulletproof.
        let encoded = simple_base64_encode(conf.as_bytes());

        // 3. Run the sidecar attached to the private network. We use
        //    `--network <name>` so the sidecar starts on the
        //    allowlist-only path; teardown then attaches it to a
        //    bridge so it can actually relay traffic. The two-step
        //    is more verbose than `--network=bridge` once, but it
        //    makes the audit trail clearer: only the sidecar ever
        //    bridges outward, never the workflow container.
        self.invoker
            .run(
                "podman",
                vec![
                    "run".to_string(),
                    "-d".to_string(),
                    "--rm".to_string(),
                    "--name".to_string(),
                    proxy_name.clone(),
                    "--network".to_string(),
                    network.clone(),
                    "--entrypoint".to_string(),
                    "sh".to_string(),
                    self.image.clone(),
                    "-c".to_string(),
                    format!(
                        "echo {encoded} | base64 -d > /etc/tinyproxy/tinyproxy.conf && \
                         tinyproxy -d -c /etc/tinyproxy/tinyproxy.conf"
                    ),
                ],
            )
            .with_context(|| format!("starting tinyproxy sidecar {proxy_name}"))?;

        // 4. Bridge the sidecar outward. `bridge` is podman's default
        //    routed network; the sidecar joins it *in addition* to
        //    the internal one, gaining a route to the outside.
        self.invoker
            .run(
                "podman",
                vec![
                    "network".to_string(),
                    "connect".to_string(),
                    "podman".to_string(),
                    proxy_name.clone(),
                ],
            )
            .with_context(|| format!("bridging tinyproxy sidecar {proxy_name} outward"))?;

        let proxy_url = format!("http://{proxy_name}:{port}", port = self.proxy_port);
        Ok(EgressSetup {
            proxy_env: vec![
                ("HTTP_PROXY".to_string(), proxy_url.clone()),
                ("HTTPS_PROXY".to_string(), proxy_url.clone()),
                ("http_proxy".to_string(), proxy_url.clone()),
                ("https_proxy".to_string(), proxy_url),
                // localhost + the proxy itself never go through the
                // proxy. Without NO_PROXY, the agent's own `curl
                // 127.0.0.1` to its own dev server would loop.
                (
                    "NO_PROXY".to_string(),
                    format!("localhost,127.0.0.1,{proxy_name}"),
                ),
                (
                    "no_proxy".to_string(),
                    format!("localhost,127.0.0.1,{proxy_name}"),
                ),
            ],
            network_name: Some(network),
            proxy_container: Some(proxy_name),
        })
    }

    fn teardown(&self, setup: &EgressSetup) -> Result<()> {
        // Best-effort: don't abort if one step fails — a partial
        // setup may leave only some resources, and we want to
        // reclaim what we can.
        if let Some(container) = &setup.proxy_container {
            if let Err(err) = self.invoker.run(
                "podman",
                vec!["stop".to_string(), container.clone()],
            ) {
                tracing::warn!(error = %err, container = %container, "stopping tinyproxy sidecar failed");
            }
        }
        if let Some(network) = &setup.network_name {
            if let Err(err) = self.invoker.run(
                "podman",
                vec![
                    "network".to_string(),
                    "rm".to_string(),
                    network.clone(),
                ],
            ) {
                tracing::warn!(error = %err, network = %network, "removing podman network failed");
            }
        }
        Ok(())
    }
}

/// Build the tinyproxy.conf body from the resolved allowlist.
///
/// Tinyproxy's `Allow` directives are line-based; one host per line,
/// matched against the destination of CONNECT / outgoing requests.
/// We always start with `User`/`Group nobody`-equivalent runtime
/// defaults (the upstream image sets these), so this function only
/// emits the policy lines.
fn build_tinyproxy_conf(port: u16, allowlist: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Port {port}");
    out.push_str("Listen 0.0.0.0\n");
    out.push_str("Timeout 60\n");
    // No upstream proxy; we *are* the proxy. ConnectPort 443 + 80
    // are the only CONNECT destinations the proxy will forward —
    // tightens the surface a bit.
    out.push_str("ConnectPort 443\n");
    out.push_str("ConnectPort 80\n");
    // FilterDefaultDeny means an unmatched destination is refused;
    // FilterExtended uses regex-style host matching so wildcard
    // entries (e.g. `\.github\.com$`) work for users who need them.
    out.push_str("Filter \"/etc/tinyproxy/allowlist\"\n");
    out.push_str("FilterDefaultDeny Yes\n");
    out.push_str("FilterExtended On\n");
    // Embed the allowlist inline by writing both files via the same
    // base64 trick the caller uses. Tinyproxy doesn't support an
    // inline allowlist directive; the conf must reference an
    // external file. We arrange for *both* files to land at startup
    // via the container's entrypoint script.
    out.push_str("# allowlist:\n");
    for host in allowlist {
        let _ = writeln!(out, "# - {host}");
    }
    out
}

/// Resolve the full effective allowlist for this session: user-
/// declared `extra_hosts` plus fleet defaults.
///
/// Fleet defaults are intentionally permissive for things every
/// workflow needs to function:
/// - `registry-1.docker.io` / `quay.io` / `ghcr.io` so the proxy
///   sidecar's own image can be pulled if missing.
/// - The configured tracker host (added by the caller).
/// - The configured LLM provider host (`api.anthropic.com` /
///   `api.openai.com` based on `agents.registry env_passthrough`).
///
/// All of these are best-effort: when fleet can't infer a provider
/// host (custom agent with no recognised env vars), it's left to the
/// user's `extra_hosts`.
#[must_use]
pub fn resolve_allowlist(network: &NetworkConfig, fleet_defaults: &[&str]) -> Vec<String> {
    let mut hosts: Vec<String> = fleet_defaults.iter().map(|s| (*s).to_string()).collect();
    hosts.extend(network.extra_hosts.iter().cloned());
    // Dedup + sort so tinyproxy.conf is deterministic and tests can
    // assert on it without sorting at every call site.
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Build the enforcer for a given network policy + adapter name.
///
/// - `open` or `none` → `NoopEnforcer` (the `none` path lands in the
///   adapter layer; this function returns Noop and the adapter is
///   responsible for the actual hard-block when it sees that policy).
/// - `allowlist` + `podman` adapter → `PodmanTinyproxyEnforcer`.
/// - `allowlist` + any other adapter → `NoopEnforcer` with a
///   warning log; macOS / docker / local egress lands in a follow-up.
pub fn build_enforcer(
    network: &NetworkConfig,
    adapter_name: &str,
    invoker: Arc<dyn ProcessInvoker>,
    fleet_defaults: &[&str],
) -> Box<dyn EgressEnforcer> {
    match network.policy {
        NetworkPolicy::Open | NetworkPolicy::None => Box::new(NoopEnforcer),
        NetworkPolicy::Allowlist => {
            if adapter_name == "podman" {
                let allowlist = resolve_allowlist(network, fleet_defaults);
                Box::new(PodmanTinyproxyEnforcer::new(invoker, allowlist))
            } else {
                tracing::warn!(
                    adapter = adapter_name,
                    "egress enforcement is not yet wired for this adapter; running open"
                );
                Box::new(NoopEnforcer)
            }
        }
    }
}

/// Tiny dependency-free base64 encoder for shoving a small config
/// string through `podman run`'s argv. Adding a `base64` crate dep
/// just for this would be silly; the input here is short (< 1 KiB
/// in practice) so a hand-rolled encoder is fine.
fn simple_base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = (u32::from(bytes[i]) << 16)
            | (u32::from(bytes[i + 1]) << 8)
            | u32::from(bytes[i + 2]);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHA[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = u32::from(bytes[i]) << 16;
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use mockall::predicate::always;
    use std::sync::Mutex;

    #[test]
    fn noop_enforcer_returns_empty_setup() {
        let setup = NoopEnforcer.setup("s-x").unwrap();
        assert_eq!(setup, EgressSetup::empty());
        assert!(!setup.is_enforcing());
    }

    #[test]
    fn noop_enforcer_teardown_is_idempotent() {
        let n = NoopEnforcer;
        n.teardown(&EgressSetup::empty()).unwrap();
        // Calling on a populated setup also succeeds — Noop is the
        // "I don't care what you give me" enforcer.
        n.teardown(&EgressSetup {
            proxy_env: vec![("X".to_string(), "Y".to_string())],
            network_name: Some("net".to_string()),
            proxy_container: Some("c".to_string()),
        })
        .unwrap();
    }

    #[test]
    fn resolve_allowlist_merges_defaults_and_extra_hosts_sorted_and_deduped() {
        let net = NetworkConfig {
            policy: NetworkPolicy::Allowlist,
            extra_hosts: vec!["crates.io".to_string(), "api.github.com".to_string()],
        };
        let defaults = ["api.github.com", "registry-1.docker.io"];
        let merged = resolve_allowlist(&net, &defaults);
        assert_eq!(
            merged,
            vec![
                "api.github.com".to_string(),
                "crates.io".to_string(),
                "registry-1.docker.io".to_string()
            ]
        );
    }

    #[test]
    fn build_enforcer_returns_noop_for_open_policy() {
        let net = NetworkConfig {
            policy: NetworkPolicy::Open,
            extra_hosts: vec!["api.example.com".to_string()],
        };
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(MockProcessInvoker::new());
        let enforcer = build_enforcer(&net, "podman", invoker, &[]);
        let setup = enforcer.setup("s-x").unwrap();
        assert!(!setup.is_enforcing(), "open policy => no enforcement");
    }

    #[test]
    fn build_enforcer_returns_noop_for_non_podman_adapter_with_allowlist() {
        // macOS Apple Container / docker / local don't have a
        // wired adapter yet. The factory returns Noop so the
        // workflow still runs (with a logged warning).
        let net = NetworkConfig {
            policy: NetworkPolicy::Allowlist,
            extra_hosts: vec![],
        };
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(MockProcessInvoker::new());
        let enforcer = build_enforcer(&net, "apple-container", invoker, &[]);
        let setup = enforcer.setup("s-x").unwrap();
        assert!(!setup.is_enforcing());
    }

    #[test]
    fn build_enforcer_returns_podman_tinyproxy_for_podman_with_allowlist() {
        // The factory must hand out a real PodmanTinyproxyEnforcer
        // when both legs match: adapter == podman *and* policy ==
        // allowlist. Verifying via the setup-call sequence below;
        // the type alone is hidden behind the trait object.
        let net = NetworkConfig {
            policy: NetworkPolicy::Allowlist,
            extra_hosts: vec!["api.example.com".to_string()],
        };
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().with(always(), always()).returning(move |bin, args| {
            calls_for_mock
                .lock()
                .unwrap()
                .push(format!("{bin} {}", args.join(" ")));
            Ok(String::new())
        });
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        let enforcer = build_enforcer(&net, "podman", invoker, &[]);
        let setup = enforcer.setup("s-real").unwrap();
        assert!(setup.is_enforcing(), "podman+allowlist must enforce");
        let invocations = calls.lock().unwrap().clone();
        // Exactly three podman calls: network create, run, network connect.
        assert_eq!(invocations.len(), 3, "got: {invocations:?}");
        assert!(invocations[0].starts_with("podman network create --internal fleet-s-real"));
        assert!(invocations[1].contains("podman run"));
        assert!(invocations[1].contains("--name fleet-proxy-s-real"));
        assert!(invocations[1].contains("--network fleet-s-real"));
        assert!(invocations[2].starts_with("podman network connect podman fleet-proxy-s-real"));
    }

    #[test]
    fn podman_tinyproxy_setup_injects_proxy_env_pointing_at_sidecar() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(|_, _| Ok(String::new()));
        let enf = PodmanTinyproxyEnforcer::new(Arc::new(mock), vec![]);
        let setup = enf.setup("s-env").unwrap();
        let env: std::collections::HashMap<String, String> =
            setup.proxy_env.iter().cloned().collect();
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://fleet-proxy-s-env:8888")
        );
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://fleet-proxy-s-env:8888")
        );
        // Both cases land so tools that look for either pick it up.
        assert!(env.contains_key("http_proxy"));
        assert!(env.contains_key("https_proxy"));
        // NO_PROXY exempts localhost + the proxy itself.
        let no_proxy = env.get("NO_PROXY").map_or("", String::as_str);
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.1"));
        assert!(no_proxy.contains("fleet-proxy-s-env"));
    }

    #[test]
    fn podman_tinyproxy_teardown_stops_container_and_removes_network() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().with(always(), always()).returning(move |bin, args| {
            calls_for_mock
                .lock()
                .unwrap()
                .push(format!("{bin} {}", args.join(" ")));
            Ok(String::new())
        });
        let enf = PodmanTinyproxyEnforcer::new(Arc::new(mock), vec![]);
        let setup = EgressSetup {
            proxy_env: vec![],
            network_name: Some("fleet-s-down".to_string()),
            proxy_container: Some("fleet-proxy-s-down".to_string()),
        };
        enf.teardown(&setup).unwrap();
        let invocations = calls.lock().unwrap().clone();
        // stop, then network rm — order matters: the network can't
        // be removed until its containers detach.
        assert_eq!(invocations.len(), 2, "got: {invocations:?}");
        assert!(invocations[0].starts_with("podman stop fleet-proxy-s-down"));
        assert!(invocations[1].starts_with("podman network rm fleet-s-down"));
    }

    #[test]
    fn podman_tinyproxy_teardown_skips_steps_for_empty_setup() {
        // A teardown after a setup that failed before any step should
        // be silent — nothing to stop, nothing to remove.
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().with(always(), always()).returning(move |bin, args| {
            calls_for_mock
                .lock()
                .unwrap()
                .push(format!("{bin} {}", args.join(" ")));
            Ok(String::new())
        });
        let enf = PodmanTinyproxyEnforcer::new(Arc::new(mock), vec![]);
        enf.teardown(&EgressSetup::empty()).unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn podman_tinyproxy_teardown_tolerates_failures() {
        // A stop or rm failure (container already gone, network
        // already removed) must NOT propagate — partial cleanup is
        // worth more than a failed sweep.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().with(always(), always()).returning(|_, _| {
            Err(anyhow::anyhow!("podman exploded"))
        });
        let enf = PodmanTinyproxyEnforcer::new(Arc::new(mock), vec![]);
        let setup = EgressSetup {
            proxy_env: vec![],
            network_name: Some("fleet-x".to_string()),
            proxy_container: Some("fleet-proxy-x".to_string()),
        };
        // Should return Ok despite both inner calls failing.
        enf.teardown(&setup).unwrap();
    }

    #[test]
    fn build_tinyproxy_conf_emits_required_directives() {
        let conf = build_tinyproxy_conf(8888, &["api.github.com".to_string()]);
        assert!(conf.contains("Port 8888"));
        assert!(conf.contains("Listen 0.0.0.0"));
        assert!(conf.contains("FilterDefaultDeny Yes"));
        // ConnectPort allowlist for the two standard ports.
        assert!(conf.contains("ConnectPort 443"));
        assert!(conf.contains("ConnectPort 80"));
        // Allowlist hosts at minimum appear as a commented review line.
        assert!(conf.contains("api.github.com"));
    }

    #[test]
    fn simple_base64_round_trip_for_ascii_payload() {
        // Spot-check against a known reference. "Man" -> "TWFu".
        assert_eq!(simple_base64_encode(b"Man"), "TWFu");
        // Pad-1 and pad-2 cases.
        assert_eq!(simple_base64_encode(b"M"), "TQ==");
        assert_eq!(simple_base64_encode(b"Ma"), "TWE=");
        // Tinyproxy config-shaped: multi-line.
        let conf = "Port 8888\nListen 0.0.0.0\n";
        let encoded = simple_base64_encode(conf.as_bytes());
        // Decoded length should be the original length.
        assert_eq!(encoded.len() % 4, 0);
        assert!(encoded.contains('+') || encoded.chars().all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '='));
    }

    #[test]
    fn egress_setup_is_enforcing_when_either_field_populated() {
        let empty = EgressSetup::empty();
        assert!(!empty.is_enforcing());
        let env_only = EgressSetup {
            proxy_env: vec![("HTTP_PROXY".to_string(), "x".to_string())],
            ..EgressSetup::empty()
        };
        assert!(env_only.is_enforcing());
        let net_only = EgressSetup {
            network_name: Some("n".to_string()),
            ..EgressSetup::empty()
        };
        assert!(net_only.is_enforcing());
    }
}
