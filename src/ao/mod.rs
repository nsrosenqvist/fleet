//! `ao` CLI wrappers. Both invocation (`Ao::status`, etc.) and the response
//! type model. The state model is permissive: every field is `#[serde(default)]`
//! so AO version drift doesn't crash us — fields we don't yet model just
//! land in `None`/empty.

pub mod config;
pub mod invoke;
pub mod state;

// Re-exported for the CLI dispatch + TUI to consume in subsequent tasks. The
// `#[allow]` goes away when those modules use these names.
#[allow(unused_imports)]
pub use invoke::Ao;
#[allow(unused_imports)]
pub use state::{AoMeta, AoResponse, SessionInfo};
