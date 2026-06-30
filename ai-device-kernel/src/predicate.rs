//! Predicate engine types — see v3 §3.2.4.
//!
//! The kernel's predicate engine lets a host register a *declarative* condition
//! and only resumes the agent when the condition is satisfied (or a timeout
//! fires). This replaces the 16 ms tap sleep + the "agent polls the daemon
//! asking `dump_active` until something changes" anti-pattern that the legacy
//! daemon forced on callers.
//!
//! ## Key design
//!
//! - **6 variants** matching v3 §3.2.4: `TextAppears`, `Activity`,
//!   `SceneStable`, `A11yIdle`, `SelectorMatches`, `EventFires`.
//! - **`timeout_ms` on every variant** — the deadline is a *backstop*,
//!   not the usual waiting mechanism. Most predicates resolve via the
//!   event-loop wakeup; the timeout only fires when the engine
//!   genuinely stalls.
//! - **No polling**: predicates are checked on every relevant event,
//!   then on a fixed 50 ms heartbeat for sub-event liveness — see
//!   v3 §3.2.4 "0 polling, 0 CPU 浪费 (P4 间接解)".

use serde::{Deserialize, Serialize};

use crate::observation::DeviceEvent;

/// Identifier for a registered predicate. Returned by
/// `Protocol::register_predicate` so the host can later cancel or
/// differentiate completions when multiple predicates are in flight.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct PredicateHandle(pub u64);

/// Predicate taxonomy — v3 §3.2.4 (6 variants). Each predicate
/// carries a `timeout_ms` (the deadline after which the predicate
/// engine gives up and returns `PredicateResult::Timeout`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Predicate {
    /// Wait until `text` appears in any a11y node's text content.
    TextAppears {
        /// The text to look for. Exact (case-sensitive) match.
        text: String,
        /// Optional a11y node-id hint to scope the search.
        node_id: Option<u32>,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
    /// Wait until activity `component` is the foreground activity.
    Activity {
        /// `pkg/.Activity` form.
        component: String,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
    /// Wait until the frame has been visually stable (no
    /// `SceneChangeDetected` event above threshold) for
    /// `duration_ms` consecutive ms.
    SceneStable {
        /// Required stable duration in ms.
        duration_ms: u32,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
    /// Wait until the a11y tree has been idle (no
    /// `A11yNodeDiff` event) for `duration_ms` consecutive ms.
    A11yIdle {
        /// Required idle duration in ms.
        duration_ms: u32,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
    /// Wait until an a11y node matching `selector` is present.
    /// `selector` is a handsets-style CSS-like selector string.
    SelectorMatches {
        /// Selector string (delegated to the kernel's
        /// selector engine, which is compatible with the
        /// `android-hid-agent::Selector` parser).
        selector: String,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
    /// Wait until a device event matching `kind` fires.
    EventFires {
        /// Which device-event kind to wait for.
        kind: EventKind,
        /// Backstop deadline in ms.
        timeout_ms: u32,
    },
}

impl Predicate {
    /// The `timeout_ms` regardless of variant. Convenience for the
    /// engine's bookkeeping.
    #[must_use]
    pub const fn timeout_ms(&self) -> u32 {
        match self {
            Self::TextAppears { timeout_ms, .. }
            | Self::Activity { timeout_ms, .. }
            | Self::SceneStable { timeout_ms, .. }
            | Self::A11yIdle { timeout_ms, .. }
            | Self::SelectorMatches { timeout_ms, .. }
            | Self::EventFires { timeout_ms, .. } => *timeout_ms,
        }
    }

    /// Short stable label suitable for predicate-engine log lines.
    /// Mirrors the `EventKind` name space where possible.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::TextAppears { .. } => "text-appears",
            Self::Activity { .. } => "activity",
            Self::SceneStable { .. } => "scene-stable",
            Self::A11yIdle { .. } => "a11y-idle",
            Self::SelectorMatches { .. } => "selector-matches",
            Self::EventFires { .. } => "event-fires",
        }
    }
}

/// Subset of `DeviceEvent` that can be used as a predicate trigger
/// (`EventFires`). Mirrors the v3 design's `EventKind` filter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    ActivityResumed,
    ActivityPaused,
    WindowFocusChanged,
    PackageAdded,
    PackageRemoved,
    ConfigurationChanged,
    SceneChangeDetected,
    NotificationPosted,
    ClipboardChanged,
}

impl EventKind {
    /// Translate a `DeviceEvent` variant into its `EventKind` (if
    /// the variant is filterable). `None` for events that don't
    /// appear in [`EventKind`] (lifecycle / heartbeat).
    #[must_use]
    pub fn from_event(event: &DeviceEvent) -> Option<Self> {
        match event {
            DeviceEvent::ActivityResumed { .. } => Some(Self::ActivityResumed),
            DeviceEvent::ActivityPaused { .. } => Some(Self::ActivityPaused),
            DeviceEvent::WindowFocusChanged { .. } => Some(Self::WindowFocusChanged),
            DeviceEvent::PackageAdded { .. } => Some(Self::PackageAdded),
            DeviceEvent::PackageRemoved { .. } => Some(Self::PackageRemoved),
            DeviceEvent::ConfigurationChanged => Some(Self::ConfigurationChanged),
            DeviceEvent::SceneChangeDetected { .. } => Some(Self::SceneChangeDetected),
            DeviceEvent::NotificationPosted { .. } => Some(Self::NotificationPosted),
            DeviceEvent::ClipboardChanged => Some(Self::ClipboardChanged),
            DeviceEvent::ActionCompleted { .. }
            | DeviceEvent::PlanCompleted { .. }
            | DeviceEvent::Uptime { .. } => None,
        }
    }

    /// Short stable label for tracing.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::ActivityResumed => "activity-resumed",
            Self::ActivityPaused => "activity-paused",
            Self::WindowFocusChanged => "window-focus-changed",
            Self::PackageAdded => "package-added",
            Self::PackageRemoved => "package-removed",
            Self::ConfigurationChanged => "configuration-changed",
            Self::SceneChangeDetected => "scene-change-detected",
            Self::NotificationPosted => "notification-posted",
            Self::ClipboardChanged => "clipboard-changed",
        }
    }
}

/// The result of a predicate wait. Returned on the wire after the
/// predicate resolves, regardless of whether it succeeded or
/// timed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateResult {
    /// Predicate matched within the deadline.
    Matched {
        /// Predicate handle that resolved.
        handle: PredicateHandle,
        /// Wall-clock elapsed ms.
        elapsed_ms: u32,
    },
    /// Backstop timeout fired before the predicate could match.
    Timeout {
        /// Predicate handle that expired.
        handle: PredicateHandle,
    },
    /// Predicate was explicitly cancelled by the host (e.g.
    /// `protocol.cancel_predicate(handle)`).
    Cancelled {
        /// Predicate handle that was cancelled.
        handle: PredicateHandle,
    },
    /// Predicate ID was unknown (already expired or never
    /// registered). Surfaces only if the host races the engine.
    Unknown {
        /// Predicate handle that came back unknown.
        handle: PredicateHandle,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_timeout_ms_extractor() {
        let cases: [(Predicate, u32); 6] = [
            (
                Predicate::TextAppears {
                    text: "x".into(),
                    node_id: None,
                    timeout_ms: 1000,
                },
                1000,
            ),
            (
                Predicate::Activity {
                    component: "p/.a".into(),
                    timeout_ms: 2000,
                },
                2000,
            ),
            (
                Predicate::SceneStable {
                    duration_ms: 200,
                    timeout_ms: 3000,
                },
                3000,
            ),
            (
                Predicate::A11yIdle {
                    duration_ms: 100,
                    timeout_ms: 4000,
                },
                4000,
            ),
            (
                Predicate::SelectorMatches {
                    selector: "Button".into(),
                    timeout_ms: 5000,
                },
                5000,
            ),
            (
                Predicate::EventFires {
                    kind: EventKind::ActivityResumed,
                    timeout_ms: 6000,
                },
                6000,
            ),
        ];
        for (pred, expected) in cases {
            assert_eq!(
                pred.timeout_ms(),
                expected,
                "timeout_ms mismatch for {}",
                pred.kind_label()
            );
        }
    }

    #[test]
    fn predicate_labels_are_distinct() {
        let pred_a = Predicate::TextAppears {
            text: "x".into(),
            node_id: None,
            timeout_ms: 0,
        };
        let pred_b = Predicate::Activity {
            component: "x".into(),
            timeout_ms: 0,
        };
        let pred_c = Predicate::SceneStable {
            duration_ms: 0,
            timeout_ms: 0,
        };
        let pred_d = Predicate::A11yIdle {
            duration_ms: 0,
            timeout_ms: 0,
        };
        let pred_e = Predicate::SelectorMatches {
            selector: "x".into(),
            timeout_ms: 0,
        };
        let pred_f = Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 0,
        };
        let labels = [
            pred_a.kind_label(),
            pred_b.kind_label(),
            pred_c.kind_label(),
            pred_d.kind_label(),
            pred_e.kind_label(),
            pred_f.kind_label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len(), "duplicate predicate labels");
    }

    #[test]
    fn event_kind_from_event_mapping() {
        assert_eq!(
            EventKind::from_event(&DeviceEvent::ActivityResumed {
                component: "x".into()
            }),
            Some(EventKind::ActivityResumed),
        );
        assert_eq!(
            EventKind::from_event(&DeviceEvent::SceneChangeDetected { score: 0.1 }),
            Some(EventKind::SceneChangeDetected),
        );
        assert!(
            EventKind::from_event(&DeviceEvent::Uptime { uptime_ms: 0 })
                .is_none(),
        );
    }

    #[test]
    fn predicate_postcard_round_trip() {
        let p = Predicate::SelectorMatches {
            selector: "Button[id=login]".into(),
            timeout_ms: 5000,
        };
        let bytes = postcard::to_allocvec(&p).expect("encode");
        let decoded: Predicate = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, p);
    }

    #[test]
    fn predicate_result_postcard_round_trip() {
        let p = PredicateResult::Matched {
            handle: PredicateHandle(7),
            elapsed_ms: 1234,
        };
        let bytes = postcard::to_allocvec(&p).expect("encode");
        let decoded: PredicateResult = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, p);
    }

    #[test]
    fn event_kind_labels_are_distinct() {
        use std::collections::HashSet;
        let all = [
            EventKind::ActivityResumed,
            EventKind::ActivityPaused,
            EventKind::WindowFocusChanged,
            EventKind::PackageAdded,
            EventKind::PackageRemoved,
            EventKind::ConfigurationChanged,
            EventKind::SceneChangeDetected,
            EventKind::NotificationPosted,
            EventKind::ClipboardChanged,
        ];
        let labels: HashSet<_> = all.iter().map(EventKind::label).collect();
        assert_eq!(labels.len(), all.len(), "duplicate event-kind labels");
    }
}
