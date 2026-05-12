//! ratatui dashboard. Entry point + module wiring; the app state machine
//! and panel rendering live in submodules.
//!
//! Aesthetic inspiration: `example.png` (a similar tool, `keel`) — dark
//! background, rounded borders, section titles inline on the border, single
//! dot-separated status bar at the bottom. Stage 3 task #23 implements the
//! panels in that style.

pub mod app;
pub mod subprocess;

use anyhow::Result;
use std::path::Path;

pub fn run(repo_root: &Path) -> Result<i32> {
    app::App::new(repo_root)?.run()
}
