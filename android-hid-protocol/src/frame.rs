//! Length-prefixed wire frame.
//!
//! On the wire every message is `u32 big-endian length` + payload bytes.
//! The 16 MiB cap is the maximum payload we will accept — well above the
//! largest expected screenshot stream chunk and far below the allocation
//! that would let a hostile peer exhaust memory.
//!
//! A zero-length frame (`length == 0`, no payload bytes) is reserved as
//! a stream terminator: streamed verbs (`stream`, `pull`, `dumpsys`,
//! `logcat`, `monitor`, `state_watch`, `clip_watch`, `shell`) end the
//! multi-frame response with a single `Frame::empty_marker()` so the
//! client can detect end-of-stream without an out-of-band EOF on the
//! socket.

use std::io::{Read, Write};

use thiserror::Error;

/// Length of the wire length prefix (big-endian `u32`).
pub const HEADER_LEN: usize = 4;

/// Hard cap on a single frame's payload.
///
/// 16 MiB is comfortably above any legitimate single-frame payload
/// (a JPEG screenshot at 1080p rarely exceeds 1 MiB) and small enough
/// to bound per-frame allocation.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Owned wire frame.
///
/// `bytes` is the on-wire payload (everything after the 4-byte length
/// prefix). The struct is `Clone` so callers can stash a frame for
/// replay; it does **not** implement `Copy` because that would force
/// every payload — including a 16 MiB screenshot — into a stack value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Payload bytes (post-prefix). May be empty when `is_terminator()`.
    bytes: Vec<u8>,
}

impl Frame {
    /// Wrap the given payload in a frame. Allocates a `Vec<u8>`.
    ///
    /// Does **not** check against [`MAX_FRAME_LEN`] — callers that
    /// accept untrusted input must validate via [`Frame::try_from_vec`].
    #[inline]
    #[must_use]
    pub fn new(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: payload.into(),
        }
    }

    /// Build a frame from a `&'static [u8]` without allocating.
    ///
    /// The payload is copied into a fresh `Vec<u8>` so the resulting
    /// `Frame` is owned and can outlive the `'static` lifetime
    /// expectation of any caller. Use this for short command-line
    /// literals like `b"quit\n"` where copy-cost is negligible.
    #[inline]
    #[must_use]
    pub fn from_static(payload: &'static [u8]) -> Self {
        Self {
            bytes: payload.to_vec(),
        }
    }

    /// The wire payload (without the length prefix).
    #[inline]
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.bytes
    }

    /// Payload length in bytes. Zero means stream terminator.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when the payload is empty. Equivalent to
    /// [`Self::is_terminator`] but matches the `is_empty` convention
    /// other Rust collections follow.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// True when this frame has no payload — the wire stream terminator.
    #[inline]
    #[must_use]
    pub fn is_terminator(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The canonical zero-length frame used as a stream terminator.
    ///
    /// On the wire this encodes as the four bytes `0x00 0x00 0x00 0x00`
    /// (length = 0, no payload).
    #[inline]
    #[must_use]
    pub fn empty_marker() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Encode this frame onto `out` as `[u32 BE length][payload]`.
    ///
    /// Returns [`std::io::Result`] so callers can wire it straight
    /// into a buffered socket / file / test vector. Writes are not
    /// buffered internally — wrap `out` in a `BufWriter` if you care
    /// about syscall count.
    ///
    /// # Errors
    /// - `ErrorKind::InvalidData` if the payload exceeds `u32::MAX`.
    /// - Any `io::Error` returned by `out.write`.
    pub fn encode(&self, out: &mut impl Write) -> std::io::Result<()> {
        let len: u32 = self.bytes.len().try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "frame payload {} bytes exceeds u32::MAX",
                    self.bytes.len()
                ),
            )
        })?;
        out.write_all(&len.to_be_bytes())?;
        out.write_all(&self.bytes)?;
        Ok(())
    }

    /// Decode the next frame from `reader`.
    ///
    /// Reads exactly `[u32 BE length][payload]` and enforces the
    /// [`MAX_FRAME_LEN`] cap. A declared length of `0` returns
    /// [`Frame::empty_marker()`] without touching `reader` for a
    /// payload — this is the stream-terminator case and it must
    /// consume exactly the four header bytes so the next frame in a
    /// stream is aligned correctly.
    ///
    /// # Errors
    /// - `UnexpectedEof` if the header or payload is short.
    /// - `InvalidData` if the declared length exceeds `MAX_FRAME_LEN`.
    pub fn decode(reader: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(e);
            }
            Err(e) => return Err(e),
        }
        let declared = u32::from_be_bytes(header) as usize;
        if declared == 0 {
            return Ok(Self::empty_marker());
        }
        if declared > MAX_FRAME_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "frame length {declared} exceeds cap {MAX_FRAME_LEN}"
                ),
            ));
        }
        let mut payload = vec![0u8; declared];
        reader.read_exact(&mut payload)?;
        Ok(Self { bytes: payload })
    }
}

/// Build a [`Frame`] from a `Vec<u8>`, rejecting oversized payloads.
///
/// This is the checked variant of [`Frame::new`].
impl Frame {
    /// Fallible constructor that enforces [`MAX_FRAME_LEN`].
    ///
    /// # Errors
    /// - [`FrameError::TooLarge`] when `bytes.len() > MAX_FRAME_LEN`.
    pub fn try_from_vec(bytes: Vec<u8>) -> Result<Self, FrameError> {
        if bytes.len() > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(bytes.len()));
        }
        Ok(Self { bytes })
    }
}

/// Failure modes for frame construction.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    /// Declared length exceeds [`MAX_FRAME_LEN`].
    #[error("frame length {0} exceeds cap {MAX_FRAME_LEN}")]
    TooLarge(usize),
    /// Declared length is larger than the available bytes.
    #[error("declared frame length {declared} exceeds available {available}")]
    Truncated {
        /// Declared length from the prefix.
        declared: usize,
        /// Actual bytes available.
        available: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_len_is_four() {
        assert_eq!(HEADER_LEN, 4);
    }

    #[test]
    fn max_frame_len_is_16mib() {
        assert_eq!(MAX_FRAME_LEN, 16 * 1024 * 1024);
    }

    #[test]
    fn new_and_accessors() {
        let f = Frame::new(b"hello".to_vec());
        assert_eq!(f.payload(), b"hello");
        assert_eq!(f.len(), 5);
        assert!(!f.is_terminator());
    }

    #[test]
    fn from_static_does_not_allocate_logically() {
        let f = Frame::from_static(b"ping\n");
        assert_eq!(f.payload(), b"ping\n");
        assert_eq!(f.len(), 5);
    }

    #[test]
    fn empty_marker_is_terminator() {
        let f = Frame::empty_marker();
        assert_eq!(f.len(), 0);
        assert!(f.is_terminator());
        assert_eq!(f.payload(), b"");
    }

    #[test]
    fn encode_round_trip() {
        let original = Frame::new(b"tap x=540 y=1200".to_vec());
        let mut buf = Vec::new();
        original.encode(&mut buf).unwrap();

        // [u32 BE len][payload]
        assert_eq!(&buf[..4], &16u32.to_be_bytes());
        assert_eq!(&buf[4..], b"tap x=540 y=1200");

        let mut cursor = Cursor::new(buf);
        let decoded = Frame::decode(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn encode_writes_zero_length_for_terminator() {
        let mut buf = Vec::new();
        Frame::empty_marker().encode(&mut buf).unwrap();
        assert_eq!(buf, [0u8; 4]);
    }

    #[test]
    fn decode_terminator_returns_empty_marker() {
        let bytes = [0u8; 4];
        let mut cur = Cursor::new(bytes);
        let f = Frame::decode(&mut cur).unwrap();
        assert!(f.is_terminator());
    }

    #[test]
    fn decode_short_header_returns_unexpected_eof() {
        let bytes = [0u8; 2];
        let mut cur = Cursor::new(bytes);
        let err = Frame::decode(&mut cur).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_short_payload_returns_unexpected_eof() {
        // header says 10 bytes, but only 3 follow
        let bytes = [0u8, 0u8, 0u8, 10, b'a', b'b', b'c'];
        let mut cur = Cursor::new(bytes.to_vec());
        let err = Frame::decode(&mut cur).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_oversize_returns_invalid_data() {
        // length = MAX_FRAME_LEN + 1
        let bad = ((MAX_FRAME_LEN as u32) + 1).to_be_bytes();
        let mut cur = Cursor::new(bad.to_vec());
        let err = Frame::decode(&mut cur).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_max_allowed_succeeds_when_truncated_at_boundary() {
        // Length = MAX_FRAME_LEN. We provide a header but no payload,
        // expecting the read to fail with UnexpectedEof (NOT InvalidData).
        let header = (MAX_FRAME_LEN as u32).to_be_bytes();
        let mut cur = Cursor::new(header.to_vec());
        let err = Frame::decode(&mut cur).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn try_from_vec_enforces_cap() {
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert_eq!(
            Frame::try_from_vec(big),
            Err(FrameError::TooLarge(MAX_FRAME_LEN + 1))
        );
        let ok = vec![0u8; MAX_FRAME_LEN];
        assert_eq!(Frame::try_from_vec(ok).map(|f| f.len()), Ok(MAX_FRAME_LEN));
    }

    #[test]
    fn encode_decode_various_sizes() {
        for size in [0usize, 1, 100, 1024, 65_535] {
            let payload: Vec<u8> = (0..size).map(|i| (i & 0xFF) as u8).collect();
            let frame = Frame::new(payload.clone());
            let mut buf = Vec::new();
            frame.encode(&mut buf).unwrap();

            // Sanity-check the prefix is BE.
            assert_eq!(
                &buf[..4],
                &(size as u32).to_be_bytes(),
                "BE prefix wrong for size {size}"
            );

            let mut cur = Cursor::new(buf);
            let decoded = Frame::decode(&mut cur).unwrap();
            assert_eq!(decoded.payload(), payload.as_slice());
        }
    }

    #[test]
    fn stream_terminator_after_payload() {
        // The classic multi-frame reply: one payload, then terminator.
        let mut bytes = Vec::new();
        Frame::new(b"chunk-1".to_vec())
            .encode(&mut bytes)
            .unwrap();
        Frame::empty_marker().encode(&mut bytes).unwrap();

        let mut cur = Cursor::new(bytes);
        let first = Frame::decode(&mut cur).unwrap();
        assert_eq!(first.payload(), b"chunk-1");
        assert!(!first.is_terminator());

        let second = Frame::decode(&mut cur).unwrap();
        assert!(second.is_terminator());
    }

    #[test]
    fn exact_bytes_on_wire_for_simple_payload() {
        // Hand-rolled wire bytes for payload b"\x01\x02\x03\x04" (len 4)
        // should be 00 00 00 04 01 02 03 04.
        let f = Frame::new(vec![1, 2, 3, 4]);
        let mut buf = Vec::new();
        f.encode(&mut buf).unwrap();
        assert_eq!(buf, vec![0, 0, 0, 4, 1, 2, 3, 4]);
    }
}