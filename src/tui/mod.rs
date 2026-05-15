//! Placeholder for the v2 ratatui session browser.
//!
//! Real implementation lands in the next commit. This module exists so
//! the dispatcher can wire `fleet ui` even before the TUI rebuild.

use anyhow::Result;

/// Entry point for `fleet ui`. Placeholder until the v2 ratatui browser
/// lands in the next commit — the real impl needs `Result` for terminal
/// setup, so the wrapper stays despite the unnecessary-wraps lint.
#[allow(clippy::unnecessary_wraps)]
pub fn run() -> Result<i32> {
    eprintln!(
        "fleet ui: v2 TUI not yet wired — use `fleet workflow run`, `fleet sessions list`, and `fleet runtime attach <id>` for the moment."
    );
    Ok(0)
}

