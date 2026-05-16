//! Brainstorm-agent system prompt: a static template + a per-
//! session repo snapshot.
//!
//! The prompt lives at `<fleet_root>/.fleet/planning/<id>/prompt.md`
//! and is the first thing the agent sees on startup. It establishes
//! the agent's role, lists the fleet CLI subcommands the agent can
//! shell to, and snapshots the repo's current open tickets +
//! active plans so the agent has immediate context.
//!
//! Pure: takes a `RepoSnapshot` value object and returns a string.
//! The CLI in `cli::brainstorm` is responsible for building the
//! snapshot from the tracker + plan store at session-spawn time.
//!
//! Future direction (out of scope for v1): per-repo prompt
//! overrides under `.fleet/prompts/brainstorm.md` would let teams
//! customise the role description without touching fleet
//! internals — same pattern as the workflow persona prompts.

use crate::plans::Plan;
use crate::tracker::Issue;

/// Snapshot of the repo state shown to the brainstorm agent at
/// startup. Tracker issues are filtered to "open" by the caller;
/// plans are filtered to "Active". Both lists are passed as-is in
/// their existing sort order (open-first for issues; ms-prefixed
/// id-order ≈ chronological for plans).
#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    pub open_issues: Vec<Issue>,
    pub active_plans: Vec<Plan>,
}

/// Render the system prompt for a fresh brainstorm session. Combines
/// the static template with the dynamic repo snapshot.
#[must_use]
pub fn render_prompt(snapshot: &RepoSnapshot) -> String {
    let mut out = String::with_capacity(STATIC_TEMPLATE.len() + 4096);
    out.push_str(STATIC_TEMPLATE);
    out.push_str("\n\n## Current repo state\n\n");
    out.push_str(&render_open_issues(&snapshot.open_issues));
    out.push('\n');
    out.push_str(&render_active_plans(&snapshot.active_plans));
    out
}

fn render_open_issues(issues: &[Issue]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if issues.is_empty() {
        out.push_str("### Open issues\n\n(none)\n");
        return out;
    }
    let _ = writeln!(out, "### Open issues ({})", issues.len());
    out.push('\n');
    for issue in issues {
        let labels = if issue.labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", issue.labels.join(", "))
        };
        let _ = writeln!(out, "- #{} — {}{labels}", issue.human_id, issue.title);
    }
    out
}

fn render_active_plans(plans: &[Plan]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if plans.is_empty() {
        out.push_str("### Active plans\n\n(none)\n");
        return out;
    }
    let _ = writeln!(out, "### Active plans ({})", plans.len());
    out.push('\n');
    for plan in plans {
        let (done, total) = plan.progress();
        let _ = writeln!(
            out,
            "- **{}** ({}) — {}/{}",
            plan.name, plan.id, done, total
        );
        for (idx, item) in plan.items.iter().enumerate() {
            let marker = if item.injected { " (injected)" } else { "" };
            let _ = writeln!(
                out,
                "  {idx:>3}. {ticket} — {state:?}{marker}",
                idx = idx,
                ticket = item.ticket_id,
                state = item.state,
            );
        }
        out.push('\n');
    }
    out
}

/// Static template, prepended verbatim. Defines the agent's role
/// and the CLI-tool surface so the agent doesn't have to guess at
/// either. The dynamic repo snapshot is appended after.
const STATIC_TEMPLATE: &str = r#"# Fleet brainstorm session

You are the **brainstorm agent** for this fleet-managed repo. Your
job is to help the user plan work, triage and file tickets, and
manage fleet plans + dependency edges. You run on the *host*, not
in a container, with broad authority — but the user is in the
loop on every tracker mutation, so confirm anything that creates,
closes, or moves a ticket before executing.

## How to act on the repo

You have a normal shell inside this tmux pane. Every action lands
through fleet's CLI — no HTTP, no auth tokens. Run commands with
the standard tool you'd use to invoke `gh` or `git`.

Commands print human-readable text and exit 0 on success;
non-zero exit + stderr surfaces what went wrong. Surface stderr
to the user verbatim when something fails.

### Tools (fleet CLI subcommands)

**Plans:**
- `fleet plan list`                                  — every plan + progress
- `fleet plan show <id>`                             — one plan, full detail
- `fleet plan new "<name>" --tickets <a,b,c>`        — create
- `fleet plan pause <id>`                            — supervisor stops picking from it
- `fleet plan resume <id>`                           — back to Active
- `fleet plan complete <id>`                         — mark done
- `fleet plan abandon <id> [--reason "<text>"]`      — mark abandoned
- `fleet plan inject <id> <ticket> [--before <other>]` — add an item

**Tracker (issues):**
- `fleet issues list`                                — every issue, open first
- `fleet issues create "<title>" [--body "<body>"] [--label <name>]…` — file
- `fleet issues comment <id> "<body>"`               — comment on a ticket
- `fleet issues set-status <id> <open|in-progress|closed>`
- `fleet issues add-label <id> <label>`
- `fleet issues remove-label <id> <label>`

`fleet issues create` prints the new ticket's id on stdout (one
line) — capture it for follow-up `fleet plan new --tickets …`
calls.

**Sessions:**
- `fleet sessions list`                              — all workflow sessions
- `fleet sessions show <id>`                         — one session, full detail
- `fleet sessions unblock <id> [--reason "<text>"]`  — clear deps edges
- `fleet sessions logs <id> [--node <name>]`         — printed captured logs

**Deps (rarely needed; mostly a read for surgery):**
- `cat .fleet/deps.json`                             — every blocked-on edge

## Behaviour expectations

- **Confirm before any write.** State the proposed action ("I'm
  going to file three tickets and create a plan called X") and
  wait for the user to acknowledge. The user is the last line of
  defence against prompt-injected mistakes.
- **Read before write.** When the user asks you to plan around an
  epic / ticket, fetch the current state first (`fleet issues
  list`, `fleet plan list`) rather than guessing.
- **Use plans as preference, deps as requirement.** A plan is the
  user's ordered preference; the deps graph is what physically
  blocks scheduling. They compose — when filing a follow-up that
  blocks an existing ticket, both the plan injection and the deps
  edge should be in place.
- **Don't recommend tickets for transient issues.** Same rule
  workflow agents follow: a flaky CI run isn't a ticket.

## Workflow

Typical patterns:

- **"What's open?"** → `fleet issues list` + `fleet plan list`,
  summarise.
- **"Plan a refactor"** → discuss with user, then `fleet issues
  create …` for each ticket (capture each id), then `fleet plan
  new "<name>" --tickets <ids,joined,by,commas>`.
- **"Where are we on plan X?"** → `fleet plan show <id>` + recent
  `fleet sessions list` entries.
- **"Unblock session Y"** → `fleet sessions unblock <id>` (with
  `--reason` if the user gave one).
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::{Plan, PlanId, PlanItemState};

    fn issue(human: &str, title: &str, labels: &[&str]) -> Issue {
        Issue {
            id: format!("gh:{human}"),
            human_id: human.into(),
            title: title.into(),
            status: "open".into(),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn sample_plan(id: &str, name: &str, tickets: &[&str]) -> Plan {
        Plan::new(
            PlanId::new(id),
            name,
            tickets.iter().map(|s| (*s).to_string()).collect(),
            1_700_000_000_000,
        )
    }

    #[test]
    fn render_prompt_includes_static_template_sections() {
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        // Role section + CLI tool surface + behaviour expectations
        // all present.
        assert!(p.contains("brainstorm agent"), "missing role");
        assert!(p.contains("fleet plan list"), "missing plan tool");
        assert!(p.contains("fleet issues create"), "missing tracker create");
        assert!(
            p.contains("fleet sessions unblock"),
            "missing sessions tool"
        );
        assert!(
            p.contains("Confirm before any write"),
            "missing behaviour rule"
        );
        // Stale HTTP-server references shouldn't leak — the prompt
        // is CLI-only since the parallel HTTP path got removed.
        assert!(!p.contains("FLEET_BRAINSTORM_URL"), "stale http env var");
        assert!(!p.contains("FLEET_BRAINSTORM_TOKEN"), "stale http token");
    }

    #[test]
    fn render_prompt_lists_open_issues_with_labels() {
        let snapshot = RepoSnapshot {
            open_issues: vec![
                issue("42", "Fix parser", &["bug", "urgent"]),
                issue("43", "Add docs", &[]),
            ],
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        assert!(p.contains("### Open issues (2)"));
        assert!(p.contains("#42 — Fix parser [bug, urgent]"));
        assert!(p.contains("#43 — Add docs"));
        // No labels marker on the second issue.
        let line_43 = p.lines().find(|l| l.contains("#43")).unwrap();
        assert!(!line_43.contains('['), "got: {line_43}");
    }

    #[test]
    fn render_prompt_says_none_when_no_open_issues() {
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: vec![sample_plan("plan-1", "p", &["42"])],
        };
        let p = render_prompt(&snapshot);
        assert!(p.contains("### Open issues\n\n(none)"));
    }

    #[test]
    fn render_prompt_lists_active_plans_with_items_and_progress() {
        let mut plan = sample_plan("plan-1", "Parser refactor", &["42", "43", "44"]);
        plan.items[0].state = PlanItemState::Completed;
        plan.items[1].state = PlanItemState::InProgress;
        plan.items[2].injected = true;
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: vec![plan],
        };
        let p = render_prompt(&snapshot);
        assert!(p.contains("### Active plans (1)"));
        assert!(p.contains("**Parser refactor**"));
        assert!(p.contains("1/3"));
        assert!(p.contains("42 — Completed"));
        assert!(p.contains("43 — InProgress"));
        assert!(p.contains("44 — Pending (injected)"));
    }

    #[test]
    fn render_prompt_says_none_when_no_active_plans() {
        let snapshot = RepoSnapshot {
            open_issues: vec![issue("42", "T", &[])],
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        assert!(p.contains("### Active plans\n\n(none)"));
    }

    #[test]
    fn render_prompt_preserves_caller_supplied_order() {
        // The renderer doesn't sort — it trusts the caller. Useful
        // when the caller wants e.g. tracker-native priority order
        // for issues.
        let snapshot = RepoSnapshot {
            open_issues: vec![issue("99", "Z-last", &[]), issue("42", "A-first", &[])],
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        let z_idx = p.find("Z-last").unwrap();
        let a_idx = p.find("A-first").unwrap();
        assert!(z_idx < a_idx, "render must preserve input order");
    }
}
