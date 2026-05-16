//! `fleet plan …` — CLI surface over [`crate::plans::store::PlanStore`].
//!
//! Ten subcommands: `list`, `show`, `new`, `edit`, `pause`, `resume`,
//! `complete`, `abandon`, `inject`, `sync`. Render helpers are pure
//! functions over `Plan` so output tests don't need a real filesystem;
//! the `run_*` wrappers do the I/O.
//!
//! State transitions are user-driven through this module — the
//! autonomous supervisor (Phase 4) only advances *item* states; plan-
//! level moves between Active/Paused/Completed/Abandoned go through
//! the CLI (or, in Phase 5, the brainstorm agent).

use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::plans::store::PlanStore;
use crate::plans::{
    ClockPlanIdSource, Plan, PlanId, PlanIdSource, PlanItem, PlanItemState, PlanState,
};
use crate::repo;
use crate::session::now_ms;

// === Top-level entry points (called from `cli::dispatch`) ===

/// `fleet plan list`. Sorted by id (chronological in practice thanks
/// to the ms-prefixed id format).
pub fn run_list() -> Result<i32> {
    let store = open_store()?;
    let ids = store
        .list()
        .with_context(|| format!("reading {}", store.root().display()))?;
    let mut plans = Vec::with_capacity(ids.len());
    for id in ids {
        match store.load(&id) {
            Ok(p) => plans.push(p),
            Err(err) => {
                eprintln!("warning: skipping unreadable plan {}: {err:#}", id.as_str());
            }
        }
    }
    print!("{}", render_list(store.root(), &plans));
    Ok(0)
}

/// `fleet plan show <id>`.
pub fn run_show(id: &str) -> Result<i32> {
    let store = open_store()?;
    let plan = load_plan(&store, id)?;
    print!("{}", render_show(&plan));
    Ok(0)
}

/// `fleet plan new <name> --tickets 42,43,44`.
pub fn run_new(name: &str, tickets_csv: &str) -> Result<i32> {
    let store = open_store()?;
    let tickets = parse_tickets_csv(tickets_csv)?;
    if name.trim().is_empty() {
        bail!("plan name must be a non-empty string");
    }
    let id_source = ClockPlanIdSource;
    let plan = Plan::new(id_source.mint(), name, tickets, now_ms());
    store
        .create(&plan)
        .with_context(|| format!("creating plan `{}`", plan.id))?;
    println!("{}", plan.id);
    Ok(0)
}

/// `fleet plan pause <id>`.
pub fn run_pause(id: &str) -> Result<i32> {
    transition_state(id, PlanState::Paused)
}

/// `fleet plan resume <id>`.
pub fn run_resume(id: &str) -> Result<i32> {
    transition_state(id, PlanState::Active)
}

/// `fleet plan complete <id>`.
pub fn run_complete(id: &str) -> Result<i32> {
    transition_state(id, PlanState::Completed)
}

/// `fleet plan abandon <id> [--reason <text>]`. The reason is
/// recorded to stderr for now — Phase 5's brainstorm agent will be
/// the natural place to record it onto an epic comment.
pub fn run_abandon(id: &str, reason: Option<&str>) -> Result<i32> {
    transition_state(id, PlanState::Abandoned)?;
    if let Some(reason) = reason {
        eprintln!("abandoned reason: {reason}");
    }
    Ok(0)
}

/// `fleet plan inject <id> <ticket-id> [--before <other-id>]`. Adds
/// a `Pending` item with `injected: false` (the CLI surface is for
/// human edits; `injected: true` is reserved for the workflow
/// engine's `tracker-create` plan-injector).
pub fn run_inject(id: &str, ticket_id: &str, before: Option<&str>) -> Result<i32> {
    let store = open_store()?;
    let mut plan = load_plan(&store, id)?;
    let position = match before {
        Some(other) => plan.position_of(other).ok_or_else(|| {
            anyhow::anyhow!("plan `{id}` has no item with ticket id `{other}` to insert before")
        })?,
        // Default: append at the end. Matches the typical "I've
        // thought of one more thing to do" usage.
        None => plan.items.len(),
    };
    plan.items.insert(position, PlanItem::pending(ticket_id));
    plan.updated_at_ms = now_ms();
    store
        .save(&plan)
        .with_context(|| format!("saving plan `{id}` after inject"))?;
    Ok(0)
}

/// `fleet plan sync <id>` — additively reconcile a plan with its
/// tracker-native epic. Reads the epic body, parses the markdown
/// task-list lines, and appends every referenced ticket that
/// isn't already a plan item. Existing items keep their state and
/// order; the epic itself isn't written back.
///
/// Errors out when:
/// - the plan has no `epic_ref` (nothing to sync against),
/// - no tracker is configured (`.fleet/config.yaml` `tracker:`),
/// - the configured tracker's `read` impl bails (Linear/Jira at
///   v1 — git-bug supports it because epics are encoded via
///   labels, but `parse_task_list_tickets` won't find anything in
///   that body and will report 0 new items).
pub fn run_sync(id: &str) -> Result<i32> {
    use std::sync::Arc;
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let store = PlanStore::for_repo(&root);
    let mut plan = load_plan(&store, id)?;
    let epic = plan.epic_ref.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "plan `{id}` has no epic_ref; `fleet plan sync` needs an epic \
             to reconcile against. Edit the plan (`fleet plan edit {id}`) \
             and add an `epic_ref:` block, or let the brainstorm agent \
             populate it via the supervisor."
        )
    })?;
    let config = crate::repo_config::RepoConfig::load(root.join(".fleet/config.yaml"))
        .with_context(|| format!("loading repo config under {}", root.display()))?;
    let invoker: Arc<dyn crate::process::ProcessInvoker> =
        Arc::new(crate::process::RealProcessInvoker);
    let tracker = crate::tracker::build(config.tracker, invoker).ok_or_else(|| {
        anyhow::anyhow!(
            "no tracker configured for this repo (.fleet/config.yaml `tracker:`); \
             `fleet plan sync` needs one to read the epic body"
        )
    })?;
    let detail = tracker
        .read(&root, &epic.id)
        .with_context(|| format!("reading epic `{}:{}`", epic.tracker, epic.id))?;
    let outcome = sync_plan_against_body(&mut plan, &detail.body);
    if outcome.added_ticket_ids.is_empty() {
        println!(
            "plan `{id}` already in sync with epic `{}:{}` ({} items)",
            epic.tracker,
            epic.id,
            plan.items.len()
        );
        return Ok(0);
    }
    plan.updated_at_ms = now_ms();
    store
        .save(&plan)
        .with_context(|| format!("saving plan `{id}` after sync"))?;
    println!(
        "plan `{id}`: added {} item{} from epic `{}:{}` ({} items total)",
        outcome.added_ticket_ids.len(),
        if outcome.added_ticket_ids.len() == 1 {
            ""
        } else {
            "s"
        },
        epic.tracker,
        epic.id,
        plan.items.len()
    );
    for tid in &outcome.added_ticket_ids {
        println!("  + {tid}");
    }
    Ok(0)
}

/// Outcome of a plan-sync mutation. Pure value object so tests
/// can assert on the report without re-reading the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Ticket ids appended to the plan in this sync, in the order
    /// they were discovered in the epic body.
    pub added_ticket_ids: Vec<String>,
}

/// Append every ticket id mentioned in `body`'s task-list lines
/// that isn't already a plan item. Pure: doesn't touch
/// `updated_at_ms` (the caller does, only when there's something
/// to save). Items are appended at the end so the existing plan
/// order is preserved.
#[must_use]
pub fn sync_plan_against_body(plan: &mut Plan, body: &str) -> SyncOutcome {
    let found = parse_task_list_tickets(body);
    let mut added = Vec::new();
    for tid in found {
        if plan.position_of(&tid).is_some() {
            continue;
        }
        plan.items.push(PlanItem::pending(tid.clone()));
        added.push(tid);
    }
    SyncOutcome {
        added_ticket_ids: added,
    }
}

/// Pure parser for GitHub-flavored task-list lines. Recognises:
///
/// - `- [ ] #42` / `- [x] #42` (open / completed; we treat both
///   as "in the epic" and don't distinguish state here — the
///   supervisor will reconcile item state via session results),
/// - leading whitespace (so nested task lists work),
/// - trailing text after the ticket id (descriptions are
///   ignored).
///
/// Ticket ids are returned in the order they appear in `body`,
/// with duplicates dropped after the first occurrence (so a
/// task-list line that repeats a ticket doesn't double-count).
#[must_use]
pub fn parse_task_list_tickets(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        // Match `- [ ] ` or `- [x] ` / `- [X] ` at the start of a
        // task-list line. Anything else is skipped — we don't
        // want to pick up `#42` mentions in prose.
        let rest = if let Some(r) = trimmed.strip_prefix("- [ ] ") {
            r
        } else if let Some(r) = trimmed.strip_prefix("- [x] ") {
            r
        } else if let Some(r) = trimmed.strip_prefix("- [X] ") {
            r
        } else {
            continue;
        };
        // First whitespace-delimited token after the checkbox.
        let token = rest.split_whitespace().next().unwrap_or("");
        let ticket = if let Some(stripped) = token.strip_prefix('#') {
            stripped.to_string()
        } else {
            // No `#` prefix — accept bare ids too (git-bug doesn't
            // use `#` in human ids). Refuse anything that doesn't
            // start with an alphanumeric, though, so prose lines
            // that happen to start with `- [ ] note...` don't get
            // picked up.
            if !token.chars().next().is_some_and(char::is_alphanumeric) {
                continue;
            }
            token.to_string()
        };
        if ticket.is_empty() {
            continue;
        }
        if seen.insert(ticket.clone()) {
            out.push(ticket);
        }
    }
    out
}

/// `fleet plan edit <id>` — open the YAML in `$EDITOR`, parse the
/// result back, atomic-save if valid. A parse failure leaves the
/// on-disk file untouched.
pub fn run_edit(id: &str) -> Result<i32> {
    let store = open_store()?;
    let plan = load_plan(&store, id)?;
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map_err(|_| {
            anyhow::anyhow!(
                "neither $EDITOR nor $VISUAL is set; set one (e.g. `EDITOR=vi`) \
                 and re-run"
            )
        })?;
    // Use the system temp dir with a per-process-unique filename so
    // concurrent `fleet plan edit` calls (e.g. two terminals) don't
    // race over the same buffer. tempfile is dev-only so we manage
    // cleanup by hand below.
    let tmp_path = std::env::temp_dir().join(format!(
        "fleet-plan-edit-{}-{}.yaml",
        plan.id,
        std::process::id()
    ));
    let body = serde_yml::to_string(&plan).context("serialising plan to edit buffer")?;
    std::fs::write(&tmp_path, body)
        .with_context(|| format!("writing edit buffer at {}", tmp_path.display()))?;

    let edit_result = run_edit_inner(&store, id, &plan, &editor, &tmp_path);
    // Best-effort cleanup. A stale file in $TMPDIR is mildly
    // unfriendly but won't break anything; we leave it on cleanup
    // failure rather than masking the real error.
    let _ = std::fs::remove_file(&tmp_path);
    edit_result
}

/// Inner half of `run_edit`: invoke `$EDITOR` against `tmp_path`,
/// parse the edited buffer, and save. Split out so the caller can
/// always run cleanup regardless of how this returns.
fn run_edit_inner(
    store: &PlanStore,
    id: &str,
    plan: &Plan,
    editor: &str,
    tmp_path: &Path,
) -> Result<i32> {
    let status = std::process::Command::new(editor)
        .arg(tmp_path)
        .status()
        .with_context(|| format!("invoking editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited non-zero ({status}); plan `{id}` left unchanged");
    }

    let edited = std::fs::read_to_string(tmp_path)
        .with_context(|| format!("re-reading edit buffer at {}", tmp_path.display()))?;
    let mut new_plan: Plan = serde_yml::from_str(&edited).with_context(|| {
        format!("parsing edited plan YAML (the on-disk plan `{id}` was left unchanged)")
    })?;
    // Guard against a user renaming the id mid-edit — the file lives
    // under the original id, so a new id would silently orphan the
    // record. Restore the original.
    new_plan.id = plan.id.clone();
    new_plan.updated_at_ms = now_ms();
    store
        .save(&new_plan)
        .with_context(|| format!("saving edited plan `{id}`"))?;
    Ok(0)
}

// === Shared helpers ===

fn open_store() -> Result<PlanStore> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    Ok(PlanStore::for_repo(&root))
}

fn load_plan(store: &PlanStore, id: &str) -> Result<Plan> {
    let pid = PlanId::new(id);
    store
        .load(&pid)
        .with_context(|| format!("loading plan `{id}`"))
}

fn transition_state(id: &str, target: PlanState) -> Result<i32> {
    let store = open_store()?;
    let mut plan = load_plan(&store, id)?;
    plan.state = target;
    plan.updated_at_ms = now_ms();
    store
        .save(&plan)
        .with_context(|| format!("saving plan `{id}` after state transition to {target:?}"))?;
    Ok(0)
}

/// Parse `42,43,44` (or `42, 43, 44`) into `vec!["42", "43", "44"]`.
/// Empty entries are rejected — a missed comma or trailing one
/// shouldn't silently produce a zero-ticket plan.
pub fn parse_tickets_csv(csv: &str) -> Result<Vec<String>> {
    let parts: Vec<&str> = csv.split(',').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!("`--tickets` contains an empty entry (check for trailing/double commas)");
    }
    if parts.is_empty() {
        bail!("`--tickets` must list at least one ticket id");
    }
    Ok(parts.into_iter().map(String::from).collect())
}

// === Renderers ===

#[must_use]
pub fn render_list(root: &Path, plans: &[Plan]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "plans (root: {}):", root.display());
    if plans.is_empty() {
        out.push_str("  (no plans yet)\n");
        return out;
    }
    for plan in plans {
        let (done, total) = plan.progress();
        let _ = writeln!(
            out,
            "  {state:<10} {id} {progress:>7} {name}",
            state = state_label(plan.state),
            id = plan.id,
            progress = format!("{done}/{total}"),
            name = plan.name,
        );
    }
    out
}

#[must_use]
pub fn render_show(plan: &Plan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "id:    {}", plan.id);
    let _ = writeln!(out, "name:  {}", plan.name);
    let _ = writeln!(out, "state: {}", state_label(plan.state));
    let (done, total) = plan.progress();
    let _ = writeln!(out, "items: {done}/{total} completed");
    let _ = writeln!(out, "on_item_failure: {:?}", plan.on_item_failure);
    if let Some(epic) = &plan.epic_ref {
        let _ = writeln!(out, "epic_ref: {}:{}", epic.tracker, epic.id);
    }
    out.push('\n');
    for (idx, item) in plan.items.iter().enumerate() {
        let marker = if item.injected { " (injected)" } else { "" };
        let session = item
            .session_id
            .as_ref()
            .map_or(String::new(), |s| format!(" · session {s}"));
        let _ = writeln!(
            out,
            "  {idx:>3}. {item_state:<11} {ticket}{marker}{session}",
            idx = idx,
            item_state = item_state_label(item.state),
            ticket = item.ticket_id,
        );
    }
    out
}

#[must_use]
fn state_label(state: PlanState) -> &'static str {
    match state {
        PlanState::Active => "active",
        PlanState::Paused => "paused",
        PlanState::Completed => "completed",
        PlanState::Abandoned => "abandoned",
    }
}

#[must_use]
fn item_state_label(state: PlanItemState) -> &'static str {
    match state {
        PlanItemState::Pending => "pending",
        PlanItemState::InProgress => "in-progress",
        PlanItemState::Completed => "completed",
        PlanItemState::Skipped => "skipped",
        PlanItemState::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::{EpicRef, PlanId, PlanItemState};

    fn sample_plan() -> Plan {
        Plan::new(
            PlanId::new("plan-1"),
            "Parser refactor",
            vec!["42".into(), "43".into(), "44".into()],
            1_700_000_000_000,
        )
    }

    #[test]
    fn parse_tickets_csv_handles_plain_comma_separated() {
        let v = parse_tickets_csv("42,43,44").unwrap();
        assert_eq!(v, vec!["42", "43", "44"]);
    }

    #[test]
    fn parse_tickets_csv_trims_whitespace_around_entries() {
        let v = parse_tickets_csv("42 , 43,  44 ").unwrap();
        assert_eq!(v, vec!["42", "43", "44"]);
    }

    #[test]
    fn parse_tickets_csv_rejects_empty_entries() {
        // Trailing comma → empty trailing entry; double comma → empty
        // middle entry. Both should error so a typo doesn't silently
        // produce a malformed plan.
        assert!(parse_tickets_csv("42,").is_err());
        assert!(parse_tickets_csv("42,,43").is_err());
        assert!(parse_tickets_csv(",42").is_err());
        assert!(parse_tickets_csv("").is_err());
    }

    #[test]
    fn render_list_with_no_plans_says_so() {
        let out = render_list(Path::new("/r/.fleet/plans"), &[]);
        assert!(out.contains("no plans yet"), "got: {out}");
        assert!(out.contains("/r/.fleet/plans"));
    }

    #[test]
    fn render_list_shows_state_id_progress_and_name() {
        let mut p = sample_plan();
        p.items[0].state = PlanItemState::Completed;
        let out = render_list(Path::new("/r/.fleet/plans"), &[p]);
        assert!(out.contains("active"), "got: {out}");
        assert!(out.contains("plan-1"), "got: {out}");
        assert!(out.contains("1/3"), "got: {out}");
        assert!(out.contains("Parser refactor"), "got: {out}");
    }

    #[test]
    fn render_show_includes_per_item_state_and_injected_flag() {
        let mut p = sample_plan();
        p.items[0].state = PlanItemState::Completed;
        p.items[1].state = PlanItemState::InProgress;
        p.items[2].injected = true;
        let out = render_show(&p);
        assert!(out.contains("Parser refactor"));
        assert!(out.contains("1/3 completed"));
        assert!(out.contains("completed"));
        assert!(out.contains("in-progress"));
        assert!(out.contains("(injected)"));
    }

    #[test]
    fn render_show_displays_epic_ref_when_present() {
        let mut p = sample_plan();
        p.epic_ref = Some(EpicRef {
            tracker: "github".into(),
            id: "200".into(),
        });
        let out = render_show(&p);
        assert!(out.contains("epic_ref: github:200"), "got: {out}");
    }

    #[test]
    fn render_show_omits_epic_ref_when_absent() {
        let p = sample_plan();
        let out = render_show(&p);
        assert!(!out.contains("epic_ref"), "got: {out}");
    }

    #[test]
    fn state_label_round_trip() {
        assert_eq!(state_label(PlanState::Active), "active");
        assert_eq!(state_label(PlanState::Paused), "paused");
        assert_eq!(state_label(PlanState::Completed), "completed");
        assert_eq!(state_label(PlanState::Abandoned), "abandoned");
    }

    #[test]
    fn item_state_label_uses_kebab_case_for_in_progress() {
        // The on-disk YAML uses `in-progress`; the rendered label
        // should match so users grep across both surfaces.
        assert_eq!(item_state_label(PlanItemState::InProgress), "in-progress");
    }

    // ---- parse_task_list_tickets ------------------------------------

    #[test]
    fn parse_task_list_tickets_recognises_open_and_completed_boxes() {
        let body = "- [ ] #42 open one\n- [x] #43 done\n- [X] #44 also done";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["42", "43", "44"]);
    }

    #[test]
    fn parse_task_list_tickets_skips_prose_lines_with_hash_mentions() {
        let body = "We need to fix #42 soon.\n- [ ] #43 actually scheduled";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["43"]);
    }

    #[test]
    fn parse_task_list_tickets_handles_leading_whitespace_for_nested_lists() {
        let body = "  - [ ] #42\n    - [x] #43";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["42", "43"]);
    }

    #[test]
    fn parse_task_list_tickets_dedupes_repeated_ids_after_first_occurrence() {
        let body = "- [ ] #42\n- [x] #42 same ticket twice";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["42"]);
    }

    #[test]
    fn parse_task_list_tickets_accepts_bare_ids_for_non_github_trackers() {
        // git-bug uses opaque hash ids without the `#` prefix.
        let body = "- [ ] abc1234 first\n- [x] def5678 second";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["abc1234", "def5678"]);
    }

    #[test]
    fn parse_task_list_tickets_skips_lines_with_no_id_token() {
        // An empty checkbox row contributes nothing.
        let body = "- [ ] \n- [x] #42";
        let v = parse_task_list_tickets(body);
        assert_eq!(v, vec!["42"]);
    }

    #[test]
    fn parse_task_list_tickets_skips_non_alphanumeric_first_chars() {
        // A free-text line beginning with `- [ ] ...` shouldn't
        // pollute the list. We guard the bare-id branch with an
        // is_alphanumeric() check on the first character.
        let body = "- [ ] ...placeholder";
        let v = parse_task_list_tickets(body);
        assert!(v.is_empty(), "got: {v:?}");
    }

    // ---- sync_plan_against_body -------------------------------------

    #[test]
    fn sync_plan_against_body_is_a_noop_when_epic_matches_plan() {
        let mut plan = sample_plan(); // items: 42, 43, 44
        let body = "- [ ] #42\n- [x] #43\n- [ ] #44";
        let outcome = sync_plan_against_body(&mut plan, body);
        assert!(outcome.added_ticket_ids.is_empty());
        assert_eq!(plan.items.len(), 3);
    }

    #[test]
    fn sync_plan_against_body_appends_new_ticket_ids_in_epic_order() {
        // Epic mentions 42 (already in plan), 99 (new), 100 (new).
        // The order of appended items should match epic order.
        let mut plan = sample_plan();
        let body = "- [ ] #42\n- [ ] #99\n- [ ] #100";
        let outcome = sync_plan_against_body(&mut plan, body);
        assert_eq!(outcome.added_ticket_ids, vec!["99", "100"]);
        // Existing items kept their position; new ones appended.
        let ticket_ids: Vec<&str> = plan.items.iter().map(|i| i.ticket_id.as_str()).collect();
        assert_eq!(ticket_ids, vec!["42", "43", "44", "99", "100"]);
    }

    #[test]
    fn sync_plan_against_body_preserves_existing_item_states() {
        // Even if the epic has the ticket as `- [x] #42`, the in-plan
        // item state must not be touched by sync — item state moves
        // happen through the supervisor and the workflow engine, not
        // through reconcile.
        let mut plan = sample_plan();
        plan.items[0].state = PlanItemState::InProgress;
        let body = "- [x] #42 done in epic\n- [ ] #99 new";
        let _ = sync_plan_against_body(&mut plan, body);
        assert_eq!(plan.items[0].state, PlanItemState::InProgress);
    }

    #[test]
    fn sync_outcome_uses_epic_ref_to_be_distinguishable_from_inject() {
        // EpicRef isn't part of SyncOutcome but tests can rely on
        // the plan field staying populated through a sync.
        let mut plan = sample_plan();
        plan.epic_ref = Some(EpicRef {
            tracker: "github".into(),
            id: "200".into(),
        });
        let _ = sync_plan_against_body(&mut plan, "- [ ] #99");
        assert!(plan.epic_ref.is_some());
    }
}
