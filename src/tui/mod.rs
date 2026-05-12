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

mod app;
mod input;
mod preflight;
mod refresh;
pub mod subprocess;
mod terminal;
mod theme;
mod ui;

use anyhow::Result;
use std::path::Path;

pub fn run(repo_root: &Path) -> Result<i32> {
    let mut app = app::App::new(repo_root)?;
    terminal::run(&mut app)
}
