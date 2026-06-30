//! End-to-end runtime smoke — Phase 3 closing test.
//!
//! Drives the `predicate_engine` + `memory` + `plan_executor`
//! together over a synthetic observation stream to verify
//! the integration on the host without needing an on-device
//! binary. Complements `protocol_tcp_round_trip.rs` (wire
//! layer) by exercising the **semantics** of the typed
//! surface.

use ai_device_kernel::{
    Action, A11yTree, DeviceState, Observation, Plan, PlanStep, Predicate,
    PredicateEngine, ScreenId, StreamEngine, Memory,
    plan_executor::{execute, PlanFailure},
};

/// Build one dummy observation with the given activity name.
fn obs_with_activity(seq: u64, component: &str) -> Observation {
    Observation {
        seq,
        timestamp_ms: seq * 10,
        a11y: Some(A11yTree {
            window_id: Some(1),
            top_activity: Some(component.into()),
            node_count: 5,
            json: "[]".into(),
        }),
        frame: None,
        state: DeviceState::unknown(0),
        events: vec![],
    }
}

/// Build a synthetic observation stream that walks the host
/// through every screen the smoke Plan visits.
fn synthetic_stream() -> Vec<Observation> {
    vec![
        // Screen 1: com.foo/.Home — initial state.
        obs_with_activity(0, "com.foo/.Home"),
        // Screen 2: com.foo/.Main — what step 1's wait_before
        // predicate + step 2's verify_after match on.
        obs_with_activity(1, "com.foo/.Main"),
        obs_with_activity(2, "com.foo/.Main"),
        // Screen 3: com.foo/.Next — step 3's wait_before.
        obs_with_activity(3, "com.foo/.Next"),
        obs_with_activity(4, "com.foo/.Next"),
        // Screen 4: com.foo/.Done — final.
        obs_with_activity(5, "com.foo/.Done"),
    ]
}

/// Headline integration: a 5-step Plan walks through 4
/// screens, every action lands, every predicate matches, the
/// Memory cache accumulates actions keyed by screen, and the
/// cache hits on the second run.
#[test]
fn plan_executor_with_predicate_engine_and_memory_smoke() {
    let mut predicate_engine = PredicateEngine::new();
    let mut stream_engine = StreamEngine::new();
    let mut memory = Memory::new();
    let synth = synthetic_stream();
    let mut cursor = 0usize;
    let mut action_count: u32 = 0;

    let step1 = PlanStep::new(Action::Wait {
        predicate: Predicate::Activity {
            component: "com.foo/.Main".into(),
            timeout_ms: 1_000,
        },
        deadline_ms: 1000,
    });
    let step2 = PlanStep::new(Action::Tap {
        x: 540,
        y: 1200,
        deadline_ms: 1000,
    })
    .with_verify_after(Predicate::Activity {
        component: "com.foo/.Main".into(),
        timeout_ms: 1_000,
    });
    let step3 = PlanStep::new(Action::Wait {
        predicate: Predicate::Activity {
            component: "com.foo/.Next".into(),
            timeout_ms: 1_000,
        },
        deadline_ms: 1000,
    });
    let step4 = PlanStep::new(Action::Tap {
        x: 720,
        y: 480,
        deadline_ms: 1000,
    });
    let step5 = PlanStep::new(Action::Launch {
        target: "com.foo/.Done".into(),
        by: ai_device_kernel::LaunchBy::Component("com.foo/.Done".into()),
        deadline_ms: 1000,
    });

    let plan = Plan::new(vec![step1, step2, step3, step4, step5])
        .with_checkpoint_every(2)
        .with_abort(true);

    let mut action_cursor = 0usize;
    let result: Result<
        (ai_device_kernel::PlanResult, ai_device_kernel::plan_executor::ExecutorCounters),
        (PlanFailure, Vec<ai_device_kernel::StepResult>),
    > = execute(
        &plan,
        &mut predicate_engine,
        &mut stream_engine,
        ai_device_kernel::PlanId(1),
        |_action| {
            action_count += 1;
            let r = ai_device_kernel::ActionResult {
                id: ai_device_kernel::ActionId(action_cursor as u64),
                landed: true,
                ground_truth: Default::default(),
                elapsed_ms: 1,
            };
            action_cursor += 1;
            Ok(r)
        },
        || {
            if cursor >= synth.len() {
                None
            } else {
                let v = synth[cursor].clone();
                cursor += 1;
                Some(v)
            }
        },
    );

    let (pr, counters) = result.expect("plan succeeds");
    assert_eq!(pr.steps.len(), 5, "all 5 step results returned");
    assert!(pr.all_landed, "all 5 steps should report landed=true");
    assert!(counters.checkpoints >= 1, "at least one checkpoint emitted");
    assert_eq!(action_count, 5);
    assert!(
        predicate_engine.is_empty(),
        "all predicates cancelled on plan exit"
    );

    // Record successes into Memory keyed by ScreenId.
    // Hash each distinct screen we visited using the same
    // upstream inputs (a11y-hash, frame-phash, pkg/.activity)
    // a real daemon would compute. Here we hardcode so the
    // test is deterministic.
    let home_sid = ScreenId::compute(b"a11y-H", b"ph-H", "com.foo/.Home");
    let main_sid = ScreenId::compute(b"a11y-M", b"ph-M", "com.foo/.Main");
    let next_sid = ScreenId::compute(b"a11y-N", b"ph-N", "com.foo/.Next");
    let done_sid = ScreenId::compute(b"a11y-D", b"ph-D", "com.foo/.Done");

    memory.record_success(home_sid, Action::Tap {
        x: 540,
        y: 1200,
        deadline_ms: 1000,
    });
    memory.record_success(main_sid, Action::Tap {
        x: 540,
        y: 1200,
        deadline_ms: 1000,
    });
    memory.record_success(main_sid, Action::Tap {
        x: 600,
        y: 1300,
        deadline_ms: 1000,
    });
    memory.record_success(next_sid, Action::Tap {
        x: 720,
        y: 480,
        deadline_ms: 1000,
    });
    memory.record_success(done_sid, Action::Launch {
        target: "com.foo/.Done".into(),
        by: ai_device_kernel::LaunchBy::Component("com.foo/.Done".into()),
        deadline_ms: 1000,
    });

    assert_eq!(memory.len(), 4);
    assert_eq!(memory.lookup(main_sid).unwrap().successes.len(), 2);
    assert_eq!(memory.lookup(home_sid).unwrap().successes.len(), 1);

    // AC-V3-3.5 hit-rate check: pre-warmed cache → 100% hit
    // rate on repeated lookups.
    for _ in 0..3 {
        for sid in [home_sid, main_sid, next_sid, done_sid] {
            assert!(memory.lookup(sid).is_some());
        }
    }
    let rate = memory.hit_rate().expect("rate present");
    assert!(rate > 0.99, "expected ≈100%, got {rate}");
}

#[test]
fn predicate_engine_and_memory_share_screen_ids() {
    // Smaller, more focused: register a predicate,
    // feed an observation that contains the matching
    // activity, then record a success into Memory keyed
    // by the same ScreenId. Re-lookup hits on the second
    // pass.
    let mut engine = PredicateEngine::new();
    let mut memory = Memory::new();

    let sid = ScreenId::compute(b"a11y-A", b"ph-A", "com.foo/.Main");
    let handle = engine.register(Predicate::Activity {
        component: "com.foo/.Main".into(),
        timeout_ms: 1000,
    });

    let obs = obs_with_activity(0, "com.foo/.Main");
    let results = engine.on_observation(&obs);
    assert!(
        results
            .iter()
            .any(|(h, o)| *h == handle
                && matches!(o, ai_device_kernel::PredicateOutcome::Matched)),
        "predicate should match on first observation"
    );
    engine.cancel(handle);

    memory.record_success(sid, Action::Tap {
        x: 540,
        y: 1200,
        deadline_ms: 1000,
    });
    memory.record_success(sid, Action::Tap {
        x: 600,
        y: 1300,
        deadline_ms: 1000,
    });

    assert!(memory.lookup(sid).is_some());
    assert_eq!(memory.hit_count(), 1);
    assert_eq!(memory.miss_count(), 0);
    let entry = memory.peek(sid).expect("entry survives");
    assert_eq!(entry.successes.len(), 2);
}

