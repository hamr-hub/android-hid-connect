//! `ai-device-kernel` — on-device AI Device Kernel.
//!
//! Phase 1 of the v3 redesign ([`docs/ai-device-kernel-v3-design.md`]).
//! This crate provides the **typed** action surface, the 4-verb
//! binary protocol, and the capability-registry contract — but
//! does **not** yet ship the on-device implementation that
//! replaces [`android-hid-daemon`]. That port lands in Phase 2
//! alongside the state model, observation stream, and predicate
//! engine.
//!
//! ## Layering
//!
//! ```text
//! ai-device-kernel (this crate)
//!        ▲
//!        │
//! android-hid-protocol  (verb / error / frame types — no I/O)
//! ```
//!
//! Direction constraint (AGENTS.md §2.7): `ai-device-kernel`
//! depends on `android-hid-protocol` only — it must NOT depend on
//! `android-hid-connect` (byte-exact scrcpy core), `android-hid-daemon`
//! (legacy daemon), `android-hid-agent` (host-side facade), or any
//! other sibling crate.
//!
//! ## Modules
//!
//! - [`ids`] — `ActionId`, `PlanId`, `StepId`, `PredicateHandle`,
//!   `ScreenId` (16-byte blake3 fingerprint).
//! - [`action`] — 12 typed [`Action`] variants, [`ActionResult`],
//!   [`GroundTruth`], [`A11yNodeDiff`], [`FrameDiff`].
//! - [`plan`] — [`Plan`], [`PlanStep`], [`PlanResult`], [`StepResult`].
//! - [`observation`] — [`Observation`], [`DeviceEvent`], [`DeviceState`].
//! - [`predicate`] — 6-variant [`Predicate`], [`EventKind`], [`PredicateResult`].
//! - [`protocol`] — 4-verb [`Frame`] layout with postcard-encoded payloads.
//! - [`capability`] — [`Capability`] trait + [`CapabilityRegistry`].
//! - [`state`] — in-memory [`StateModel`] (Phase 2 will fill in the
//!   streaming logic on top).
//!
//! [`docs/ai-device-kernel-v3-design.md`]: ../../docs/ai-device-kernel-v3-design.md
//! [`android-hid-daemon`]: ../android-hid-daemon/index.html

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod action;
pub mod capability;
pub mod ids;
pub mod memory;
#[cfg(feature = "sqlite")]
pub mod memory_sqlite;
pub mod observation;
pub mod plan;
pub mod plan_executor;
pub mod predicate;
pub mod predicate_engine;
pub mod predicate_wait;
pub mod protocol;
pub mod state;
pub mod stream_engine;
pub mod ui_repr;

pub use action::{
    Action, ActionResult, A11yNodeChangeKind, A11yNodeDiff, FrameDiff, GroundTruth,
    LaunchBy, ObservationComponent, Rect,
};
pub use capability::{
    Capability, CapabilityContext, CapabilityError, CapabilityName, CapabilityOutput,
    CapabilityRegistry, CapabilityRequirements, ALL_CAPABILITY_NAMES,
};
pub use ids::{ActionId, PlanId, ScreenId, StepId};
pub use observation::{
    A11yTree, DeviceEvent, DeviceState, FrameSnapshot, Observation,
};
pub use plan::{Plan, PlanResult, PlanStep, StepResult};
pub use memory::{ActionSequence, Memory};
#[cfg(feature = "sqlite")]
pub use memory_sqlite as memory_sqlite_backend;
pub use plan_executor::{execute, ExecutorCounters, PlanFailure};
pub use predicate::{EventKind, Predicate, PredicateHandle, PredicateResult};
pub use predicate_engine::{
    PredicateEngine, PredicateOutcome, RegisteredPredicate,
};
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
            RequestPayload::Query {
                a11y,
                frame,
                state,
            } => {
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
