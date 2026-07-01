//! `LlmDeployment` config — Hybrid LLM deployment per grill ADR 0007.
//!
//! Per PRD-A: a `LlmDeployment` is the on-disk + on-wire config
//! that tells `ai-device-kernel` which LLM to use. Three modes:
//!
//! - `OnDevice` — only the on-device model. **No network egress.**
//! - `Hybrid` (default) — on-device first, escalate to cloud
//!   on `selector_miss` (per `EscalationPolicy`).
//! - `CloudOnly` — no on-device model; everything goes to
//!   cloud. (Useful for testing.)
//!
//! JSON-derivable (per ADR 0012) so it can ship as part of the
//! LLM deployment config wire format.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentKind {
    OnDevice,
    Hybrid,
    CloudOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    OpenAI,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ModelSpec {
    /// Display name (e.g. "Phi-3-mini", "Gemma-2-2B").
    pub name: String,
    /// Context window in tokens.
    pub context_window: u32,
    /// Optional path to weights (None = bundled or default).
    pub weights_path: Option<PathBuf>,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            name: "stub-on-device".into(),
            context_window: 4096,
            weights_path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct CloudModelSpec {
    pub provider: Provider,
    /// Model id (`claude-sonnet-4-20250514` / `gpt-4o`).
    pub model_id: String,
    /// Environment variable name holding the API key.
    /// (Never inline the key in config.)
    pub api_key_env: String,
    /// Optional `base_url` override (e.g. for proxies).
    pub base_url: Option<String>,
}

impl CloudModelSpec {
    /// Default Anthropic config.
    #[must_use]
    pub fn default_anthropic() -> Self {
        Self {
            provider: Provider::Anthropic,
            model_id: "claude-sonnet-4-20250514".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EscalationPolicy {
    /// Number of consecutive `selector_miss` errors that
    /// trigger cloud escalation (per ADR 0008).
    pub selector_miss_threshold: u32,
    /// Whether `Timeout` (per `AtomicErrorCode::Timeout`) also
    /// escalates. Default true.
    pub on_timeout: bool,
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self {
            selector_miss_threshold: 2,
            on_timeout: true,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LlmDeployment {
    pub kind: DeploymentKind,
    pub on_device: ModelSpec,
    pub cloud: CloudModelSpec,
    pub escalation: EscalationPolicy,
}

impl Default for LlmDeployment {
    fn default() -> Self {
        Self {
            kind: DeploymentKind::Hybrid,
            on_device: ModelSpec::default(),
            cloud: CloudModelSpec::default_anthropic(),
            escalation: EscalationPolicy::default(),
        }
    }
}

impl std::fmt::Debug for LlmDeployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmDeployment")
            .field("kind", &self.kind)
            .field("on_device_name", &self.on_device.name)
            .field(
                "cloud",
                &format!("{:?}/{}", self.cloud.provider, self.cloud.model_id),
            )
            .field(
                "escalation_threshold",
                &self.escalation.selector_miss_threshold,
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_hybrid_anthropic() {
        let d = LlmDeployment::default();
        assert_eq!(d.kind, DeploymentKind::Hybrid);
        assert_eq!(d.cloud.provider, Provider::Anthropic);
        assert!(d.cloud.model_id.contains("claude"));
    }

    #[test]
    fn default_escalation_threshold_is_2() {
        let d = LlmDeployment::default();
        assert_eq!(d.escalation.selector_miss_threshold, 2);
        assert!(d.escalation.on_timeout);
    }

    #[test]
    fn model_spec_default_has_sane_context() {
        let m = ModelSpec::default();
        assert!(m.context_window >= 1024);
        assert!(!m.name.is_empty());
    }

    #[test]
    fn deployment_serializes_to_snake_case_json() {
        let d = LlmDeployment::default();
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "hybrid");
        assert_eq!(v["on_device"]["name"], "stub-on-device");
        assert_eq!(v["cloud"]["provider"], "anthropic");
        assert_eq!(v["escalation"]["selector_miss_threshold"], 2);
    }
}
