//! `EgressPolicy` — controls which Observation fields may leave the
//! device when the kernel forwards an Observation to a cloud LLM.
//!
//! Per grill ADR 0008 (egress-policy-text-and-snapshot-only) +
//! PRD-B ADR 0015: `history` is **always denied** by default.
//! `text` (UI tree + foreground app) and `snapshot` (PNG) are
//! allowed by default but can be turned off per-deployment.
//!
//! The global escape valve is `LlmDeployment.kind = on_device_only`
//! (per ADR 0007) which disables the entire cloud path — `EgressPolicy`
//! is irrelevant in that mode.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::observation::{DeviceEvent, ForegroundApp, Observation};

/// Egress policy controlling which fields of an `Observation` may
/// be forwarded to a cloud LLM. JSON-derivable (per ADR 0012) so
/// it can ship as part of the LLM deployment config wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct EgressPolicy {
    /// Allow `Observation.a11y` (UI tree text content) and
    /// `Observation.foreground_app` (package + activity) to leave
    /// the device. **Default: true** (text is the LLM's primary
    /// perception channel).
    pub allow_text: bool,
    /// Allow `Observation.frame.png` (raw PNG bytes) to leave the
    /// device. **Default: true** (LLM-vision path). Setting
    /// `allow_snapshot = false` saves bandwidth but disables the
    /// vision-based selector_miss rescue.
    pub allow_snapshot: bool,
    /// Allow `Observation.events` (the recent atomic execution log
    /// + lifecycle events) to leave the device. **Default: false**
    ///   because the events list leaks the user's *actions* (not just
    ///   the screen state). The LLM does not need history to make
    ///   forward-planning decisions; it can re-observe on demand.
    pub allow_history: bool,
}

impl Default for EgressPolicy {
    /// Per ADR 0008 / PRD-B ADR 0015: text + snapshot allowed,
    /// history denied.
    fn default() -> Self {
        Self {
            allow_text: true,
            allow_snapshot: true,
            allow_history: false,
        }
    }
}

impl EgressPolicy {
    /// Strictest possible policy: nothing leaves the device.
    /// Equivalent to `LlmDeployment.kind = on_device_only` but
    /// enforced at this layer too (defence in depth).
    pub const DENY_ALL: Self = Self {
        allow_text: false,
        allow_snapshot: false,
        allow_history: false,
    };

    /// Apply this policy in-place to an `Observation`. The
    /// `observation` argument is mutated: fields disallowed by
    /// the policy are set to `None` or cleared.
    ///
    /// **No allocation** — operates on the existing `Observation`
    /// in place. Idempotent: calling `filter` twice with the same
    /// policy yields the same result.
    pub fn filter(&self, observation: &mut Observation) {
        // 1. history: events list leaks user actions
        if !self.allow_history {
            observation.events.clear();
        } else {
            strip_lifecycle_events(&mut observation.events);
        }
        // 2. text: UI tree + foreground app
        if !self.allow_text {
            observation.a11y = None;
            observation.foreground_app = None;
        }
        // 3. snapshot: PNG bytes inside frame
        if !self.allow_snapshot {
            if let Some(frame) = observation.frame.as_mut() {
                frame.png = None;
            }
        }
    }

    /// Convenience: apply policy and return a filtered clone.
    /// Equivalent to `let mut o = obs.clone(); policy.filter(&mut o); o`
    /// but spelled out for clarity.
    #[must_use]
    pub fn filter_clone(&self, observation: &Observation) -> Observation {
        let mut cloned = observation.clone();
        self.filter(&mut cloned);
        cloned
    }

    /// True if the policy denies *all* egress. Useful for early-out
    /// in the cloud-bound codepath (skip the network call entirely).
    #[must_use]
    pub fn is_deny_all(&self) -> bool {
        !self.allow_text && !self.allow_snapshot && !self.allow_history
    }
}

/// Internal helper: strip history events whose `kind` is "lifecycle"
/// even when the policy allows history. Used by `EgressPolicy` to
/// reduce noise in audit log forward (we still keep history on
/// device per ADR 0009; only the *forwarded* slice is filtered).
pub(crate) fn strip_lifecycle_events(events: &mut Vec<DeviceEvent>) {
    events.retain(|ev| {
        !matches!(
            ev,
            DeviceEvent::Uptime { .. } | DeviceEvent::ConfigurationChanged
        )
    });
}

/// Internal helper: build a minimal `ForegroundApp` for a package
/// name. Used by `LlmContext` when only the package (no full
/// activity) is available — collapses verbose `ForegroundApp` to
/// the `package` field only.
#[must_use]
pub fn package_only(app: Option<&ForegroundApp>) -> Option<ForegroundApp> {
    app.map(|a| ForegroundApp {
        package: a.package.clone(),
        activity: String::new(),
        focused: a.focused,
        since_ms: a.since_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{A11yTree, DeviceState, FrameSnapshot};

    fn sample_observation() -> Observation {
        Observation {
            seq: 1,
            timestamp_ms: 1000,
            a11y: Some(A11yTree {
                window_id: Some(7),
                top_activity: Some("com.example/.Main".into()),
                node_count: 50,
                json: "[]".into(),
            }),
            frame: Some(FrameSnapshot {
                width: 1080,
                height: 1920,
                codec: 1,
                is_keyframe: true,
                pts: 90000,
                scene_change_score: 0.5,
                png: Some(vec![0x89, 0x50, 0x4E, 0x47]),
            }),
            state: DeviceState::unknown(1000),
            events: vec![
                DeviceEvent::ActivityResumed {
                    component: "com.example/.Main".into(),
                },
                DeviceEvent::Uptime { uptime_ms: 1000 },
                DeviceEvent::SceneChangeDetected { score: 0.7 },
            ],
            foreground_app: Some(ForegroundApp {
                package: "com.example".into(),
                activity: ".Main".into(),
                focused: true,
                since_ms: 500,
            }),
        }
    }

    #[test]
    fn default_is_text_snapshot_allow_history_deny() {
        let p = EgressPolicy::default();
        assert!(p.allow_text);
        assert!(p.allow_snapshot);
        assert!(!p.allow_history);
        assert!(!p.is_deny_all());
    }

    #[test]
    fn deny_all_deny_all_returns_true() {
        let policy = EgressPolicy::DENY_ALL;
        assert!(policy.is_deny_all());
        assert!(!policy.allow_history);
    }

    #[test]
    fn filter_strips_history_when_disabled() {
        let p = EgressPolicy::default();
        let mut obs = sample_observation();
        assert_eq!(obs.events.len(), 3);
        p.filter(&mut obs);
        assert!(
            obs.events.is_empty(),
            "history must be stripped on default policy"
        );
    }

    #[test]
    fn filter_strips_png_when_disabled() {
        let mut obs = sample_observation();
        let p = EgressPolicy {
            allow_snapshot: false,
            ..EgressPolicy::default()
        };
        p.filter(&mut obs);
        let frame = obs.frame.as_ref().expect("frame preserved");
        assert!(
            frame.png.is_none(),
            "PNG must be stripped when allow_snapshot=false"
        );
    }

    #[test]
    fn filter_strips_text_when_disabled() {
        let mut obs = sample_observation();
        let p = EgressPolicy {
            allow_text: false,
            ..EgressPolicy::default()
        };
        p.filter(&mut obs);
        assert!(
            obs.a11y.is_none(),
            "a11y must be stripped when allow_text=false"
        );
        assert!(
            obs.foreground_app.is_none(),
            "foreground_app must be stripped when allow_text=false"
        );
    }

    #[test]
    fn filter_keeps_history_when_allowed() {
        let mut obs = sample_observation();
        let p = EgressPolicy {
            allow_history: true,
            ..EgressPolicy::default()
        };
        p.filter(&mut obs);
        assert_eq!(obs.events.len(), 2, "non-lifecycle history is kept");
        assert!(obs
            .events
            .iter()
            .all(|event| !matches!(event, DeviceEvent::Uptime { .. })));
    }

    #[test]
    fn filter_is_idempotent() {
        let p = EgressPolicy::default();
        let mut obs = sample_observation();
        p.filter(&mut obs);
        let snapshot1 = obs.clone();
        p.filter(&mut obs);
        assert_eq!(obs, snapshot1, "filter must be idempotent");
    }

    #[test]
    fn filter_clone_returns_independent_observation() {
        let p = EgressPolicy::default();
        let obs = sample_observation();
        let mut filtered = p.filter_clone(&obs);
        filtered.events.clear();
        // original is unaffected
        assert_eq!(obs.events.len(), 3);
    }

    #[test]
    fn egress_policy_serializes_to_snake_case_json() {
        let p = EgressPolicy::default();
        let v: serde_json::Value = serde_json::to_value(p).unwrap();
        assert_eq!(v["allow_text"], true);
        assert_eq!(v["allow_snapshot"], true);
        assert_eq!(v["allow_history"], false);
    }

    #[test]
    fn strip_lifecycle_events_removes_uptime_and_config_only() {
        let mut events = vec![
            DeviceEvent::Uptime { uptime_ms: 1 },
            DeviceEvent::ConfigurationChanged,
            DeviceEvent::SceneChangeDetected { score: 0.7 },
        ];
        strip_lifecycle_events(&mut events);
        assert_eq!(
            events,
            vec![DeviceEvent::SceneChangeDetected { score: 0.7 }]
        );
    }

    #[test]
    fn package_only_drops_activity_but_keeps_focus_metadata() {
        let app = ForegroundApp {
            package: "com.example".into(),
            activity: ".Main".into(),
            focused: true,
            since_ms: 123,
        };
        let compact = package_only(Some(&app)).expect("app");
        assert_eq!(compact.package, "com.example");
        assert_eq!(compact.activity, "");
        assert!(compact.focused);
        assert_eq!(compact.since_ms, 123);
        assert_eq!(package_only(None), None);
    }
}
