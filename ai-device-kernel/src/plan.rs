//! Typed `Plan` surface — see v3 §3.2.2.
//!
//! A `Plan` is a multi-action sequence sent in 1 RTT; the daemon
//! executes it atomically and returns all step results + a final
//! observation. `PlanStep` carries optional `wait_before` /
//! `verify_after` predicates — declarative preconditions and
//! postconditions on the daemon side (replaces "agent polls daemon
//! asking `dump_active` after each action").
//!
//! ## Key design
//!
//! - **1 plan = 1 RTT = 1 reply** — atomic semantics (P2 mitigation);
//!   no per-action round trips needed by the agent.
//! - **`abort_on_error`** — first step that fails halts the plan
//!   (default). Off = best-effort, return collected results.
//! - **`checkpoint_every`** — every N steps the daemon emits a
//!   `DeviceEvent::PlanStepCompleted` so observers can stream
//!   progress (and bound memory usage of in-flight result batches).
//! - **`verify_after` predicate** — daemon self-checks
//!   "did I actually tap the login button?" — saves a follow-up
//!   `dump_active` round trip (P3 hardening).

use serde::{Deserialize, Serialize};

use crate::action::{Action, ActionResult};
use crate::ids::{PlanId, StepId};
use crate::observation::Observation;
use crate::predicate::Predicate;

/// Typed multi-action plan — v3 §3.2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Server-assigned plan ID (filled in by the daemon; ignored
    /// on the wire from the host side).
    pub id: PlanId,
    /// Action steps to execute in order.
    pub steps: Vec<PlanStep>,
    /// If `true` (default), stop on the first step failure. If
    /// `false`, execute as many steps as possible and return
    /// per-step results.
    pub abort_on_error: bool,
    /// Emit a `PlanStepCompleted` event every N steps. `0` disables.
    pub checkpoint_every: u32,
}

impl Plan {
    /// New empty plan with default flags (abort_on_error = true,
    /// checkpoint_every = 0).
    #[must_use]
    pub fn new(steps: Vec<PlanStep>) -> Self {
        Self {
            id: PlanId::ZERO,
            steps,
            abort_on_error: true,
            checkpoint_every: 0,
        }
    }

    /// Set `abort_on_error`. Builder-style.
    #[must_use]
    pub fn with_abort(mut self, abort: bool) -> Self {
        self.abort_on_error = abort;
        self
    }

    /// Set `checkpoint_every`. Builder-style.
    #[must_use]
    pub fn with_checkpoint_every(mut self, n: u32) -> Self {
        self.checkpoint_every = n;
        self
    }

    /// Number of steps in the plan.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// True if the plan has zero steps.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// One step within a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Server-assigned step index (filled in by the daemon).
    pub id: StepId,
    /// Action to execute.
    pub action: Action,
    /// Optional predicate — daemon waits for it before executing
    /// this step. Skips the step on timeout (failure if
    /// `abort_on_error`).
    pub wait_before: Option<Predicate>,
    /// Optional predicate — daemon verifies the action landed
    /// correctly before continuing. Skips subsequent steps on
    /// mismatch (when `abort_on_error`).
    pub verify_after: Option<Predicate>,
    /// Optional label for human-readable log lines.
    pub label: Option<String>,
}

impl PlanStep {
    /// Convenience — bare action, no predicates, no label.
    #[must_use]
    pub fn new(action: Action) -> Self {
        Self {
            id: StepId::ZERO,
            action,
            wait_before: None,
            verify_after: None,
            label: None,
        }
    }

    /// Set `wait_before` predicate.
    #[must_use]
    pub fn with_wait_before(mut self, p: Predicate) -> Self {
        self.wait_before = Some(p);
        self
    }

    /// Set `verify_after` predicate.
    #[must_use]
    pub fn with_verify_after(mut self, p: Predicate) -> Self {
        self.verify_after = Some(p);
        self
    }

    /// Set `label`.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Result of a [`Plan`] execution — returned in 1 frame (v3 §3.2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanResult {
    /// Plan ID echoed back.
    pub plan_id: PlanId,
    /// Per-step results, in execution order.
    pub steps: Vec<StepResult>,
    /// Final observation snapshot (taken after the last step).
    pub final_observation: Observation,
    /// Total wall-clock elapsed ms (server-measured).
    pub total_elapsed_ms: u32,
    /// True if all steps landed; false if `abort_on_error` kicked
    /// in and subsequent steps were skipped.
    pub all_landed: bool,
}

impl PlanResult {
    /// Index of the first step that didn't land (if any).
    #[must_use]
    pub fn first_failure(&self) -> Option<usize> {
        self.steps.iter().position(|s| !s.landed)
    }
}

/// Per-step result within a [`PlanResult`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// Step ID echoed back.
    pub step_id: StepId,
    /// Index of this step in the plan (for convenience).
    pub index: u32,
    /// ActionResult for the step.
    pub action_result: ActionResult,
    /// Whether the step landed (mirrors `ActionResult::landed` so
    /// callers don't have to unwrap).
    pub landed: bool,
    /// Error message string if the step failed at the predicate
    /// level (`wait_before` timeout / `verify_after` mismatch).
    /// `None` on success.
    pub error: Option<String>,
}

impl StepResult {
    /// Convenience: did the step succeed?
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.landed && self.action_result.landed && self.error.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{
        Action, A11yNodeChangeKind, GroundTruth, LaunchBy, ObservationComponent,
    };
    use crate::ids::ActionId;
    use crate::observation::{DeviceEvent, DeviceState, FrameSnapshot};

    fn dummy_action() -> Action {
        Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        }
    }

    #[test]
    fn plan_defaults() {
        let plan = Plan::new(vec![]);
        assert!(plan.abort_on_error);
        assert_eq!(plan.checkpoint_every, 0);
        assert_eq!(plan.id, PlanId::ZERO);
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
    }

    #[test]
    fn plan_builders_chain() {
        let plan = Plan::new(vec![PlanStep::new(dummy_action())])
            .with_abort(false)
            .with_checkpoint_every(2);
        assert!(!plan.abort_on_error);
        assert_eq!(plan.checkpoint_every, 2);
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn plan_step_builders_chain() {
        let step = PlanStep::new(Action::Wait {
            predicate: Predicate::Activity {
                component: "p/.a".into(),
                timeout_ms: 0,
            },
            deadline_ms: 0,
        })
        .with_wait_before(Predicate::Activity {
            component: "p/.home".into(),
            timeout_ms: 1000,
        })
        .with_verify_after(Predicate::Activity {
            component: "p/.target".into(),
            timeout_ms: 1000,
        })
        .with_label("wait-then-verify");
        assert!(step.wait_before.is_some());
        assert!(step.verify_after.is_some());
        assert_eq!(step.label.as_deref(), Some("wait-then-verify"));
    }

    #[test]
    fn step_result_is_ok() {
        let ok = StepResult {
            step_id: StepId(0),
            index: 0,
            action_result: ActionResult {
                id: ActionId(1),
                landed: true,
                ground_truth: GroundTruth::default(),
                elapsed_ms: 1,
            },
            landed: true,
            error: None,
        };
        assert!(ok.is_ok());

        let err = StepResult {
            step_id: StepId(1),
            index: 1,
            action_result: ActionResult {
                id: ActionId(2),
                landed: true,
                ground_truth: GroundTruth::default(),
                elapsed_ms: 1,
            },
            landed: true,
            error: Some("verify timeout".into()),
        };
        assert!(!err.is_ok());
    }

    #[test]
    fn step_result_is_not_ok_when_action_didnt_land() {
        let r = StepResult {
            step_id: StepId(0),
            index: 0,
            action_result: ActionResult {
                id: ActionId(1),
                landed: false,
                ground_truth: GroundTruth::default(),
                elapsed_ms: 1,
            },
            landed: true,
            error: None,
        };
        assert!(!r.is_ok(), "ActionResult.landed=false ⇒ step.is_ok()=false");
    }

    #[test]
    fn plan_result_first_failure() {
        // 3 steps: ok, fail, ok.
        let steps = vec![
            StepResult {
                step_id: StepId(0),
                index: 0,
                action_result: ActionResult {
                    id: ActionId(1),
                    landed: true,
                    ground_truth: GroundTruth::default(),
                    elapsed_ms: 1,
                },
                landed: true,
                error: None,
            },
            StepResult {
                step_id: StepId(1),
                index: 1,
                action_result: ActionResult {
                    id: ActionId(2),
                    landed: false,
                    ground_truth: GroundTruth::default(),
                    elapsed_ms: 1,
                },
                landed: false,
                error: Some("action refused".into()),
            },
            // Pretend `abort_on_error=true` so this got skipped;
            // the daemon still echoes it back with `landed=false`
            // and a marker error.
            StepResult {
                step_id: StepId(2),
                index: 2,
                action_result: ActionResult {
                    id: ActionId(3),
                    landed: false,
                    ground_truth: GroundTruth::default(),
                    elapsed_ms: 0,
                },
                landed: false,
                error: Some("skipped (plan aborted)".into()),
            },
        ];
        let pr = PlanResult {
            plan_id: PlanId(0),
            steps,
            final_observation: Observation {
                seq: 0,
                timestamp_ms: 0,
                a11y: None,
                frame: None,
                state: DeviceState::unknown(0),
                events: vec![],
            },
            total_elapsed_ms: 5,
            all_landed: false,
        };
        assert_eq!(pr.first_failure(), Some(1), "first failure at index 1");
        assert!(!pr.all_landed);
    }

    #[test]
    fn plan_postcard_round_trip() {
        let plan = Plan::new(vec![PlanStep::new(Action::TapSelector {
            selector: "Button".into(),
            deadline_ms: 1000,
        })])
        .with_checkpoint_every(1);
        let bytes = postcard::to_allocvec(&plan).expect("encode");
        let decoded: Plan = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn plan_result_postcard_round_trip() {
        let pr = PlanResult {
            plan_id: PlanId(42),
            steps: vec![],
            final_observation: Observation {
                seq: 7,
                timestamp_ms: 12,
                a11y: None,
                frame: Some(FrameSnapshot {
                    width: 1080,
                    height: 1920,
                    codec: 1,
                    is_keyframe: true,
                    pts: 90000,
                    scene_change_score: 0.0,
                }),
                state: DeviceState::unknown(12),
                events: vec![DeviceEvent::ActionCompleted {
                    action_id: ActionId(1),
                    landed: true,
                    elapsed_ms: 5,
                }],
            },
            total_elapsed_ms: 5,
            all_landed: true,
        };
        let bytes = postcard::to_allocvec(&pr).expect("encode");
        let decoded: PlanResult =
            postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, pr);
        assert_eq!(decoded.first_failure(), None);
    }

    #[test]
    fn plan_supports_observation_only_steps() {
        // A `DumpObservation` step (no input side-effect) is the
        // canonical way to wedge a snapshot between two actions
        // without breaking atomicity.
        let steps = vec![
            PlanStep::new(Action::Tap {
                x: 100,
                y: 100,
                deadline_ms: 100,
            })
            .with_label("tap-once"),
            PlanStep::new(Action::DumpObservation {
                components: vec![ObservationComponent::A11y],
                deadline_ms: 100,
            })
            .with_label("observe-after-tap"),
            PlanStep::new(Action::Launch {
                target: "com.foo/.Main".into(),
                by: LaunchBy::Component("com.foo/.Main".into()),
                deadline_ms: 100,
            })
            .with_label("launch-foo"),
        ];
        let plan = Plan::new(steps);
        assert_eq!(plan.len(), 3);
        // Capability composition: union of all step capabilities.
        let _ = plan; // serialise round-trip already covered above.
    }

    #[test]
    fn predicate_in_wait_before_does_not_block_serialization() {
        // Predicates may have arbitrary data; confirm a
        // `PlanStep` with a non-empty predicate round-trips.
        let step = PlanStep::new(Action::Wait {
            predicate: Predicate::Activity {
                component: "p/.a".into(),
                timeout_ms: 0,
            },
            deadline_ms: 0,
        })
        .with_wait_before(Predicate::SelectorMatches {
            selector: "Button[id=login]".into(),
            timeout_ms: 1500,
        });
        let bytes = postcard::to_allocvec(&step).expect("encode");
        let decoded: PlanStep =
            postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, step);
    }

    #[test]
    fn a11y_node_diff_change_kind_distinct() {
        // Trivially true for `Copy + Eq`, but pinning so an
        // accidental future PartialEq drop is caught.
        let kinds = [
            A11yNodeChangeKind::Added,
            A11yNodeChangeKind::Removed,
            A11yNodeChangeKind::TextChanged,
            A11yNodeChangeKind::VisibilityChanged,
            A11yNodeChangeKind::BoundsChanged,
        ];
        let enc: std::collections::HashSet<_> = kinds
            .iter()
            .map(|k| postcard::to_allocvec(k).unwrap())
            .collect();
        assert_eq!(enc.len(), kinds.len(), "duplicate postcard encoding");
    }
}
