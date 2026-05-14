//! ratatui dashboard.
//!
//! Module layout (mirrors keel's TUI structure, adapted to fleet's scale):
//!
//! - [`app`]        — state only; no rendering, no I/O, no crossterm
//! - [`ui`]         — pure render `(&App, &mut Frame) -> ()`
//! - [`theme`]      — colour tokens + small inline-span helpers
//! - [`input`]      — per-mode keyboard handlers
//! - [`terminal`]   — crossterm lifecycle, event loop, command executor
//! - [`refresh`]    — background refresh thread + channel types
//! - [`subprocess`] — alt-screen suspend/resume around foreground children
//!
//! Aesthetic inspiration: keel — dark background, rounded borders, section
//! titles inline on the border, dot-separated status bar at the bottom.

mod ao_task;
mod app;
mod bringup;
mod cleanup;
mod input;
mod preflight;
mod refresh;
mod register;
pub mod subprocess;
mod terminal;
mod theme;
mod tracker_install;
mod ui;
mod vm_migrate;

use anyhow::Result;
use std::path::Path;

pub fn run(repo_root: &Path) -> Result<i32> {
    // One-time migration for existing fleet-vm instances that predate
    // the idempotent-provisioning fix in templates/fleet-vm.yaml. Safe
    // to call every launch — bails immediately when there's nothing to
    // do. Runs before the alt-screen enters so any tracing output
    // doesn't get swallowed by the TUI repaint.
    vm_migrate::run();
    let mut app = app::App::new(repo_root)?;
    terminal::run(&mut app)
}
