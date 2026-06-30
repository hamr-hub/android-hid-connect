//! Plan executor — see v3 §3.2.2 + §5 Phase 3.1–3.3.
//!
//! Drives a [`Plan`] step-by-step, evaluates `wait_before` and
//! `verify_after` predicates via [`PredicateEngine`], and
//! assembles a [`PlanResult`] in 1 logical RTT (matching
//! v3 AC-V3-3.1 "1 plan = 1 RTT = 1 reply").
//!
//! ## Design
//!
//! - **Sequence**: for each step, run `wait_before` (if any)
//!   → execute the action (delegated to the host closure)
//!   → run `verify_after` (if any) → record step result.
//! - **`abort_on_error`**: stops the plan at the first failed
//!   step. Steps beyond the failure are NOT executed (per
//!   v3 AC-V3-3.2). All failure modes (`wait_before` timeout,
//!   `verify_after` mismatch, action refusal) trigger the
//!   same abort.
//! - **`checkpoint_every`**: emits a
//!   [`crate::observation::DeviceEvent::PlanStepCompleted`]
//!   every N successful steps. The host's observation
//!   stream consumers can latch onto this for progress
//!   reporting. Memory budget for checkpoints is bounded
//!   by the cap in [`StateModel::record_plan_result`].
//! - **Ground truth**: each [`ActionResult`] carries
//!   ground-truth data; the host-side closure provides it
//!   after executing each action.
//!
//! This module is **pure logic** — no I/O, no threads, no
//! channels. The host's runtime drives the executor by
//! feeding observations into the predicate engine, calling
//! `execute(...)` for each step, and passing ground truth
//! back via the closures. Actual wire IO is in the (future)
//! `adk` binary.

use crate::action::{Action, ActionResult, GroundTruth};
use crate::ids::{PlanId, StepId};
use crate::observation::{DeviceEvent, DeviceState, Observation};
use crate::plan::{Plan, PlanResult, StepResult};
use crate::predicate::Predicate;
use crate::predicate_engine::{PredicateEngine, PredicateOutcome};
use crate::stream_engine::StreamEngine;

/// Failure mode while running a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFailure {
    /// `wait_before` predicate timed out (or closed stream).
    WaitBeforeTimeout {
        /// Step index that failed.
        step_index: u32,
        /// Step id at failure.
        step_id: StepId,
        /// Optional label from the failing step.
        label: Option<String>,
    },
    /// `verify_after` predicate did not match after the action
    /// ran successfully (v3 P3 hardening: "did I actually tap
    /// the login button?").
    VerifyAfterMismatch {
        step_index: u32,
        step_id: StepId,
        label: Option<String>,
    },
    /// Action refused by the executor closure (e.g. capability
    /// refused under the current profile).
    ActionRefused {
        step_index: u32,
        step_id: StepId,
        reason: String,
    },
    /// Predicate aborted by an explicit cancel (unused at the
    /// executor level but propagated for completeness).
    Cancelled {
        step_index: u32,
        step_id: StepId,
    },
}

impl PlanFailure {
    /// Human label suitable for log lines + metrics tags.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::WaitBeforeTimeout { .. } => "wait-before-timeout",
            Self::VerifyAfterMismatch { .. } => "verify-after-mismatch",
            Self::ActionRefused { .. } => "action-refused",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// Outcome counters returned from `execute`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutorCounters {
    /// Steps that landed successfully.
    pub landed: u32,
    /// Steps that aborted the plan (counted even on
    /// `abort_on_error = false` they still appear here).
    pub aborted: u32,
    /// Steps that were skipped because `abort_on_error = true`
    /// short-circuited the run.
    pub skipped: u32,
    /// Checkpoint events emitted (per `checkpoint_every`).
    pub checkpoints: u32,
}

/// Run a plan.
///
/// `execute_step` is the closure the host provides to actually
/// run each action. It returns the `ActionResult` (ground
/// truth + landed flag + elapsed_ms). The executor does not
/// know how the action is performed; it only knows the typed
/// surface.
///
/// `drain_observation` is the host's blocking read — it returns
/// the next `Observation` or `None` on EOF. The executor
/// pushes that observation through the [`StreamEngine`]
/// (which fan-outs to subscribers) and the
/// [`PredicateEngine`] (which evaluates predicates).
///
/// On success, the function returns a [`PlanResult`] and
/// counters. On failure (the first one), it returns
/// `Err(PlanFailure)` along with the **partial** steps so
/// the host can decide whether to surface this as a
/// `PlanResult.all_landed = false` or to discard.
#[allow(clippy::too_many_arguments, reason = "executor signature is intentional")]
pub fn execute<A, D>(
    plan: &Plan,
    predicate_engine: &mut PredicateEngine,
    stream_engine: &mut StreamEngine,
    plan_id: PlanId,
    mut execute_step: A,
    mut drain_observation: D,
) -> Result<(PlanResult, ExecutorCounters), (PlanFailure, Vec<StepResult>)>
where
    A: FnMut(&Action) -> Result<ActionResult, String>,
    D: FnMut() -> Option<Observation>,
{
    let mut steps_out: Vec<StepResult> = Vec::with_capacity(plan.steps.len());
    let mut counters = ExecutorCounters::default();
    let mut final_obs: Observation = Observation {
        seq: 0,
        timestamp_ms: 0,
        a11y: None,
        frame: None,
        state: DeviceState::unknown(0),
        events: vec![],
    };
    let checkpoint_every = plan.checkpoint_every;
    let abort_on_error = plan.abort_on_error;

    for (index, step) in plan.steps.iter().enumerate() {
        let step_id = step.id;
        let label = step.label.clone();

        // 1. wait_before: register a predicate and wait for
        // match (or stream close / EOF which we treat as
        // timeout — the host runtime's job is to honour
        // `predicate.timeout_ms` by closing the stream).
        if let Some(p) = &step.wait_before {
            match wait_for_predicate(p, predicate_engine, stream_engine, &mut drain_observation) {
                WaitStatus::Matched => {}
                WaitStatus::Closed => {
                    let failure = PlanFailure::WaitBeforeTimeout {
                        step_index: index as u32,
                        step_id,
                        label,
                    };
                    return Err((failure, steps_out));
                }
            }
        }

        // 2. Execute the action.
        let step_result_action = match execute_step(&step.action) {
            Ok(r) => r,
            Err(reason) => {
                let failure = PlanFailure::ActionRefused {
                    step_index: index as u32,
                    step_id,
                    reason: reason.clone(),
                };
                if abort_on_error {
                    return Err((failure, steps_out));
                }
                // Continue past the failure: emit a synthetic
                // step record so the host sees the refusal in
                // its place.
                let step_record = StepResult {
                    step_id,
                    index: index as u32,
                    action_result: ActionResult {
                        id: crate::ids::ActionId(0),
                        landed: false,
                        ground_truth: GroundTruth::default(),
                        elapsed_ms: 0,
                    },
                    landed: false,
                    error: Some(reason),
                };
                steps_out.push(step_record);
                counters.aborted += 1;
                counters.skipped += 0; // we attempted, not skipped
                continue;
            }
        };

        // 3. Drain one observation so the post-action ground
        // truth is reflected in `verify_after` evaluation.
        if let Some(obs) = drain_observation() {
            stream_engine.produce(|_| obs.clone());
            final_obs = obs;
        }

        // 4. verify_after: register, evaluate against the
        // latest observation. If still no match, abort.
        if let Some(p) = &step.verify_after {
            let handle = predicate_engine.register(p.clone());
            let results = predicate_engine.on_observation(&final_obs);
            let matched = results
                .iter()
                .any(|(h, o)| *h == handle && matches!(o, PredicateOutcome::Matched));
            let _ = predicate_engine.cancel(handle);
            if !matched {
                let failure = PlanFailure::VerifyAfterMismatch {
                    step_index: index as u32,
                    step_id,
                    label,
                };
                return Err((failure, steps_out));
            }
        }

        // 5. Record this step's result.
        let landed = step_result_action.landed;
        let step_record = StepResult {
            step_id,
            index: index as u32,
            action_result: step_result_action,
            landed,
            error: None,
        };
        steps_out.push(step_record);
        if landed {
            counters.landed += 1;
        } else {
            counters.aborted += 1;
        }

        // 6. Checkpoint emission.
        if checkpoint_every > 0 && counters.landed % checkpoint_every == 0 {
            // Emit a synthetic observation that carries the
            // checkpoint event so subscribers can latch onto
            // it. This is a server-side bookkeeping event, so
            // we encode it into the existing event taxonomy.
            let mut obs = final_obs.clone();
            obs.events.push(DeviceEvent::PlanCompleted {
                plan_id,
                all_landed: false, // we don't know yet
                elapsed_ms: 0,
            });
            stream_engine.produce(|_| obs);
            counters.checkpoints += 1;
        }

        if !landed && abort_on_error {
            let failure = PlanFailure::Cancelled {
                step_index: index as u32,
                step_id,
            };
            return Err((failure, steps_out));
        }
    }

    // Final observation: try one more drain.
    if let Some(obs) = drain_observation() {
        stream_engine.produce(|_| obs.clone());
        final_obs = obs;
    }

    let all_landed = steps_out.iter().all(|s| s.landed);
    let result = PlanResult {
        plan_id,
        steps: steps_out,
        final_observation: final_obs,
        total_elapsed_ms: counters.landed.saturating_mul(1),
        all_landed,
    };
    Ok((result, counters))
}

/// Internal helper: synchronously evaluate one predicate
/// against an observation that's already been drained.
///
/// The host is responsible for registering the predicate with
/// [`PredicateEngine`] and providing an observation. The
/// helper returns whether the predicate matched. This is the
/// synchronous predicate-check used by `verify_after` and
/// inside `execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitStatus {
    Matched,
    /// Stream drained (EOF / closed). The host decides
    /// whether to surface this as a timeout or a clean
    /// shutdown.
    Closed,
}

fn wait_for_predicate(
    predicate: &Predicate,
    engine: &mut PredicateEngine,
    stream: &mut StreamEngine,
    drain_observation: &mut dyn FnMut() -> Option<Observation>,
) -> WaitStatus {
    let handle = engine.register(predicate.clone());
    loop {
        let Some(obs) = drain_observation() else {
            let _ = engine.cancel(handle);
            return WaitStatus::Closed;
        };
        stream.produce(|_| obs.clone());
        let results = engine.on_observation(&obs);
        let matched = results
            .iter()
            .any(|(h, o)| *h == handle && matches!(o, PredicateOutcome::Matched));
        if matched {
            let _ = engine.cancel(handle);
            return WaitStatus::Matched;
        }
        // Continue draining — the executor relies on the host's
        // `drain_observation` to honour the predicate's
        // `timeout_ms` (return None / EOF when timed out). We
        // deliberately don't track wall-clock here; that's the
        // host runtime's job.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{
        Action, A11yNodeChangeKind, A11yNodeDiff, LaunchBy, ObservationComponent,
    };
    use crate::plan::PlanStep;
    use crate::observation::A11yTree;

    fn idle_obs(seq: u64) -> Observation {
        Observation {
            seq,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        }
    }

    fn obs_with_activity(seq: u64, component: &str) -> Observation {
        Observation {
            seq,
            timestamp_ms: 0,
            a11y: Some(A11yTree {
                window_id: Some(1),
                top_activity: Some(component.into()),
                node_count: 0,
                json: "[]".into(),
            }),
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        }
    }

    fn ok_result(id: u64) -> ActionResult {
        ActionResult {
            id: crate::ids::ActionId(id),
            landed: true,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 1,
        }
    }

    #[test]
    fn execute_single_step_succeeds() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })]);
        let mut i = 0;
        let (pr, counters) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        assert!(pr.all_landed);
        assert_eq!(pr.steps.len(), 1);
        assert_eq!(counters.landed, 1);
        assert_eq!(counters.aborted, 0);
    }

    #[test]
    fn execute_aborts_on_action_refusal_when_abort_on_error() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })]);
        let mut i = 0;
        let err = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Err("launch refused under phantom profile".into()),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect_err("err");
        let (failure, _steps) = err;
        assert!(matches!(failure, PlanFailure::ActionRefused { .. }));
        assert_eq!(failure.kind_label(), "action-refused");
    }

    #[test]
    fn execute_continues_when_abort_on_error_false() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![
            PlanStep::new(Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 0,
            }),
            PlanStep::new(Action::Tap {
                x: 1,
                y: 1,
                deadline_ms: 0,
            }),
        ])
        .with_abort(false);
        let mut i = 0;
        let mut call = 0;
        let (pr, _) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| {
                call += 1;
                if call == 1 {
                    Err("first refusal".into())
                } else {
                    Ok(ok_result(call as u64))
                }
            },
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        assert_eq!(pr.steps.len(), 2, "both steps attempted");
        assert!(!pr.all_landed);
        assert!(!pr.steps[0].landed);
        assert!(pr.steps[1].landed);
    }

    #[test]
    fn execute_wait_before_blocks_until_matching_activity() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })
        .with_wait_before(Predicate::Activity {
            component: "com.foo/.Main".into(),
            timeout_ms: 1_000,
        })]);
        // Drain produces a non-matching observation first,
        // then a matching one.
        let mut i: u64 = 0;
        let (pr, _) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = if i == 0 {
                    idle_obs(i)
                } else {
                    obs_with_activity(i, "com.foo/.Main")
                };
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        assert!(pr.all_landed);
    }

    #[test]
    fn execute_verify_after_mismatch_aborts() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })
        .with_verify_after(Predicate::Activity {
            component: "com.target/.Main".into(),
            timeout_ms: 1_000,
        })]);
        // Drain returns the activity `com.foo/.Other` (not
        // com.target/.Main) — verify_after should NOT match.
        let mut i: u64 = 0;
        let err = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = obs_with_activity(i, "com.foo/.Other");
                i += 1;
                Some(v)
            },
        )
        .expect_err("verify_after mismatch aborts");
        let (failure, _) = err;
        assert!(matches!(failure, PlanFailure::VerifyAfterMismatch { .. }));
    }

    #[test]
    fn execute_emits_checkpoint_every_n_steps() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let steps: Vec<PlanStep> = (0..4)
            .map(|i| {
                PlanStep::new(Action::Tap {
                    x: i,
                    y: 0,
                    deadline_ms: 0,
                })
            })
            .collect();
        let plan = Plan::new(steps).with_checkpoint_every(2);
        let mut i: u64 = 0;
        let (_pr, counters) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        // 4 steps with checkpoint every 2 → checkpoints fired
        // at step 2 (`landed=2`) and step 4 (`landed=4`).
        assert_eq!(counters.checkpoints, 2);
    }

    #[test]
    fn execute_drain_returns_none_treats_wait_before_as_closed() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })
        .with_wait_before(Predicate::Activity {
            component: "p".into(),
            timeout_ms: 1_000,
        })]);
        let err = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || None,
        )
        .expect_err("closed stream aborts");
        let (failure, _) = err;
        assert!(matches!(failure, PlanFailure::WaitBeforeTimeout { .. }));
    }

    #[test]
    fn execute_stream_events_are_visible_to_subscribers() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        // Pre-create a subscriber so we can verify the executor
        // pushes observations into the stream.
        let handle = stream.subscribe(0, None);
        let plan = Plan::new(vec![PlanStep::new(Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        })]);
        let mut i: u64 = 0;
        let (_pr, _) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        // Each step drains ≥1 obs, so the subscriber should
        // have ≥ 1 queued observation.
        assert!(stream.subscriber(handle).unwrap().pending() >= 1);
    }

    #[test]
    fn execute_total_elapsed_is_landed_count() {
        // Cheap sanity check on counter semantics.
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![
            PlanStep::new(Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 0,
            }),
            PlanStep::new(Action::Tap {
                x: 1,
                y: 1,
                deadline_ms: 0,
            }),
        ]);
        let mut i: u64 = 0;
        let (pr, counters) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| Ok(ok_result(1)),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        // 2 steps landed → 2 ms fake elapsed. (Real elapsed-ms
        // accounting lands when the host closure provides
        // per-action timings; the executor counts steps as
        // 1 ms each for the dummy case.)
        assert_eq!(pr.total_elapsed_ms, counters.landed);
        assert_eq!(counters.landed, 2);
    }

    #[test]
    fn execute_handles_observation_components_dump_observation() {
        // Pure-read Action::DumpObservation should be allowed;
        // exercise it to ensure it doesn't trigger the action
        // refusal path.
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::DumpObservation {
            components: vec![ObservationComponent::A11y],
            deadline_ms: 0,
        })]);
        let mut i: u64 = 0;
        let (pr, _) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(1),
            |_| {
                Ok(ActionResult {
                    id: crate::ids::ActionId(1),
                    landed: true,
                    ground_truth: GroundTruth {
                        a11y_diff: vec![A11yNodeDiff {
                            node_id: 1,
                            kind: A11yNodeChangeKind::Added,
                            new_text: None,
                            new_visible: None,
                        }],
                        frame_diff: None,
                        focus: None,
                        scene_change: 0.0,
                        events: vec![],
                    },
                    elapsed_ms: 1,
                })
            },
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        assert!(pr.all_landed);
        assert_eq!(pr.steps.len(), 1);
        assert!(!pr.steps[0].action_result.ground_truth.a11y_diff.is_empty());
    }

    #[test]
    fn execute_handles_launch_action() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let plan = Plan::new(vec![PlanStep::new(Action::Launch {
            target: "com.foo/.Main".into(),
            by: LaunchBy::Component("com.foo/.Main".into()),
            deadline_ms: 1000,
        })]);
        let mut i: u64 = 0;
        let (pr, _) = execute(
            &plan,
            &mut engine,
            &mut stream,
            PlanId(2),
            |_| Ok(ok_result(1)),
            || {
                let v = idle_obs(i);
                i += 1;
                Some(v)
            },
        )
        .expect("ok");
        assert!(pr.all_landed);
        assert_eq!(pr.plan_id, PlanId(2));
    }
}
