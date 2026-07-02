# 0020 — `LlmDeployment` config + Hybrid LLM deployment

## Context

Grill session ADR 0007 mandated Hybrid LLM deployment: on-device first, cloud escalation on `selector_miss` (or other structured failure). Until Phase 5.3, `ai-device-kernel` had no `LlmDeployment` type — the LLM layer was just `LlmAdapter` trait + `MockLlmAdapter` (Phase 5.2 第二批). Real cloud SDK integration is out of scope (Phase 5.3+ follow-up), but the **config surface** can be in scope now.

## Decision

**`LlmDeployment` config type — `ai_device_kernel::llm_deployment::LlmDeployment`**:

```rust
pub enum DeploymentKind { OnDevice, Hybrid, CloudOnly }
pub enum Provider { Anthropic, OpenAI }
pub struct ModelSpec { name: String, context_window: u32, weights_path: Option<PathBuf> }
pub struct CloudModelSpec { provider: Provider, model_id: String, api_key_env: String, base_url: Option<String> }
pub struct EscalationPolicy { selector_miss_threshold: u32, on_timeout: bool }
pub struct LlmDeployment { kind, on_device: ModelSpec, cloud: CloudModelSpec, escalation: EscalationPolicy }
```

- `Default` = `Hybrid { on_device = ModelSpec::default(), cloud = CloudModelSpec::default_anthropic() (claude-sonnet-4-20250514), escalation = { selector_miss_threshold: 2, on_timeout: true } }`
- `EscalationPolicy::selector_miss_threshold = 2` per ADR 0008
- JSON-derivable (per ADR 0012) so it ships as part of the LLM deployment config wire format

Plus two `LlmAdapter` impls:

- **`CloudLlmAdapter`** — `LlmAdapter` impl. **Phase 5.3 stub**: `plan()` returns `Plan::new(vec![])`;`name() = "CloudLlmAdapter(stub)"`. Real Anthropic / OpenAI SDK integration deferred to Phase 5.3+ (post-grill).
- **`StubOnDeviceLlm`** + `OnDeviceLlm` trait — `LlmAdapter` impl with a `model_spec()` accessor. **Phase 5.3 stub**: `plan()` returns empty plan. Real Phi-3 / Gemma-2 model loading deferred to Phase 5.3+.

## Why

1. **Aligned with ADR 0007** — Hybrid deployment is the grill decision; this is the code-level hook.
2. **JSON-derivable** — `LlmDeployment` is the wire format for `bin/adk --config=path.json` (Phase 5.3+).
3. **`CloudLlmAdapter` / `StubOnDeviceLlm` stubs** — even without real SDKs, the config can be **constructed and validated** at config-load time. Tests can verify `Hybrid` + Anthropic configs without firing any HTTP.
4. **`api_key_env` is env-var name, not the key itself** — secrets never inline in config.

## Considered Options

- **A. `LlmDeployment` as a new type (this ADR)** — selected.
- **B. Stuff everything into a JSON file + skip typed config** — Rejected: loses type safety; user can mistype field names.
- **C. Skip Hybrid, do `CloudOnly` only** — Rejected: violates ADR 0007.

## Consequences

- `LlmDeployment::default()` works out-of-the-box; no setup needed for simulator use.
- `CloudLlmAdapter` is feature-gated? **No** — it's a stub, no extra deps. Real SDK integration in Phase 5.3+ would gate `reqwest` (or `ureq`) behind `cloud-llm` feature.
- `OnDeviceLlm` trait is **additive** — `LlmAdapter` impls without `model_spec()` are still valid.
- Any future `RealLlmAdapter` (Phase 5.3+) follows the same pattern: `LlmAdapter` impl, takes `LlmDeployment` (or part of it) in constructor.

## 关联

- 上游:ADR 0007 (Hybrid LLM deployment),ADR 0008 (escalation policy)
- 下游:Phase 5.3 ADR 0021 (`ColdStartStrategy` uses `LlmDeployment`)
- 验证:`tests/llm_deployment.rs` (4 tests), `tests/cloud_llm.rs` (3 tests), `tests/on_device_llm.rs` (2 tests) — 9 tests, all green
