//! `fleet plan …` — CLI surface over [`crate::plans::store::PlanStore`].
//!
//! Nine subcommands: `list`, `show`, `new`, `edit`, `pause`, `resume`,
//! `complete`, `abandon`, `inject`. Render helpers are pure functions
//! over `Plan` so output tests don't need a real filesystem; the
//! `run_*` wrappers do the I/O.
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
}
