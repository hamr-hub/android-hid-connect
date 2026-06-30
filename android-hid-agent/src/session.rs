//! Agent session — placeholder.
//!
//! Phase 6 will turn this into a unified façade that owns one
//! transport (either the daemon wire or scrcpy UHID) plus the typed
//! action dispatcher.

/// Placeholder host-side session. Will gain constructor variants
/// (`Daemon { addr }`, `Scrcpy { addr }`, `Auto`) in Phase 6.
#[derive(Debug, Default, Clone)]
pub struct AgentSession {
    _private: (),
}