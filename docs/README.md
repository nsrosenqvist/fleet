# fleet docs

User-facing documentation for fleet's behaviour and configuration. The
docs in this directory document **what fleet does** for the operator,
not how the Rust internals work — for those, read the source under
`src/` (each module has a doc-comment explaining its role).

## Sandbox model

[`security-model.md`](./security-model.md) is the umbrella: a single
page describing the three boundaries fleet enforces and the explicit
non-goals. Start there.

The per-boundary deep-dives:

- [`sandbox.md`](./sandbox.md) — filesystem isolation. What's mounted,
  what isn't, what changes for the host's own dotfiles. Read first if
  upgrading from a fleet that bind-mounted the whole `~`.
- [`auth.md`](./auth.md) — identity brokering. How Claude OAuth, the
  gh CLI token, and the worker `.gitconfig` reach the VM now that the
  host home is no longer bind-mounted.
- [`network.md`](./network.md) — egress allowlist. How `[network]
  mode = "allowlist"` routes worker traffic through the in-VM
  tinyproxy, and what the cooperative model does and doesn't enforce.

## Workspace model

- [`worktrees.md`](./worktrees.md) — per-session git worktrees. Why
  every `fleet workflow run` gets its own branch + checked-out
  working directory, how replay re-uses the prior session's branch
  tip, and how `fleet sessions prune` reclaims disk while keeping
  agent commits inspectable.
