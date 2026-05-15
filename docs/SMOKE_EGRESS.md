# Egress proxy — manual smoke verification

The fleet test suite exercises the `egress` module against a mock
`ProcessInvoker`: it proves we send the *right* podman / tinyproxy
commands, but not that those commands actually filter traffic on a
real host. This document is the manual checklist that closes the
gap — run it once per Phase-3 release on each supported backend.

## What this verifies

- An agent inside a fleet container can `curl` an **allowlisted**
  host successfully.
- An agent inside a fleet container `curl`-ing a **non-allowlisted**
  host receives a tinyproxy `403 Forbidden` (or a connection refused
  for the host-proxy variant), not a 200.
- The tinyproxy sidecar / host process is stopped after the workflow
  exits — no orphaned proxy lingering after a clean run.
- The proxy is also reclaimed after a crashed run (verifies the
  `teardown` path on the failure branch).

## What this does NOT verify

- **DNS-level exfiltration.** Tinyproxy filters CONNECT and HTTP
  request lines, not DNS queries. A test that uses
  `evil.example.com` resolves through the host's resolver before the
  proxy sees it.
- **A malicious agent who unsets `HTTP_PROXY`.** On the host-proxy
  path (Apple Container / Docker) the env is the only enforcement;
  the agent can bypass by setting `HTTP_PROXY=` before calling its
  HTTP client. The Podman path closes this via `--internal` network
  pinning; the host-proxy path is documented as a guardrail.
- **SNI-on-IP bypass.** Tinyproxy keys on the hostname the client
  sent. `curl --resolve allowed.com:1.2.3.4` defeats this. Out of
  scope for v1.

If you're red-teaming the boundary properly, treat this smoke as a
sanity check and pair it with an external pen-test pass.

## Prerequisites

| Backend | Host packages |
|---|---|
| Linux + Podman | `podman` (rootless configured), network `podman` exists, internet reachable from the host. Optional: `runsc` for the gVisor leg. |
| macOS + Apple Container | `tinyproxy` via `brew install tinyproxy`. macOS 26+. |
| macOS + Docker Desktop | `tinyproxy` via `brew install tinyproxy`. Docker Desktop running (provides `host.docker.internal`). |

`tinyproxy --version` must work for the host-proxy path; the Podman
path pulls `kalaksi/tinyproxy:latest` on first use.

## Test workflow

In a scratch repo (or any repo with `fleet init` already run), drop
the following into `.fleet/config.yaml`:

```yaml
runtime:
  adapter: podman                 # or apple-container / docker
  hardening: auto
  network:
    policy: allowlist
    extra_hosts:
      - api.github.com
      # Note: NO entry for example.com or anywhere else.

agents:
  default: claude-code

workflows:
  default: standard

tracker: github
```

Then drop this smoke workflow into `.fleet/workflows/smoke-egress.yaml`:

```yaml
name: smoke-egress
description: Manual verification of the egress allowlist boundary.
trigger:
  manual: true
nodes:
  - id: allowed
    type: bash
    # Expected to succeed: api.github.com is on the allowlist.
    # `-fS` makes curl exit non-zero on any 4xx/5xx so a proxy 403
    # would fail the node, not silently log a body.
    script: 'curl -fsS -o /dev/null https://api.github.com/zen && echo ALLOWED_OK'

  - id: denied
    depends_on: [allowed]
    type: bash
    # Expected to FAIL with proxy denial. The `!` inverts so the node
    # *succeeds* iff curl fails — that's the assertion shape we want.
    # `-S` keeps stderr so the run log surfaces tinyproxy's denial.
    script: '! curl -fsS -o /dev/null https://example.com/ 2>&1 && echo DENIED_OK'
```

Run it:

```sh
fleet workflow run smoke-egress
```

Then verify each of the following.

### Pass criteria

1. `fleet workflow run` exits **0** (workflow Completed).
2. `.fleet/sessions/<id>/logs/allowed.log` contains `ALLOWED_OK`.
3. `.fleet/sessions/<id>/logs/denied.log` contains `DENIED_OK` *and*
   stderr captured a tinyproxy error (`403 Access denied`, or
   `Couldn't resolve host` if the `--internal` network has no DNS
   for non-allowlisted hosts).
4. Process listing on the host after exit shows **no** stray
   tinyproxy / `fleet-proxy-*` container:
   - Podman: `podman ps --filter name=fleet-proxy- --quiet` returns
     empty. `podman network ls --filter name=fleet- --quiet` also
     empty.
   - macOS: `pgrep -fa tinyproxy` returns no fleet-owned entry.
     `ls /tmp/fleet-egress-* 2>/dev/null` returns empty.

### Failure shapes & what they mean

| Symptom | Likely cause |
|---|---|
| `ALLOWED_OK` missing, `denied` never ran | The proxy didn't start; check `fleet workflow run`'s stderr for setup errors. Common: tinyproxy not in PATH (host-proxy), or `kalaksi/tinyproxy:latest` couldn't be pulled (Podman, registry blocked). |
| `ALLOWED_OK` present, `DENIED_OK` missing because curl succeeded | The allowlist isn't being enforced. Either the env vars aren't reaching the agent's bash shell, or the proxy isn't running. Inspect: `cat .fleet/sessions/<id>/logs/denied.log`. |
| Both nodes pass but `podman ps` still lists `fleet-proxy-<sid>` | teardown skipped — fleet may have been SIGKILL'd. Run `fleet sessions reap` (which now stops leaked containers) and re-check. |
| `denied` node fails for reasons other than the proxy (DNS lookup failure with no proxy log line) | Expected on the Podman `--internal` network — DNS for non-allowlisted hosts has no resolution path. Both "proxy denied 403" and "DNS resolution failed" are pass-shaped. |

## Crashed-run cleanup verification

To exercise the reaper's auto-stop path:

1. Start the smoke run, then `Ctrl-C` it mid-execution (e.g. as the
   `allowed` node fires).
2. The on-disk session will be left in `running` state with a
   leaked tinyproxy. Confirm:
   - `fleet sessions list` shows the session as `running`.
   - `podman ps` (or `pgrep tinyproxy`) shows the proxy.
3. Run `fleet sessions reap`. It should:
   - Transition the session to `crashed`.
   - Stop the leaked tinyproxy (`stopped 1 leaked container(s):
     fleet-proxy-<sid>` in the reap summary on the Podman path; on
     the host-proxy path the pidfile read + `kill -TERM` runs).
   - Write a `crash.json` listing the leaked container id.
4. Confirm the proxy is gone afterwards via the same `podman ps` /
   `pgrep tinyproxy` checks as in the pass criteria.

## Matrix coverage

Run the smoke on every adapter you ship:

- [ ] Linux Fedora — Podman + runsc (gVisor on)
- [ ] Linux Ubuntu — Podman without runsc
- [ ] macOS 26+ — Apple Container
- [ ] macOS 15 — Podman machine (treats this as the Linux Podman case)
- [ ] macOS — Docker Desktop (only if shipping the Docker adapter as
      a supported tier)

A green run on Linux Podman + one macOS variant is the bar for
cutting a release; the rest are nice-to-have signals on whichever
hardware you have access to.
