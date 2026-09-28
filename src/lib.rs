//! Wake: Wake-on-LAN plus a one-shot boot selection that GRUB reads over HTTP.
//!
//! The binary in `main.rs` wires these modules together; they live in a library
//! so the integration tests in `tests/` can drive them directly.

pub mod api;
pub mod config;
pub mod error;
pub mod grub;
pub mod probe;
pub mod state;
pub mod store;
pub mod web;
pub mod wol;

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. All timestamps in Wake use this unit so
/// they serialise as plain numbers and the browser formats them in local time.
pub type Millis = u64;

pub fn now_ms() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// Bumped whenever anything the UI shows changes; SSE clients wait on it.
pub type Revision = std::sync::Arc<tokio::sync::watch::Sender<u64>>;

pub fn new_revision() -> Revision {
    std::sync::Arc::new(tokio::sync::watch::channel(0).0)
}

pub fn bump(revision: &Revision) {
    revision.send_modify(|v| *v = v.wrapping_add(1));
}
