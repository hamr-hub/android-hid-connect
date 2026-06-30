//! `android-hid-daemon` — on-device daemon library.
//!
//! Runs inside an Android process (`app_process` or a small native binary),
//! speaks the `android-hid-protocol` wire format on one TCP socket, and
//! dispatches every verb to the corresponding subsystem (a11y,
//! screenshot, clipboard, pm/am/wm, files, providers, etc.).
//!
//! ## Layering
//!
//! ```text
//! android-hid-protocol   (verb / error / frame types — no I/O)
//!        ▲
//!        │
//! android-hid-daemon     (this crate — runs on-device)
//! ```
//!
//! The daemon is intentionally a **library** so it can be linked into
//! either a static `hsd` binary, a test harness, or an
//! instrumentation-runner. The companion `hsd` binary is in `src/main.rs`.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod a11y;
pub mod am;
pub mod binder;
pub mod clipboard;
pub mod dumpsys;
pub mod files;
pub mod handlers;
pub mod input;
pub mod installer;
pub mod lifecycle;
pub mod location;
pub mod logcat;
pub mod manifest;
pub mod notifications;
pub mod pm;
pub mod props;
pub mod providers;
pub mod screencap;
pub mod server;
pub mod settings;
pub mod shell;
pub mod state;
pub mod stream;
pub mod uiautomation;
pub mod waits;
pub mod wm;

// Public re-exports for the on-device daemon's stable surface.
//
// The `hsd` binary, integration tests, and (eventually) the
// `app_process`-hosted Android entry point all reach for the dispatcher
// through these names rather than the module path so the internal
// layout can churn without breaking external consumers.
pub use handlers::{getprop, info, ping, quit, wm_info};
pub use server::{BindError, HandlerFn, Response, Server, ServerConfig, HANDSHAKE};

/// Wire-protocol version this daemon build speaks.
///
/// Mirrors `android_hid_protocol::PROTOCOL_VERSION` for convenience — the
/// daemon's `ServerConfig::version` defaults to this value.
pub const VERSION: u32 = android_hid_protocol::PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    use crate::server::{Server, ServerConfig};

    #[test]
    fn server_default_binds_and_exposes_local_addr() {
        let server = Server::bind(ServerConfig::default()).expect("bind");
        let local = server.local_addr().expect("local_addr");
        assert_ne!(local.port(), 0);
    }

    #[test]
    fn handshake_literal_is_what_we_promise() {
        assert_eq!(crate::server::HANDSHAKE, b"PROTO/1\n");
    }
}