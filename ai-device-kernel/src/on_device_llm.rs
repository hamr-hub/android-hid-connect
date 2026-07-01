//! `OnDeviceLlm` — stub `LlmAdapter` for on-device models.
//!
//! Per grill ADR 0007 (Hybrid LLM deployment) + PRD-A: real
//! Phi-3 / Gemma-2 model loading is deferred to Phase 5.3+.
//! The stub returns `Plan::new(vec![])` for any observation.
//!
//! `OnDeviceLlm` is a *sub-trait* of `LlmAdapter` that adds a
//! `model_spec()` accessor for the LLM-deployment config.

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

use crate::llm_deployment::ModelSpec;
use crate::mock_llm::LlmAdapter;
use crate::observation::Observation;
use crate::plan::Plan;

/// A `LlmAdapter` that runs **on-device** (no network egress).
pub trait OnDeviceLlm: LlmAdapter {
    /// The model spec (name, context window, weights path).
    fn model_spec(&self) -> &ModelSpec;
}

/// Stub `OnDeviceLlm` — returns empty plans. Real model loading
/// is a Phase 5.3+ follow-up.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StubOnDeviceLlm {
    spec: ModelSpec,
}

impl StubOnDeviceLlm {
    #[must_use]
    pub fn new(spec: ModelSpec) -> Self {
        Self { spec }
    }
}

impl Default for StubOnDeviceLlm {
    fn default() -> Self {
        Self::new(ModelSpec::default())
    }
}

impl LlmAdapter for StubOnDeviceLlm {
    fn plan(&mut self, _obs: &Observation) -> Plan {
        // Phase 5.3 stub: real Phi-3 / Gemma-2 inference would
        // happen here. For now, return an empty plan.
        Plan::new(vec![])
    }

    fn reset(&mut self) {
        // No internal state in the stub.
    }

    fn name(&self) -> &str {
        "StubOnDeviceLlm"
    }
}

impl OnDeviceLlm for StubOnDeviceLlm {
    fn model_spec(&self) -> &ModelSpec {
        &self.spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_empty_plan() {
        let mut a = StubOnDeviceLlm::default();
        let p = a.plan(&Observation::default());
        assert!(p.steps.is_empty());
    }

    #[test]
    fn model_spec_accessible() {
        let a = StubOnDeviceLlm::default();
        let spec = a.model_spec();
        assert!(!spec.name.is_empty());
        assert!(spec.context_window > 0);
    }
}
