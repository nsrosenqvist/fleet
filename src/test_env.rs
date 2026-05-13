//! Shared serialisation lock for tests that mutate `XDG_CONFIG_HOME`
//! (or any other process-global env var). `cargo test` runs unit
//! tests in parallel by default, so two tests calling
//! `std::env::set_var("XDG_CONFIG_HOME", …)` can clobber each other's
//! state. Any test that touches the env should hold this lock for
//! the duration of the set / read / restore sequence.

use std::sync::{Mutex, MutexGuard};

/// Global mutex protecting `XDG_CONFIG_HOME` mutations across the
/// entire test binary. Acquire with [`xdg_lock`] before any
/// `set_var` / `remove_var`.
static XDG_MUTEX: Mutex<()> = Mutex::new(());

/// Hold-this-guard pattern — drop releases the lock, which restores
/// concurrency for the next waiting test. We use `unwrap_or_else`
/// against poisoning so a panicking test under the lock doesn't
/// strand the whole suite: env mutation is recoverable since the
/// caller resets it on every entry.
pub fn xdg_lock() -> MutexGuard<'static, ()> {
    XDG_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
