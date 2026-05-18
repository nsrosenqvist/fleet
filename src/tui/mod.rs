//! `fleet ui` — ratatui session browser.
//!
//! Per-view DDD-style split: each view (sessions, plans, spawn, doctor)
//! owns its state + behaviour in [`app`] and its renderers in [`ui`].
//! The cross-cutting modules are:
//!
//! - [`theme`]    — colour tokens + inline-span helpers shared across views
//! - [`input`]    — keyboard dispatch seam between event loop and state
//! - [`terminal`] — crossterm lifecycle, event loop, subprocess-suspend
//! - [`refresh`]  — background thread polling tmux capture-pane
//!
//! Tests live under [`tests`] (a sibling test file) — they used to sit
//! inline in this module, which made `mod.rs` a 2.7k-line file that
//! collided with every parallel TUI edit.
//!
//! The only public surface outside the `tui` module is [`run`] (the
//! entry point), [`build_workflow_run_command`] (used by
//! `cli::autonomous` to detach workflow-run subprocesses), and
//! [`format_epoch_ms`] (used by `cli::sessions` for the list view).

mod app;
mod input;
mod refresh;
mod terminal;
mod theme;
mod ui;

pub use app::build_workflow_run_command;
pub use terminal::run;
pub use ui::format_epoch_ms;

#[cfg(test)]
mod tests;
