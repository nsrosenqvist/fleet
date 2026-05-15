//! Agent abstractions.
//!
//! Currently scoped to the [`registry`] of named agent specs that workflow
//! Agent nodes resolve against. As the workflow executor lands, this module
//! will also own:
//! - prompt-prefix construction (workflow name / node id / persona +
//!   prior artifacts + AGENTS.md);
//! - PTY attach helpers (one wrapper around each adapter's `attach_pty`);
//! - exit-status interpretation (mapping agent exits to workflow decisions).
//!
//! The split is intentional: spec parsing has no I/O, so it lives in its own
//! file under `#[cfg(test)]`-light tests; the run-time helpers will be a
//! sibling module so spec evolution doesn't drag the spawn path along.

pub mod cost;
pub mod registry;

// `AgentSpec` is re-exported when the workflow executor (sibling module)
// starts consuming it. Until then, callers reach for `registry::AgentSpec`
// directly — re-exporting now would trip `unused_imports` under our pedantic
// clippy config.
pub use registry::AgentRegistry;
