//! Workflow DSL: YAML parsing, DAG validation, and (next chunk) execution.
//!
//! Module split:
//! - [`spec`] — value-object types (`Workflow`, `Node`, `NodeKind`, …) +
//!   YAML deserialisation.
//! - [`validate`] — static DAG checks (unique ids, dangling refs, cycles).
//!   Pure; takes a parsed [`spec::Workflow`] and returns `Result<()>`.
//!
//! The executor sits alongside these two in a sibling module that lands in
//! the next chunk. Keeping spec/validate separate from execution lets the
//! TUI's "validate this workflow file" path use the same code without
//! pulling the adapter/session machinery in.

pub mod spec;
pub mod validate;

// Re-exports land when the executor (next chunk) consumes these types.
// Without a consumer they trip `unused_imports` under the pedantic config —
// callers reach for `workflow::spec::Workflow` etc. directly for now.
