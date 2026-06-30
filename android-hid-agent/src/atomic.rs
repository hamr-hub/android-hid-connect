//! Atomic operations combining UHID input with a11y observation.
//!
//! These are the LLM "see + act" primitives. Each one issues a
//! single wire request and returns a typed result that contains
//! the input effect **plus** a fresh observation of what the
//! device did — no agent-side glue required.
//!
//! ## Why a single round-trip?
//!
//! Without atomic ops, an LLM agent that wants to
//!
//! 1. tap a target,
//! 2. read the resulting a11y tree,
//!
//! has to issue two round-trips:
//!
//! ```text
//! tap x=540 y=1200      ──►  daemon   (a) injectInputEvent
//!                                          (b) sleep until settled
//!                                          (c) UiAutomation.dump
//! dump_active           ◄──  ~5ms RTT + ~5ms RTT = 10ms
//! ```
//!
//! The atomic op collapses this into one round-trip, so the
//! server-side ordering is preserved (input fires before
//! observation) and the network adds 0ms overhead between the
//! two halves.
//!
//! ## Server-side contract
//!
//! Each atomic op is a single verb on the wire. The daemon's
//! handler runs the input half, sleeps an
//! op-specific settle interval, and then runs the observation
//! half. The full response is one JSON object so the host can
//! deserialize in one read.
//!
//! ## Timing accounting
//!
//! `AtomicTimings` breaks the wall-clock into sub-phases so the
//! agent can see where the budget went:
//!
//! - `selector_resolve_ms` — a11y dump + selector eval on device
//! - `inject_ms`           — input dispatch (UHID write + daemon settle)
//! - `settle_ms`           — wait for animation / layout to finish
//! - `dump_ms`             — a11y tree re-serialisation
//! - `total_ms`            — sum of the above plus network RTT

use std::time::Duration;

use crate::selectors::Selector;

/// Per-phase wall-clock timing for one atomic operation.
///
/// All fields are in milliseconds. `total_ms` is the authoritative
/// number for SLA purposes; the breakdown is informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AtomicTimings {
    /// Time the daemon spent resolving the selector (a11y dump +
    /// eval). `0` for ops that don't take a selector.
    pub selector_resolve_ms: u32,
    /// Time the daemon spent on input dispatch (UHID write +
    /// InputDispatcher settle).
    pub inject_ms: u32,
    /// Wait time between input and observation.
    pub settle_ms: u32,
    /// Time the daemon spent serialising the observation payload.
    pub dump_ms: u32,
    /// Wall-clock sum of the above plus network RTT. This is the
    /// number the host's stopwatch measures; the others come from
    /// the daemon's own accounting.
    pub total_ms: u32,
}

impl AtomicTimings {
    /// Sum of the server-side phases. Excludes `total_ms`
    /// because the daemon computes that from this sum plus the
    /// transport overhead.
    #[inline]
    #[must_use]
    pub const fn server_total_ms(&self) -> u32 {
        self.selector_resolve_ms
            .saturating_add(self.inject_ms)
            .saturating_add(self.settle_ms)
            .saturating_add(self.dump_ms)
    }

    /// Network + deserialisation overhead (host-measured minus
    /// server-measured). Useful for capacity planning.
    #[inline]
    #[must_use]
    pub const fn transport_overhead_ms(&self) -> u32 {
        self.total_ms.saturating_sub(self.server_total_ms())
    }
}

/// The kind of observation an atomic op returned.
///
/// Most ops only need the a11y tree (the agent's primary
/// perception channel). Some need a frame sample too — those
/// return [`Observation::Both`] and the caller can decide which
/// surface to plan against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// a11y tree only.
    A11y(A11ySnapshot),
    /// AI frame summary only.
    Frame(FrameSnapshot),
    /// Both — the daemon sent a11y + a fresh frame in one shot.
    Both {
        /// a11y tree.
        a11y: A11ySnapshot,
        /// Most recent keyframe IDR.
        frame: FrameSnapshot,
    },
}

impl Observation {
    /// Convenience — return the a11y half, ignoring frame data
    /// when both are present.
    #[must_use]
    pub fn a11y(&self) -> Option<&A11ySnapshot> {
        match self {
            Self::A11y(a) | Self::Both { a11y: a, .. } => Some(a),
            Self::Frame(_) => None,
        }
    }

    /// Convenience — return the frame half, ignoring a11y data
    /// when both are present.
    #[must_use]
    pub fn frame(&self) -> Option<&FrameSnapshot> {
        match self {
            Self::Frame(f) | Self::Both { frame: f, .. } => Some(f),
            Self::A11y(_) => None,
        }
    }
}

/// Minimal a11y tree snapshot returned by atomic ops.
///
/// Carries just enough for the agent to decide "is my tap
/// resolved?" without having to re-fetch the full tree:
/// - the matched node (or `None` if the selector hit nothing)
/// - a short tail of the active window's children for spatial
///   reasoning (`near`, `below`, `right-of`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A11ySnapshot {
    /// ID of the active window at observation time.
    pub window_id: Option<u32>,
    /// The node that matched the op's selector (or any node the
    /// daemon's atomic handler decided was the "anchor" — e.g.
    /// for `ai_anchor_tap` this is the a11y node that the AI
    /// detection box projected onto).
    pub matched: Option<MatchedNode>,
    /// Children of the matched node's parent, for spatial
    /// reasoning on the agent side.
    pub siblings: Vec<MatchedNode>,
    /// Best-effort top activity string (e.g. `com.foo/.Main`).
    pub top_activity: Option<String>,
}

/// One a11y node, in compact form for atomic-op results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedNode {
    /// Class short name (e.g. `Button`).
    pub class: String,
    /// Best identifier (resource-id, content-desc, or text — in
    /// that priority order).
    pub id: Option<String>,
    /// Text content.
    pub text: Option<String>,
    /// Center of the rendered bounds, in screen pixels.
    pub center: (i32, i32),
    /// Width × height of the rendered bounds, in pixels.
    pub size: (i32, i32),
    /// True if the node is on-screen and not occluded.
    pub visible: bool,
    /// True if the node has the `clickable` accessibility flag.
    pub clickable: bool,
    /// True if the node is currently enabled.
    pub enabled: bool,
}

impl MatchedNode {
    /// Bounding box `(left, top, right, bottom)` in screen pixels.
    #[inline]
    #[must_use]
    pub const fn bbox(&self) -> (i32, i32, i32, i32) {
        let (cx, cy) = self.center;
        let (w, h) = self.size;
        (cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2)
    }
}

/// AI frame summary snapshot (H.265 keyframe descriptor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// Width of the frame, in pixels.
    pub width: u16,
    /// Height of the frame, in pixels.
    pub height: u16,
    /// Presentation timestamp (90 kHz units, MPEG-TS convention).
    pub pts: u64,
    /// Detection boxes the on-device model produced for this
    /// frame. The atomic op returns up to N most-confident
    /// detections; the cap is server-side configurable.
    pub detections: Vec<Detection>,
    /// True if this frame is a keyframe (decoder can restart
    /// here).
    pub is_keyframe: bool,
}

/// One AI object detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// Bounding box `(left, top, right, bottom)` in frame pixels.
    pub bbox: (i32, i32, i32, i32),
    /// Class id. The class index table is server-side
    /// configurable.
    pub class_id: u8,
    /// Confidence in `[0, 100]`.
    pub confidence: u8,
}

impl Detection {
    /// Center of the bounding box, in frame pixels.
    #[inline]
    #[must_use]
    pub const fn center(&self) -> (i32, i32) {
        let (l, t, r, b) = self.bbox;
        ((l + r) / 2, (t + b) / 2)
    }
}

/// One atomic operation's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicResult {
    /// True if the input half of the op landed (e.g. tap fired,
    /// text typed, key sent). For selector-based ops this is
    /// `true` even if no a11y node matched — the input is
    /// unconditional; the selector just decides **where**.
    pub matched: bool,
    /// Anchor node for the op (the a11y node that the input
    /// targeted). `None` when the op didn't try to resolve a
    /// node.
    pub anchor: Option<MatchedNode>,
    /// The post-input observation. Always present for ops that
    /// take a snapshot; `None` for input-only ops.
    pub observation: Option<Observation>,
    /// Wall-clock + server-side timings.
    pub timings: AtomicTimings,
    /// Wire error code (if the daemon rejected the op). `Ok`
    /// when the daemon accepted and the result is meaningful.
    pub error: Option<String>,
}

impl AtomicResult {
    /// Convenience — return the a11y observation, ignoring frame
    /// data when both are present.
    #[must_use]
    pub fn a11y(&self) -> Option<&A11ySnapshot> {
        self.observation.as_ref().and_then(|o| o.a11y())
    }

    /// Convenience — return the frame observation, ignoring a11y
    /// data when both are present.
    #[must_use]
    pub fn frame(&self) -> Option<&FrameSnapshot> {
        self.observation.as_ref().and_then(|o| o.frame())
    }

    /// True if the op succeeded (no error, anchor present or not
    /// required).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Selector-based atomic op request.
#[derive(Debug, Clone)]
pub struct SelectAndTap {
    /// Selector to resolve on the a11y tree.
    pub selector: Selector,
    /// How long the daemon is allowed to spend trying to find a
    /// matching node. 0 means "single try, no wait".
    pub timeout: Duration,
    /// Settle interval between the input and the observation
    /// snapshot. The daemon sleeps for this long to let the
    /// animation / layout settle.
    pub idle_ms: u32,
}

impl SelectAndTap {
    /// Encode as the wire payload of the `select_and_tap` verb.
    ///
    /// The daemon expects a single ASCII line with k=v pairs.
    /// We serialise the selector as a quoted string so any
    /// embedded whitespace doesn't desync the parser.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "sel=\"{}\" timeout_ms={} idle_ms={}",
            escape_kv(&self.selector.to_string()),
            self.timeout.as_millis(),
            self.idle_ms,
        )
    }
}

/// AI-frame → a11y atomic tap.
///
/// The daemon's AI pipeline produces a detection box, the
/// atomic handler projects it onto the a11y tree, and the
/// resulting node is the tap target.
#[derive(Debug, Clone)]
pub struct AiAnchorTap {
    /// ID of the detection to use as the anchor. The frame
    /// reference is the most recent keyframe on the daemon's
    /// H.265 stream.
    pub detection_id: u32,
    /// Minimum confidence (0-100) the detection must have. The
    /// daemon's atomic handler will reject the op if the chosen
    /// detection is below this bar.
    pub min_confidence: u8,
    /// Settle interval between tap and observation.
    pub idle_ms: u32,
}

impl AiAnchorTap {
    /// Encode as the wire payload of the `ai_anchor_tap` verb.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "detection_id={} min_confidence={} idle_ms={}",
            self.detection_id, self.min_confidence, self.idle_ms,
        )
    }
}

/// Tap + idle + dump_active atomic op.
#[derive(Debug, Clone, Copy)]
pub struct TapAndDump {
    /// Tap x coordinate, in screen pixels.
    pub x: i32,
    /// Tap y coordinate, in screen pixels.
    pub y: i32,
    /// Settle interval (ms) before the a11y dump.
    pub idle_ms: u32,
}

impl TapAndDump {
    /// Encode as the wire payload of the `tap_and_dump` verb.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("x={} y={} idle_ms={}", self.x, self.y, self.idle_ms)
    }
}

/// Type-text + wait-for-text atomic op.
#[derive(Debug, Clone)]
pub struct TypeAndWait {
    /// Text to inject.
    pub text: String,
    /// Text the agent wants to see after the input lands.
    pub wait_for: String,
    /// How long the daemon is allowed to poll before giving up.
    pub timeout: Duration,
}

impl TypeAndWait {
    /// Encode as the wire payload of the `type_and_wait` verb.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "text=\"{}\" wait_for=\"{}\" timeout_ms={}",
            escape_kv(&self.text),
            escape_kv(&self.wait_for),
            self.timeout.as_millis(),
        )
    }
}

/// Escape an ASCII string for embedding inside a quoted k=v
/// value. The daemon's `kvs` parser supports backslash-escaped
/// quotes and backslashes; everything else passes through.
#[must_use]
pub fn escape_kv(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selectors::Selector;

    #[test]
    fn atomic_timings_sum_is_accurate() {
        let t = AtomicTimings {
            selector_resolve_ms: 5,
            inject_ms: 2,
            settle_ms: 50,
            dump_ms: 4,
            total_ms: 70,
        };
        assert_eq!(t.server_total_ms(), 61);
        assert_eq!(t.transport_overhead_ms(), 9);
    }

    #[test]
    fn atomic_timings_saturating_arithmetic() {
        // If the host's stopwatch reports a smaller value than
        // the server total (clock skew, very fast daemon), the
        // transport overhead should clamp to 0, not underflow.
        let t = AtomicTimings {
            selector_resolve_ms: 5,
            inject_ms: 2,
            settle_ms: 50,
            dump_ms: 4,
            total_ms: 30,
        };
        assert_eq!(t.transport_overhead_ms(), 0);
    }

    #[test]
    fn observation_a11y_accessor() {
        let snap = A11ySnapshot {
            window_id: Some(1),
            matched: None,
            siblings: vec![],
            top_activity: None,
        };
        let o = Observation::A11y(snap.clone());
        assert_eq!(o.a11y(), Some(&snap));
        assert_eq!(o.frame(), None);
    }

    #[test]
    fn observation_frame_accessor() {
        let snap = FrameSnapshot {
            width: 1080,
            height: 1920,
            pts: 0,
            detections: vec![],
            is_keyframe: true,
        };
        let o = Observation::Frame(snap.clone());
        assert_eq!(o.frame(), Some(&snap));
        assert_eq!(o.a11y(), None);
    }

    #[test]
    fn observation_both_returns_each_half() {
        let a = A11ySnapshot {
            window_id: Some(1),
            matched: None,
            siblings: vec![],
            top_activity: None,
        };
        let f = FrameSnapshot {
            width: 1080,
            height: 1920,
            pts: 0,
            detections: vec![],
            is_keyframe: true,
        };
        let o = Observation::Both {
            a11y: a.clone(),
            frame: f.clone(),
        };
        assert_eq!(o.a11y(), Some(&a));
        assert_eq!(o.frame(), Some(&f));
    }

    #[test]
    fn matched_node_bbox() {
        let n = MatchedNode {
            class: "Button".into(),
            id: None,
            text: Some("Login".into()),
            center: (540, 1200),
            size: (200, 100),
            visible: true,
            clickable: true,
            enabled: true,
        };
        assert_eq!(n.bbox(), (440, 1150, 640, 1250));
    }

    #[test]
    fn detection_center() {
        let d = Detection {
            bbox: (100, 200, 300, 600),
            class_id: 1,
            confidence: 95,
        };
        assert_eq!(d.center(), (200, 400));
    }

    #[test]
    fn select_and_tap_encode_quotes_selector() {
        let req = SelectAndTap {
            selector: Selector::parse("Button[id=login]").unwrap(),
            timeout: Duration::from_secs(2),
            idle_ms: 200,
        };
        let wire = req.encode();
        assert!(wire.starts_with("sel=\"Button[id=login]\""));
        assert!(wire.contains("timeout_ms=2000"));
        assert!(wire.contains("idle_ms=200"));
    }

    #[test]
    fn ai_anchor_tap_encode() {
        let req = AiAnchorTap {
            detection_id: 7,
            min_confidence: 80,
            idle_ms: 150,
        };
        let wire = req.encode();
        assert!(wire.contains("detection_id=7"));
        assert!(wire.contains("min_confidence=80"));
        assert!(wire.contains("idle_ms=150"));
    }

    #[test]
    fn tap_and_dump_encode() {
        let req = TapAndDump {
            x: 540,
            y: 1200,
            idle_ms: 250,
        };
        let wire = req.encode();
        assert!(wire.contains("x=540"));
        assert!(wire.contains("y=1200"));
        assert!(wire.contains("idle_ms=250"));
    }

    #[test]
    fn type_and_wait_encode_quotes_both_strings() {
        let req = TypeAndWait {
            text: "user@example.com".into(),
            wait_for: "Welcome".into(),
            timeout: Duration::from_secs(5),
        };
        let wire = req.encode();
        assert!(wire.contains("text=\"user@example.com\""));
        assert!(wire.contains("wait_for=\"Welcome\""));
        assert!(wire.contains("timeout_ms=5000"));
    }

    #[test]
    fn escape_kv_handles_special_chars() {
        assert_eq!(escape_kv("hello"), "hello");
        assert_eq!(escape_kv("a\"b"), "a\\\"b");
        assert_eq!(escape_kv("a\\b"), "a\\\\b");
        assert_eq!(escape_kv("a\nb"), "a\\nb");
        assert_eq!(escape_kv("a\tb"), "a\\tb");
    }

    #[test]
    fn atomic_result_is_ok() {
        let r = AtomicResult {
            matched: true,
            anchor: None,
            observation: None,
            timings: AtomicTimings::default(),
            error: None,
        };
        assert!(r.is_ok());
    }

    #[test]
    fn atomic_result_is_err() {
        let r = AtomicResult {
            matched: false,
            anchor: None,
            observation: None,
            timings: AtomicTimings::default(),
            error: Some("AMBIGUOUS".into()),
        };
        assert!(!r.is_ok());
    }
}
