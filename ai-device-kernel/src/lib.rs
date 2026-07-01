//! `ai-device-kernel` — on-device AI Device Kernel — THE AI middleware
//! for controlling Android devices.
//!
//! Phase 6.5 (2026-06-30) is the consolidated release per ADR-0012:
//! this crate is the **single** AI middleware — 1 binary (`adk`),
//! 1 port (9008), 1 protocol (postcard/4-verb), 1 typed-action surface,
//! 1 capability registry. The legacy `android-hid-{daemon,agent,cli,py}`
//! crates and the root `src/agent/` facade were retired; the kernel
//! is the sole façade.
//!
//! ## Layering
//!
//! ```text
//! ai-device-kernel  (this crate — THE AI middleware)
//!        │
//!        ├─► android-hid-connect   (byte-exact scrcpy UHID core)
//!        │       │
//!        │       ▼
//!        │   scrcpy-server         (on-device, AOSP process)
//!        │
//!        └─► android-hid-protocol  (verb / error / frame types — no I/O)
//! ```
//!
//! Direction: `ai-device-kernel` depends on both
//! `android-hid-connect` (for the byte-exact UHID hot path via the
//! [`bridge`] module) and `android-hid-protocol` (for the wire
//! types). It depends on nothing else.
//!
//! ## Modules
//!
//! - [`ids`] — `ActionId`, `PlanId`, `StepId`, `PredicateHandle`,
//!   `ScreenId` (16-byte blake3 fingerprint).
//! - [`action`] — 12 typed [`Action`] variants + 2 Phase 4 +
//!   2 Phase 5 (read-back) variants, [`ActionResult`],
//!   [`GroundTruth`], [`A11yNodeDiff`], [`FrameDiff`].
//! - [`plan`] — [`Plan`], [`PlanStep`], [`PlanResult`], [`StepResult`].
//! - [`observation`] — [`Observation`], [`DeviceEvent`], [`DeviceState`].
//! - [`predicate`] — 6-variant [`Predicate`], [`EventKind`], [`PredicateResult`].
//! - [`protocol`] — 4-verb [`Frame`] layout with postcard-encoded payloads.
//! - [`capability`] — [`Capability`] trait + [`CapabilityRegistry`].
//! - [`state`] — in-memory [`StateModel`].
//! - [`bridge`] — typed `Action` → byte-exact UHID execution path.
//! - [`plan_executor`] — drive a [`Plan`] to completion via [`bridge`]
//!   + [`predicate_engine`].
//! - [`predicate_engine`] / [`predicate_wait`] — predicate resolution.
//! - [`stream_engine`] — bounded observation stream.
//! - [`ui_repr`] — ~500 B AutoDroid-style HTML-tagged UI repr.
//! - [`memory`] / [`memory_sqlite`] — selector cache + scenario library.
//!
//! [`docs/ai-device-kernel-v3-design.md`]: ../../docs/ai-device-kernel-v3-design.md

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod action;
pub mod bridge;
pub mod capability;
pub mod egress;
pub mod ids;
pub mod json;
pub mod memory;
#[cfg(feature = "sqlite")]
pub mod memory_sqlite;
pub mod mock_llm;
pub mod observation;
pub mod plan;
pub mod plan_executor;
pub mod predicate;
pub mod predicate_engine;
pub mod predicate_wait;
pub mod protocol;
pub mod scenario_library;
pub mod session_state;
pub mod state;
pub mod chunk_planner;
pub mod selector_cache;
pub mod stream_engine;
pub mod ui_repr;

pub use action::{
    A11yNodeChangeKind, A11yNodeDiff, Action, ActionResult, AtomicErrorCode, FrameDiff,
    GroundTruth, LaunchBy, ObservationComponent, Rect,
};
pub use bridge::{ActionInjector, BridgeOutcome};
pub use capability::{
    Capability, CapabilityContext, CapabilityError, CapabilityName, CapabilityOutput,
    CapabilityRegistry, CapabilityRequirements, ALL_CAPABILITY_NAMES,
};
pub use egress::EgressPolicy;
pub use ids::{ActionId, PlanId, ScreenId, StepId};
pub use memory::{ActionSequence, Memory};
#[cfg(feature = "sqlite")]
pub use memory_sqlite as memory_sqlite_backend;
pub use observation::{
    A11yTree, DeviceEvent, DeviceState, ForegroundApp, FrameSnapshot, Observation,
};
pub use plan::{Plan, PlanResult, PlanStep, StepResult};
pub use plan_executor::{execute, ExecutorCounters, PlanFailure};
pub use predicate::{EventKind, Predicate, PredicateHandle, PredicateResult};
pub use predicate_engine::{PredicateEngine, PredicateOutcome, RegisteredPredicate};
pub use predicate_wait::{wait_for, WaitOutcome};
pub use protocol::{Frame, FrameFlags, ReplyPayload, RequestPayload, Verb};
pub use state::StateModel;
pub use stream_engine::{StreamEngine, Subscriber, SubscriberHandle, SUBSCRIBER_QUEUE_CAP};
pub use ui_repr::{UiReprClass, UiReprHtml, UiReprNode};

/// Protocol version the kernel speaks. Bumped whenever the wire
/// format changes incompatibly. v3 design Phase 1 marks this as
/// `3.0.0-alpha.1` — pre-1.0 release, subject to change.
pub const PROTOCOL_VERSION: &str = "3.0.0-alpha.1";

/// Default port (matches the legacy daemon at 9008).
pub const DEFAULT_PORT: u16 = 9008;

/// Convenience: build a [`RequestPayload::Query`] that asks the
/// daemon for an idle-fallback observation. Useful for the
/// "policy says no observation stream; ask once" path.
#[must_use]
pub fn idle_query() -> RequestPayload {
    RequestPayload::Query {
        a11y: true,
        frame: false,
        state: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_stable() {
        // Locked in; bumping requires a major-version bump.
        assert_eq!(PROTOCOL_VERSION, "3.0.0-alpha.1");
    }

    #[test]
    fn default_port_matches_legacy_daemon() {
        // 9008 is the legacy daemon's port; the new kernel reuses
        // it so existing `adb forward tcp:9008 localabstract:ahdk`
        // setups continue to work.
        assert_eq!(DEFAULT_PORT, 9008);
    }

    #[test]
    fn idle_query_includes_a11y_and_state() {
        let q = idle_query();
        match q {
            RequestPayload::Query { a11y, frame, state } => {
                assert!(a11y);
                assert!(state);
                assert!(!frame);
            }
            _ => panic!("idle_query must be a Query request"),
        }
    }

    #[test]
    fn all_modules_are_publicly_exported() {
        // Spot-check the public re-exports by name (compile-time
        // check; the test harness would fail to build if any of
        // these weren't accessible).
        let _: Action = Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        };
        let _: Plan = Plan::new(vec![]);
        let _: Observation = Observation {
            seq: 0,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
            ..Default::default()
        };
        let _: Predicate = Predicate::Activity {
            component: "p/.a".into(),
            timeout_ms: 0,
        };
        let _: Frame = Frame::request(&idle_query());
        let _: CapabilityRegistry = CapabilityRegistry::new();
        let _: StateModel = StateModel::new();
    }
}
