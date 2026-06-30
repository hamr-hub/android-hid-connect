//! In-memory state model — see v3 §3.2.
//!
//! Phase 1 lands the *skeleton* — typed fields, no I/O — so the
//! observable stream / predicate engine / memory layer (Phase 2–3)
//! have somewhere to hang themselves. The full
//! `events → predicate wakeup` loop is filled in during Phase 2
//! (see `AgentPlanBoundedPrefix::step` and friends).
//!
//! ## Why in-memory (v3 §1.2 P7 fix)
//!
//! The legacy daemon reads/writes
//! `~/.handsets/state-<port>.json`. That's:
//! - File I/O on every observation tick (slow, races).
//! - Stale by the time the host reads it.
//! - Sync-point between two processes (scrcpy + hs.jar + daemon)
//!   that no one ever reconciles.
//!
//! The AI Device Kernel puts the same data in process-local memory
//! and pushes it to subscribers via [`Observation`]. There's no
//! file on disk to corrupt.

use std::collections::VecDeque;

use crate::action::ActionResult;
use crate::ids::{ActionId, PlanId};
use crate::observation::Observation;
use crate::plan::PlanResult;

/// In-memory kernel state.
///
/// The state is process-local (one daemon process = one
/// `StateModel`) and acts as the **single source of truth** that
/// replaces `~/.handsets/state-*.json`. See v3 §3.1 "State model
/// (single source of truth)".
///
/// Fields are kept `pub(crate)` so the predicate engine (Phase 2)
/// can read them directly without exposing private details
/// outside the crate.
///
/// Phase 1 ships only the typed shape; the predicate engine and
/// observation-stream consumers wire to it in Phase 2. Until then,
/// some fields (`last_input` / `LastInput`) are intentionally
/// unused — they're reserved for Phase 2 wiring.
#[derive(Debug, Default)]
#[allow(dead_code, reason = "Phase 2 predicate engine wires to these fields")]
pub struct StateModel {
    /// Last `Action::Tap` / `TapSelector` coordinates, if any.
    /// Used by the predicate engine for `A11yIdle` checks and
    /// by the Memory layer to bind screen fingerprints to
    /// "tapped here" hot spots.
    pub(crate) last_input: Option<LastInput>,

    /// Last observation snapshot pushed to subscribers.
    pub(crate) last_observation: Option<Observation>,

    /// Pending predicate registrations (Phase 2 will fill this
    /// in — Phase 1 only carries the type-erased counter).
    pub(crate) predicate_set_size: u32,

    /// Bounded event queue; older events are dropped on overflow.
    pub(crate) event_queue: VecDeque<Observation>,

    /// Capped action-result history (most-recent N). Phase 3
    /// reads this to power `idempotent` replay.
    pub(crate) recent_action_results: VecDeque<(ActionId, ActionResult)>,

    /// Capped plan-result history.
    pub(crate) recent_plan_results: VecDeque<(PlanId, PlanResult)>,
}

/// Last-input bookkeeping for `tap` / `tap-selector`.
///
/// Phase 1 reserves these variants for Phase 2 (the predicate
/// engine will read them when checking `A11yIdle` / `SceneStable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "Phase 2 predicate engine consumes these")]
pub(crate) enum LastInput {
    Tap { x: i32, y: i32 },
    Swipe { x1: i32, y1: i32, x2: i32, y2: i32 },
}

/// Caps for the bounded queues. Sized so the worst-case memory
/// footprint is < 1 MiB (v3 AC-V3-3.3).
const EVENT_QUEUE_CAP: usize = 1024;
const ACTION_RESULT_CAP: usize = 256;
const PLAN_RESULT_CAP: usize = 64;

impl StateModel {
    /// Build an empty state model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the most recent observation. Trims the event queue
    /// to [`EVENT_QUEUE_CAP`].
    pub fn record_observation(&mut self, obs: Observation) {
        if self.event_queue.len() >= EVENT_QUEUE_CAP {
            self.event_queue.pop_front();
        }
        self.event_queue.push_back(obs.clone());
        self.last_observation = Some(obs);
    }

    /// Record an action result for idempotency replay.
    pub fn record_action_result(&mut self, id: ActionId, result: ActionResult) {
        if self.recent_action_results.len() >= ACTION_RESULT_CAP {
            self.recent_action_results.pop_front();
        }
        self.recent_action_results.push_back((id, result));
    }

    /// Record a plan result.
    pub fn record_plan_result(&mut self, id: PlanId, result: PlanResult) {
        if self.recent_plan_results.len() >= PLAN_RESULT_CAP {
            self.recent_plan_results.pop_front();
        }
        self.recent_plan_results.push_back((id, result));
    }

    /// Last observation snapshot, if any has been recorded.
    #[must_use]
    pub fn last_observation(&self) -> Option<&Observation> {
        self.last_observation.as_ref()
    }

    /// Find a previously-recorded action result by ID (for
    /// idempotent retry; v3 §6.1 "idempotent + retry").
    #[must_use]
    pub fn action_result(&self, id: ActionId) -> Option<&ActionResult> {
        self.recent_action_results
            .iter()
            .rev()
            .find(|(aid, _)| *aid == id)
            .map(|(_, r)| r)
    }

    /// Number of observers (Phase 2: predicate registrations
    /// count toward this). Phase 1 only returns the explicit
    /// counter.
    #[must_use]
    pub fn pending_predicates(&self) -> u32 {
        self.predicate_set_size
    }

    /// Bump the predicate-set size (Phase 2 will replace this
    /// with proper registration bookkeeping).
    pub fn predicate_registered(&mut self) {
        self.predicate_set_size = self.predicate_set_size.saturating_add(1);
    }

    /// Drop one pending predicate registration. Saturating.
    pub fn predicate_resolved(&mut self) {
        if self.predicate_set_size > 0 {
            self.predicate_set_size -= 1;
        }
    }

    /// Number of observations currently held in the queue.
    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.event_queue.len()
    }

    /// Bytes-of-RAM estimate for the bounded queues. Used by
    /// tests to assert the v3 AC-V3-3.3 "memory < 1 MiB" budget.
    #[must_use]
    pub fn approx_memory_bytes(&self) -> usize {
        // Rough estimate: 256 B per observation event in the queue.
        // Real number will be set during Phase 2 benchmarking; this
        // is just an order-of-magnitude check.
        self.event_queue.len() * 256
            + self.recent_action_results.len() * std::mem::size_of::<(
                ActionId,
                ActionResult,
            )>()
            + self.recent_plan_results.len() * std::mem::size_of::<(
                PlanId,
                PlanResult,
            )>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::GroundTruth;
    use crate::observation::DeviceState;

    fn dummy_obs(seq: u64) -> Observation {
        Observation {
            seq,
            timestamp_ms: seq * 10,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(seq * 10),
            events: vec![],
        }
    }

    fn dummy_result(id: u64) -> (ActionId, ActionResult) {
        (
            ActionId(id),
            ActionResult {
                id: ActionId(id),
                landed: true,
                ground_truth: GroundTruth::default(),
                elapsed_ms: 1,
            },
        )
    }

    #[test]
    fn empty_state_model_has_no_observations() {
        let s = StateModel::new();
        assert!(s.last_observation().is_none());
        assert_eq!(s.queue_len(), 0);
        assert_eq!(s.pending_predicates(), 0);
        assert!(s.action_result(ActionId(1)).is_none());
    }

    #[test]
    fn record_observation_trims_to_cap() {
        let mut s = StateModel::new();
        for seq in 0..(EVENT_QUEUE_CAP + 100) as u64 {
            s.record_observation(dummy_obs(seq));
        }
        assert_eq!(s.queue_len(), EVENT_QUEUE_CAP);
        // The head was dropped, so the queue's front seq is the
        // first one we kept.
        let back = s.last_observation().unwrap();
        assert_eq!(back.seq, (EVENT_QUEUE_CAP + 99) as u64);
    }

    #[test]
    fn action_result_lookup_returns_recent_match() {
        let mut s = StateModel::new();
        s.record_action_result(ActionId(1), dummy_result(1).1);
        s.record_action_result(ActionId(2), dummy_result(2).1);
        assert!(s.action_result(ActionId(1)).is_some());
        assert!(s.action_result(ActionId(2)).is_some());
        assert!(s.action_result(ActionId(99)).is_none());
    }

    #[test]
    fn action_result_cap_drops_oldest() {
        let mut s = StateModel::new();
        for i in 0..(ACTION_RESULT_CAP + 5) as u64 {
            s.record_action_result(ActionId(i), dummy_result(i).1);
        }
        // First ones are dropped; the last few remain.
        assert!(
            s.action_result(ActionId(0)).is_none(),
            "oldest dropped past cap"
        );
        assert!(s.action_result(ActionId((ACTION_RESULT_CAP + 4) as u64)).is_some());
    }

    #[test]
    fn predicate_registered_saturates() {
        let mut s = StateModel::new();
        for _ in 0..u32::MAX {
            s.predicate_registered();
        }
        s.predicate_registered();
        assert_eq!(s.pending_predicates(), u32::MAX);
        s.predicate_resolved();
        assert_eq!(s.pending_predicates(), u32::MAX - 1);
    }

    #[test]
    fn predicate_resolved_does_not_underflow() {
        let mut s = StateModel::new();
        for _ in 0..5 {
            s.predicate_resolved();
        }
        assert_eq!(s.pending_predicates(), 0);
    }

    #[test]
    fn approx_memory_bytes_includes_queues() {
        let mut s = StateModel::new();
        s.record_observation(dummy_obs(1));
        s.record_observation(dummy_obs(2));
        s.record_action_result(ActionId(1), dummy_result(1).1);
        // Non-zero memory used.
        assert!(s.approx_memory_bytes() > 0);
    }

    #[test]
    fn v3_ac_3_3_memory_below_1mib() {
        // Fill to the cap and assert we're well below 1 MiB.
        let mut s = StateModel::new();
        for seq in 0..EVENT_QUEUE_CAP as u64 {
            s.record_observation(dummy_obs(seq));
        }
        for i in 0..ACTION_RESULT_CAP as u64 {
            s.record_action_result(ActionId(i), dummy_result(i).1);
        }
        let bytes = s.approx_memory_bytes();
        assert!(
            bytes < 1 << 20,
            "StateModel exceeded 1 MiB: {bytes} bytes"
        );
    }
}
