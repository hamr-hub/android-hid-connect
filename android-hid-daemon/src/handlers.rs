//! Default verb handlers registered by [`Server::bind`].
//!
//! Each verb in the wire vocabulary has a corresponding `pub fn` here.
//! They are intentionally tiny: real UiAutomation / binder / screencap
//! plumbing lands in later phases. For Phase 2B the goal is purely to
//! wire the dispatcher end-to-end with deterministic, hard-coded replies
//! so the host agent and integration tests have something real to talk
//! to.
//!
//! All handlers share the same signature so they slot into the
//! [`Server`] dispatch table unmodified:
//!
//! ```text
//! fn(&[u8]) -> Result<Response, Box<dyn Error + Send + Sync>>
//! ```
//!
//! The argument slice is the **entire** request payload, including the
//! verb head — handlers that need the argument tail re-split it with
//! [`split_head`].
//!
//! [`Server`]: crate::server::Server
//! [`Response`]: crate::server::Response

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;

use android_hid_protocol::Frame;

use crate::server::Response;

// ---------------------------------------------------------------------------
// Handler entrypoints (registered by `Server::bind`)
// ---------------------------------------------------------------------------

/// `ping` → single-frame `pong`.
pub fn ping(_args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    Ok(Response::One(Frame::from_static(b"pong")))
}

/// `info` → `"<screen_width> <screen_height>\n"`.
///
/// Handsets' Java reference returns the Screenshot mirror's source size
/// (e.g. `1080 2400`). Phase 2B returns a hard-coded `1080 1920\n` —
/// real display queries need UiAutomation which lands in Phase 4.
pub fn info(_args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    Ok(Response::One(Frame::new(b"1080 1920\n".to_vec())))
}

/// `wm_info` → JSON dump of the current display configuration.
///
/// Handsets returns a real JSON built from `WindowManager.getDefaultDisplay`
/// plus rotation/orientation listeners. Phase 2B hard-codes the same
/// shape so downstream consumers (CLI, agent) can parse it.
pub fn wm_info(_args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    Ok(Response::One(Frame::new(
        br#"{"width":1080,"height":1920,"density":2.0,"xdpi":2.0,"ydpi":2.0,"rotation":0}"#
            .to_vec(),
    )))
}

/// `getprop <KEY>` → the value of the named Android system property.
///
/// In Phase 2B this consults a tiny in-memory table that matches the
/// values `hsd` would otherwise fetch over `libc::SystemProperties_get`
/// (added in Phase 3 once JNI is wired). Unknown keys return
/// `ERR:NOT_FOUND:no-such-prop`.
pub fn getprop(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    let text = std::str::from_utf8(args).map_err(|e| -> Box<dyn StdError + Send + Sync> {
        Box::new(GetpropError::BadUtf8(e))
    })?;
    let key = split_head(text).1.trim();
    if key.is_empty() {
        return Ok(Response::One(Frame::from_static(b"ERR:BAD_ARG:getprop-needs-key")));
    }

    match known_props().get(key) {
        Some(value) => Ok(Response::One(Frame::new(value.as_bytes().to_vec()))),
        None => Ok(Response::One(Frame::from_static(b"ERR:NOT_FOUND:no-such-prop"))),
    }
}

/// `quit` → single-frame `bye` and signals the accept loop to stop.
///
/// The `Server::serve` loop watches an `AtomicBool` set by the
/// dispatcher when the verb resolves to `Quit`. After the bye frame
/// is written the connection is dropped and the listener closes.
pub fn quit(_args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    Ok(Response::One(Frame::from_static(b"bye")))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Hard-coded property table. Mirrors the values that
/// `props.doGet` returns against a SM-G9910 / Android 11 device, so the
/// agent's device-fingerprint tests can rely on stable bytes.
///
/// Phase 3 will swap this for `libc::SystemProperties_get` via JNI.
fn known_props() -> &'static HashMap<&'static str, &'static str> {
    use std::sync::OnceLock;
    static PROPS: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    PROPS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("ro.build.version.sdk", "30");
        m.insert("ro.build.version.release", "11");
        m.insert("ro.product.model", "SM-G9910");
        m.insert("ro.product.manufacturer", "samsung");
        // A handful of extras so test cases that want a "valid unknown"
        // feel can pick something predictable.
        m.insert("ro.product.brand", "samsung");
        m.insert("ro.product.device", "o1s");
        m
    })
}

/// Split a request payload into `(verb_head, argument_tail)`.
///
/// Splits on the first ASCII whitespace byte. The verb head is **not**
/// trimmed — callers that pre-trimmed the request (e.g. handsets'
/// `cmd.trim()`) pass through unchanged. The argument tail has both
/// the leading separator and any trailing whitespace removed so the
/// next layer doesn't have to re-trim.
pub fn split_head(input: &str) -> (&str, &str) {
    let trimmed_end = input
        .find(|c: char| c.is_whitespace())
        .unwrap_or(input.len());
    let head = &input[..trimmed_end];
    let tail_start = input[trimmed_end..]
        .find(|c: char| !c.is_whitespace())
        .map(|rel| trimmed_end + rel)
        .unwrap_or(input.len());
    let tail_end = input
        .trim_end()
        .len();
    (head, &input[tail_start..tail_end])
}

/// Failure modes for [`getprop`].
#[derive(Debug)]
enum GetpropError {
    /// Request payload was not valid UTF-8.
    BadUtf8(std::str::Utf8Error),
}

impl fmt::Display for GetpropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadUtf8(e) => write!(f, "getprop-payload-not-utf8: {e}"),
        }
    }
}

impl StdError for GetpropError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::BadUtf8(e) => Some(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_returns_pong() {
        let r = ping(b"ping").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"pong"),
            _ => panic!("expected One(pong), got {r:?}"),
        }
    }

    #[test]
    fn info_returns_screen_size() {
        let r = info(b"info").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"1080 1920\n"),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn wm_info_returns_json() {
        let r = wm_info(b"wm_info").unwrap();
        match r {
            Response::One(f) => {
                assert_eq!(
                    f.payload(),
                    br#"{"width":1080,"height":1920,"density":2.0,"xdpi":2.0,"ydpi":2.0,"rotation":0}"#
                );
            }
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn getprop_returns_known_value() {
        let r = getprop(b"getprop ro.build.version.sdk").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"30"),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn getprop_unknown_returns_not_found_error_frame() {
        let r = getprop(b"getprop ro.foo.bar").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"ERR:NOT_FOUND:no-such-prop"),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn getprop_missing_key_returns_bad_arg() {
        let r = getprop(b"getprop").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"ERR:BAD_ARG:getprop-needs-key"),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn quit_returns_bye() {
        let r = quit(b"quit").unwrap();
        match r {
            Response::One(f) => assert_eq!(f.payload(), b"bye"),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    #[test]
    fn split_head_basic() {
        assert_eq!(split_head("ping"), ("ping", ""));
        assert_eq!(
            split_head("getprop ro.build.version.sdk"),
            ("getprop", "ro.build.version.sdk")
        );
        // No leading trim — the dispatcher already trims before calling.
        assert_eq!(split_head("  tap  x=540  "), ("", "tap  x=540"));
    }

    #[test]
    fn split_head_quoted_args_kept_intact() {
        let (h, t) = split_head(r#"text "hello world""#);
        assert_eq!(h, "text");
        assert_eq!(t, r#""hello world""#);
    }
}