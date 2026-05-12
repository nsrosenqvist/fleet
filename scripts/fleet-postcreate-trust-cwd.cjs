#!/usr/bin/env node
// Marks the current working directory as trusted + bypass-mode-accepted in
// ~/.claude.json. Invoked by AO's worktree workspace plugin via
// `projects.<id>.postCreate` — that hook runs with `cwd: <worktree path>`
// AFTER the worktree is created and BEFORE claude is launched, so writing
// `projects[cwd].{hasTrustDialogAccepted, bypassPermissionsModeAccepted}`
// makes claude skip both first-run dialogs.
//
// claude 2.1.139 gates the bypass dialog on the per-project entry, not on
// the top-level `bypassPermissionsModeAccepted` field — both have to be set.
//
// Idempotent: merges into any existing `~/.claude.json` shape.

const fs = require("node:fs");
const path = require("node:path");

const home = process.env.HOME;
if (!home) {
  console.error("fleet-postcreate-trust-cwd: $HOME is not set");
  process.exit(1);
}

const file = path.join(home, ".claude.json");
const cwd = process.cwd();

let data = {};
try {
  data = JSON.parse(fs.readFileSync(file, "utf8"));
} catch {
  // Missing or invalid file — treat as empty; claude will populate on next run.
}

data.projects = data.projects ?? {};
data.projects[cwd] = {
  ...data.projects[cwd],
  hasTrustDialogAccepted: true,
  bypassPermissionsModeAccepted: true,
};

fs.writeFileSync(file, JSON.stringify(data, null, 2));
