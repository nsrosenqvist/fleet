//! `fleet scheduler …` — CLI driver for the loop-based workflow
//! scheduler.
//!
//! Four subcommands:
//! - `enable` / `disable`: toggle `.fleet/scheduler.enabled` (a flag
//!   file the engine consults each tick).
//! - `status`: human-readable summary of every workflow with a
//!   `loop:` declaration plus its last-run / next-due times.
//! - `tick --once`: single-shot tick suitable for cron. Loads the
//!   workflows + sessions, instantiates a fresh `SchedulerEngine`,
//!   dispatches the produced spawns, and persists the updated
//!   state.
//!
//! Mirrors `cli::autonomous` for shape but the underlying engines
//! are independent: a repo can opt into one, both, or neither.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::scheduler::{
    SchedulerEngine, SchedulerOutcome, SchedulerState, dispatcher,
    store::SchedulerStateStore,
};
use crate::session::store::SessionStore;
use crate::workflow::spec::Workflow;

/// `.fleet/scheduler.enabled` — when present, scheduler ticks fire.
/// Absent = disabled. Same minimal flag-file pattern the autonomous
/// engine uses for its own opt-in.
fn enabled_flag_path(fleet_dir: &Path) -> PathBuf {
    fleet_dir.join("scheduler.enabled")
}

fn scheduler_is_enabled_on_disk(fleet_dir: &Path) -> bool {
    enabled_flag_path(fleet_dir).is_file()
}

pub fn run_enable() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let fleet_dir = root.join(".fleet");
    fs::create_dir_all(&fleet_dir)
        .with_context(|| format!("creating {}", fleet_dir.display()))?;
    let flag = enabled_flag_path(&fleet_dir);
    fs::write(&flag, b"")
        .with_context(|| format!("writing flag file at {}", flag.display()))?;
    println!("fleet scheduler: enabled");
    Ok(0)
}

pub fn run_disable() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let fleet_dir = root.join(".fleet");
    let flag = enabled_flag_path(&fleet_dir);
    match fs::remove_file(&flag) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Already disabled — idempotent.
        }
        Err(err) => {
            return Err(anyhow!(err))
                .with_context(|| format!("removing flag file at {}", flag.display()));
        }
    }
    println!("fleet scheduler: disabled");
    Ok(0)
}

pub fn run_status() -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let fleet_dir = root.join(".fleet");
    let store = SchedulerStateStore::for_fleet_dir(&fleet_dir);
    let state = store.load().context("loading scheduler state")?;
    let workflows = load_loop_workflows(&root)?;
    print!("{}", render_status(scheduler_is_enabled_on_disk(&fleet_dir), &workflows, &state, SystemTime::now()));
    Ok(0)
}

pub fn run_tick(once: bool) -> Result<i32> {
    if !once {
        return Err(anyhow!(
            "fleet scheduler tick currently requires `--once` — a watch loop \
             will land alongside the TUI integration",
        ));
    }
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = repo::fleet_root(&cwd);
    let fleet_dir = root.join(".fleet");
    if !scheduler_is_enabled_on_disk(&fleet_dir) {
        println!("fleet scheduler: disabled (run `fleet scheduler enable` first)");
        return Ok(0);
    }
    let config = RepoConfig::load(root.join(".fleet/config.yaml"))
        .context("loading .fleet/config.yaml")?;
    let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
    let store = SchedulerStateStore::for_fleet_dir(&fleet_dir);
    let mut state = store.load().context("loading scheduler state")?;
    let session_store = SessionStore::for_repo(&root);
    let session_ids = session_store
        .list()
        .context("listing sessions for the scheduler overlap check")?;
    let sessions: Vec<_> = session_ids
        .iter()
        .filter_map(|id| session_store.load(id).ok())
        .collect();
    let workflows = load_loop_workflows(&root)?;
    let code_host = crate::code_host::build(config.code_host, Arc::clone(&invoker), &root);

    let list_prs = |_workflow: &str, filter: &crate::code_host::PrFilter| {
        let host = code_host
            .as_ref()
            .ok_or_else(|| "no code host configured for pr-list".to_string())?;
        host.list_prs(&root, filter).map_err(|e| format!("{e:#}"))
    };

    let mut engine = SchedulerEngine::new();
    engine.toggle(); // We checked the on-disk flag above; flip in-memory.
    let outcome = engine.step(
        SystemTime::now(),
        Instant::now(),
        &workflows,
        &sessions,
        &state,
        list_prs,
    );

    let (spawns, marks) = match outcome {
        SchedulerOutcome::Idle => {
            println!("fleet scheduler: idle");
            return Ok(0);
        }
        SchedulerOutcome::WaitedFor(reason) => match reason {
            crate::scheduler::SkipReason::NoWorkflowDue => {
                println!("fleet scheduler: {}", engine.status());
                return Ok(0);
            }
            crate::scheduler::SkipReason::AllSuppressedOrEmpty { marks } => {
                (Vec::new(), marks)
            }
        },
        SchedulerOutcome::Spawn { spawns, marks } => (spawns, marks),
    };

    // Persist the marks first so an interrupted dispatch (which fires
    // a child off but loses our handle) still advances the clock —
    // otherwise the next tick would re-spawn duplicates.
    for (name, ts) in &marks {
        state.last_run_at.insert(name.clone(), *ts);
    }
    store.save(&state).context("saving scheduler state")?;

    if spawns.is_empty() {
        println!("fleet scheduler: {} (no spawns)", engine.status());
        return Ok(0);
    }
    let fleet_binary = std::env::current_exe().context("locating current fleet binary")?;
    let results = dispatcher::dispatch_spawns(&spawns, &fleet_binary);
    let mut ok = 0_usize;
    let mut failed = 0_usize;
    for (spawn, res) in spawns.iter().zip(results) {
        match res {
            Ok(()) => ok += 1,
            Err(err) => {
                failed += 1;
                eprintln!(
                    "fleet scheduler: spawn for `{}` failed: {err:#}",
                    spawn.workflow,
                );
            }
        }
    }
    println!(
        "fleet scheduler: spawned {ok} session(s); {failed} spawn failure(s)",
    );
    // Non-zero exit when any spawn failed, so cron can alert.
    Ok(i32::from(failed > 0))
}

/// Load every `.fleet/workflows/*.yaml` that has a `loop:` field set.
/// Workflows that fail to parse are skipped with a warning — better
/// than aborting the whole tick on one bad file.
fn load_loop_workflows(root: &Path) -> Result<Vec<Workflow>> {
    let wf_dir = root.join(".fleet/workflows");
    let entries = match fs::read_dir(&wf_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(anyhow!(err))
                .with_context(|| format!("reading {}", wf_dir.display()));
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        match Workflow::from_path(&path) {
            Ok(wf) if wf.loop_interval.is_some() => out.push(wf),
            Ok(_) => {}
            Err(err) => {
                eprintln!("fleet scheduler: skipping unparseable {}: {err:#}", path.display());
            }
        }
    }
    Ok(out)
}

/// Render `fleet scheduler status` output. Pure so the format is
/// testable without filesystem fixtures.
#[must_use]
fn render_status(
    enabled: bool,
    workflows: &[Workflow],
    state: &SchedulerState,
    now: SystemTime,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let header = if enabled { "enabled" } else { "disabled" };
    let _ = writeln!(out, "fleet scheduler: {header}");
    if workflows.is_empty() {
        let _ = writeln!(out, "  no workflows with `loop:` configured");
        return out;
    }
    for wf in workflows {
        let interval = wf.loop_interval.unwrap_or(Duration::ZERO);
        let last_run = state.last_run_at.get(&wf.name);
        let last_display = last_run.map_or_else(
            || "never".to_string(),
            |t| {
                let secs = saturating_secs(now, *t);
                if secs == 0 {
                    "now".to_string()
                } else {
                    format!("{secs}s ago")
                }
            },
        );
        let due_display = last_run.map_or_else(
            || "due now".to_string(),
            |last| {
                let elapsed = now.duration_since(*last).unwrap_or(Duration::ZERO);
                interval.checked_sub(elapsed).map_or_else(
                    || "due now".to_string(),
                    |remaining| {
                        if remaining.is_zero() {
                            "due now".to_string()
                        } else {
                            format!("in {}s", remaining.as_secs())
                        }
                    },
                )
            },
        );
        let _ = writeln!(
            out,
            "  {} · loop={}s · on_overlap={:?} · last_run={} · next={}",
            wf.name,
            interval.as_secs(),
            wf.on_overlap,
            last_display,
            due_display,
        );
    }
    out
}

fn saturating_secs(now: SystemTime, then: SystemTime) -> u64 {
    now.duration_since(then).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn enable_disable_round_trip_via_flag_file() {
        let outer = tempdir().unwrap();
        let fleet_dir = outer.path().join(".fleet");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        assert!(!scheduler_is_enabled_on_disk(&fleet_dir));
        // Simulate `run_enable` against the explicit dir path:
        let flag = enabled_flag_path(&fleet_dir);
        std::fs::write(&flag, b"").unwrap();
        assert!(scheduler_is_enabled_on_disk(&fleet_dir));
        std::fs::remove_file(&flag).unwrap();
        assert!(!scheduler_is_enabled_on_disk(&fleet_dir));
    }

    fn parse_loop_workflow(name: &str, secs: u64) -> Workflow {
        let yaml = format!(
            "name: {name}\nloop: {secs}s\ntrigger: {{ issueless: true }}\nnodes:\n  - id: n\n    agent: c\n",
        );
        Workflow::from_str_at(&yaml, "/x").unwrap()
    }

    #[test]
    fn render_status_lists_each_loop_workflow_with_next_due_seconds() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let mut state = SchedulerState::default();
        state.last_run_at.insert("hourly".into(), now - Duration::from_secs(300));
        let workflows = vec![
            parse_loop_workflow("hourly", 3600),
            parse_loop_workflow("daily", 86_400),
        ];
        let out = render_status(true, &workflows, &state, now);
        assert!(out.contains("fleet scheduler: enabled"), "got: {out}");
        assert!(out.contains("hourly"), "got: {out}");
        assert!(out.contains("daily"), "got: {out}");
        // hourly: last 300s ago, interval 3600 → next in 3300s.
        assert!(out.contains("in 3300s"), "got: {out}");
        // daily never ran → due now.
        assert!(out.contains("due now"), "got: {out}");
    }

    #[test]
    fn render_status_with_no_loop_workflows_is_explicit() {
        let out = render_status(true, &[], &SchedulerState::default(), SystemTime::now());
        assert!(out.contains("no workflows with `loop:` configured"));
    }

    #[test]
    fn render_status_disabled_header_when_flag_absent() {
        let out = render_status(false, &[], &SchedulerState::default(), SystemTime::now());
        assert!(out.contains("fleet scheduler: disabled"));
    }
}
