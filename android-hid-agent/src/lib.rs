//! `android-hid-agent` — host-side typed Rust facade.
//!
//! Phase 2A ships the real backend layer:
//!
//! - `android-hid-daemon` running on-device (preferred, full verb
//!   surface: a11y, screenshot, clipboard, pm/am/wm, files, providers,
//!   waits, …), **or**
//! - the byte-exact scrcpy UHID control surface via the existing
//!   `android-hid-connect` root crate (fallback, UHID-only).
//!
//! ## Layering
//!
//! ```text
//! android-hid-protocol   (verb / error / frame types)
//! android-hid-connect    (byte-exact scrcpy UHID core)
//!        ▲          ▲
//!        │          │
//!        └──────────┴───── android-hid-agent   (this crate)
//! ```
//!
//! The `backend/` module owns the per-backend implementation;
//! `verbs/` (Phase 6) will translate typed requests into either
//! wire frames (daemon) or `ControlMessage`s (scrcpy).

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod backend;
pub mod errors;
pub mod geometry;
pub mod plan;
pub mod selectors;
pub mod session;
pub mod verbs;

pub use backend::daemon::{DaemonBackend, DaemonError, DaemonStream};
pub use backend::scrcpy::ScrcpyBackend;
pub use backend::unified::{BackendChoice, UnifiedBackend};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_session_default() {
        let s = session::AgentSession::default();
        // Placeholder field check — the real session shape lands in Phase 6.
        let _ = s;
    }

    #[test]
    fn smoke_unified_default() {
        let u = UnifiedBackend::default();
        assert!(!u.is_connected());
    }
}