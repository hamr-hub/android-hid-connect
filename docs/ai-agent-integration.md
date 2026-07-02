# AI Agent Integration — `ai-device-kernel`

> How to embed the AI middleware in an LLM / agent runtime. This
> document is for engineers writing agent runtimes; it assumes
> you're familiar with the basic `Plan` / `Action` / `Observation`
> types.
>
> Per [ADR-0015](adr/0015-ai-middleware-consolidation.md) (2026-07-01)
> the only AI façade is `ai-device-kernel`. The legacy
> `AgentControlSession` API was retired — see ADR-0015 for the
> deprecation path.

---

## 1. The contract: kernel returns *what happened*, LLM decides *what to do next*

```
LLM (cloud or on-device)
  │ emits a PlanMessage JSON (ADR-0012)
  ▼
ai-device-kernel: bridge::ActionInjector::apply(action) × N
  │ emits UHID bytes
  ▼
Android device
  │ emits observation (a11y tree + frame + foreground app)
  ▼
LLM: re-observe → next chunk | re-plan | abort
```

The kernel never retries, branches, waits, or loops on its own. All
flow control lives in the LLM (ADR-0002). Errors flow back as
structured `AtomicResult` (ADR-0004) with a fixed 6-variant
`AtomicErrorCode` (ADR-0013).

---

## 2. Minimal LLM agent loop (Rust)

```rust
use ai_device_kernel::{
    Action, ActionInjector, AtomicErrorCode, Observation, Plan, PlanStep,
};
use ai_device_kernel::bridge::BridgeOutcome;

let transport = /* open_tcp("127.0.0.1", 9008) */ todo!();
let mut injector = ActionInjector::open_default(transport)?;

loop {
    // 1. LLM emits a Plan (≤ 4 atomics, terminated by ≥ 1 observe)
    let plan: Plan = llm_emit_plan();

    // 2. Execute the plan (kernel-internal, no LLM calls)
    let mut outcomes: Vec<BridgeOutcome> = Vec::new();
    for step in plan.steps {
        let action: Action = step.into();
        let outcome = injector.apply(&action);
        outcomes.push(outcome);
        if !outcome.landed {
            // Decide: retry | branch | wait | abort — LLM's call
            break;
        }
    }

    // 3. Observe (kernel returns Observation; LLM parses)
    let obs: Observation = kernel_observe()?;

    // 4. Feed observation back to LLM; loop until plan completes
    if plan_complete(&outcomes) {
        break;
    }
}
```

---

## 3. Prompt contract (per ADR-0012)

The LLM system prompt must declare:

- The atomic vocabulary (12 typed `Action` variants — see
  [`ai-device-kernel/src/action.rs`](../ai-device-kernel/src/action.rs))
- Chunk size ≤ 4 atomics, every chunk terminated by ≥ 1 observe
- Escalation policy: `selector_miss` × N=2 on the same target
  → cloud escalation (ADR-0007)
- Egress policy: text + snapshot only, history never leaves
  device (ADR-0008)
- Wire format: `PlanMessage` JSON matching the schema in
  [`ai-device-kernel/src/plan.rs`](../ai-device-kernel/src/plan.rs)

Few-shot examples (3-5) sourced from real-device traces under
`fixtures/llm/traces/`.

---

## 4. Routing pure reads vs writes

**Pure reads** (`DumpObservation`, `GetUiRepr`, `Wait`,
`TapSelector`, `Launch`, `SetClipboard`) — these need the daemon-side
resolver. The `bridge` returns `AtomicErrorCode::NotImplemented`
with a structured reason pointing you to `protocol::Frame::request`:

```rust
match injector.apply(&Action::DumpObservation { ... }) {
    BridgeOutcome { landed: false, error: Some(e), .. }
        if e.starts_with("not_implemented:") =>
    {
        // Route via protocol::Frame::request instead
        let reply = injector.protocol_request(RequestPayload::Query {
            a11y: true, frame: false, state: true,
        })?;
    }
    outcome => { /* landed or other error */ }
}
```

**Pure writes** (`Tap`, `Swipe`, `TypeText`, `Key`, `GamepadFrame`)
— these go straight through `bridge`. Latency: p99 = 5 µs in-process
per `adk-bench`.

---

## 5. Streaming observations

For low-latency observation streams (UI change detection, video
frames), use the [`stream_engine::StreamEngine`](../ai-device-kernel/src/stream_engine.rs):

```rust
let engine = StreamEngine::new();
let handle = engine.subscribe()?;
// `handle` is a bounded MPSC receiver — pull observations in your
// agent loop or a separate thread. Queue cap = SUBSCRIBER_QUEUE_CAP.
```

Egress policy (ADR-0008) gates what can flow out: text + snapshot
yes, history no.

---

## 6. Session state (per ADR-0009)

All session state stays on-device:

- `selector_cache` — invalidated on `selector_miss`
- `recent_atomics` — bounded `VecDeque` (typical last 20)
- `foreground_app_history` — bounded
- `deadline: Instant`

Session boundary: "user stops sending new instructions for 30s" OR
"scenario abort / success". Cross-cloud observability uses a single
`trace_id: UUID`.

---

## 7. CI gate

`cargo run --release -p ai-device-kernel --bin adk-bench` — emits
non-zero exit code if p99 > 5 ms. Run this on every PR that touches
`action.rs`, `bridge.rs`, or `protocol.rs`.

---

## 8. Common pitfalls

| Pitfall | Mitigation |
| ------- | ---------- |
| LLM emits a chunk > 4 atomics | Chunk-size guard at executor boundary → atomic truncation + warn |
| LLM retries the same selector blindly | EscalationPolicy `selector_miss × 2` → cloud escalation |
| 操作轨迹 leaks to cloud | EgressPolicy.history = false → strictly bounded local |
| Bridge returns `not_implemented` | Route pure reads via `protocol::Frame::request` |
| `adk-bench` p99 > 5 ms | CI gate fails; profile `bridge` + `coalesce` |

---

Last updated: 2026-07-01 (post-ADR-0015 consolidation).