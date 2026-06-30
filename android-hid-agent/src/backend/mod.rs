//! Backend abstraction — one trait, two implementations.
//!
//! `Daemon` talks `android-hid-protocol` to the on-device daemon (full
//! verb surface). `Scrcpy` falls back to the byte-exact UHID control
//! surface in the root `android-hid-connect` crate. `Unified` picks
//! the best backend at runtime based on which sockets are reachable.
//!
//! Phase 6 lands the real trait.

pub mod daemon;
pub mod scrcpy;
pub mod unified;