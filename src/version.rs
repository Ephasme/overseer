//! What the running binary is: its crate version and the commit `build.rs` stamped into it.

/// Shown in the status line, so a stale install is a glance away instead of a guess.
pub(crate) const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"), " ", env!("OVERSEER_GIT"));
