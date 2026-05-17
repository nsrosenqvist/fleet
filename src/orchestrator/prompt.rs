//! Orchestrator-agent system prompt: a static template + a per-
//! session repo snapshot.
//!
//! The prompt lives at `<fleet_root>/.fleet/planning/<id>/prompt.md`
//! and is the first thing the agent sees on startup. It establishes
//! the agent's role, lists the fleet CLI subcommands the agent can
//! shell to, and snapshots the repo's current open tickets +
//! active plans so the agent has immediate context.
//!
//! Pure: takes a `RepoSnapshot` value object and returns a string.
//! The CLI in `cli::orchestrator` is responsible for building the
//! snapshot from the tracker + plan store at session-spawn time.
//!
//! Per-repo overrides land at `.fleet/prompts/orchestrator.md` —
//! same pattern as the workflow persona prompts. Use
//! [`render_prompt_for_repo`] to apply them; [`render_prompt`]
//! always uses the built-in template.

use crate::plans::Plan;
use crate::tracker::Issue;

/// Snapshot of the repo state shown to the orchestrator agent at
/// startup. Tracker issues are filtered to "open" by the caller;
/// plans are filtered to "Active". Both lists are passed as-is in
/// their existing sort order (open-first for issues; ms-prefixed
/// id-order ≈ chronological for plans).
#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    pub open_issues: Vec<Issue>,
    pub active_plans: Vec<Plan>,
}

/// Render the system prompt for a fresh orchestrator session. Combines
/// the static template with the dynamic repo snapshot.
#[must_use]
pub fn render_prompt(snapshot: &RepoSnapshot) -> String {
    render_prompt_with_template(STATIC_TEMPLATE, snapshot)
}

/// Same as [`render_prompt`] but with a caller-supplied template
/// string. Used by [`render_prompt_for_repo`] to apply a per-repo
/// override; also handy for tests that pin the exact template.
#[must_use]
pub fn render_prompt_with_template(template: &str, snapshot: &RepoSnapshot) -> String {
    let mut out = String::with_capacity(template.len() + 4096);
    out.push_str(template);
    out.push_str("\n\n## Current repo state\n\n");
    out.push_str(&render_open_issues(&snapshot.open_issues));
    out.push('\n');
    out.push_str(&render_active_plans(&snapshot.active_plans));
    out
}

/// Render the prompt with a per-repo override if present.
/// `<fleet_root>/.fleet/prompts/orchestrator.md` overrides the
/// built-in template when it exists — same pattern fleet uses
/// for workflow persona prompts (`prompts/planner.md` etc.) but
/// applied to orchestrator's system prompt. The override is the
/// whole template; the dynamic repo snapshot (open issues +
/// active plans) is appended after either way.
///
/// File-read failures other than "not found" surface in the
/// returned [`Result`] — a malformed-encoding override is a real
/// error, not a silent fallback to the built-in. The CLI surfaces
/// it on stderr and proceeds with the built-in.
pub fn render_prompt_for_repo(
    fleet_root: &std::path::Path,
    snapshot: &RepoSnapshot,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let override_path = fleet_root.join(".fleet/prompts/orchestrator.md");
    let template = match std::fs::read_to_string(&override_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => STATIC_TEMPLATE.to_string(),
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!(
                    "reading orchestrator prompt override at {}",
                    override_path.display()
                )
            });
        }
    };
    Ok(render_prompt_with_template(&template, snapshot))
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
const STATIC_TEMPLATE: &str = r#"# Fleet orchestrator session

You are the **orchestrator agent** for this fleet-managed repo. Your
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
- `fleet sessions unblock <id> [--reason "<text>"]`  — clear *all* deps edges keyed by this session's ticket
- `fleet sessions logs <id> [--node <name>]`         — printed captured logs

**Deps (cross-ticket blocked-on edges):**
- `fleet deps list`                                  — every blocked-on edge with its kind
- `fleet deps add <blocked> --blocked-on <ticket>`   — record a ticket-blocks-ticket edge
- `fleet deps add <blocked> --blocked-on-tag <slug>` — record a freeform-tag edge (e.g. blocked on the apt mirror)
- `fleet deps remove <blocked> --blocked-on <id>`    — surgical single-edge removal

`fleet deps add` refuses to create cycles — if the proposed edge
would close a path back to itself, you'll see a clear error and no
edge will be recorded.

## Behaviour expectations

- **Confirm before any write.** State the proposed action ("I'm
  going to file three tickets and create a plan called X") and
  wait for the user to acknowledge. The user is the last line of
  defence against prompt-injected mistakes.
- **Read before write.** When the user asks you to plan around an
  epic / ticket, fetch the current state first (`fleet issues
  list`, `fleet plan list`, `fleet deps list`) rather than
  guessing.
- **Decompose multi-component work contracts-first.** When the
  proposed work spans more than one independently-deployable
  component (backend + frontend, service + client, library +
  consumer, schema + migrations), file a *contracts* ticket
  first — the API shape, message schemas, type definitions, or
  an ADR capturing the architectural decision. Then file the
  per-component implementation tickets, each with a
  `fleet deps add <impl-ticket> --blocked-on <contracts-ticket>`
  edge. The supervisor will leave the implementation tickets
  alone until the contracts ticket closes, so two agents can't
  diverge into incompatible shapes. **Trigger check:** "could
  two implementations land in parallel and discover at
  integration time that they assumed different interfaces?" If
  yes, contracts-first. Single-component work doesn't need this.
- **ADRs vs schemas.** If the interface is executable (OpenAPI,
  protobuf, shared types package, SQL migration) the contracts
  ticket produces that artifact. If the decision is
  architectural with no single executable shape (auth strategy,
  state machine boundaries, ownership of a concept) the
  contracts ticket produces an ADR in `docs/decisions/<n>-<slug>.md`
  capturing the choice + rationale. Pick the lighter of the two
  that still pins the interface enough for the implementation
  tickets to proceed independently.
- **Use plans as preference, deps as requirement.** A plan is the
  user's ordered preference; the deps graph is what physically
  blocks scheduling. They compose — when filing a follow-up that
  blocks an existing ticket, both the plan injection and the deps
  edge should be in place.
- **Don't recommend tickets for transient issues.** Same rule
  workflow agents follow: a flaky CI run isn't a ticket.

## Workflow

Typical patterns:

- **"What's open?"** → `fleet issues list` + `fleet plan list` +
  `fleet deps list`, summarise.
- **"Plan a refactor"** → discuss with user, then `fleet issues
  create …` for each ticket (capture each id), then `fleet plan
  new "<name>" --tickets <ids,joined,by,commas>`.
- **"Plan a feature spanning backend + frontend"** (or any
  multi-component work) → propose the contracts-first
  decomposition to the user:
    1. Decide with the user whether the contracts ticket is an
       executable schema (OpenAPI/protobuf/types) or an ADR. If
       in doubt, ask. Either way it's one ticket.
    2. `fleet issues create "Define <feature> contracts" …` →
       capture the new id as `T-contracts`.
    3. `fleet issues create "Implement <feature> backend" …` →
       capture as `T-backend`.
    4. `fleet issues create "Implement <feature> frontend" …` →
       capture as `T-frontend`.
    5. `fleet deps add T-backend --blocked-on T-contracts`
    6. `fleet deps add T-frontend --blocked-on T-contracts`
    7. `fleet plan new "<feature>" --tickets T-contracts,T-backend,T-frontend`
   The supervisor will pick `T-contracts` first; only after it
   closes will the implementation tickets become eligible. The
   plan keeps them visible together so the user can `fleet plan
   show` and see the whole feature at a glance.
- **"Where are we on plan X?"** → `fleet plan show <id>` + recent
  `fleet sessions list` entries.
- **"Unblock session Y"** → `fleet sessions unblock <id>` (with
  `--reason` if the user gave one). When you want to remove a
  specific dep edge rather than clear all of them, use
  `fleet deps remove <blocked> --blocked-on <id>` instead.
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
        assert!(p.contains("orchestrator agent"), "missing role");
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
        assert!(!p.contains("FLEET_ORCHESTRATOR_URL"), "stale http env var");
        assert!(!p.contains("FLEET_ORCHESTRATOR_TOKEN"), "stale http token");
    }

    #[test]
    fn render_prompt_advertises_deps_cli_instead_of_cat_jsonfile() {
        // After the `fleet deps add/list/remove` CLI shipped, the
        // prompt must point the agent at the real surface, not at
        // `cat .fleet/deps.json`. Catches a stale-doc regression.
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        assert!(p.contains("fleet deps list"), "missing deps list tool");
        assert!(p.contains("fleet deps add"), "missing deps add tool");
        assert!(p.contains("fleet deps remove"), "missing deps remove tool");
        assert!(
            !p.contains("cat .fleet/deps.json"),
            "stale deps.json reference leaked: prompt now uses fleet deps list"
        );
    }

    #[test]
    fn render_prompt_includes_contracts_first_decomposition_rule() {
        // The contracts-first behaviour rule is the load-bearing
        // bit of guidance this prompt carries — without it the
        // agent will happily file a backend + frontend ticket
        // pair without an interface ticket. Pin both the rule and
        // the trigger language so the prompt can't silently lose
        // either half.
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        assert!(
            p.contains("contracts-first") || p.contains("Decompose multi-component"),
            "missing contracts-first behaviour rule"
        );
        assert!(
            p.contains("integration time"),
            "missing trigger-check phrasing"
        );
        // ADR vs schema choice is documented.
        assert!(p.contains("ADR"), "ADR option missing");
        assert!(
            p.contains("docs/decisions/"),
            "ADR file location missing"
        );
    }

    #[test]
    fn render_prompt_walks_through_multi_component_workflow_pattern() {
        // The Workflow section's multi-component pattern should
        // spell out the exact CLI sequence — agents pattern-match
        // on examples, so the worked example matters more than the
        // abstract rule.
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt(&snapshot);
        assert!(
            p.contains("backend + frontend"),
            "missing multi-component pattern label"
        );
        assert!(
            p.contains("--blocked-on T-contracts"),
            "missing deps edge example"
        );
        assert!(
            p.contains("fleet plan new") && p.contains("T-contracts,T-backend,T-frontend"),
            "missing plan-creation step that ties the three tickets together"
        );
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
    fn render_prompt_for_repo_uses_builtin_when_override_absent() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt_for_repo(dir.path(), &snapshot).unwrap();
        assert!(p.contains("fleet plan list"), "should use built-in");
    }

    #[test]
    fn render_prompt_for_repo_uses_override_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".fleet/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let override_body = "# Custom orchestrator role\n\nYou are the team's planner.";
        std::fs::write(prompts_dir.join("orchestrator.md"), override_body).unwrap();

        let snapshot = RepoSnapshot {
            open_issues: Vec::new(),
            active_plans: Vec::new(),
        };
        let p = render_prompt_for_repo(dir.path(), &snapshot).unwrap();
        assert!(p.contains("# Custom orchestrator role"), "override missing");
        assert!(p.contains("team's planner"), "override body missing");
        // Built-in template's content shouldn't leak in.
        assert!(!p.contains("fleet plan list"), "built-in leaked: {p}");
        // The dynamic snapshot section is still appended.
        assert!(p.contains("## Current repo state"), "snapshot missing");
    }

    #[test]
    fn render_prompt_for_repo_appends_snapshot_to_override_template() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".fleet/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("orchestrator.md"), "# Minimal").unwrap();
        let snapshot = RepoSnapshot {
            open_issues: vec![issue("42", "Fix parser", &[])],
            active_plans: Vec::new(),
        };
        let p = render_prompt_for_repo(dir.path(), &snapshot).unwrap();
        assert!(p.contains("# Minimal"));
        assert!(p.contains("### Open issues (1)"));
        assert!(p.contains("#42 — Fix parser"));
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
