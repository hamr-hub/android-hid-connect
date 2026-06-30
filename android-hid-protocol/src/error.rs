//! Numeric error codes + `ERR:<TAG>[:<detail>]` framing shared between
//! the daemon and its clients.
//!
//! On the wire every failure path returns a single frame whose payload
//! starts with the ASCII bytes `ERR:` followed by an
//! [`ErrorCode`] tag word and an optional human-readable detail
//! (e.g. `ERR:NOT_FOUND:no-such-app`). Streamed responses send the
//! `ERR:` frame then the `len=0` terminator.

use thiserror::Error;

/// Wire prefix for every error frame payload.
///
/// Lives here as a `const` so the daemon and the agent never
/// drift on the literal bytes. The tag after this prefix is
/// uppercase ASCII (e.g. `NOT_FOUND`, `TIMEOUT`, `BAD_ARG`).
pub const ERR_PREFIX: &[u8] = b"ERR:";

/// Outcome codes returned by the daemon for every request.
///
/// The 9 variants cover every error documented in
/// `handsets/docs/wire.md` §"Errors". `Ok` doubles as a success
/// tag when serialised (`ERR:OK` is technically legal but not
/// used — successful responses just don't carry the prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ErrorCode {
    /// Request completed successfully.
    Ok = 0,
    /// Server did not recognise the verb.
    UnknownCmd = 1,
    /// A required argument was missing or malformed.
    BadArg = 2,
    /// Operation exceeded its time budget.
    Timeout = 3,
    /// Target resource did not exist.
    NotFound = 4,
    /// Selector matched multiple nodes and the verb required exactly one.
    Ambiguous = 5,
    /// Server-side invariant was violated; safe to retry.
    Internal = 6,
    /// Target UI element is offscreen.
    OffScreen = 7,
    /// Operation blocked by a secure (FLAG_SECURE) window.
    SecureWindow = 8,
}

/// Failure mode when decoding an [`ErrorCode`] byte that does not map to
/// any known variant.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown error code byte: {0}")]
pub struct ErrorCodeByteError(pub u8);

impl ErrorCode {
    /// Encode this code as a single byte for the wire.
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte into an [`ErrorCode`].
    pub const fn from_byte(b: u8) -> Result<Self, ErrorCodeByteError> {
        match b {
            0 => Ok(Self::Ok),
            1 => Ok(Self::UnknownCmd),
            2 => Ok(Self::BadArg),
            3 => Ok(Self::Timeout),
            4 => Ok(Self::NotFound),
            5 => Ok(Self::Ambiguous),
            6 => Ok(Self::Internal),
            7 => Ok(Self::OffScreen),
            8 => Ok(Self::SecureWindow),
            other => Err(ErrorCodeByteError(other)),
        }
    }

    /// Uppercase ASCII tag used in the wire `ERR:<TAG>` form.
    ///
    /// Mirrors the handsets spelling exactly (e.g. `NOT_FOUND`,
    /// `SECURE_WINDOW`, `UNKNOWN_CMD`).
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::UnknownCmd => "UNKNOWN_CMD",
            Self::BadArg => "BAD_ARG",
            Self::Timeout => "TIMEOUT",
            Self::NotFound => "NOT_FOUND",
            Self::Ambiguous => "AMBIGUOUS",
            Self::Internal => "INTERNAL",
            Self::OffScreen => "OFF_SCREEN",
            Self::SecureWindow => "SECURE_WINDOW",
        }
    }

    /// Parse an uppercase ASCII tag (after the `ERR:` prefix has
    /// been stripped) into an [`ErrorCode`].
    ///
    /// The tag is matched case-sensitively against the canonical
    /// uppercase spelling. Unrecognised tags return `None` so the
    /// caller can decide whether to surface them as `ProtocolError`
    /// or swallow them.
    pub const fn from_tag(tag: &[u8]) -> Option<Self> {
        match tag {
            b"OK" => Some(Self::Ok),
            b"UNKNOWN_CMD" => Some(Self::UnknownCmd),
            b"BAD_ARG" => Some(Self::BadArg),
            b"TIMEOUT" => Some(Self::Timeout),
            b"NOT_FOUND" => Some(Self::NotFound),
            b"AMBIGUOUS" => Some(Self::Ambiguous),
            b"INTERNAL" => Some(Self::Internal),
            b"OFF_SCREEN" => Some(Self::OffScreen),
            b"SECURE_WINDOW" => Some(Self::SecureWindow),
            _ => None,
        }
    }
}

/// Parsed `ERR:<CODE>[:<detail>]` payload.
///
/// Wire format (matches handsets):
/// ```text
/// ERR:NOT_FOUND
/// ERR:NOT_FOUND:no-such-app
/// ERR:BAD_ARG:key=x? missing value
/// ```
/// Detail bytes are kept verbatim — callers can re-render them
/// in their own encoding (UTF-8 lossy, ASCII strip, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFrame {
    /// Numeric / symbolic error code parsed from the tag.
    pub code: ErrorCode,
    /// Optional human-readable detail that followed `:` (may be empty).
    pub detail: String,
}

impl ErrorFrame {
    /// Build an error frame.
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Parse a payload as `ERR:<CODE>[:<detail>]`.
    ///
    /// Returns:
    /// - `Ok(None)` if `payload` does not start with [`ERR_PREFIX`].
    /// - `Ok(Some(frame))` if it does and the code parses.
    /// - `Err(ProtocolError::UnknownCode(name))` if the tag is
    ///   unrecognised. The original (unparsed) tag is preserved in
    ///   the error so the caller can log it.
    pub fn parse(payload: &[u8]) -> Result<Option<Self>, ProtocolError> {
        let Some(rest) = payload.strip_prefix(ERR_PREFIX) else {
            return Ok(None);
        };
        // Tag is everything up to (but not including) the next `:`.
        // If there is no `:`, the entire rest is the tag.
        let (tag, detail) = match rest.iter().position(|&b| b == b':') {
            Some(idx) => (&rest[..idx], &rest[idx + 1..]),
            None => (rest, &[][..]),
        };
        let code = ErrorCode::from_tag(tag).ok_or_else(|| {
            // Tag bytes are not guaranteed UTF-8, so build a lossy String.
            let name = String::from_utf8_lossy(tag).into_owned();
            ProtocolError::UnknownCode(name)
        })?;
        let detail = String::from_utf8_lossy(detail).into_owned();
        Ok(Some(Self { code, detail }))
    }

    /// Encode back to the canonical wire form.
    ///
    /// Includes the [`ERR_PREFIX`], the uppercase tag, and — when
    /// `detail` is non-empty — the `:` separator + detail bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ERR_PREFIX.len() + self.code.as_tag().len() + self.detail.len() + 1);
        out.extend_from_slice(ERR_PREFIX);
        out.extend_from_slice(self.code.as_tag().as_bytes());
        if !self.detail.is_empty() {
            out.push(b':');
            out.extend_from_slice(self.detail.as_bytes());
        }
        out
    }
}

/// Protocol-layer error returned by [`ErrorFrame::parse`].
///
/// This is **not** the same as an [`ErrorCode`] — `ProtocolError`
/// signals that the daemon returned a syntactically malformed
/// `ERR:` frame (unknown tag) rather than a recognised semantic
/// failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// `ERR:` frame had a tag that does not match any known code.
    #[error("unknown error code tag: {0}")]
    UnknownCode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_variant_byte() {
        for code in [
            ErrorCode::Ok,
            ErrorCode::UnknownCmd,
            ErrorCode::BadArg,
            ErrorCode::Timeout,
            ErrorCode::NotFound,
            ErrorCode::Ambiguous,
            ErrorCode::Internal,
            ErrorCode::OffScreen,
            ErrorCode::SecureWindow,
        ] {
            let b = code.as_byte();
            assert_eq!(ErrorCode::from_byte(b), Ok(code));
        }
    }

    #[test]
    fn unknown_byte_is_error() {
        assert_eq!(ErrorCode::from_byte(255), Err(ErrorCodeByteError(255)));
        assert_eq!(ErrorCode::from_byte(9), Err(ErrorCodeByteError(9)));
    }

    #[test]
    fn tags_are_distinct_uppercase() {
        let tags = [
            ErrorCode::Ok.as_tag(),
            ErrorCode::UnknownCmd.as_tag(),
            ErrorCode::BadArg.as_tag(),
            ErrorCode::Timeout.as_tag(),
            ErrorCode::NotFound.as_tag(),
            ErrorCode::Ambiguous.as_tag(),
            ErrorCode::Internal.as_tag(),
            ErrorCode::OffScreen.as_tag(),
            ErrorCode::SecureWindow.as_tag(),
        ];
        let unique: std::collections::HashSet<_> = tags.iter().copied().collect();
        assert_eq!(unique.len(), tags.len());
        for t in tags {
            assert!(
                t.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                "tag {t:?} must be uppercase ASCII / underscore"
            );
        }
    }

    #[test]
    fn from_tag_round_trip() {
        for code in [
            ErrorCode::Ok,
            ErrorCode::UnknownCmd,
            ErrorCode::BadArg,
            ErrorCode::Timeout,
            ErrorCode::NotFound,
            ErrorCode::Ambiguous,
            ErrorCode::Internal,
            ErrorCode::OffScreen,
            ErrorCode::SecureWindow,
        ] {
            let tag = code.as_tag();
            assert_eq!(ErrorCode::from_tag(tag.as_bytes()), Some(code));
        }
    }

    #[test]
    fn from_tag_lowercase_is_rejected() {
        // Tags must be uppercase on the wire.
        assert_eq!(ErrorCode::from_tag(b"not_found"), None);
        assert_eq!(ErrorCode::from_tag(b"Ok"), None);
    }

    #[test]
    fn err_prefix_is_exact_bytes() {
        assert_eq!(ERR_PREFIX, b"ERR:");
    }

    #[test]
    fn parse_returns_none_when_no_prefix() {
        assert_eq!(ErrorFrame::parse(b"hello world"), Ok(None));
        assert_eq!(ErrorFrame::parse(b""), Ok(None));
        assert_eq!(ErrorFrame::parse(b"err:not_found"), Ok(None)); // lowercase prefix
    }

    #[test]
    fn parse_tag_only() {
        let frame =
            ErrorFrame::parse(b"ERR:NOT_FOUND").unwrap().unwrap();
        assert_eq!(frame.code, ErrorCode::NotFound);
        assert_eq!(frame.detail, "");
    }

    #[test]
    fn parse_tag_and_detail() {
        let frame = ErrorFrame::parse(b"ERR:NOT_FOUND:no-such-app")
            .unwrap()
            .unwrap();
        assert_eq!(frame.code, ErrorCode::NotFound);
        assert_eq!(frame.detail, "no-such-app");
    }

    #[test]
    fn parse_unknown_tag_is_error() {
        let err = ErrorFrame::parse(b"ERR:NOPE:something").unwrap_err();
        assert_eq!(err, ProtocolError::UnknownCode("NOPE".to_owned()));
    }

    #[test]
    fn parse_empty_detail_after_colon() {
        let frame =
            ErrorFrame::parse(b"ERR:TIMEOUT:").unwrap().unwrap();
        assert_eq!(frame.code, ErrorCode::Timeout);
        assert_eq!(frame.detail, "");
    }

    #[test]
    fn encode_round_trip_no_detail() {
        let frame = ErrorFrame::new(ErrorCode::Ambiguous, "");
        let bytes = frame.encode();
        assert_eq!(&bytes, b"ERR:AMBIGUOUS");
        let parsed = ErrorFrame::parse(&bytes).unwrap().unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn encode_round_trip_with_detail() {
        let frame = ErrorFrame::new(ErrorCode::NotFound, "no-such-app");
        let bytes = frame.encode();
        assert_eq!(&bytes, b"ERR:NOT_FOUND:no-such-app");
        let parsed = ErrorFrame::parse(&bytes).unwrap().unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn encode_skips_colon_when_detail_empty() {
        let bytes = ErrorFrame::new(ErrorCode::BadArg, "").encode();
        assert!(!bytes.ends_with(b":"));
        assert_eq!(&bytes, b"ERR:BAD_ARG");
    }
}