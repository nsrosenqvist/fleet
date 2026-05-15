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

pub mod executor;
pub mod expr;
pub mod spec;
pub mod validate;
