//! `fleet ui` — launches the ratatui dashboard. Thin shim around `tui::run`.

use anyhow::Result;
use std::path::Path;

use crate::tui;

pub fn run(repo_root: &Path) -> Result<i32> {
    tui::run(repo_root)
}
