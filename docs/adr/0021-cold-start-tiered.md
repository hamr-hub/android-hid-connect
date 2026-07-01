# 0021 — `ColdStartStrategy` tiered cold start (ADR 0011)

## Context

Grill session ADR 0011 mandated tiered cold start: **ship-time templates → per-user preferences → per-device recordings → fresh plan**. Until Phase 5.3, the kernel had no orchestrator for this — `ScenarioLibrary` (Phase 5.2 第三批) supports `templates` + `recordings`, but **the start-path resolution was implicit**.

## Decision

**`ColdStartStrategy` orchestrator — `ai_device_kernel::cold_start::ColdStartStrategy`**:

```rust
pub enum StartPath {
    ShipTimeTemplate(ScenarioId),
    PerUserPreference(u64 /* plan_id */),
    PerDeviceRecording(ScenarioId),
    FreshPlan,
}

pub struct ColdStartStrategy {
    ship_time: Vec<Plan>,
    per_user: HashMap<String, Plan>,
    per_device: ScenarioLibrary,
    scenario_library: ScenarioLibrary,  // shared with the rest of the system
    pub deployment: LlmDeployment,
}

impl ColdStartStrategy {
    pub fn new(deployment: LlmDeployment) -> Self;
    pub fn add_ship_time_template(&mut self, plan: Plan) -> ScenarioId;
    pub fn set_per_user_preference(&mut self, user_id: impl Into<String>, plan: Plan);
    pub fn start(&self, task: Option<&str>) -> StartPath;
    pub fn record_outcome(&mut self, plan_id: PlanId, success: bool) -> Option<ScenarioId>;
}
```

- `start(task)` priority order (per ADR 0011):
  1. Per-user preference (if `task = user_id` resolves in `per_user`)
  2. Ship-time template (first in `ship_time` vec; can be filtered by task tag in future)
  3. Per-device recording (first `ScenarioKind::Recording` in `per_device`)
  4. `FreshPlan` (escape valve)
- `record_outcome(plan_id, success)` propagates to the shared `ScenarioLibrary` (per 第三批 promote/demote logic). Returns the promoted `ScenarioId` if any.
- `LlmAdapter` impl exists for free — the LLM step loop can use the strategy directly, OR (preferred) call `start()` first to pick a scenario and then call the appropriate LLM.

## Why

1. **Aligned with ADR 0011** — tiered cold start is the grill decision; this is the orchestrator.
2. **`ScenarioLibrary` reuse** — per-device + per-user recordings flow into the same library, so promote/demote (Phase 5.2 第三批) just works.
3. **`bin/adk --record` is a future single-binary tool** (Phase 5.3+) that records user actions into the per-device store. Phase 5.3 ships the type only.
4. **No IO** — pure in-memory state, fully testable.

## Considered Options

- **A. `ColdStartStrategy` as a separate orchestrator (this ADR)** — selected.
- **B. Fold into `ScenarioLibrary`** — Rejected: mixes storage with resolution; the four-tier priority is a *policy*, not a *storage* concern.
- **C. Just a function `start_cold_start(plan_id) -> StartPath`** — Rejected: no state means no per-user / per-device learning.

## Consequences

- `start()` priority is **user > ship-time > device > fresh**, **not** ship-time > user > device > fresh. The grill session Priority list (ADR 0011) is ambiguous on the user-vs-ship-time order; this ADR codifies the user-first choice (per-user preferences trump ship-time defaults).
- `record_outcome` integrates with the existing `ScenarioLibrary` promote/demote logic — no separate bookkeeping.
- `bin/adk --record` and `bin/adk --cold-start` CLI flags are deferred (Phase 5.3+); the type is in scope now.

## 关联

- 上游:ADR 0011 (tiered cold start),ADR 0010 (ScenarioLibrary),ADR 0007 (LlmDeployment via Phase 5.3 ADR 0020)
- 下游:Phase 5.3+ single-binary client (`bin/adk --record`, `--cold-start`)
- 验证:`tests/cold_start.rs` (6 tests), all green
