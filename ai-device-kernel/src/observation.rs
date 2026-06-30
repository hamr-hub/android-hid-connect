//! Observation stream + device event types — see v3 §3.2.3.
//!
//! The kernel maintains a single observation stream per session;
//! multiple subscribers receive the same sequence of `Observation`s via
//! a server-push model. `seq` is strictly monotonic across the
//! daemon's lifetime and acts as the observe-then-act race guard
//! (see v3 §3.2.3 "关键设计").
//!
//! ## Design
//!
//! - **`Observation`** = one snapshot pushed by the daemon. Carries an
//!   optional a11y tree, optional frame, current device state, and
//!   the events that fired since the previous observation.
//! - **`DeviceEvent`** = one lifecycle / system event. 10 variants per
//!   v3 §3.2.3.
//! - **`DeviceState`** = current device-state bits (focused activity,
//!   foreground window, uptime, screen-on/off).
//!
//! All types are `Serialize + Deserialize` so the wire layer (see
//! [`crate::protocol`]) can use postcard without bespoke glue.

use serde::{Deserialize, Serialize};

use crate::ids::{ActionId, PlanId};

/// One observation snapshot, server-pushed to subscribers.
///
/// `seq` is strictly monotonic; `observe(since_seq=N)` returns all
/// observations with `seq > N` in causal order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Monotonic sequence number. Strictly increasing across the
    /// daemon's lifetime; clients use it to fetch
    /// "everything since N" without dropping or duplicating.
    pub seq: u64,
    /// Wall-clock ms since daemon start (not Unix time — keeps
    /// clock-skew out of the picture).
    pub timestamp_ms: u64,
    /// Latest a11y tree snapshot, if one was sampled (controlled by
    /// the observation request filter).
    pub a11y: Option<A11yTree>,
    /// Latest frame snapshot (H.265 keyframe IDR or JPEG tile), if
    /// one was sampled.
    pub frame: Option<FrameSnapshot>,
    /// Current device state.
    pub state: DeviceState,
    /// Events that fired since the previous observation (or since
    /// subscription start, for the first frame).
    pub events: Vec<DeviceEvent>,
}

impl Observation {
    /// True if no payload other than `seq` + `events` is present
    /// (i.e. cheap header-only push).
    #[inline]
    #[must_use]
    pub fn is_header_only(&self) -> bool {
        self.a11y.is_none() && self.frame.is_none()
    }

    /// Number of subscribers' attention hooks in this observation.
    /// Used for sanity-checking event ordering.
    #[inline]
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

/// A11y tree snapshot. The kernel records both the JSON-equivalent
/// shape (for LLM-friendly prompts) and the typed children for fast
/// selector resolution on subsequent actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A11yTree {
    /// Active window id (matches `DeviceEvent::WindowFocusChanged`).
    pub window_id: Option<u32>,
    /// Top activity string (`com.example/.MainActivity`).
    pub top_activity: Option<String>,
    /// Number of nodes in the tree (cheap for size budgeting).
    pub node_count: u32,
    /// JSON-equivalent blob of the tree (matches
    /// `android-hid-daemon`'s `dump_active` output; lets LLM agents
    /// reuse handsets-compatible parsers).
    pub json: String,
}

/// One frame snapshot for the LLM-vision path.
///
/// Note: `Eq` is *not* derived because `scene_change_score` is an
/// `f32` and `Eq` isn't implemented for floats.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameSnapshot {
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Codec id (0 = H.264, 1 = H.265, 2 = JPEG tile).
    pub codec: u8,
    /// Is this a keyframe (decoder-restart point)?
    pub is_keyframe: bool,
    /// Presentation timestamp in 90 kHz MPEG-TS units.
    pub pts: u64,
    /// Best-effort scene-change score [0, 1] (server-computed).
    pub scene_change_score: f32,
}

/// Current device state bits — cheap to copy, snapshotted on every
/// observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    /// Foreground window id (`0` if unknown).
    pub focused_window: u32,
    /// Currently focused a11y node id (`u32::MAX` if none).
    pub focused_node: u32,
    /// Screen is on (true) or off / locked (false).
    pub screen_on: bool,
    /// Daemon uptime in ms since the kernel started.
    pub uptime_ms: u64,
}

impl DeviceState {
    /// Default-initialised state used when the daemon hasn't yet
    /// sampled a snapshot.
    #[must_use]
    pub const fn unknown(uptime_ms: u64) -> Self {
        Self {
            focused_window: 0,
            focused_node: u32::MAX,
            screen_on: true,
            uptime_ms,
        }
    }
}

/// Device-event taxonomy — v3 §3.2.3 lists 10 variants. We map them
/// 1:1 to `DeviceEvent` so agent-side filters can subscribe by
/// `EventKind` bitfield.
///
/// Note: `Eq` and `Hash` are *not* derived because some variants
/// carry `f32` payloads (e.g. `SceneChangeDetected { score }`)
/// which don't implement `Eq`/`Hash`. Use [`Self::kind_label`] for
/// map-keys on the agent side instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DeviceEvent {
    /// Activity resumed (foreground navigation target).
    ActivityResumed {
        /// `pkg/.Activity` form.
        component: String,
    },
    /// Activity paused (lost foreground).
    ActivityPaused {
        /// `pkg/.Activity` form.
        component: String,
    },
    /// Window focus changed (a11y event).
    WindowFocusChanged {
        /// New focused window id.
        window_id: u32,
    },
    /// Package added (install or system update finished).
    PackageAdded {
        /// Package name.
        pkg: String,
    },
    /// Package removed (uninstall).
    PackageRemoved {
        /// Package name.
        pkg: String,
    },
    /// Configuration changed (orientation, locale, density, …).
    ConfigurationChanged,
    /// Scene change detected (frame-diff score above threshold).
    SceneChangeDetected {
        /// Score in `[0, 1]`.
        score: f32,
    },
    /// Notification posted (system tray entry).
    NotificationPosted {
        /// Notification key (`pkg:tag` typically).
        key: String,
    },
    /// Clipboard changed.
    ClipboardChanged,
    /// Action completed (cross-reference into `ActionId`).
    ActionCompleted {
        /// Which action produced the event.
        action_id: ActionId,
        /// Did the action land successfully?
        landed: bool,
        /// Server-measured elapsed ms.
        elapsed_ms: u32,
    },
    /// Plan completed.
    PlanCompleted {
        /// Which plan produced the event.
        plan_id: PlanId,
        /// All steps landed? (else plan aborted on first failure).
        all_landed: bool,
        /// Server-measured elapsed ms.
        elapsed_ms: u32,
    },
    /// Daemon heartbeat — every 1 s when no other events. Not a
    /// "device" event per se; included so idle subscribers don't
    /// starve.
    Uptime {
        /// Daemon uptime in ms.
        uptime_ms: u64,
    },
}

impl DeviceEvent {
    /// Short stable label suitable for log lines and metrics tags.
    /// Mirrors the `EventKind` name space so host-side filters can
    /// `format!("{kind:?}")` and match.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::ActivityResumed { .. } => "activity-resumed",
            Self::ActivityPaused { .. } => "activity-paused",
            Self::WindowFocusChanged { .. } => "window-focus-changed",
            Self::PackageAdded { .. } => "package-added",
            Self::PackageRemoved { .. } => "package-removed",
            Self::ConfigurationChanged => "configuration-changed",
            Self::SceneChangeDetected { .. } => "scene-change-detected",
            Self::NotificationPosted { .. } => "notification-posted",
            Self::ClipboardChanged => "clipboard-changed",
            Self::ActionCompleted { .. } => "action-completed",
            Self::PlanCompleted { .. } => "plan-completed",
            Self::Uptime { .. } => "uptime",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_is_header_only_when_no_payloads() {
        let obs = Observation {
            seq: 42,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        };
        assert!(obs.is_header_only());
        assert_eq!(obs.event_count(), 0);
    }

    #[test]
    fn observation_is_header_only_false_when_a11y_present() {
        let obs = Observation {
            seq: 1,
            timestamp_ms: 0,
            a11y: Some(A11yTree {
                window_id: Some(7),
                top_activity: Some("com.foo/.Main".into()),
                node_count: 100,
                json: "[]".into(),
            }),
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        };
        assert!(!obs.is_header_only());
    }

    #[test]
    fn device_event_labels_are_distinct() {
        // Each variant maps to a unique kind label.
        let events = [
            DeviceEvent::ActivityResumed { component: "x".into() },
            DeviceEvent::ActivityPaused { component: "x".into() },
            DeviceEvent::WindowFocusChanged { window_id: 0 },
            DeviceEvent::PackageAdded { pkg: "x".into() },
            DeviceEvent::PackageRemoved { pkg: "x".into() },
            DeviceEvent::ConfigurationChanged,
            DeviceEvent::SceneChangeDetected { score: 0.5 },
            DeviceEvent::NotificationPosted { key: "x".into() },
            DeviceEvent::ClipboardChanged,
            DeviceEvent::ActionCompleted {
                action_id: ActionId(1),
                landed: true,
                elapsed_ms: 10,
            },
            DeviceEvent::PlanCompleted {
                plan_id: PlanId(1),
                all_landed: true,
                elapsed_ms: 100,
            },
            DeviceEvent::Uptime { uptime_ms: 0 },
        ];
        let labels: std::collections::HashSet<_> =
            events.iter().map(DeviceEvent::kind_label).collect();
        assert_eq!(
            labels.len(),
            events.len(),
            "duplicate event kind labels"
        );
    }

    #[test]
    fn device_state_unknown_is_deterministic() {
        let a = DeviceState::unknown(100);
        let b = DeviceState::unknown(100);
        assert_eq!(a, b);
        assert_eq!(a.uptime_ms, 100);
        assert!(a.screen_on);
        assert_eq!(a.focused_node, u32::MAX);
    }

    #[test]
    fn frame_snapshot_round_trips_compactly() {
        // postcard uses varint and skips trailing zeros; the exact
        // wire size depends on `pts` and `scene_change_score`. We
        // pin a *ceiling* (≤ 18 B) and assert the round-trip
        // instead of the literal length.
        let f = FrameSnapshot {
            width: 1080,
            height: 1920,
            codec: 1, // H.265
            is_keyframe: true,
            pts: 90_000,
            scene_change_score: 0.42,
        };
        let bytes = postcard::to_allocvec(&f).expect("encode frame");
        // Worst case: 2 + 2 + 1 + 1 + varint(pts) + 4 (f32)
        // pts=90_000 = 0x015F90 fits in 3 varint bytes (≤5).
        assert!(bytes.len() <= 18, "unexpectedly large: {}", bytes.len());
        assert!(bytes.len() >= 10, "unexpectedly tiny: {}", bytes.len());
        assert_eq!(postcard::from_bytes::<FrameSnapshot>(&bytes).unwrap(), f);
    }

    #[test]
    fn observation_postcard_round_trip() {
        let obs = Observation {
            seq: 7,
            timestamp_ms: 123,
            a11y: Some(A11yTree {
                window_id: Some(1),
                top_activity: Some("com.foo/.Bar".into()),
                node_count: 50,
                json: "[{\"id\":1}]".into(),
            }),
            frame: None,
            state: DeviceState::unknown(123),
            events: vec![
                DeviceEvent::ActivityResumed { component: "com.foo/.Bar".into() },
                DeviceEvent::SceneChangeDetected { score: 0.9 },
            ],
        };
        let bytes = postcard::to_allocvec(&obs).expect("encode observation");
        let decoded: Observation =
            postcard::from_bytes(&bytes).expect("decode observation");
        assert_eq!(decoded, obs);
        assert_eq!(decoded.event_count(), 2);
    }
}
