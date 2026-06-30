//! `android-hid-protocol` — wire-level protocol types shared by all sibling crates.
//!
//! This crate is the **lowest layer** in the `android-hid-connect` workspace:
//! it contains no I/O, no FFI, no async runtime, and no platform code.
//! Every other sibling crate (`-daemon`, `-agent`, `-cli`, `-py`) depends on
//! the types defined here so that the on-device daemon, host-side agent,
//! CLI and Python SDK all speak the same wire vocabulary.
//!
//! ## Modules
//!
//! - [`frame`]    — length-prefixed binary frame with a 16 MiB hard cap.
//! - [`error`]    — numeric error code enum (`Ok`, `UnknownCmd`, `BadArg`, …).
//! - [`verb`]     — every wire verb dispatched by the daemon (60+ names).
//! - [`kvs`]      — `k=v` token parser + writer + head-splitter.
//! - [`version`]  — `PROTOCOL_VERSION` + `USER_AGENT` constants.
//!
//! The byte layout mirrors `handsets/docs/wire.md` so that an `android-hid-*`
//! client can talk to either a handsets daemon or the future
//! `android-hid-daemon` running on-device.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod frame;
pub mod kvs;
pub mod verb;
pub mod version;

// Flat re-exports for `use android_hid_protocol::Frame;`-style imports.
pub use error::{ErrorCode, ErrorCodeByteError, ErrorFrame, ProtocolError};
pub use frame::{Frame, FrameError, HEADER_LEN, MAX_FRAME_LEN};
pub use kvs::{parse_kv, quote_value, split_head, KvPair, KvWriter};
pub use verb::{Verb, VerbParseError};
pub use version::{PROTOCOL_VERSION, USER_AGENT};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_round_trip() {
        let frame = Frame::new(b"hello".to_vec());
        assert_eq!(frame.payload(), b"hello");
        assert_eq!(PROTOCOL_VERSION, 1);
        assert!(!USER_AGENT.is_empty());
        assert_eq!(ErrorCode::Ok.as_byte(), 0);
        assert_eq!(ErrorCode::from_byte(0), Ok(ErrorCode::Ok));
        assert_eq!(ErrorCode::from_tag(b"NOT_FOUND"), Some(ErrorCode::NotFound));
        assert_eq!(Verb::Ping.as_str(), "ping");
        assert_eq!(Verb::parse("ping"), Ok(Verb::Ping));
    }

    #[test]
    fn frame_cap_is_16mib() {
        assert_eq!(MAX_FRAME_LEN, 16 * 1024 * 1024);
        assert_eq!(HEADER_LEN, 4);
    }

    #[test]
    fn user_agent_matches_format() {
        assert_eq!(USER_AGENT, "android-hid-agent/0.1");
    }
}