//! Wire protocol layer — see v3 §3.3, §3.4.
//!
//! Replaces the legacy daemon's 70+ ASCII verbs with a typed, binary,
//! 4-verb protocol (`action` / `plan` / `observe` / `query`).
//! 70+ internal capabilities stay (see [`crate::capability`]) but
//! are not exposed at this layer — they're daemon-side
//! implementation detail routed by the typed surface.
//!
//! ## Frame layout (v3 §3.4)
//!
//! ```text
//! ┌──────────┬──────────┬─────────────┬────────────────────────┐
//! │ type(1B) │ flags(1B) │ len(varint) │ payload(len bytes)     │
//! └──────────┴──────────┴─────────────┴────────────────────────┘
//! ```
//!
//! - **`type`** — discriminant for [`Verb`]: `Action`, `Plan`,
//!   `Observe`, `Query`, `EndOfStream`.
//! - **`flags`** — see [`FrameFlags`]: `Idempotent`, `WaitGroundTruth`,
//!   `CheckpointEveryN`, etc.
//! - **`len`** — unsigned LEB128 (varint) length of `payload`. Small
//!   frames (< 128 B) save 1–3 bytes vs a `u32 BE` length prefix.
//! - **`payload`** — postcard-encoded typed struct (see
//!   [`RequestPayload`] / [`ReplyPayload`]).
//!
//! ## Observe streaming
//!
//! `Observe` is a server-stream: the daemon emits one
//! [`ReplyPayload::Observation`] frame per observation; the host
//! keeps reading until it sees a [`ReplyPayload::EndOfStream`]
//! marker (server-side natural end) or hits its own timeout.
//!
//! ## Design decisions (v3 §6.1)
//!
//! - **Binary (postcard), not JSON** — parsing 10× faster, 0-token
//!   wastage on the wire, free cross-language binding.
//! - **varint length prefix, not `u32 BE`** — small frames save 2–3
//!   B.
//! - **typed `Action`, not verb-string** — LLM-friendly, compile-time
//!   validation, automatic docs.
//! - **4 core verbs, not 70+** — minimal API surface; the 70+
//!   legacy verbs become internal capabilities behind
//!   [`crate::capability`].

use serde::{Deserialize, Serialize};

use crate::action::{Action, ActionResult};
use crate::ids::{ActionId, PlanId};
use crate::observation::Observation;
use crate::plan::{Plan, PlanResult};
use crate::predicate::EventKind;

// ---------------------------------------------------------------------------
// Verb discriminator (type byte)
// ---------------------------------------------------------------------------

/// The 4 core verbs (plus end-of-stream) — see v3 §3.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Verb {
    /// Single action → ActionResult reply (with ground truth).
    Action = 0x01,
    /// Multi-action plan → PlanResult reply (atomic, 1 RTT).
    Plan = 0x02,
    /// Server-push observation stream subscription.
    Observe = 0x03,
    /// One-shot Observation pull (used for idle fallback).
    Query = 0x04,
    /// Stream terminator — sent as the last frame of an
    /// `Observe` server-stream so the host knows when to stop
    /// reading.
    EndOfStream = 0x05,
}

impl Verb {
    /// Wire byte value. Same as `as u8` but `const`-friendly.
    #[inline]
    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Parse the wire byte into a verb. Returns `None` for
    /// unknown discriminators so a server-version mismatch surfaces
    /// as a typed error rather than a panic.
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x01 => Self::Action,
            0x02 => Self::Plan,
            0x03 => Self::Observe,
            0x04 => Self::Query,
            0x05 => Self::EndOfStream,
            _ => return None,
        })
    }

    /// Short stable label suitable for log lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Plan => "plan",
            Self::Observe => "observe",
            Self::Query => "query",
            Self::EndOfStream => "end-of-stream",
        }
    }
}

// ---------------------------------------------------------------------------
// Frame flags (1-byte)
// ---------------------------------------------------------------------------

/// Per-frame flags. Some are host→daemon (requests), some are
/// daemon→host (replies). Bit positions follow v3 §3.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrameFlags(pub u8);

impl FrameFlags {
    /// Idempotent: payload carries a server-assigned ID; a
    /// replay returns the original result rather than executing
    /// twice (v3 §6.1).
    pub const IDEMPOTENT: Self = Self(0x80);
    /// Wait for ground truth (host→daemon, Action / Plan / Query).
    pub const WAIT_GROUND_TRUTH: Self = Self(0x40);
    /// Checkpoint every N (host→daemon, Plan).
    pub const CHECKPOINT_EVERY_N: Self = Self(0x20);
    /// Final observation included in the reply (daemon→host, Plan).
    pub const INCLUDES_FINAL_OBS: Self = Self(0x10);
    /// Compressed payload (deflate). Currently unused; reserved.
    pub const COMPRESSED: Self = Self(0x08);
    /// Reply is an error frame (daemon→host).
    pub const IS_ERROR: Self = Self(0x04);
    /// Reply is the end of an observation stream (daemon→host).
    pub const IS_END_OF_STREAM: Self = Self(0x02);
    /// Reply is a checkpoint (mid-plan, daemon→host).
    pub const IS_CHECKPOINT: Self = Self(0x01);

    /// Build a flag set from a raw byte.
    #[inline]
    #[must_use]
    pub const fn from_bits(b: u8) -> Self {
        Self(b)
    }

    /// Raw byte representation.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True if the given flag is set.
    #[inline]
    #[must_use]
    pub const fn contains(self, flag: FrameFlags) -> bool {
        (self.0 & flag.0) == flag.0
    }

    /// Set the given flag.
    #[inline]
    pub fn set(&mut self, flag: FrameFlags) {
        self.0 |= flag.0;
    }
}

impl std::ops::BitOr for FrameFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for FrameFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// ---------------------------------------------------------------------------
// Request payloads (host → daemon)
// ---------------------------------------------------------------------------

/// Request payloads keyed by [`Verb`]. Defined as a single enum
/// so postcard has a consistent tag for the request frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestPayload {
    /// `Verb::Action` payload.
    Action {
        /// Server-assigned ID (filled by daemon on first sight;
        /// ignored on first send from host).
        id: ActionId,
        /// The action.
        action: Action,
    },
    /// `Verb::Plan` payload.
    Plan {
        /// Server-assigned ID.
        id: PlanId,
        /// The plan.
        plan: Plan,
    },
    /// `Verb::Observe` payload.
    Observe {
        /// Subscribe to observations with `seq > since_seq`.
        since_seq: u64,
        /// Filter: which [`EventKind`]s to include in the
        /// `Observation::events` list. Empty = all.
        filter: Vec<EventKind>,
    },
    /// `Verb::Query` payload.
    Query {
        /// Include a11y tree snapshot.
        a11y: bool,
        /// Include frame snapshot (H.265 keyframe / JPEG tile).
        frame: bool,
        /// Include current device state.
        state: bool,
    },
    /// `Verb::EndOfStream` carries no payload; reserved for
    /// symmetry so a single tag dispatch is enough on the wire.
    EndOfStream,
}

impl RequestPayload {
    /// Which verb this request is sent under.
    #[must_use]
    pub const fn verb(&self) -> Verb {
        match self {
            Self::Action { .. } => Verb::Action,
            Self::Plan { .. } => Verb::Plan,
            Self::Observe { .. } => Verb::Observe,
            Self::Query { .. } => Verb::Query,
            Self::EndOfStream => Verb::EndOfStream,
        }
    }
}

// ---------------------------------------------------------------------------
// Reply payloads (daemon → host)
// ---------------------------------------------------------------------------

/// Reply payloads keyed by [`Verb`]. Defined as a single enum so
/// postcard encoding matches `RequestPayload`'s shape (the daemon
/// can dispatch with one tag byte).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplyPayload {
    /// `Action` reply.
    Action(ActionResult),
    /// `Plan` reply.
    Plan(PlanResult),
    /// Observe stream frame (one of N).
    Observation(Observation),
    /// Query reply.
    Query(Observation),
    /// Stream terminator (last frame of an `Observe` reply).
    EndOfStream {
        /// Highest seq observed in this stream run.
        final_seq: u64,
    },
}

impl ReplyPayload {
    /// Which verb this reply corresponds to.
    #[must_use]
    pub const fn verb(&self) -> Verb {
        match self {
            Self::Action(_) => Verb::Action,
            Self::Plan(_) => Verb::Plan,
            Self::Observation(_) => Verb::Observe,
            Self::EndOfStream { .. } => Verb::EndOfStream,
            Self::Query(_) => Verb::Query,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire frame
// ---------------------------------------------------------------------------

/// One wire frame — see v3 §3.4 layout.
///
/// ```text
/// ┌──────────┬──────────┬─────────────┬────────────────────────┐
/// │ type(1B) │ flags(1B) │ len(varint) │ payload(len bytes)     │
/// └──────────┴──────────┴─────────────┴────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// `Verb` discriminant byte.
    pub verb: Verb,
    /// `FrameFlags` byte.
    pub flags: FrameFlags,
    /// postcard-encoded payload (RequestPayload or ReplyPayload).
    pub payload: Vec<u8>,
}

impl Frame {
    /// Encode a request frame for the given payload.
    #[must_use]
    pub fn request(payload: &RequestPayload) -> Self {
        let bytes = postcard::to_allocvec(payload).expect("encode request");
        Self {
            verb: payload.verb(),
            flags: FrameFlags::default(),
            payload: bytes,
        }
    }

    /// Encode a reply frame for the given payload.
    #[must_use]
    pub fn reply(payload: &ReplyPayload) -> Self {
        let bytes = postcard::to_allocvec(payload).expect("encode reply");
        let mut flags = FrameFlags::default();
        if matches!(payload, ReplyPayload::EndOfStream { .. }) {
            flags.set(FrameFlags::IS_END_OF_STREAM);
        }
        Self {
            verb: payload.verb(),
            flags,
            payload: bytes,
        }
    }

    /// Total frame size in bytes — sum of header (1 + 1 + varint) and
    /// payload. Useful for `transport.write_all(encoded)`.
    #[must_use]
    pub fn encoded_size(&self) -> usize {
        2 + varint_size(self.payload.len()) + self.payload.len()
    }

    /// Encode to a flat byte vector suitable for writing to a
    /// `TcpStream`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = self.payload.len();
        let mut buf =
            Vec::with_capacity(2 + varint_size(len) + len);
        buf.push(self.verb.byte());
        buf.push(self.flags.bits());
        encode_varint(&mut buf, len);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a frame from the given byte buffer. Returns `None`
    /// if the buffer is shorter than the header indicates
    /// (caller should read more).
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 2 {
            return None;
        }
        let verb = Verb::from_byte(buf[0])?;
        let flags = FrameFlags::from_bits(buf[1]);
        let (payload_len, header_len) = decode_varint(&buf[2..])?;
        let total = 2 + header_len + payload_len;
        if buf.len() < total {
            return None;
        }
        Some(Self {
            verb,
            flags,
            payload: buf[2 + header_len..total].to_vec(),
        })
    }

    /// Decode the payload as a [`RequestPayload`].
    pub fn decode_request(&self) -> postcard::Result<RequestPayload> {
        postcard::from_bytes(&self.payload)
    }

    /// Decode the payload as a [`ReplyPayload`].
    pub fn decode_reply(&self) -> postcard::Result<ReplyPayload> {
        postcard::from_bytes(&self.payload)
    }
}

// ---------------------------------------------------------------------------
// Unsigned LEB128 (varint)
// ---------------------------------------------------------------------------

/// Number of bytes needed to encode `n` as an unsigned varint.
#[must_use]
const fn varint_size(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut n = n;
    let mut size = 0;
    while n > 0 {
        n >>= 7;
        size += 1;
    }
    size
}

/// Append the unsigned varint encoding of `n` to `buf`.
fn encode_varint(buf: &mut Vec<u8>, mut n: usize) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

/// Decode an unsigned varint at the start of `buf`. Returns
/// `(value, bytes_consumed)` or `None` if the varint is truncated.
fn decode_varint(buf: &[u8]) -> Option<(usize, usize)> {
    let mut result: usize = 0;
    let mut shift = 0;
    for (i, b) in buf.iter().enumerate() {
        let cont = b & 0x80 != 0;
        let value = (b & 0x7F) as usize;
        result |= value.checked_shl(shift as u32)?;
        shift += 7;
        if !cont {
            return Some((result, i + 1));
        }
        if shift > usize::BITS as usize {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::LaunchBy;
    use crate::observation::DeviceState;

    #[test]
    fn verb_round_trip_from_byte() {
        for byte in 0x01..=0x05 {
            let verb = Verb::from_byte(byte).expect("known verb");
            assert_eq!(verb.byte(), byte);
            assert!(!verb.as_str().is_empty());
        }
    }

    #[test]
    fn verb_unknown_byte_returns_none() {
        assert_eq!(Verb::from_byte(0x00), None);
        assert_eq!(Verb::from_byte(0x06), None);
        assert_eq!(Verb::from_byte(0xFF), None);
    }

    #[test]
    fn verb_labels_are_distinct() {
        let labels = [
            Verb::Action.as_str(),
            Verb::Plan.as_str(),
            Verb::Observe.as_str(),
            Verb::Query.as_str(),
            Verb::EndOfStream.as_str(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn frame_flags_contain_and_or() {
        let mut f = FrameFlags::default();
        assert!(!f.contains(FrameFlags::IDEMPOTENT));
        f.set(FrameFlags::IDEMPOTENT);
        assert!(f.contains(FrameFlags::IDEMPOTENT));

        let combined = FrameFlags::IDEMPOTENT | FrameFlags::WAIT_GROUND_TRUTH;
        assert_eq!(combined.bits(), 0x80 | 0x40);
    }

    #[test]
    fn varint_small_number_is_one_byte() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);
        encode_varint(&mut buf, 1);
        assert_eq!(buf, vec![0x00, 0x01]);
        encode_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x00, 0x01, 0x7F]);
        encode_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x00, 0x01, 0x7F, 0x80, 0x01]);
        assert_eq!(decode_varint(&[0x00]).unwrap(), (0, 1));
        assert_eq!(decode_varint(&[0x7F]).unwrap(), (127, 1));
        assert_eq!(decode_varint(&[0x80, 0x01]).unwrap(), (128, 2));
    }

    #[test]
    fn varint_round_trip_random_values() {
        for n in [
            0usize,
            1,
            100,
            127,
            128,
            16_383,
            16_384,
            1 << 21,
            1_000_000,
            usize::MAX / 2,
            usize::MAX,
        ] {
            let mut buf = Vec::new();
            encode_varint(&mut buf, n);
            assert_eq!(varint_size(n), buf.len(), "wrong size for n={n}");
            let (decoded, consumed) =
                decode_varint(&buf).expect("decode varint");
            assert_eq!(decoded, n, "mismatch for n={n}");
            assert_eq!(consumed, buf.len(), "wrong consumed for n={n}");
        }
    }

    #[test]
    fn varint_truncated_returns_none() {
        // 0x80 indicates continuation but no following byte.
        assert_eq!(decode_varint(&[0x80]), None);
        assert_eq!(decode_varint(&[]), None);
    }

    #[test]
    fn frame_encode_decode_round_trip() {
        let frame = Frame::request(&RequestPayload::Action {
            id: ActionId(99),
            action: Action::Launch {
                target: "com.foo/.Main".into(),
                by: LaunchBy::Component("com.foo/.Main".into()),
                deadline_ms: 1000,
            },
        });
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).expect("decode frame");
        assert_eq!(decoded.verb, frame.verb);
        assert_eq!(decoded.flags, frame.flags);
        assert_eq!(decoded.payload, frame.payload);

        let request: RequestPayload = decoded.decode_request().unwrap();
        assert_eq!(request.verb(), Verb::Action);
    }

    #[test]
    fn request_payload_verb_dispatch() {
        let cases: [(RequestPayload, Verb); 5] = [
            (
                RequestPayload::Action {
                    id: ActionId(0),
                    action: Action::Tap {
                        x: 0,
                        y: 0,
                        deadline_ms: 0,
                    },
                },
                Verb::Action,
            ),
            (
                RequestPayload::Plan {
                    id: PlanId(0),
                    plan: Plan::new(vec![]),
                },
                Verb::Plan,
            ),
            (
                RequestPayload::Observe {
                    since_seq: 0,
                    filter: vec![],
                },
                Verb::Observe,
            ),
            (
                RequestPayload::Query {
                    a11y: true,
                    frame: false,
                    state: false,
                },
                Verb::Query,
            ),
            (RequestPayload::EndOfStream, Verb::EndOfStream),
        ];
        for (request, expected) in cases {
            assert_eq!(request.verb(), expected);
        }
    }

    #[test]
    fn reply_payload_verb_dispatch() {
        assert_eq!(
            ReplyPayload::Action(ActionResult {
                id: ActionId(0),
                landed: true,
                ground_truth: crate::action::GroundTruth::default(),
                elapsed_ms: 0,
            })
            .verb(),
            Verb::Action
        );
        let obs = Observation {
            seq: 0,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        };
        assert_eq!(ReplyPayload::Observation(obs.clone()).verb(), Verb::Observe);
        assert_eq!(ReplyPayload::Query(obs.clone()).verb(), Verb::Query);
        assert_eq!(
            ReplyPayload::EndOfStream { final_seq: 0 }.verb(),
            Verb::EndOfStream
        );
    }

    #[test]
    fn frame_decode_short_buffer_returns_none() {
        // Empty buffer — no header at all.
        assert!(Frame::decode(&[]).is_none());
        // Header only (no payload).
        assert!(Frame::decode(&[Verb::Action.byte(), 0]).is_none());
        // Length claims more bytes than available.
        assert!(Frame::decode(&[Verb::Action.byte(), 0, 0x05, 0x01, 0x02])
            .is_none());
    }

    #[test]
    fn unknown_verb_byte_returns_none() {
        let buf = [0xFE, 0x00, 0x00];
        assert!(Frame::decode(&buf).is_none());
    }

    #[test]
    fn end_of_stream_frame_sets_flag() {
        let reply = ReplyPayload::EndOfStream { final_seq: 7 };
        let frame = Frame::reply(&reply);
        assert!(frame.flags.contains(FrameFlags::IS_END_OF_STREAM));
        assert_eq!(frame.verb, Verb::EndOfStream);
    }

    #[test]
    fn frame_encoded_size_matches() {
        let frame = Frame::request(&RequestPayload::Query {
            a11y: true,
            frame: false,
            state: true,
        });
        let encoded = frame.encode();
        assert_eq!(frame.encoded_size(), encoded.len());
    }

    #[test]
    fn frame_round_trip_preserves_full_postcard_payload() {
        // Plan round-trip preserves all plan fields.
        let request = RequestPayload::Plan {
            id: PlanId(7),
            plan: Plan::new(vec![])
                .with_abort(false)
                .with_checkpoint_every(3),
        };
        let frame = Frame::request(&request);
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).expect("decode");
        let decoded_request: RequestPayload = decoded.decode_request().unwrap();
        assert_eq!(decoded_request, request);
    }
}
