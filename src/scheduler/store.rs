#![allow(dead_code)]
//! Persistence for the loop scheduler.
//!
//! `.fleet/scheduler_state.json` holds a single map `workflow_name →
//! last_run_at` (Unix epoch seconds). Reloaded every tick — the engine
//! is stateless across ticks by design, so the on-disk file is the
//! authoritative record. Same atomic tmp+rename writer
//! [`crate::plans::store::PlanStore`] uses, for the same reason: a
//! crash mid-write leaves the previous file intact.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// In-memory snapshot of the scheduler's persistent state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerState {
    /// `workflow_name → last_run_at`. Absent entries mean "never run"
    /// — handled by `workflow_is_due` as immediately-due.
    pub last_run_at: BTreeMap<String, SystemTime>,
}

/// Where `.fleet/scheduler_state.json` lives for a given repo. Public
/// so the CLI's `fleet scheduler status` can name the path without
/// duplicating the path build.
#[must_use]
pub fn scheduler_state_path(fleet_dir: &Path) -> PathBuf {
    fleet_dir.join("scheduler_state.json")
}

/// On-disk repository for [`SchedulerState`]. One JSON file per repo.
pub struct SchedulerStateStore {
    path: PathBuf,
}

impl SchedulerStateStore {
    /// Construct a store against an explicit `.fleet/` directory. The
    /// file doesn't have to exist yet; [`Self::load`] returns
    /// [`SchedulerState::default`] for missing files (fresh repo).
    #[must_use]
    pub fn for_fleet_dir(fleet_dir: &Path) -> Self {
        Self {
            path: scheduler_state_path(fleet_dir),
        }
    }

    /// Read the persisted state. Missing file → empty state (fresh
    /// repo). IO errors other than `NotFound` surface unchanged with
    /// file-path context.
    pub fn load(&self) -> Result<SchedulerState> {
        let body = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(SchedulerState::default()),
            Err(err) => {
                return Err(anyhow::anyhow!(err))
                    .with_context(|| format!("reading scheduler state at {}", self.path.display()));
            }
        };
        let raw: RawState = serde_json::from_str(&body).with_context(|| {
            format!("parsing scheduler state JSON at {}", self.path.display())
        })?;
        Ok(raw.into())
    }

    /// Atomic write: tmp + rename. Materialises the parent directory
    /// on first save. Mirrors `plans::store::PlanStore::save`.
    pub fn save(&self, state: &SchedulerState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating scheduler-state dir at {}", parent.display()))?;
        }
        let raw: RawState = state.into();
        let body = serde_json::to_string_pretty(&raw)
            .context("serialising scheduler state to JSON")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing tmp scheduler state at {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "renaming {} to {}",
                tmp.display(),
                self.path.display(),
            )
        })?;
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RawState {
    /// Stored as Unix-epoch seconds for human-friendliness. JSON
    /// can't represent `SystemTime` natively; serde would need a
    /// custom serializer either way.
    #[serde(default)]
    last_run_at: BTreeMap<String, u64>,
}

impl From<RawState> for SchedulerState {
    fn from(raw: RawState) -> Self {
        let last_run_at = raw
            .last_run_at
            .into_iter()
            .map(|(k, secs)| (k, UNIX_EPOCH + Duration::from_secs(secs)))
            .collect();
        Self { last_run_at }
    }
}

impl From<&SchedulerState> for RawState {
    fn from(state: &SchedulerState) -> Self {
        let last_run_at = state
            .last_run_at
            .iter()
            .map(|(k, t)| {
                let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                (k.clone(), secs)
            })
            .collect();
        Self { last_run_at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixed_ts(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn load_missing_file_returns_default_empty_state() {
        let dir = tempdir().unwrap();
        let store = SchedulerStateStore::for_fleet_dir(dir.path());
        let s = store.load().unwrap();
        assert!(s.last_run_at.is_empty());
    }

    #[test]
    fn save_load_round_trip_preserves_last_run_at_per_workflow() {
        let dir = tempdir().unwrap();
        let store = SchedulerStateStore::for_fleet_dir(dir.path());
        let mut state = SchedulerState::default();
        state.last_run_at.insert("a".into(), fixed_ts(1_700_000_000));
        state.last_run_at.insert("b".into(), fixed_ts(1_700_000_120));
        store.save(&state).unwrap();
        let back = store.load().unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn save_uses_tmp_then_rename_so_no_partial_files_remain() {
        let dir = tempdir().unwrap();
        let store = SchedulerStateStore::for_fleet_dir(dir.path());
        let mut state = SchedulerState::default();
        state.last_run_at.insert("a".into(), fixed_ts(1_700_000_000));
        store.save(&state).unwrap();

        // The final JSON file is present, the tmp scratch file is not.
        let final_path = scheduler_state_path(dir.path());
        let tmp_path = final_path.with_extension("json.tmp");
        assert!(final_path.exists(), "final file should exist");
        assert!(
            !tmp_path.exists(),
            "tmp file should have been renamed away; lingering tmp at {}",
            tmp_path.display()
        );
    }

    #[test]
    fn load_surfaces_parse_error_with_path_context() {
        let dir = tempdir().unwrap();
        let path = scheduler_state_path(dir.path());
        std::fs::write(&path, "not json").unwrap();
        let store = SchedulerStateStore::for_fleet_dir(dir.path());
        let err = store.load().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("scheduler_state.json"), "got: {msg}");
    }

    #[test]
    fn save_creates_parent_directory_when_missing() {
        let outer = tempdir().unwrap();
        let nested = outer.path().join("subdir").join(".fleet");
        // Parent dir doesn't exist yet.
        assert!(!nested.exists());
        let store = SchedulerStateStore::for_fleet_dir(&nested);
        store.save(&SchedulerState::default()).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn raw_state_serdes_as_unix_epoch_seconds() {
        // Lock the wire format so future readers / external tooling
        // can rely on the seconds-since-epoch encoding.
        let mut state = SchedulerState::default();
        state.last_run_at.insert("hourly".into(), fixed_ts(1_700_000_000));
        let raw = RawState::from(&state);
        let json = serde_json::to_string(&raw).unwrap();
        assert!(
            json.contains("\"hourly\":1700000000"),
            "expected epoch-seconds encoding; got: {json}"
        );
    }
}
