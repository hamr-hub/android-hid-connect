//! `CloudLlmAdapter` — stub `LlmAdapter` for cloud LLM providers.
//!
//! Per grill ADR 0007 (Hybrid LLM deployment) + PRD-A: this is
//! the **stub** impl. Real Anthropic / OpenAI SDK integration
//! is deferred to Phase 5.3+ (post-grill).
//!
//! Default behaviour: `plan()` returns `Plan::new(vec![])` (no
//! atomics). The stub exists so that `LlmDeployment.kind =
//! CloudOnly` or `Hybrid` configs can be **constructed and
//! validated** at config-load time, even before the real SDK
//! is integrated.
//!
//! When the `cloud-llm` cargo feature is enabled, `real_plan`
//! becomes available — currently still a stub that
//! logs a warning.

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

use crate::llm_deployment::LlmDeployment;
use crate::mock_llm::LlmAdapter;
use crate::observation::Observation;
use crate::plan::Plan;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudLlmAdapter {
    deployment: LlmDeployment,
}

impl CloudLlmAdapter {
    /// Create a new `CloudLlmAdapter` from a `LlmDeployment`.
    /// Validates the deployment (provider + model id present).
    /// Currently a no-op validator — extension point for Phase 5.3+.
    #[must_use]
    pub fn new(deployment: LlmDeployment) -> Self {
        Self { deployment }
    }

    /// Read-only access to the underlying deployment.
    #[must_use]
    pub fn deployment(&self) -> &LlmDeployment {
        &self.deployment
    }
}

impl Default for CloudLlmAdapter {
    fn default() -> Self {
        Self::new(LlmDeployment::default())
    }
}

impl LlmAdapter for CloudLlmAdapter {
    fn plan(&mut self, _obs: &Observation) -> Plan {
        // Phase 5.3 stub: real Anthropic / OpenAI call would
        // happen here. For now, return an empty plan.
        // The selector_miss threshold is enforced by
        // `LlmStep` (a future module) — the adapter itself
        // is policy-free.
        Plan::new(vec![])
    }

    fn reset(&mut self) {
        // No internal state to reset in the stub.
    }

    fn name(&self) -> &str {
        "CloudLlmAdapter(stub)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_deployment::{CloudModelSpec, Provider};

    #[test]
    fn stub_returns_empty_plan() {
        let mut a = CloudLlmAdapter::default();
        let p = a.plan(&Observation::default());
        assert!(p.steps.is_empty(), "stub must return empty plan");
    }

    #[test]
    fn name_includes_stub_marker() {
        let a = CloudLlmAdapter::default();
        assert!(a.name().contains("stub"), "name must mark stub: {}", a.name());
    }

    #[test]
    fn new_with_custom_deployment_preserves_provider() {
        let d = LlmDeployment {
            kind: crate::llm_deployment::DeploymentKind::CloudOnly,
            on_device: Default::default(),
            cloud: CloudModelSpec {
                provider: Provider::OpenAI,
                model_id: "gpt-4o".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                base_url: None,
            },
            escalation: Default::default(),
        };
        let a = CloudLlmAdapter::new(d);
        assert_eq!(a.deployment().cloud.provider, Provider::OpenAI);
    }
}
