//! Predicate wait helpers — see v3 §3.2.4 + Phase 2 §5.
//!
//! The predicate engine itself is *pure logic* over `&mut self`
//! — `no_polling_lints` forbids `thread::sleep`, `Instant::now`,
//! etc. inside `predicate_engine.rs`. But the *host's* driver can
//! use [`std::sync::mpsc`] channels and time-based cancellation;
//! this module is where that lives.
//!
//! ## Design
//!
//! - **`wait_for(handle, predicate_engine, stream_engine, timeout_ms)`**
//!   — registers the predicate with the engine, drives
//!   observations into both engines, returns when matched or
//!   timed out. Cooperative: the host passes in a closure
//!   `produce_one()` that is expected to deliver a single
//!   observation (or `None` for EOF).
//! - **0 polling**: the helper is driven by the host's closure,
//!   not by sleep-and-check. The host's runtime typically
//!   uses a blocking read on its socket (not us) to wake
//!   when bytes arrive; we just glue the bytes into the
//!   engine each turn.
//! - **Timeout via deadline**: the helper returns when
//!   `deadline_ms` has elapsed since `start_instant`. Deadline
//!   checks happen at the top of the loop iteration, not via
//!   sleep — they're cheap `Instant::elapsed` returns.

use std::time::{Duration, Instant};

use crate::observation::Observation;
use crate::predicate::Predicate;
use crate::predicate_engine::PredicateEngine;
use crate::stream_engine::StreamEngine;

/// One predicate-wait attempt's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Predicate matched within the deadline. Wrapper around
    /// [`PredicateResult::Matched`] with the elapsed ms.
    Matched {
        elapsed: Duration,
    },
    /// Predicate timed out.
    Timeout {
        elapsed: Duration,
    },
    /// Stream exhausted before the predicate could match.
    StreamClosed {
        elapsed: Duration,
    },
}

/// Drive the predicate engine until it sees a match (or the
/// timeout fires / the producer signals EOF).
///
/// `produce_one` should return `Some(obs)` for a fresh
/// observation, or `None` if the upstream is closed and the
/// wait should bail.
///
/// `predicate_engine` and `stream_engine` are both updated
/// each iteration. The host typically uses one engine for
/// stream fan-out and the other for predicate registration;
/// we wire both here so the helper is the right shape for
/// the v3 §1 architecture.
pub fn wait_for<P>(
    predicate: &Predicate,
    predicate_engine: &mut PredicateEngine,
    stream_engine: &mut StreamEngine,
    timeout: Duration,
    mut produce_one: P,
) -> WaitOutcome
where
    P: FnMut() -> Option<Observation>,
{
    let start = Instant::now();
    let handle = predicate_engine.register(predicate.clone());
    let deadline = start + timeout;

    loop {
        // Fan the next observation through both engines.
        let Some(obs) = produce_one() else {
            let _ = predicate_engine.cancel(handle);
            return WaitOutcome::StreamClosed {
                elapsed: start.elapsed(),
            };
        };
        // Drive stream first (bumped seq) so subscriber joins
        // see the same observation that the predicate engine
        // sees.
        stream_engine.produce(|_| obs.clone());
        let results = predicate_engine.on_observation(&obs);
        // Match against the predicate's handle.
        for (h, outcome) in results {
            if h == handle
                && matches!(outcome, crate::predicate_engine::PredicateOutcome::Matched)
            {
                let _ = predicate_engine.cancel(handle);
                return WaitOutcome::Matched {
                    elapsed: start.elapsed(),
                };
            }
        }
        // Deadline check — at the top of the loop, not via
        // sleep. `Instant::elapsed` is monotonic; safe across
        // clock adjustments.
        if Instant::now() >= deadline {
            let _ = predicate_engine.cancel(handle);
            return WaitOutcome::Timeout {
                elapsed: start.elapsed(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::DeviceState;
    use crate::predicate::EventKind;

    fn dummy_obs(seq: u64) -> Observation {
        Observation {
            seq,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        }
    }

    fn obs_with_event(seq: u64, ev: crate::observation::DeviceEvent) -> Observation {
        Observation {
            seq,
            timestamp_ms: 0,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![ev],
        }
    }

    #[test]
    fn wait_for_returns_matched_when_event_fires() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 1_000,
        };
        let ev = crate::observation::DeviceEvent::ActivityResumed {
            component: "p/.a".into(),
        };
        // Two observations; the second carries the event.
        let mut iter = 0..;
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(500),
            || match iter.next() {
                Some(0) => Some(obs_with_event(0, ev.clone())),
                Some(_) => None, // EOF
                None => None,
            },
        );
        assert!(matches!(outcome, WaitOutcome::Matched { .. }));
    }

    #[test]
    fn wait_for_times_out_when_event_never_fires() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::EventFires {
            kind: EventKind::SceneChangeDetected,
            timeout_ms: 1_000,
        };
        // Producer is effectively infinite: keep emitting
        // dummy observations. Deadline (50 ms) fires before
        // the predicate can ever match because the dummy
        // observations never carry the matching event.
        let mut i: u64 = 0;
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(50),
            || {
                let v = dummy_obs(i);
                i = i.wrapping_add(1);
                Some(v)
            },
        );
        assert!(matches!(outcome, WaitOutcome::Timeout { .. }));
    }

    #[test]
    fn wait_for_returns_closed_when_producer_eof() {
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 1_000,
        };
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(500),
            || None,
        );
        assert!(matches!(outcome, WaitOutcome::StreamClosed { .. }));
    }

    #[test]
    fn wait_for_does_not_double_register_after_cancel() {
        // After returning, the handle is cancelled; the engine
        // doesn't keep the predicate alive.
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 1_000,
        };
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(50),
            || None,
        );
        assert!(matches!(outcome, WaitOutcome::StreamClosed { .. }));
        assert_eq!(engine.len(), 0, "predicate must be cancelled on exit");
    }

    #[test]
    fn wait_for_handles_long_timeout_via_bounded_obs() {
        // Producer is effectively infinite; deadline fires
        // before the predicate matches because the dummy
        // observations never carry the matched text.
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::TextAppears {
            text: "needle".into(),
            node_id: None,
            timeout_ms: 1_000,
        };
        let mut i: u64 = 0;
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(50),
            || {
                let v = dummy_obs(i);
                i = i.wrapping_add(1);
                Some(v)
            },
        );
        assert!(matches!(outcome, WaitOutcome::Timeout { .. }));
    }

    #[test]
    fn stream_engine_seq_advances_during_wait() {
        // Sanity: the wait_for helper produces observations
        // through the stream engine too, not just the
        // predicate engine. Producer hands out 2 non-matching
        // obs then a matching one; predicate matches on the
        // third.
        let mut engine = PredicateEngine::new();
        let mut stream = StreamEngine::new();
        let pred = Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 1_000,
        };
        let matching = crate::observation::DeviceEvent::ActivityResumed {
            component: "p/.a".into(),
        };
        let non_matching = crate::observation::DeviceEvent::SceneChangeDetected {
            score: 0.0,
        };
        let mut i = 0;
        let outcome = wait_for(
            &pred,
            &mut engine,
            &mut stream,
            Duration::from_millis(500),
            || {
                let ev = if i < 2 {
                    non_matching.clone()
                } else if i == 2 {
                    matching.clone()
                } else {
                    return None;
                };
                let v = obs_with_event(i, ev);
                i += 1;
                Some(v)
            },
        );
        // Matched on the third emitted observation (seq=2).
        assert!(matches!(outcome, WaitOutcome::Matched { .. }));
        // Three observations produced through both engines.
        assert_eq!(stream.head_seq(), Some(2));
        assert_eq!(engine.len(), 0, "predicate must be cancelled after match");
    }
}
