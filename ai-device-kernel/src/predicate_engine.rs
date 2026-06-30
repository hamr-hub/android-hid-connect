//! Predicate engine — see v3 §3.2.4.
//!
//! The kernel's predicate engine lets a host register a
//! *declarative* condition (`TextAppears`, `Activity`, …) and
//! only resumes the agent when the condition is satisfied or
//! the timeout fires. This is **event-driven, not poll-driven**
//! (v3 §3.2.4: "0 polling, 0 CPU 浪费 (P4 间接解)").
//!
//! ## Design
//!
//! - **6 variants** matching v3 §3.2.4. Each predicate carries
//!   a `timeout_ms` (backstop, not the usual wait).
//! - **`PredicateEngine::on_observation(obs)`** is the single
//!   event hook. Push every new `Observation` here and the
//!   engine walks the registered predicates, marking each
//!   `Matched` whose content is in the observation.
//! - **`on_event(kind)`** is a fast path for `EventKind` matches
//!   (no need to read the observation's content).
//! - **No network I/O, no select-style loops, no sleeping.**
//!   The engine is pure logic over `&mut self`. The host's
//!   runtime is responsible for delivering events on time; the
//!   engine decides "is this a match" in O(N_predicates).
//!
//! AC-V3-2.3 (`grep` verifies 0 polling): the engine source
//! contains none of the four forbidden timer / net / poll
//! tokens. Verified manually + by the `no_polling_lints`
//! test below.

use std::collections::HashMap;

use crate::ids::PredicateHandle;
use crate::observation::{DeviceEvent, Observation};
use crate::predicate::{EventKind, Predicate};

/// One registered predicate plus its bookkeeping.
#[derive(Debug, Clone)]
pub struct RegisteredPredicate {
    pub handle: PredicateHandle,
    pub predicate: Predicate,
}

impl RegisteredPredicate {
    /// Convenience: copy of the predicate's timeout_ms.
    #[must_use]
    pub fn timeout_ms(&self) -> u32 {
        self.predicate.timeout_ms()
    }

    /// Label of the predicate kind (mirrors `Predicate::kind_label`).
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        self.predicate.kind_label()
    }
}

/// Outcome when the engine evaluates a predicate against one
/// observation. The host interprets these to decide whether to
/// resume the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredicateOutcome {
    /// Predicate evaluated against this observation and **did
    /// not match**. Continue waiting for more events.
    NoMatch,
    /// Predicate evaluated against this observation and **did
    /// match**. Predicate is removed from the registry on the
    /// host's side.
    Matched,
}

/// The predicate engine itself.
#[derive(Debug, Default)]
pub struct PredicateEngine {
    predicates: HashMap<PredicateHandle, RegisteredPredicate>,
    next_handle: u64,
    /// Number of predicates currently registered.
    registered_count: u32,
}

impl PredicateEngine {
    /// Build an empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new predicate, returning its handle.
    pub fn register(&mut self, predicate: Predicate) -> PredicateHandle {
        let handle = PredicateHandle(self.next_handle);
        self.next_handle = self.next_handle.wrapping_add(1);
        self.predicates.insert(
            handle,
            RegisteredPredicate {
                handle,
                predicate,
            },
        );
        self.registered_count = self.registered_count.saturating_add(1);
        handle
    }

    /// Cancel a predicate (returns `true` if it existed).
    pub fn cancel(&mut self, handle: PredicateHandle) -> bool {
        let removed = self.predicates.remove(&handle).is_some();
        if removed {
            self.registered_count = self.registered_count.saturating_sub(1);
        }
        removed
    }

    /// Iterate every registered predicate (immutable).
    pub fn iter(&self) -> impl Iterator<Item = (&PredicateHandle, &RegisteredPredicate)> {
        self.predicates.iter()
    }

    /// Lookup a registered predicate by handle.
    #[must_use]
    pub fn get(&self, handle: PredicateHandle) -> Option<&RegisteredPredicate> {
        self.predicates.get(&handle)
    }

    /// Number of currently-registered predicates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.predicates.len()
    }

    /// `true` iff no predicates are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }

    /// Clear every predicate. The engine is reset to its
    /// `Default::default()` state except for `next_handle`,
    /// which keeps monotonically advancing (idempotency).
    pub fn clear(&mut self) {
        self.predicates.clear();
        self.registered_count = 0;
    }

    /// Evaluate predicates against one new observation. Returns
    /// a list of `(handle, outcome)` pairs. The host is
    /// expected to remove any handle that appears with
    /// `Matched`.
    pub fn on_observation(&self, obs: &Observation) -> Vec<(PredicateHandle, PredicateOutcome)> {
        let mut out = Vec::new();
        for (handle, reg) in &self.predicates {
            let outcome = match &reg.predicate {
                Predicate::TextAppears { text, .. } => {
                    if observation_contains_text(obs, text) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
                Predicate::Activity { component, .. } => {
                    if observation_activity_matches(obs, component) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
                Predicate::SceneStable {
                    duration_ms,
                    timeout_ms,
                } => {
                    if observation_has_scene_stable_event(obs, *duration_ms, *timeout_ms) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
                Predicate::A11yIdle {
                    duration_ms,
                    timeout_ms,
                } => {
                    if observation_has_a11y_idle_event(obs, *duration_ms, *timeout_ms) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
                Predicate::SelectorMatches { selector, .. } => {
                    if observation_selector_matches(obs, selector) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
                Predicate::EventFires { kind, .. } => {
                    if observation_contains_event_kind(obs, *kind) {
                        PredicateOutcome::Matched
                    } else {
                        PredicateOutcome::NoMatch
                    }
                }
            };
            out.push((*handle, outcome));
        }
        out
    }

    /// Fast path: evaluate predicates against a single [`EventKind`].
    /// Useful when the host only knows the kind (e.g. via a
    /// system-tray event hook) and doesn't have a full
    /// observation handy.
    pub fn on_event_kind(&self, kind: EventKind) -> Vec<(PredicateHandle, PredicateOutcome)> {
        let mut out = Vec::new();
        for (handle, reg) in &self.predicates {
            if let Predicate::EventFires { kind: want, .. } = reg.predicate {
                if want == kind {
                    out.push((*handle, PredicateOutcome::Matched));
                    continue;
                }
            }
            out.push((*handle, PredicateOutcome::NoMatch));
        }
        out
    }
}

/// True iff the observation's a11y tree contains `text` as
/// node text. Used by `Predicate::TextAppears`.
fn observation_contains_text(obs: &Observation, text: &str) -> bool {
    let Some(a11y) = &obs.a11y else { return false };
    a11y.json.contains(text)
}

/// True iff the observation's top activity matches `component`.
fn observation_activity_matches(obs: &Observation, component: &str) -> bool {
    let Some(top) = obs.a11y.as_ref().and_then(|a| a.top_activity.as_ref()) else {
        return false;
    };
    top == component
}

/// Heuristic: a SceneStable event is matched if the observation
/// carries a `SceneChangeDetected` event whose score is below
/// 0.05 (i.e., "no change") within `timeout_ms` of daemon
/// start. This is a stub for Phase 2 (the real implementation
/// will fold durations across multiple observations).
fn observation_has_scene_stable_event(obs: &Observation, duration_ms: u32, timeout_ms: u32) -> bool {
    if duration_ms > timeout_ms {
        return false;
    }
    obs.events.iter().any(|ev| {
        if let DeviceEvent::SceneChangeDetected { score } = ev {
            *score < 0.05
        } else {
            false
        }
    })
}

/// Heuristic: A11yIdle is matched if the observation's a11y
/// node_count stays constant across recent observations.
/// Stub; real impl uses stateful diffing in Phase 2.1.
fn observation_has_a11y_idle_event(obs: &Observation, duration_ms: u32, timeout_ms: u32) -> bool {
    if duration_ms > timeout_ms {
        return false;
    }
    obs.events.iter().any(|ev| {
        matches!(ev, DeviceEvent::ConfigurationChanged)
    }) || obs.a11y.as_ref().is_some_and(|a| a.node_count > 0)
}

/// Stub for SelectorMatches: matches if the a11y JSON contains
/// the selector string as a substring. A real impl delegates to
/// `android-hid-agent::selectors::Selector` — wiring that lands
/// when the binary ships.
fn observation_selector_matches(obs: &Observation, selector: &str) -> bool {
    let Some(a11y) = &obs.a11y else { return false };
    a11y.json.contains(selector)
}

/// True iff the observation contains at least one matching
/// `EventKind`.
fn observation_contains_event_kind(obs: &Observation, kind: EventKind) -> bool {
    obs.events.iter().any(|ev| EventKind::from_event(ev) == Some(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{A11yTree, DeviceState};

    fn obs_with_a11y(seq: u64, top_activity: &str, node_count: u32, json: &str) -> Observation {
        Observation {
            seq,
            timestamp_ms: 0,
            a11y: Some(A11yTree {
                window_id: Some(1),
                top_activity: Some(top_activity.into()),
                node_count,
                json: json.into(),
            }),
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![],
        }
    }

    fn obs_with_event(seq: u64, ev: DeviceEvent) -> Observation {
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
    fn register_and_cancel() {
        let mut engine = PredicateEngine::new();
        let h1 = engine.register(Predicate::Activity {
            component: "p/.a".into(),
            timeout_ms: 1000,
        });
        let h2 = engine.register(Predicate::Activity {
            component: "p/.b".into(),
            timeout_ms: 2000,
        });
        assert_eq!(engine.len(), 2);
        assert!(engine.cancel(h1));
        assert_eq!(engine.len(), 1);
        assert!(!engine.cancel(h1), "double-cancel returns false");
        assert!(engine.cancel(h2));
        assert!(engine.is_empty());
    }

    #[test]
    fn handles_are_unique() {
        let mut engine = PredicateEngine::new();
        let h1 = engine.register(Predicate::Activity {
            component: "p".into(),
            timeout_ms: 1000,
        });
        let h2 = engine.register(Predicate::Activity {
            component: "p".into(),
            timeout_ms: 1000,
        });
        assert_ne!(h1, h2);
    }

    #[test]
    fn text_appears_matches_substring() {
        let mut engine = PredicateEngine::new();
        let h = engine.register(Predicate::TextAppears {
            text: "Welcome".into(),
            node_id: None,
            timeout_ms: 5000,
        });
        let obs = obs_with_a11y(0, "p/.a", 5, "[{\"text\":\"Welcome to the app\"}]");
        let results = engine.on_observation(&obs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, h);
        assert_eq!(results[0].1, PredicateOutcome::Matched);
    }

    #[test]
    fn text_appears_no_match_returns_no_match() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::TextAppears {
            text: "Welcome".into(),
            node_id: None,
            timeout_ms: 5000,
        });
        let obs = obs_with_a11y(0, "p/.a", 5, "[{\"text\":\"Hello\"}]");
        let results = engine.on_observation(&obs);
        assert_eq!(results[0].1, PredicateOutcome::NoMatch);
    }

    #[test]
    fn activity_predicate_matches_top_activity() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::Activity {
            component: "com.foo/.Main".into(),
            timeout_ms: 5000,
        });
        let obs_match = obs_with_a11y(0, "com.foo/.Main", 0, "[]");
        let obs_other = obs_with_a11y(0, "com.foo/.Other", 0, "[]");
        assert_eq!(
            engine.on_observation(&obs_match)[0].1,
            PredicateOutcome::Matched
        );
        assert_eq!(
            engine.on_observation(&obs_other)[0].1,
            PredicateOutcome::NoMatch
        );
    }

    #[test]
    fn activity_predicate_no_top_activity_returns_no_match() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::Activity {
            component: "p/.x".into(),
            timeout_ms: 100,
        });
        let obs = obs_with_event(0, DeviceEvent::ActivityResumed {
            component: "p/.x".into(),
        });
        assert_eq!(
            engine.on_observation(&obs)[0].1,
            PredicateOutcome::NoMatch,
            "predicate ignores events-only observations"
        );
    }

    #[test]
    fn scene_stable_matches_low_score_scene_change() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::SceneStable {
            duration_ms: 100,
            timeout_ms: 5000,
        });
        let obs = obs_with_event(0, DeviceEvent::SceneChangeDetected { score: 0.01 });
        assert_eq!(
            engine.on_observation(&obs)[0].1,
            PredicateOutcome::Matched
        );
    }

    #[test]
    fn scene_stable_rejects_high_score() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::SceneStable {
            duration_ms: 100,
            timeout_ms: 5000,
        });
        let obs = obs_with_event(0, DeviceEvent::SceneChangeDetected { score: 0.5 });
        assert_eq!(
            engine.on_observation(&obs)[0].1,
            PredicateOutcome::NoMatch
        );
    }

    #[test]
    fn scene_stable_duration_must_not_exceed_timeout() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::SceneStable {
            duration_ms: 5000,
            timeout_ms: 1000,
        });
        let obs = obs_with_event(0, DeviceEvent::SceneChangeDetected { score: 0.0 });
        assert_eq!(
            engine.on_observation(&obs)[0].1,
            PredicateOutcome::NoMatch,
            "duration_ms > timeout_ms → no match"
        );
    }

    #[test]
    fn event_fires_matches_kind() {
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::EventFires {
            kind: EventKind::ActivityResumed,
            timeout_ms: 1000,
        });
        let obs_match = obs_with_event(
            0,
            DeviceEvent::ActivityResumed {
                component: "p/.x".into(),
            },
        );
        let obs_other = obs_with_event(0, DeviceEvent::SceneChangeDetected { score: 0.1 });
        assert_eq!(
            engine.on_observation(&obs_match)[0].1,
            PredicateOutcome::Matched
        );
        assert_eq!(
            engine.on_observation(&obs_other)[0].1,
            PredicateOutcome::NoMatch
        );
    }

    #[test]
    fn on_event_kind_fast_path() {
        let mut engine = PredicateEngine::new();
        let h = engine.register(Predicate::EventFires {
            kind: EventKind::ActivityPaused,
            timeout_ms: 1000,
        });
        let results = engine.on_event_kind(EventKind::ActivityPaused);
        assert_eq!(results[0].0, h);
        assert_eq!(results[0].1, PredicateOutcome::Matched);

        let results_other = engine.on_event_kind(EventKind::ActivityResumed);
        assert_eq!(results_other[0].1, PredicateOutcome::NoMatch);
    }

    #[test]
    fn multiple_predicates_evaluate_independently() {
        let mut engine = PredicateEngine::new();
        let _activity_h = engine.register(Predicate::Activity {
            component: "com.foo/.Bar".into(),
            timeout_ms: 5000,
        });
        let _event_h = engine.register(Predicate::EventFires {
            kind: EventKind::SceneChangeDetected,
            timeout_ms: 5000,
        });
        let _text_h = engine.register(Predicate::TextAppears {
            text: "Welcome".into(),
            node_id: None,
            timeout_ms: 5000,
        });
        let obs = Observation {
            seq: 0,
            timestamp_ms: 0,
            a11y: Some(A11yTree {
                window_id: Some(1),
                top_activity: Some("com.foo/.Bar".into()),
                node_count: 5,
                json: "[{\"text\":\"Settings panel\"}]".into(),
            }),
            frame: None,
            state: DeviceState::unknown(0),
            events: vec![DeviceEvent::SceneChangeDetected { score: 0.5 }],
        };
        let results = engine.on_observation(&obs);
        assert_eq!(results.len(), 3);
        let matched: Vec<PredicateHandle> = results
            .iter()
            .filter_map(|(h, o)| matches!(o, PredicateOutcome::Matched).then_some(*h))
            .collect();
        // Activity matches, Event (SceneChangeDetected) matches,
        // TextAppears (Welcome) does NOT match.
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn clear_empties_registry_but_preserves_next_handle() {
        let mut engine = PredicateEngine::new();
        let h1 = engine.register(Predicate::Activity {
            component: "p".into(),
            timeout_ms: 1000,
        });
        engine.clear();
        assert!(engine.is_empty());
        let h2 = engine.register(Predicate::Activity {
            component: "q".into(),
            timeout_ms: 1000,
        });
        assert!(
            h2.0 > h1.0,
            "handles must keep advancing across clears"
        );
    }

    #[test]
    fn no_polling_lints() {
        // AC-V3-2.3: predicate engine production source
        // contains no `std::net`, `select!`, `thread::sleep`,
        // or `Instant::now` calls. The check scans only the
        // production module (above `#[cfg(test)]`) so the
        // test's own mention of these tokens doesn't trip a
        // self-match.
        let source = include_str!("predicate_engine.rs");
        // Cut at the start of the test module.
        let production = source.split("#[cfg(test)]").next().unwrap_or("");
        let forbidden = [
            ("std::net", "FORBID_NET"),
            ("select!", "FORBID_SELECT"),
            ("thread::sleep", "FORBID_SLEEP"),
            ("Instant::now", "FORBID_NOW"),
        ];
        for (token, label) in forbidden {
            assert!(
                !production.contains(token),
                "predicate engine production code must be 0-polling (AC-V3-2.3); found `{label}`"
            );
        }
    }

    #[test]
    fn selectors_substring_stub_works() {
        // SelectorMatches has a stub implementation that
        // matches on substring presence in the a11y JSON. The
        // real impl delegates to `android-hid-agent::selectors`;
        // that wires up in Phase 6 (binary).
        let mut engine = PredicateEngine::new();
        let _ = engine.register(Predicate::SelectorMatches {
            selector: "login".into(),
            timeout_ms: 5000,
        });
        let obs_match = obs_with_a11y(
            0,
            "p/.a",
            5,
            "[{\"class\":\"Button\",\"id\":\"login\"}]",
        );
        let obs_other = obs_with_a11y(0, "p/.a", 5, "[{\"id\":\"other\"}]");
        assert_eq!(
            engine.on_observation(&obs_match)[0].1,
            PredicateOutcome::Matched
        );
        assert_eq!(
            engine.on_observation(&obs_other)[0].1,
            PredicateOutcome::NoMatch
        );
    }

    #[test]
    fn registered_predicate_kind_label_carries_through() {
        let mut engine = PredicateEngine::new();
        let h = engine.register(Predicate::SelectorMatches {
            selector: "Button".into(),
            timeout_ms: 5000,
        });
        let reg = engine.get(h).expect("present");
        assert_eq!(reg.kind_label(), "selector-matches");
        assert_eq!(reg.timeout_ms(), 5000);
    }
}
