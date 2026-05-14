# Network allowlist

This page covers the **egress** boundary — what hosts AO workers can
reach outbound. Companion docs: [`sandbox.md`](./sandbox.md) for the
filesystem layer this assumes, and [`auth.md`](./auth.md) for the
identity material that flows through these hosts.

## Modes

`~/.config/fleet/config.toml`:

```toml
[network]
mode = "allowlist"        # "open" (default) | "allowlist"
extra_allow = [
  "crates.io",
  "static.crates.io",
  "pypi.org",
]
```

- `open` (default) — workers reach any host, no proxy enforcement.
  Matches fleet's pre-Phase-3 behaviour; an upgrade doesn't silently
  start dropping traffic.
- `allowlist` — workers' `HTTPS_PROXY` and `HTTP_PROXY` point at the
  in-VM `tinyproxy`, which only proxies CONNECTs / requests to hosts
  on the allowlist. Anything else returns `403 Forbidden`.

## What's allowed by default

The built-in baseline (always present under `allowlist` mode) covers
the hosts the AO + Claude + git-bug stack itself needs:

- `api.anthropic.com` (Claude API)
- `github.com`, `api.github.com`, `raw.githubusercontent.com`,
  `objects.githubusercontent.com`, `codeload.github.com`,
  `cli.github.com` (gh + git over HTTPS + raw user content)
- `registry.npmjs.org`, `deb.nodesource.com`, `nodejs.org` (claude-code
  + AO are npm packages)
- `archive.ubuntu.com`, `security.ubuntu.com`, `ports.ubuntu.com`
  (Ubuntu apt mirrors, used by the preflight tracker-tool installer)
- `git-bug.org` (release binary for the git-bug tracker)

The exact list lives in `BUILT_IN_ALLOW` in
[`src/network.rs`](../src/network.rs).

`extra_allow` is **additive**. Suffix-match semantics: an entry of
`example.com` covers `api.example.com`, `cdn.example.com`, etc.

## How it works

```
fleet start
   │
   ├──► sync_in_vm_filter()                         (host-side)
   │      limactl shell --user root fleet-vm bash -c '
   │        cat > /etc/tinyproxy/fleet-allow.filter
   │        systemctl reload tinyproxy
   │      '
   │
   ├──► CommandSpec includes HTTPS_PROXY=http://127.0.0.1:8888
   │                         HTTP_PROXY=…
   │                         NO_PROXY=127.0.0.1,localhost,::1
   │
   └──► ao start  (worker shells inherit the proxy env via tmux)
```

The same allowlist is also rendered into the worker's `AGENTS.md` as
a `## Network access` section, so the agent sees the host list up
front and gives a clean "host X is not on the allowlist" message
instead of retry-looping against a blocked CDN.

## Cooperative, not enforcing

This is the important caveat. fleet's Phase 3 allowlist is a **proxy
env var that cooperating tools respect**. A worker process that does
`unset HTTPS_PROXY HTTP_PROXY` can still reach arbitrary hosts.

That works because the bigger boundary is the Lima VM kernel boundary
(see `sandbox.md`): even an agent that bypasses the proxy can only
talk from inside its VM, can't read host dotfiles, and its commits
still go through brokered identity. The proxy is "don't accidentally
fetch from the wrong CDN" — not "an adversary actively trying to
exfiltrate".

**Kernel-level enforcement (nftables egress block forcing all outbound
traffic through tinyproxy, no env-var bypass) is on the roadmap** and
will live alongside the cooperative layer once it ships.

## Tools that respect `HTTPS_PROXY` cleanly

- `gh` CLI (uses `HTTPS_PROXY` and respects `NO_PROXY`)
- `git` over HTTPS (`http.proxy` falls back to `HTTPS_PROXY` env)
- `curl`, `wget`
- `npm`, `pnpm`, `yarn`
- `cargo` (with `[http] check-revoke = false` may need tweaking)
- `pip`, `pipx`
- `claude-code` (Node fetch respects `HTTPS_PROXY`)

If a tool doesn't read `HTTPS_PROXY`, its requests go direct and bypass
the allowlist silently.

## Failure modes

- **Worker tries an off-list host** — tinyproxy returns 403. Tools
  surface this as e.g. `curl: (56) Received HTTP code 403 from proxy
  after CONNECT`. Worker AGENTS.md tells the agent to escalate to the
  user rather than retry.
- **tinyproxy is down** — workers fail to make outbound requests at all.
  Symptoms include `Connection refused` from anything that calls out.
  `limactl shell fleet-vm systemctl status tinyproxy` from the host
  is the diagnostic; logs at `/var/log/tinyproxy/tinyproxy.log`.
- **fleet's sync failed** — fleet logs a warning at start time
  (`tinyproxy filter sync failed; using previous filter`) and proceeds.
  The proxy will serve the last successful filter, which may be empty
  on a fresh VM. Re-run `fleet start` after fixing whatever broke
  (commonly: VM stopped between sync attempts).
- **Mode changed `allowlist → open`** — the next `fleet start` stops
  setting `HTTPS_PROXY` in the worker env, so new workers go direct.
  Existing AO sessions still have the env from when they started;
  restart the AO stack (`fleet stop && fleet start`) to refresh.

## Adding a host

```toml
[network]
mode = "allowlist"
extra_allow = ["my-cdn.example.com"]
```

Then `fleet start` (or Shift+X → Shift+S in the TUI). The next time an
AO worker spawns, the new host is reachable.

## Why tinyproxy?

It's the smallest well-known forward proxy with hostname allowlist
support that fits the cooperative model. ~50KB binary, available in
Ubuntu's main repo, configured via a plain file + regex filter, no
runtime state, fits cleanly into cloud-init.

When kernel-level enforcement lands the proxy may stay, may not —
either way the user-visible config schema (`[network]` block) doesn't
have to change.
