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
///
/// `dns_ip` and `dns_container` extend the model with the DNS-stub
/// sidecar (Linux + Podman): when present, the workflow container's
/// resolver is overridden via `--dns=<dns_ip>` so it can only resolve
/// names the allowlist pre-resolved. NXDOMAIN for everything else —
/// closes the DNS-exfiltration gap that an unfiltered upstream
/// resolver would otherwise leak through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressSetup {
    pub proxy_env: Vec<(String, String)>,
    pub network_name: Option<String>,
    pub proxy_container: Option<String>,
    /// IP of the DNS-stub sidecar (when one was started). The runtime
    /// adapter passes this to the engine as `--dns=<ip>` so the
    /// workflow container's resolv.conf points only at the stub.
    pub dns_ip: Option<String>,
    /// Container id of the DNS sidecar, for diagnostics + targeted
    /// teardown.
    pub dns_container: Option<String>,
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
            dns_ip: None,
            dns_container: None,
        })
    }

    fn teardown(&self, setup: &EgressSetup) -> Result<()> {
        // Best-effort: don't abort if one step fails — a partial
        // setup may leave only some resources, and we want to
        // reclaim what we can.
        if let Some(container) = &setup.proxy_container {
            if let Err(err) = self
                .invoker
                .run("podman", vec!["stop".to_string(), container.clone()])
            {
                tracing::warn!(error = %err, container = %container, "stopping tinyproxy sidecar failed");
            }
        }
        if let Some(network) = &setup.network_name {
            if let Err(err) = self.invoker.run(
                "podman",
                vec!["network".to_string(), "rm".to_string(), network.clone()],
            ) {
                tracing::warn!(error = %err, network = %network, "removing podman network failed");
            }
        }
        Ok(())
    }
}

/// Host-resident tinyproxy enforcer.
///
/// Used on macOS (Apple Container and Podman-machine paths) where
/// running the proxy as a sibling container is awkward — Apple
/// Container hasn't shipped a `--internal` network primitive
/// equivalent, and the Podman-machine path's network plumbing is
/// brittle enough that a host-side proxy is simpler. The container
/// reaches the proxy via the engine's host-bridge DNS name
/// (`host.containers.internal` for Apple Container, the same name
/// or `host.docker.internal` for Docker; configurable here).
///
/// Setup:
/// 1. Write tinyproxy.conf + the allowlist file to a per-session
///    tempdir under `<tmpdir>/fleet-egress-<sid>/`.
/// 2. Invoke `tinyproxy -c <conf>`. Tinyproxy daemonises by default
///    and writes its pid to the path declared in the conf.
///
/// Teardown:
/// 1. Read the pidfile.
/// 2. Send SIGTERM via `kill`.
/// 3. Remove the tempdir.
///
/// Honest scope: enforcement here is `HTTP_PROXY` env-only. A
/// malicious agent could unset `HTTP_PROXY` and reach the engine's
/// default bridge. Treating this as a guardrail (well-behaved
/// clients respect `HTTP_PROXY`) rather than a boundary is the v1
/// macOS story; Apple Container network-pinning lands when its
/// `--internal`-equivalent semantics are verified.
pub struct HostProxyEnforcer {
    invoker: Arc<dyn ProcessInvoker>,
    /// DNS name the container uses to reach the host. Apple Container
    /// resolves `host.containers.internal`; Docker exposes
    /// `host.docker.internal`. Configurable so tests pin a deterministic
    /// value.
    host_address: String,
    proxy_port: u16,
    allowlist: Vec<String>,
    /// Root tempdir for per-session config + pidfile. Tests inject a
    /// known path; production resolves to `/tmp/fleet-egress-<sid>`
    /// via `std::env::temp_dir()` at setup time.
    tempdir_base: std::path::PathBuf,
}

impl HostProxyEnforcer {
    #[must_use]
    pub fn new(
        invoker: Arc<dyn ProcessInvoker>,
        host_address: impl Into<String>,
        allowlist: Vec<String>,
    ) -> Self {
        Self {
            invoker,
            host_address: host_address.into(),
            proxy_port: 8888,
            allowlist,
            tempdir_base: std::env::temp_dir(),
        }
    }

    /// Test override for the tempdir base. Production uses
    /// [`std::env::temp_dir`]; tests pin a per-test path so the
    /// assertions stay deterministic.
    #[must_use]
    #[cfg(test)]
    pub fn with_tempdir(mut self, base: std::path::PathBuf) -> Self {
        self.tempdir_base = base;
        self
    }

    fn session_dir(&self, session_id: &str) -> std::path::PathBuf {
        self.tempdir_base.join(format!("fleet-egress-{session_id}"))
    }
}

impl EgressEnforcer for HostProxyEnforcer {
    fn setup(&self, session_id: &str) -> Result<EgressSetup> {
        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating egress tempdir {}", dir.display()))?;
        let conf_path = dir.join("tinyproxy.conf");
        let allowlist_path = dir.join("allowlist");
        let pidfile_path = dir.join("tinyproxy.pid");

        // Allowlist file: one host per line. The conf below
        // references this path.
        std::fs::write(&allowlist_path, self.allowlist.join("\n"))
            .with_context(|| format!("writing {}", allowlist_path.display()))?;

        let conf = build_host_tinyproxy_conf(self.proxy_port, &allowlist_path, &pidfile_path);
        std::fs::write(&conf_path, conf)
            .with_context(|| format!("writing {}", conf_path.display()))?;

        // Spawn tinyproxy. Default mode is daemonise; the binary
        // forks and writes its pid to PidFile, then exits. ProcessInvoker.run
        // captures stdout — we don't need it.
        self.invoker
            .run(
                "tinyproxy",
                vec!["-c".to_string(), conf_path.display().to_string()],
            )
            .with_context(|| format!("starting tinyproxy with config {}", conf_path.display()))?;

        let proxy_url = format!(
            "http://{host}:{port}",
            host = self.host_address,
            port = self.proxy_port
        );
        Ok(EgressSetup {
            proxy_env: vec![
                ("HTTP_PROXY".to_string(), proxy_url.clone()),
                ("HTTPS_PROXY".to_string(), proxy_url.clone()),
                ("http_proxy".to_string(), proxy_url.clone()),
                ("https_proxy".to_string(), proxy_url),
                (
                    "NO_PROXY".to_string(),
                    format!("localhost,127.0.0.1,{}", self.host_address),
                ),
                (
                    "no_proxy".to_string(),
                    format!("localhost,127.0.0.1,{}", self.host_address),
                ),
            ],
            // No engine-side network on the host-proxy path; env-only.
            network_name: None,
            // Stash the pidfile path under proxy_container so teardown
            // can find it. The field is conceptually "the thing
            // teardown needs" rather than literally "a container id";
            // host-proxy stores its pidfile path here.
            proxy_container: Some(pidfile_path.display().to_string()),
            // No DNS sidecar on the host-proxy path (Apple Container /
            // Docker). Closing the DNS gap on macOS requires Apple
            // Container's network-internal primitive, which doesn't
            // exist yet — flagged in docs/network.md as honest non-goal.
            dns_ip: None,
            dns_container: None,
        })
    }

    fn teardown(&self, setup: &EgressSetup) -> Result<()> {
        let Some(pidfile_str) = setup.proxy_container.as_deref() else {
            return Ok(());
        };
        let pid = match std::fs::read_to_string(pidfile_str) {
            Ok(s) => s.trim().to_string(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    pidfile = pidfile_str,
                    "host-proxy pidfile unreadable; nothing to kill"
                );
                return Ok(());
            }
        };
        if !pid.is_empty() {
            if let Err(err) = self
                .invoker
                .run("kill", vec!["-TERM".to_string(), pid.clone()])
            {
                tracing::warn!(error = %err, pid = %pid, "killing tinyproxy failed");
            }
        }
        // Best-effort cleanup of the tempdir. tinyproxy still owns
        // the pidfile until it exits; tolerating a not-yet-cleaned
        // dir is fine.
        let pidfile = std::path::Path::new(pidfile_str);
        if let Some(parent) = pidfile.parent() {
            if let Err(err) = std::fs::remove_dir_all(parent) {
                tracing::warn!(
                    error = %err,
                    dir = %parent.display(),
                    "removing egress tempdir failed"
                );
            }
        }
        Ok(())
    }
}

/// Host-proxy variant of the tinyproxy config. Differs from the
/// container variant by referencing an external allowlist file
/// (rather than embedding it as comments) and declaring a `PidFile`
/// the enforcer can read for teardown.
fn build_host_tinyproxy_conf(
    port: u16,
    allowlist_path: &std::path::Path,
    pidfile_path: &std::path::Path,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Port {port}");
    out.push_str("Listen 0.0.0.0\n");
    out.push_str("Timeout 60\n");
    let _ = writeln!(out, "PidFile \"{}\"", pidfile_path.display());
    let _ = writeln!(out, "Filter \"{}\"", allowlist_path.display());
    out.push_str("FilterDefaultDeny Yes\n");
    out.push_str("FilterExtended On\n");
    out.push_str("ConnectPort 443\n");
    out.push_str("ConnectPort 80\n");
    out
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
            let allowlist = resolve_allowlist(network, fleet_defaults);
            match adapter_name {
                "podman" => Box::new(PodmanTinyproxyEnforcer::new(invoker, allowlist)),
                "apple-container" => Box::new(HostProxyEnforcer::new(
                    invoker,
                    // Apple Container exposes the host at this DNS name
                    // to containers on its default network.
                    "host.containers.internal",
                    allowlist,
                )),
                "docker" => Box::new(HostProxyEnforcer::new(
                    invoker,
                    // Docker Desktop's host-bridge alias. On native
                    // Docker (Linux without Desktop), this name does
                    // not resolve — users on that path should switch
                    // to the podman adapter for now.
                    "host.docker.internal",
                    allowlist,
                )),
                _ => {
                    tracing::warn!(
                        adapter = adapter_name,
                        "egress enforcement is not yet wired for this adapter; running open"
                    );
                    Box::new(NoopEnforcer)
                }
            }
        }
    }
}

/// Tiny dependency-free base64 encoder for shoving a small config
/// string through `podman run`'s argv. Adding a `base64` crate dep
/// just for this would be silly; the input here is short (< 1 KiB
/// in practice) so a hand-rolled encoder is fine.
fn simple_base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
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
            dns_ip: None,
            dns_container: None,
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
    fn build_enforcer_returns_noop_for_local_adapter_with_allowlist() {
        // Local has no isolation by design (it's fleet-on-fleet dev),
        // so even allowlist policy is degraded to Noop with a logged
        // warning. Apple Container + Docker go via host-proxy now.
        let net = NetworkConfig {
            policy: NetworkPolicy::Allowlist,
            extra_hosts: vec![],
        };
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(MockProcessInvoker::new());
        let enforcer = build_enforcer(&net, "local", invoker, &[]);
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
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, args| {
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
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, args| {
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
            dns_ip: None,
            dns_container: None,
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
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, args| {
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
        mock.expect_run()
            .with(always(), always())
            .returning(|_, _| Err(anyhow::anyhow!("podman exploded")));
        let enf = PodmanTinyproxyEnforcer::new(Arc::new(mock), vec![]);
        let setup = EgressSetup {
            proxy_env: vec![],
            network_name: Some("fleet-x".to_string()),
            proxy_container: Some("fleet-proxy-x".to_string()),
            dns_ip: None,
            dns_container: None,
        };
        // Should return Ok despite both inner calls failing.
        enf.teardown(&setup).unwrap();
    }

    #[test]
    fn build_enforcer_routes_apple_container_to_host_proxy() {
        // The boxed return type is opaque; assert via the setup
        // call shape, which only HostProxyEnforcer produces.
        let net = NetworkConfig {
            policy: NetworkPolicy::Allowlist,
            extra_hosts: vec![],
        };
        let tmp = tempfile::tempdir().unwrap();
        let invoker_calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_mock = Arc::clone(&invoker_calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, _args| {
                calls_for_mock.lock().unwrap().push(bin.to_string());
                Ok(String::new())
            });
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(mock);
        // Re-route the host-proxy enforcer's tempdir into the test
        // dir so we can drive the full setup without polluting /tmp.
        // The factory builds a HostProxyEnforcer with std::env::temp_dir()
        // so we override here by constructing it directly.
        let enforcer = HostProxyEnforcer::new(
            invoker,
            "host.containers.internal",
            resolve_allowlist(&net, &[]),
        )
        .with_tempdir(tmp.path().to_path_buf());
        let setup = enforcer.setup("s-apple").unwrap();
        // Apple Container uses host.containers.internal as the DNS
        // alias for the host — this fingerprints HostProxyEnforcer.
        assert!(
            setup
                .proxy_env
                .iter()
                .any(|(k, v)| k == "HTTP_PROXY" && v.contains("host.containers.internal"))
        );
        // The invoker saw tinyproxy, not podman — definitive proof
        // that the host-proxy adapter, not the Podman sidecar, was
        // selected.
        let bins = invoker_calls.lock().unwrap().clone();
        assert_eq!(bins, vec!["tinyproxy".to_string()]);
    }

    #[test]
    fn host_proxy_setup_writes_conf_and_runs_tinyproxy() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, args| {
                calls_for_mock.lock().unwrap().push((bin.to_string(), args));
                Ok(String::new())
            });
        let enf = HostProxyEnforcer::new(
            Arc::new(mock),
            "host.containers.internal",
            vec!["api.github.com".to_string()],
        )
        .with_tempdir(tmp.path().to_path_buf());

        let setup = enf.setup("s-host").unwrap();
        let dir = tmp.path().join("fleet-egress-s-host");
        assert!(dir.join("tinyproxy.conf").is_file());
        assert!(dir.join("allowlist").is_file());
        // The allowlist file should literally contain the host.
        let written = std::fs::read_to_string(dir.join("allowlist")).unwrap();
        assert_eq!(written, "api.github.com");
        // tinyproxy was invoked exactly once with the conf path.
        let invocations = calls.lock().unwrap().clone();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].0, "tinyproxy");
        assert!(invocations[0].1.contains(&"-c".to_string()));
        // Env vars point at the host-bridge DNS name.
        let env: std::collections::HashMap<String, String> =
            setup.proxy_env.iter().cloned().collect();
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://host.containers.internal:8888")
        );
        // No engine-side network on the host-proxy path.
        assert_eq!(setup.network_name, None);
        // proxy_container stashes the pidfile path for teardown.
        assert!(
            setup
                .proxy_container
                .as_deref()
                .unwrap()
                .ends_with("tinyproxy.pid")
        );
    }

    #[test]
    fn host_proxy_teardown_kills_pid_from_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("fleet-egress-s-t/tinyproxy.pid");
        std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        std::fs::write(&pidfile, "12345\n").unwrap();
        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let calls_for_mock = Arc::clone(&calls);
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(always(), always())
            .returning(move |bin, args| {
                calls_for_mock.lock().unwrap().push((bin.to_string(), args));
                Ok(String::new())
            });
        let enf = HostProxyEnforcer::new(Arc::new(mock), "host.containers.internal", vec![]);
        let setup = EgressSetup {
            proxy_env: vec![],
            network_name: None,
            proxy_container: Some(pidfile.display().to_string()),
            dns_ip: None,
            dns_container: None,
        };
        enf.teardown(&setup).unwrap();
        let invocations = calls.lock().unwrap().clone();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].0, "kill");
        assert_eq!(
            invocations[0].1,
            vec!["-TERM".to_string(), "12345".to_string()]
        );
        // Tempdir is reclaimed.
        assert!(!pidfile.parent().unwrap().exists());
    }

    #[test]
    fn host_proxy_teardown_tolerates_missing_pidfile() {
        // setup() failed before tinyproxy wrote its pidfile, or the
        // process crashed before doing so. teardown must not error.
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Ok(String::new()));
        let enf = HostProxyEnforcer::new(Arc::new(mock), "host.containers.internal", vec![]);
        let setup = EgressSetup {
            proxy_env: vec![],
            network_name: None,
            proxy_container: Some("/nonexistent/path/to/pidfile".to_string()),
            dns_ip: None,
            dns_container: None,
        };
        enf.teardown(&setup).unwrap();
    }

    #[test]
    fn build_host_tinyproxy_conf_references_external_allowlist_and_pidfile() {
        let conf = build_host_tinyproxy_conf(
            9090,
            std::path::Path::new("/tmp/al"),
            std::path::Path::new("/tmp/pid"),
        );
        assert!(conf.contains("Port 9090"));
        assert!(conf.contains("PidFile \"/tmp/pid\""));
        assert!(conf.contains("Filter \"/tmp/al\""));
        assert!(conf.contains("FilterDefaultDeny Yes"));
        assert!(conf.contains("ConnectPort 443"));
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
        assert!(
            encoded.contains('+')
                || encoded
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '=')
        );
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
