//! Typed `Action` surface — see v3 §3.2.1 and §3.4.
//!
//! The 12 typed actions are the public, LLM-facing input vocabulary.
//! They replace the legacy daemon's 70+ `Verb` enum at the agent
//! boundary; the underlying daemon-side capability registry
//! ([`crate::capability`]) still dispatches to the original 60+
//! verbs internally for backwards compatibility (see v3 §3.5).
//!
//! ## Key design
//!
//! - **Each `Action` carries a `deadline_ms`** — the daemon gives up
//!   after that many ms even if predicates or settle intervals
//!   haven't fired (P10 partial mitigation).
//! - **Every `Action` returns `ActionResult { landed, ground_truth,
//!   elapsed_ms }`** — ground truth in 1 RTT (P3 mitigation,
//!   saves a follow-up `dump_active` round-trip).
//! - **`Action::capabilities()`** — which underlying capability
//!   names are involved. Used by the daemon to gate execution
//!   (e.g. a capability profile of "phantom" can refuse
//!   `Launch`-flavoured actions).
//! - **Escape hatch** — `Action::InjectRaw` lets the agent bypass
//!   the typed surface for gamepad / canvas / WebView paths that
//!   the typed enum can't reach (v3 P0 fix, see v3 §6.2).

use serde::{Deserialize, Serialize};

use crate::ids::{ActionId, ScreenId};
use crate::observation::{DeviceEvent, FrameSnapshot};

/// Launch target specifier — used by [`Action::Launch`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LaunchBy {
    /// Component name (`pkg/.Activity`).
    Component(String),
    /// Package name (any activity resolves).
    Package(String),
    /// Action + data URI (matches Android intent system).
    Intent {
        /// Intent action (`android.intent.action.VIEW` etc.).
        action: String,
        /// Optional data URI.
        data: Option<String>,
        /// Optional MIME type.
        mime_type: Option<String>,
    },
}

/// A sub-recipe of an observation — used by [`Action::DumpObservation`]
/// to select which payload bits the daemon should sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObservationComponent {
    /// Include a11y tree snapshot.
    A11y,
    /// Include frame snapshot (H.265 keyframe or JPEG tile).
    Frame,
    /// Include current device state.
    State,
    /// Include events since last observation.
    Events,
    /// Force a keyframe on the H.265 stream (forcing a snapshot).
    ForceKeyframe,
}

/// Typed action surface — 12 variants per v3 §3.2.1 / §3.8.
///
/// Every variant carries a `deadline_ms` so the daemon always knows
/// when to stop. Pure-read variants (`Wait`, `GetUiRepr`,
/// `DumpObservation`) use it as a "stop sampling after this".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    /// Tap at `(x, y)` in screen pixels. No 16 ms sleep, no fake
    /// delay (v3 P4 mitigation).
    Tap {
        /// Screen x coordinate (pixels).
        x: i32,
        /// Screen y coordinate (pixels).
        y: i32,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Tap on a11y node matched by selector. Daemon resolves the
    /// selector on the latest a11y snapshot, taps center.
    TapSelector {
        /// CSS-like selector (`EditText[hint~=Email]`).
        selector: String,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Type ASCII text into the focused field. Daemon dispatches
    /// per character via UHID or `cmd ime` depending on the focused
    /// field type.
    TypeText {
        /// Text to type.
        text: String,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Press Android key code (`KEYCODE_HOME`, `KEYCODE_BACK`, …).
    Key {
        /// Android `KeyEvent.KEYCODE_*` constant.
        code: u32,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Swipe from `(x1, y1)` to `(x2, y2)`.
    Swipe {
        /// Start x (pixels).
        x1: i32,
        /// Start y (pixels).
        y1: i32,
        /// End x (pixels).
        x2: i32,
        /// End y (pixels).
        y2: i32,
        /// Swipe duration in ms (real time, not artificial delay).
        dur_ms: u32,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Gamepad input — full 15-byte HID report (matches the
    /// `GamepadFrameRaw` shape from `android-hid-connect`).
    GamepadFrame {
        /// 15-byte HID report (see `android-hid-connect`'s
        /// `GamepadHid::report` builder).
        report: [u8; 15],
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Launch by component, package, or intent.
    Launch {
        /// Target specifier.
        target: String,
        /// Launch spec kind.
        by: LaunchBy,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Set clipboard with optional paste injection.
    SetClipboard {
        /// Text to put on the clipboard.
        text: String,
        /// Whether to trigger paste after setting.
        paste: bool,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Wait until a [`crate::predicate::Predicate`] resolves.
    /// Standalone: useful as the only step in an otherwise
    /// observation-only plan.
    Wait {
        /// Predicate to wait on.
        predicate: crate::predicate::Predicate,
        /// Daemon-side deadline (advisory; the predicate itself
        /// has a separate `timeout_ms`).
        deadline_ms: u32,
    },
    /// Return a functionality-aware HTML-tagged UI representation
    /// (`UiReprHtml`) for the current screen — AutoDroid-style
    /// ~500 B payload, see v3 §3.8.
    GetUiRepr {
        /// Optional screen-id hint: if known, the daemon can
        /// return the cached representation without re-sampling.
        screen_id: Option<ScreenId>,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Sample one observation snapshot (subset of sub-payloads).
    DumpObservation {
        /// Which sub-payloads to include.
        components: Vec<ObservationComponent>,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
    /// Inject raw UHID bytes — escape hatch for game / canvas /
    /// WebView scenarios the typed enum can't reach (v3 P0 fix).
    InjectRaw {
        /// Raw bytes to push onto the UHID descriptor.
        bytes: Vec<u8>,
        /// Daemon-side deadline.
        deadline_ms: u32,
    },
}

impl Action {
    /// `deadline_ms` regardless of variant.
    #[must_use]
    pub const fn deadline_ms(&self) -> u32 {
        match self {
            Self::Tap { deadline_ms, .. }
            | Self::TapSelector { deadline_ms, .. }
            | Self::TypeText { deadline_ms, .. }
            | Self::Key { deadline_ms, .. }
            | Self::Swipe { deadline_ms, .. }
            | Self::GamepadFrame { deadline_ms, .. }
            | Self::Launch { deadline_ms, .. }
            | Self::SetClipboard { deadline_ms, .. }
            | Self::Wait { deadline_ms, .. }
            | Self::GetUiRepr { deadline_ms, .. }
            | Self::DumpObservation { deadline_ms, .. }
            | Self::InjectRaw { deadline_ms, .. } => *deadline_ms,
        }
    }

    /// Short stable label suitable for log lines and metrics tags.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Tap { .. } => "tap",
            Self::TapSelector { .. } => "tap-selector",
            Self::TypeText { .. } => "type-text",
            Self::Key { .. } => "key",
            Self::Swipe { .. } => "swipe",
            Self::GamepadFrame { .. } => "gamepad-frame",
            Self::Launch { .. } => "launch",
            Self::SetClipboard { .. } => "set-clipboard",
            Self::Wait { .. } => "wait",
            Self::GetUiRepr { .. } => "get-ui-repr",
            Self::DumpObservation { .. } => "dump-observation",
            Self::InjectRaw { .. } => "inject-raw",
        }
    }

    /// Which underlying capabilities this action invokes. The
    /// daemon-side capability registry ([`crate::capability`])
    /// routes by these names; the typed enum is purely a
    /// LLM-facing convenience.
    ///
    /// `VEC` is the allocation-free path of last resort; for hot
    /// paths (240 Hz gamepad) the daemon should use `kind_label()`
    /// plus a custom fast dispatch instead.
    #[must_use]
    pub fn capabilities(&self) -> Vec<&'static str> {
        match self {
            Self::Tap { .. } => vec!["input.motion_event"],
            Self::TapSelector { .. } => {
                vec!["a11y.resolve", "input.motion_event"]
            }
            Self::TypeText { .. } => vec!["input.key_event", "shell.ime"],
            Self::Key { .. } => vec!["input.key_event"],
            Self::Swipe { .. } => vec!["input.motion_event"],
            Self::GamepadFrame { .. } | Self::InjectRaw { .. } => {
                vec!["uhid.inject"]
            }
            Self::Launch { .. } => vec!["pm.start_activity"],
            Self::SetClipboard { .. } => vec!["clipboard.set", "shell.ime"],
            Self::Wait { .. } => vec!["predicate.wait"],
            Self::GetUiRepr { .. } | Self::DumpObservation { .. } => {
                vec!["a11y.observe", "frame.observe"]
            }
        }
    }
}

/// Result of a single `Action` execution. Returned in the wire reply
/// (see [`crate::protocol`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    /// Server-assigned unique ID. Idempotent-replay safe: a retried
    /// `Action` carrying the same `id` returns the same result.
    pub id: ActionId,
    /// Did the action actually land? For pure-read actions, `true`
    /// means "sampling succeeded".
    pub landed: bool,
    /// Ground truth — what actually changed on device.
    pub ground_truth: GroundTruth,
    /// Server-measured elapsed ms (does not include network RTT;
    /// see v3 §3.2.1 "How long (server-measured)").
    pub elapsed_ms: u32,
}

impl ActionResult {
    /// Convenience — return `true` if the action landed and the
    /// ground truth is non-empty (a11y diff, frame diff, or
    /// events).
    #[must_use]
    pub fn has_ground_truth(&self) -> bool {
        self.landed
            && (!self.ground_truth.a11y_diff.is_empty()
                || self.ground_truth.frame_diff.is_some()
                || !self.ground_truth.events.is_empty())
    }
}

/// Ground truth — what actually changed on the device after the
/// `Action` ran. Returned in 1 RTT (no follow-up `dump_active`
/// needed). See v3 §3.2.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GroundTruth {
    /// a11y nodes that changed (added / removed / text-changed /
    /// visibility-changed) since the action landed.
    pub a11y_diff: Vec<A11yNodeDiff>,
    /// Frame diff summary, if the daemon sampled a frame.
    pub frame_diff: Option<FrameDiff>,
    /// New focused window id, if it changed.
    pub focus: Option<u32>,
    /// Scene change score `[0, 1]`.
    pub scene_change: f32,
    /// Events that fired between the action start and the
    /// observation snapshot.
    pub events: Vec<DeviceEvent>,
}

/// Per-node a11y change — see v3 §3.2.1 `a11y_diff`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A11yNodeDiff {
    /// Affected node id. Stable for the lifetime of one dump.
    pub node_id: u32,
    /// What kind of change.
    pub kind: A11yNodeChangeKind,
    /// New text content (only set when `kind` is text-changed).
    pub new_text: Option<String>,
    /// New visibility flag (only set when `kind` is visibility).
    pub new_visible: Option<bool>,
}

/// Diff kind for a single a11y node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum A11yNodeChangeKind {
    Added,
    Removed,
    TextChanged,
    VisibilityChanged,
    BoundsChanged,
}

/// Frame diff summary — see v3 §3.2.1 `frame_diff`. Lightweight
/// enough to return in every action result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameDiff {
    /// Total pixel-difference score across the whole frame, `[0, 1]`.
    pub total: f32,
    /// Per-region pixel-difference score (top-left, top-right,
    /// bottom-left, bottom-right). 4 cells — cheap to ship.
    pub regions: [f32; 4],
    /// Compact scene-change score `[0, 1]` (alias for
    /// `GroundTruth::scene_change`).
    pub scene_change: f32,
    /// Reference to the source frame snapshot.
    pub source: FrameSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ActionId;
    use crate::observation::DeviceState;

    #[test]
    fn action_deadline_ms_extractor() {
        // Spot-check one variant per group; the others share the
        // same shape via the const `* { deadline_ms, .. }` matches.
        let actions = [
            (
                Action::Tap {
                    x: 100,
                    y: 200,
                    deadline_ms: 500,
                },
                500u32,
            ),
            (
                Action::TapSelector {
                    selector: "Button".into(),
                    deadline_ms: 1500,
                },
                1500,
            ),
            (
                Action::Swipe {
                    x1: 0,
                    y1: 0,
                    x2: 1080,
                    y2: 1920,
                    dur_ms: 250,
                    deadline_ms: 1000,
                },
                1000,
            ),
            (
                Action::GamepadFrame {
                    report: [0u8; 15],
                    deadline_ms: 16,
                },
                16,
            ),
            (
                Action::Wait {
                    predicate:
                        crate::predicate::Predicate::SelectorMatches {
                            selector: "x".into(),
                            timeout_ms: 0,
                        },
                    deadline_ms: 3000,
                },
                3000,
            ),
            (
                Action::GetUiRepr {
                    screen_id: None,
                    deadline_ms: 200,
                },
                200,
            ),
        ];
        for (action, expected) in actions {
            assert_eq!(
                action.deadline_ms(),
                expected,
                "deadline mismatch for {}",
                action.kind_label()
            );
        }
    }

    #[test]
    fn action_kind_labels_are_distinct() {
        let labels = [
            Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 0,
            }
            .kind_label(),
            Action::TapSelector {
                selector: "x".into(),
                deadline_ms: 0,
            }
            .kind_label(),
            Action::TypeText {
                text: "x".into(),
                deadline_ms: 0,
            }
            .kind_label(),
            Action::Key {
                code: 0,
                deadline_ms: 0,
            }
            .kind_label(),
            Action::Swipe {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
                dur_ms: 0,
                deadline_ms: 0,
            }
            .kind_label(),
            Action::GamepadFrame {
                report: [0u8; 15],
                deadline_ms: 0,
            }
            .kind_label(),
            Action::Launch {
                target: "x".into(),
                by: LaunchBy::Package("x".into()),
                deadline_ms: 0,
            }
            .kind_label(),
            Action::SetClipboard {
                text: "x".into(),
                paste: false,
                deadline_ms: 0,
            }
            .kind_label(),
            Action::Wait {
                predicate: crate::predicate::Predicate::Activity {
                    component: "x".into(),
                    timeout_ms: 0,
                },
                deadline_ms: 0,
            }
            .kind_label(),
            Action::GetUiRepr {
                screen_id: None,
                deadline_ms: 0,
            }
            .kind_label(),
            Action::DumpObservation {
                components: vec![ObservationComponent::A11y],
                deadline_ms: 0,
            }
            .kind_label(),
            Action::InjectRaw {
                bytes: vec![],
                deadline_ms: 0,
            }
            .kind_label(),
        ];
        let unique: std::collections::HashSet<_> =
            labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len(), "duplicate kind labels");
    }

    #[test]
    fn action_capabilities_are_non_empty_and_consistent() {
        // Each action maps to at least one underlying capability
        // and `capabilities()` returns the same list twice.
        let actions = [
            Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 100,
            },
            Action::TapSelector {
                selector: "Button".into(),
                deadline_ms: 100,
            },
            Action::TypeText {
                text: "x".into(),
                deadline_ms: 100,
            },
            Action::Key {
                code: 0,
                deadline_ms: 100,
            },
            Action::Swipe {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
                dur_ms: 0,
                deadline_ms: 100,
            },
            Action::GamepadFrame {
                report: [0u8; 15],
                deadline_ms: 16,
            },
            Action::Launch {
                target: "x".into(),
                by: LaunchBy::Package("x".into()),
                deadline_ms: 100,
            },
            Action::SetClipboard {
                text: "x".into(),
                paste: false,
                deadline_ms: 100,
            },
            Action::Wait {
                predicate: crate::predicate::Predicate::SelectorMatches {
                    selector: "x".into(),
                    timeout_ms: 0,
                },
                deadline_ms: 0,
            },
            Action::GetUiRepr {
                screen_id: None,
                deadline_ms: 100,
            },
            Action::DumpObservation {
                components: vec![ObservationComponent::A11y],
                deadline_ms: 100,
            },
            Action::InjectRaw {
                bytes: vec![],
                deadline_ms: 0,
            },
        ];
        for action in &actions {
            let caps = action.capabilities();
            assert!(!caps.is_empty(), "no capabilities for {}", action.kind_label());
            // Second call returns the same set (order-preserving).
            let caps2 = action.capabilities();
            assert_eq!(caps, caps2, "non-deterministic capabilities");
        }
    }

    #[test]
    fn gamepad_actions_route_to_uhid_inject() {
        assert_eq!(
            Action::GamepadFrame {
                report: [0u8; 15],
                deadline_ms: 0,
            }
            .capabilities(),
            vec!["uhid.inject"],
        );
        assert_eq!(
            Action::InjectRaw {
                bytes: vec![],
                deadline_ms: 0,
            }
            .capabilities(),
            vec!["uhid.inject"],
        );
    }

    #[test]
    fn tap_selector_requires_a11y_resolution() {
        // v3 P0 fix: typed-enum tap MUST go through the a11y
        // resolver first; otherwise it's an opaque (x,y) tap.
        let action = Action::TapSelector {
            selector: "Button".into(),
            deadline_ms: 0,
        };
        let caps = action.capabilities();
        assert!(caps.contains(&"a11y.resolve"));
        assert!(caps.contains(&"input.motion_event"));
    }

    #[test]
    fn action_result_has_ground_truth_only_when_landed() {
        let mut ar = ActionResult {
            id: ActionId(1),
            landed: false,
            ground_truth: GroundTruth {
                a11y_diff: vec![A11yNodeDiff {
                    node_id: 1,
                    kind: A11yNodeChangeKind::Added,
                    new_text: None,
                    new_visible: None,
                }],
                ..Default::default()
            },
            elapsed_ms: 1,
        };
        assert!(!ar.has_ground_truth(), "landed=false → no ground truth flag");
        ar.landed = true;
        assert!(ar.has_ground_truth(), "landed=true + non-empty diff → true");
    }

    #[test]
    fn action_postcard_round_trip() {
        let action = Action::TapSelector {
            selector: "Button[id=login]".into(),
            deadline_ms: 1500,
        };
        let bytes = postcard::to_allocvec(&action).expect("encode action");
        let decoded: Action =
            postcard::from_bytes(&bytes).expect("decode action");
        assert_eq!(decoded, action);
    }

    #[test]
    fn action_result_postcard_round_trip() {
        let result = ActionResult {
            id: ActionId(99),
            landed: true,
            ground_truth: GroundTruth {
                a11y_diff: vec![],
                frame_diff: None,
                focus: Some(42),
                scene_change: 0.7,
                events: vec![DeviceEvent::WindowFocusChanged { window_id: 42 }],
            },
            elapsed_ms: 12,
        };
        let bytes = postcard::to_allocvec(&result).expect("encode");
        let decoded: ActionResult =
            postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn ground_truth_default_is_empty() {
        let gt = GroundTruth::default();
        assert!(gt.a11y_diff.is_empty());
        assert!(gt.frame_diff.is_none());
        assert!(gt.focus.is_none());
        assert_eq!(gt.scene_change, 0.0);
        assert!(gt.events.is_empty());
    }

    #[test]
    fn launch_by_equality() {
        assert_eq!(
            LaunchBy::Component("p/.Main".into()),
            LaunchBy::Component("p/.Main".into()),
        );
        assert_ne!(
            LaunchBy::Component("p/.Main".into()),
            LaunchBy::Package("p".into()),
        );
    }

    #[test]
    fn observation_component_distinct() {
        use std::collections::HashSet;
        let all = [
            ObservationComponent::A11y,
            ObservationComponent::Frame,
            ObservationComponent::State,
            ObservationComponent::Events,
            ObservationComponent::ForceKeyframe,
        ];
        // All distinct discriminant → set size == array size.
        let ser: HashSet<_> = all
            .iter()
            .map(|c| postcard::to_allocvec(c).unwrap())
            .collect();
        assert_eq!(ser.len(), all.len());
    }

    #[test]
    fn frame_diff_carries_source() {
        let src = FrameSnapshot {
            width: 1080,
            height: 1920,
            codec: 1,
            is_keyframe: true,
            pts: 90000,
            scene_change_score: 0.5,
        };
        let diff = FrameDiff {
            total: 0.4,
            regions: [0.1, 0.2, 0.3, 0.4],
            scene_change: 0.5,
            source: src,
        };
        let bytes = postcard::to_allocvec(&diff).expect("encode");
        let decoded: FrameDiff =
            postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, diff);
    }

    #[test]
    fn state_compiles_in_dependency_graph() {
        // Touch the `DeviceState` re-export via the observation
        // module path to confirm `action.rs` only depends on what
        // the imports actually need.
        let _ = DeviceState::unknown(0);
    }
}
