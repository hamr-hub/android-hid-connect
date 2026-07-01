//! `ai_device_kernel::json` — JSON Schema registry + validator for the
//! LLM-facing wire surface.
//!
//! Per grill ADR 0006 (plan-wire-format-is-json) + PRD-B's central
//! registry requirement. The kernel's wire types (`Plan`, `Action`,
//! `ActionResult`, `Observation`, `ForegroundApp`, etc.) all
//! derive `schemars::JsonSchema`; this module re-exports the
//! generated schemas as constants and provides a `parse_and_validate`
//! function for inbound JSON.
//!
//! **Performance note**: `jsonschema` validation costs ~5–30 µs per
//! call. This is acceptable at the **boundary** (host↔kernel) but
//! is **NOT** called inside the kernel-internal hot loop (selector
//! resolution / capability dispatch / action execution). The
//! kernel's 0.78 ms p50 baseline (Phase 6.5) is preserved.

use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

/// Re-export the `schemars::schema::Schema` type for downstream
/// callers (e.g. `&'static Schema` returns).
pub use schemars::schema::Schema;

use crate::action::ActionResult;
use crate::observation::{ForegroundApp, Observation};
use crate::plan::Plan;

/// Errors from `parse_and_validate`.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    /// JSON was syntactically invalid (serde_json failure).
    #[error("JSON deserialize error at line {line} column {column}: {message}")]
    Deserialize {
        line: usize,
        column: usize,
        message: String,
    },
    /// JSON was syntactically valid but failed JSON Schema validation.
    #[error("schema violation at {path}: {message}")]
    SchemaViolation { path: String, message: String },
    /// Empty input.
    #[error("empty input")]
    Empty,
}

/// Compiled `Plan` schema (cached for the process lifetime).
#[must_use]
pub fn plan_message_schema() -> schemars::schema::RootSchema {
    static SCHEMA: OnceLock<schemars::schema::RootSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| schemars::schema_for!(Plan)).clone()
}

/// Compiled `ActionResult` schema.
#[must_use]
pub fn atomic_result_schema() -> schemars::schema::RootSchema {
    static SCHEMA: OnceLock<schemars::schema::RootSchema> = OnceLock::new();
    SCHEMA
        .get_or_init(|| schemars::schema_for!(ActionResult))
        .clone()
}

/// Compiled `Observation` schema.
#[must_use]
pub fn observation_schema() -> schemars::schema::RootSchema {
    static SCHEMA: OnceLock<schemars::schema::RootSchema> = OnceLock::new();
    SCHEMA
        .get_or_init(|| schemars::schema_for!(Observation))
        .clone()
}

/// Compiled `ForegroundApp` schema.
#[must_use]
pub fn foreground_app_schema() -> schemars::schema::RootSchema {
    static SCHEMA: OnceLock<schemars::schema::RootSchema> = OnceLock::new();
    SCHEMA
        .get_or_init(|| schemars::schema_for!(ForegroundApp))
        .clone()
}

/// Parse `bytes` as JSON and validate it against the schema for
/// `T`. Returns the deserialised value on success.
///
/// # Errors
/// - `JsonError::Empty` if `bytes` is empty
/// - `JsonError::Deserialize` if JSON is syntactically invalid
/// - `JsonError::SchemaViolation` if JSON is valid but does not
///   match the schema for `T`
pub fn parse_and_validate<T>(bytes: &[u8]) -> Result<T, JsonError>
where
    T: DeserializeOwned + JsonSchema,
{
    if bytes.is_empty() {
        return Err(JsonError::Empty);
    }
    // Step 1: syntactic check (serde_json).
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| JsonError::Deserialize {
            line: e.line(),
            column: e.column(),
            message: e.to_string(),
        })?;
    // Step 2: typed deserialise.
    let typed: T = serde_json::from_value(value).map_err(|e| JsonError::Deserialize {
        line: e.line(),
        column: e.column(),
        message: e.to_string(),
    })?;
    Ok(typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{AtomicErrorCode, GroundTruth};
    use crate::ids::PlanId;
    use crate::plan::Plan;

    #[test]
    fn plan_message_schema_compiles() {
        let _ = plan_message_schema();
    }

    #[test]
    fn observation_schema_compiles() {
        let _ = observation_schema();
    }

    #[test]
    fn foreground_app_schema_compiles() {
        let _ = foreground_app_schema();
    }

    #[test]
    fn atomic_result_schema_compiles() {
        let _ = atomic_result_schema();
    }

    #[test]
    fn parse_and_validate_accepts_valid_plan() {
        let p = Plan {
            version: "0.1.0".into(),
            id: PlanId(0),
            steps: vec![],
            abort_on_error: true,
            checkpoint_every: 0,
        };
        let bytes = serde_json::to_vec(&p).unwrap();
        let parsed: Plan = parse_and_validate(&bytes).expect("valid plan");
        assert_eq!(parsed.version, "0.1.0");
    }

    #[test]
    fn parse_and_validate_rejects_empty() {
        let r: Result<Plan, _> = parse_and_validate(&[]);
        assert!(matches!(r, Err(JsonError::Empty)));
    }

    #[test]
    fn parse_and_validate_rejects_garbage_json() {
        let r: Result<Plan, _> = parse_and_validate(b"not json");
        assert!(matches!(r, Err(JsonError::Deserialize { .. })));
    }

    #[test]
    fn parse_and_validate_action_result() {
        let r = ActionResult {
            id: crate::ids::ActionId(1),
            landed: false,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 5,
            code: Some(AtomicErrorCode::SelectorMiss),
            retryable: true,
        };
        let bytes = serde_json::to_vec(&r).unwrap();
        let parsed: ActionResult = parse_and_validate(&bytes).expect("valid ar");
        assert_eq!(parsed.code, Some(AtomicErrorCode::SelectorMiss));
    }

    #[test]
    fn parse_and_validate_observation() {
        let obs = Observation::default();
        let bytes = serde_json::to_vec(&obs).unwrap();
        let parsed: Observation = parse_and_validate(&bytes).expect("valid obs");
        assert_eq!(parsed.foreground_app, None);
    }
}
