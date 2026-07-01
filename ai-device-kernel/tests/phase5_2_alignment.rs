//! Tests for Phase 5.2 alignment additions to `ai-device-kernel`
//! (PRD-A: incremental dual-encoding, ADR-0013 / ADR-0014 / ADR-0015).
//!
//! Covers:
//! - `AtomicErrorCode` 6 variants + snake_case JSON ser/de
//! - `ActionResult.code` + `retryable` round-trip
//! - `ForegroundApp` JSON ser/de + `Observation.foreground_app` round-trip
//! - `FrameSnapshot.png` round-trip
//! - `Plan.version` default + explicit override round-trip
//! - `Plan::version_parsed()` returns `semver::Version`
//! - Hand-rolled `JsonSchema` impls for IDs (u64 / u32 / hex32)

use ai_device_kernel::{
    Action, ActionId, ActionResult, AtomicErrorCode, ForegroundApp, FrameSnapshot, GroundTruth,
    Observation, Plan, PlanId, PredicateHandle, ScreenId, StepId, A11yNodeChangeKind, A11yNodeDiff,
    DeviceState,
};
use schemars::schema::Schema;
use serde_json::Value;

// --- AtomicErrorCode -----------------------------------------------------

#[test]
fn atomic_error_code_has_six_variants() {
    use std::collections::HashSet;
    let all = [
        AtomicErrorCode::SelectorMiss,
        AtomicErrorCode::KernelError,
        AtomicErrorCode::Timeout,
        AtomicErrorCode::PermissionDenied,
        AtomicErrorCode::AppNotForeground,
        AtomicErrorCode::NotImplemented,
    ];
    let set: HashSet<_> = all.iter().collect();
    assert_eq!(set.len(), 6, "duplicate enum variants");
}

#[test]
fn atomic_error_code_serializes_to_snake_case() {
    assert_eq!(
        serde_json::to_string(&AtomicErrorCode::SelectorMiss).unwrap(),
        "\"selector_miss\""
    );
    assert_eq!(
        serde_json::to_string(&AtomicErrorCode::AppNotForeground).unwrap(),
        "\"app_not_foreground\""
    );
    assert_eq!(
        serde_json::to_string(&AtomicErrorCode::NotImplemented).unwrap(),
        "\"not_implemented\""
    );
}

#[test]
fn atomic_error_code_as_str_matches_json() {
    for code in [
        AtomicErrorCode::SelectorMiss,
        AtomicErrorCode::KernelError,
        AtomicErrorCode::Timeout,
        AtomicErrorCode::PermissionDenied,
        AtomicErrorCode::AppNotForeground,
        AtomicErrorCode::NotImplemented,
    ] {
        let json = serde_json::to_string(&code).unwrap();
        let expected = format!("\"{}\"", code.as_str());
        assert_eq!(json, expected, "as_str/json mismatch for {code:?}");
    }
}

// --- ActionResult.code + retryable --------------------------------------

#[test]
fn action_result_default_has_no_code_and_retryable_false() {
    let r = ActionResult::default();
    assert_eq!(r.code, None);
    assert!(!r.retryable);
}

#[test]
fn action_result_with_code_round_trips_postcard() {
    let r = ActionResult {
        id: ActionId(7),
        landed: false,
        ground_truth: GroundTruth::default(),
        elapsed_ms: 12,
        code: Some(AtomicErrorCode::SelectorMiss),
        retryable: true,
    };
    let bytes = postcard::to_allocvec(&r).expect("encode");
    let decoded: ActionResult = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(decoded.code, Some(AtomicErrorCode::SelectorMiss));
    assert!(decoded.retryable);
    assert_eq!(decoded, r);
}

#[test]
fn action_result_with_code_round_trips_json() {
    let r = ActionResult {
        id: ActionId(8),
        landed: false,
        ground_truth: GroundTruth::default(),
        elapsed_ms: 5,
        code: Some(AtomicErrorCode::AppNotForeground),
        retryable: false,
    };
    let v: Value = serde_json::to_value(&r).unwrap();
    assert_eq!(v["code"], "app_not_foreground");
    assert_eq!(v["retryable"], false);
    let decoded: ActionResult = serde_json::from_value(v).unwrap();
    assert_eq!(decoded, r);
}

#[test]
fn action_result_is_structured_failure_when_landed_false_and_code_set() {
    let r = ActionResult {
        code: Some(AtomicErrorCode::Timeout),
        retryable: true,
        ..ActionResult::default()
    };
    assert!(r.is_structured_failure());
    let r2 = ActionResult { landed: true, ..r.clone() };
    assert!(!r2.is_structured_failure());
    let r3 = ActionResult { code: None, ..r };
    assert!(!r3.is_structured_failure());
}

// --- ForegroundApp -------------------------------------------------------

#[test]
fn foreground_app_serializes_json_with_snake_case() {
    let app = ForegroundApp {
        package: "com.example".into(),
        activity: ".MainActivity".into(),
        focused: true,
        since_ms: 1234,
    };
    let v: Value = serde_json::to_value(&app).unwrap();
    assert_eq!(v["package"], "com.example");
    assert_eq!(v["activity"], ".MainActivity");
    assert_eq!(v["focused"], true);
    assert_eq!(v["since_ms"], 1234);
    let decoded: ForegroundApp = serde_json::from_value(v).unwrap();
    assert_eq!(decoded, app);
}

#[test]
fn foreground_app_component_format() {
    let app = ForegroundApp {
        package: "com.example".into(),
        activity: ".MainActivity".into(),
        focused: true,
        since_ms: 0,
    };
    assert_eq!(app.component(), "com.example/.MainActivity");
}

#[test]
fn observation_with_foreground_app_round_trips() {
    let app = ForegroundApp {
        package: "com.example".into(),
        activity: ".MainActivity".into(),
        focused: true,
        since_ms: 42,
    };
    let obs = Observation {
        seq: 1,
        foreground_app: Some(app.clone()),
        ..Observation::default()
    };
    let bytes = postcard::to_allocvec(&obs).expect("encode obs");
    let decoded: Observation = postcard::from_bytes(&bytes).expect("decode obs");
    assert_eq!(decoded.foreground_app, Some(app));
}

// --- FrameSnapshot.png ---------------------------------------------------

#[test]
fn frame_snapshot_png_none_round_trips_postcard() {
    let f = FrameSnapshot {
        width: 1080,
        height: 1920,
        codec: 1,
        is_keyframe: true,
        pts: 90000,
        scene_change_score: 0.5,
        png: None,
    };
    let bytes = postcard::to_allocvec(&f).expect("encode frame");
    let decoded: FrameSnapshot = postcard::from_bytes(&bytes).expect("decode frame");
    assert_eq!(decoded.png, None);
    assert_eq!(decoded, f);
}

#[test]
fn frame_snapshot_png_some_round_trips() {
    let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG magic
    let f = FrameSnapshot {
        width: 1080,
        height: 1920,
        codec: 2,
        is_keyframe: false,
        pts: 180000,
        scene_change_score: 0.0,
        png: Some(png.clone()),
    };
    let bytes = postcard::to_allocvec(&f).expect("encode frame png");
    let decoded: FrameSnapshot = postcard::from_bytes(&bytes).expect("decode frame png");
    assert_eq!(decoded.png, Some(png));
}

// --- Plan.version --------------------------------------------------------

#[test]
fn plan_new_uses_default_version() {
    let p = Plan::new(vec![]);
    assert_eq!(p.version, "0.1.0");
    assert!(p.version_parsed().is_some());
}

#[test]
fn plan_version_round_trips_postcard() {
    let p = Plan {
        version: "0.2.0-rc.1".into(),
        id: PlanId(1),
        steps: vec![],
        abort_on_error: true,
        checkpoint_every: 0,
    };
    let bytes = postcard::to_allocvec(&p).expect("encode plan");
    let decoded: Plan = postcard::from_bytes(&bytes).expect("decode plan");
    assert_eq!(decoded.version, "0.2.0-rc.1");
    assert_eq!(decoded.version_parsed().unwrap().to_string(), "0.2.0-rc.1");
}

#[test]
fn plan_version_round_trips_json() {
    let p = Plan {
        version: "0.1.0".into(),
        id: PlanId(1),
        steps: vec![],
        abort_on_error: true,
        checkpoint_every: 0,
    };
    let v: Value = serde_json::to_value(&p).unwrap();
    assert_eq!(v["version"], "0.1.0");
    let decoded: Plan = serde_json::from_value(v).unwrap();
    assert_eq!(decoded.version, "0.1.0");
}

#[test]
fn plan_version_parsed_handles_garbage() {
    let p = Plan {
        version: "not-semver".into(),
        id: PlanId::ZERO,
        steps: vec![],
        abort_on_error: true,
        checkpoint_every: 0,
    };
    assert!(p.version_parsed().is_none());
}

// --- JsonSchema impls for IDs (smoke) -----------------------------------

#[test]
fn id_json_schemas_compile() {
    use schemars::JsonSchema;
    let mut gen = schemars::gen::SchemaGenerator::default();
    let _a: Schema = gen.subschema_for::<ActionId>();
    let _p: Schema = gen.subschema_for::<PlanId>();
    let _s: Schema = gen.subschema_for::<StepId>();
    let _h: Schema = gen.subschema_for::<PredicateHandle>();
    let _id: Schema = gen.subschema_for::<ScreenId>();
    // schema_name is reachable via the trait
    assert_eq!(<ActionId as JsonSchema>::schema_name(), "ActionId");
    assert_eq!(<PlanId as JsonSchema>::schema_name(), "PlanId");
    assert_eq!(<StepId as JsonSchema>::schema_name(), "StepId");
    assert_eq!(<PredicateHandle as JsonSchema>::schema_name(), "PredicateHandle");
    assert_eq!(<ScreenId as JsonSchema>::schema_name(), "ScreenId");
}

// --- Observation w/ DeviceState default keeps backward compat ----------

#[test]
fn observation_default_has_no_foreground_app() {
    let obs = Observation::default();
    assert_eq!(obs.foreground_app, None);
    assert_eq!(obs.state, DeviceState::unknown(0));
}

#[test]
fn ground_truth_default_remains_empty() {
    let gt = GroundTruth::default();
    assert!(gt.a11y_diff.is_empty());
    assert!(gt.frame_diff.is_none());
    assert!(gt.focus.is_none());
}

#[test]
fn a11y_node_diff_serializes_change_kind_as_snake_case() {
    let d = A11yNodeDiff {
        node_id: 42,
        kind: A11yNodeChangeKind::TextChanged,
        new_text: Some("hi".into()),
        new_visible: None,
    };
    let v: Value = serde_json::to_value(&d).unwrap();
    assert_eq!(v["kind"], "text_changed");
    let decoded: A11yNodeDiff = serde_json::from_value(v).unwrap();
    assert_eq!(decoded, d);
}

// --- ensure Action is JsonSchema'd (was the source of many derives) ---

#[test]
fn action_schema_compiles() {
    use schemars::JsonSchema;
    let mut gen = schemars::gen::SchemaGenerator::default();
    let _ = gen.subschema_for::<Action>();
}
