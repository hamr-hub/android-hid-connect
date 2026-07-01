# android-bid

LLM-as-translator architecture where the LLM emits geometry-agnostic intents, a kernel + selector layer resolves them to UHID actions, and the device executes. Anchored to `/goal` 2026-06-09: "LLM 意图 ↔ 设备操控的翻译层". Phase 6.5 (2026-06-30) established that the kernel-internal intent→UHID hot path is ~0.78 ms p50, ~850× faster than the host-adb path.

## Language

**Intent**:
The minimum unit emitted by the LLM — a declarative, geometry-agnostic instruction (e.g. `tap(search_button)`, `scenario: login_then_search`, `stream: observe(every 200ms)`) describing *what* should happen, not *where* or *how*. Resolved by the kernel + selector layer into UHID bytes.
_Avoid_: action, raw event, byte, command, gesture

**Selector**:
The address that resolves an Intent's symbolic target (e.g. `search_button`) to a concrete UI node on a concrete screen. Canonical form is **text-based**: a string matched against `resource-id` / `text` / `content-desc` fields in a UIAutomator dump. Returns either a resolved coordinate + element metadata or `selector_miss` with reason (not found / multiple matches / off-screen / disabled). Kernel-side resolution; LLM only sees the outcome, never the matching algorithm.
_Avoid_: coordinate, raw tap, screen point, geometric, byte offset, XPath

**Atomic**:
A pure single-shot primitive — either a write (tap / long_press / swipe / multi_touch_* / type / clear_text / key) or a read (observe(snapshot) / observe(ui_tree) / observe(video_frame, duration, every)). Atomic carries no retry, no wait, no branch semantics; those are Flow Control, owned by the LLM (see ADR 0001, 0002). Resolved by selector layer for writes that name a target, executed by kernel. Atomic **may** carry a `parallel: bool` flag (default false); parallel atomics within a chunk form a parallel execution group (see ADR 0001 B, multi-touch use case).
_Avoid_: verb, command, gesture, action

**Scenario**:
An ordered sequence of atomics emitted by the LLM as a single Intent (`scenario: ...`). No built-in retry / branch / wait / loop semantics inside the scenario itself — those are expressed by the LLM composing multiple scenarios. Scenario is *what to attempt*; Flow Control is *what to do if it fails*.
_Avoid_: script, plan, workflow, pipeline, DAG

**Flow Control**:
The LLM-side concern of retry, branch, wait, and loop. Owned by the LLM; never expressed inside atomic or scenario. The kernel never retries a failed atomic on its own — it returns the failure to the LLM, which decides whether to retry with a different selector, branch to a recovery scenario, wait and re-observe, or abort. Boundary: kernel returns *what happened*, LLM decides *what to do next*.
_Avoid_: control flow, recovery logic, fallback (kernel doesn't have these); retry policy (LLM doesn't bake it into individual atomics)

**Chunk**:
A bounded planning window emitted by the LLM — a short ordered sequence of atomics (default N=4, screen-level) terminated by at least one observe atomic. After a chunk executes, the LLM re-observe and either continues with the next chunk, re-plans, or aborts. Chunk size is a tunable parameter: too small → per-atomic loop (token-heavy, see rejected ADR 0003 B); too large → macro-plan (brittle on UI drift, see rejected ADR 0003 A).
_Avoid_: sub-scenario, batch, episode (LLM doesn't learn between chunks); transaction (atomicity ≠ planning granularity)

**AtomicResult**:
The structured return type for every atomic. Two variants:
- success: `{ok: true, element: ResolvedTarget, duration_ms}`
- failure: `{ok: false, code: AtomicErrorCode, reason: string, retryable: bool}`

`AtomicErrorCode` is a fixed enumeration: `selector_miss`, `kernel_error`, `timeout`, `permission_denied`, `app_not_foreground`, `not_implemented`. UI tree state is **not** embedded (LLM observes separately via observe atomic — see ADR 0004). The contract is symmetric and value-based: kernel returns *what happened*, LLM decides *what to do next*.
_Avoid_: exception (errors are values, not control flow); status code (use the code enum); response (this is not HTTP); traceback (kernel errors have no stack across the boundary)

**`app_switch` (atomic variant)**:
A write atomic that switches the device's foreground application. Always carries an explicit `target: AppIdentifier` (package name or canonical alias registered in ADK harness). Syntactically equal to other write atomics — no syntactic privilege — so the LLM cannot accidentally cross apps without producing an atomic that selector/audit layers can see and chunk boundaries can use as natural re-plan points. Default app-agnostic: every other atomic inherits the current foreground.
_Avoid_: implicit foreground change (must always be an atomic); cross-app intent (do not co-locate target app with target element)

**ForegroundApp**:
The state type returned by `app_switch` and any observe atomic: `{package: string, activity: string, focused: bool, since_ms: u64}`. Single source of truth for "what app am I acting on" — replaces any per-intent `app=` field.
_Avoid_: current app, active window (Android terminology drift); per-intent app tag

**PlanMessage**:
The root JSON wire format for LLM output. Top-level discriminated union over `kind`:
- `{kind: "atomic", id, atomic: Atomic}` — single atomic
- `{kind: "chunk", id, atomics: [Atomic]}` — bounded planning window (see Chunk, default ≤4 atomics)
- `{kind: "scenario", id, chunks: [ChunkRef]}` — multi-chunk scenario
- `{kind: "abort", id, reason}` — terminate current scenario

Every variant carries a unique `id` for idempotency / audit / retry correlation, and a `version: semver` field. Validated by `jsonschema` against `schemars`-generated schema at the kernel boundary. Wire format mirrors Claude / OpenAI `tool_use` protocol so the LLM emits this directly with **zero translation prompt-engineering**.
_Avoid_: AST / syntax tree (this is wire, not internal representation); IR (this is concrete JSON); DSL (we use JSON, not a custom language); message (overloaded); plan document (we call it scenario, not document)

**Observation**:
The structured return type of the `observe` atomic. `{ui_tree: UiTree, foreground_app: ForegroundApp, snapshot_png: bytes}`. UI tree is the primary input (text-based selectors consume it directly per ADR 0001). ForegroundApp is mandatory (per ADR 0005). PNG snapshot is multimodal input for LLM vision (selector_miss recovery, canvas-only apps); passed via separate multimodal channel — **not** embedded as base64 in the JSON token stream (would bloat context). Synchronous: observe blocks LLM planning until returned, aligning with chunk-boundary single-point semantics (ADR 0003).
_Avoid_: screenshot (overloaded); frame (ambiguous); vision (LLM only sees part of observation); state (observation is a snapshot, not state)

**LlmDeployment**:
The runtime topology for LLM inference — `{kind: "on_device" | "cloud" | "hybrid", on_device_model: ModelRef, cloud_model: ModelRef, escalation_policy: EscalationPolicy}`. Default is `hybrid` (ADR 0007): on-device is the fast path, cloud is escalated to by policy. Single source of truth for "which model is currently reasoning"; ADK harness reads this at session start.
_Avoid_: backend (overloaded); runtime (LLM is reasoning, not execution); model config (this is topology, not model weights)

**EscalationPolicy**:
The rule that triggers cloud escalation in a hybrid deployment. Default: **`selector_miss` repeated N=2 times consecutively on the same target** (selector_miss is the *only* signal that escalates — `kernel_error` / `timeout` / `permission_denied` / `app_not_foreground` are device-state issues, and cloud escalation will not fix them). User may also explicitly force escalation via a `force_escalate: true` field on a chunk. Future extensions (LLM-reported confidence threshold, app-unfamiliarity signal) are kept open as ADR 0008+ follow-ups. Upgrade point is **always at chunk boundary** (ADR 0003), so escalation state has zero cost inside a chunk.
_Avoid_: retry policy (escalation is single-shot to cloud, not a retry loop); fallback (cloud is the higher-capability path, not a degraded mode); model selection (this is *when*, not *which model*)

**EgressPolicy**:
The rule governing what Observation / state may leave the device when the cloud LLM is invoked (ADR 0008). Default: `{allow_text: true, allow_snapshot: true, allow_history: false}`. `text` is the structured Observation subset (`ui_tree`, `foreground_app`). `snapshot` is the PNG visual. `history` is the recent atomic execution log — denied by default because 操作轨迹 is the most sensitive privacy surface (it leaks user actions, not just UI state). Global escape valve: `LlmDeployment.kind = on_device_only` disables cloud entirely. Per-chunk override possible via `PlanMessage.force_escalate.allow_snapshot: false`.
_Avoid_: privacy policy (broader; this is specifically the egress filter); upload policy (this is about leaving the device, not uploads in general); data sharing policy (egress is a runtime boundary, not a contract)

**MockLlmAdapter**:
A simulator-side deterministic LLM adapter that implements the same trait as the real LLM adapter. Two flavours: `MockOnDeviceLlm` (simulates small-model output — occasional `selector_miss`, conservative planning, deterministic fail patterns) and `MockCloudLlm` (simulates large-model output — better selectors, vision reasoning). Fixtures are JSON files under `fixtures/llm/` declaring `input PlanMessage → output PlanMessage`. **CI runs all tests with both mocks; real-LLM calls only run in opt-in targets** (`make real-llm-acceptance`, off-CI, manual). This is how Hybrid (ADR 0007), EgressPolicy (ADR 0008), and EscalationPolicy contracts get verified without network.
_Avoid_: stub (broader); fake (less precise); simulator (this is one specific simulator component); LLM mock (we have two — be specific)

**PromptContract**:
The contract governing how Observation is wrapped into the LLM user message. `{system: string, examples: [Example], context_template: string}`. `system` declares atomic vocabulary, chunk size (≤4 atomics, ends with observe), EscalationPolicy, app_switch semantics, and wire format (`PlanMessage` JSON, ADR 0006). `examples` are 3-5 few-shot `PlanMessage` pairs sourced from real-device traces (`fixtures/llm/traces/`). `context_template` is a string template that injects the current Observation + ForegroundApp + task description into the user message. Token cost: ~500-1500 tokens per LLM call (system + examples fixed; context_template dynamic). Atomic vocabulary changes only require updating `system` + 1-2 examples — no fine-tune (ADR 0007 alignment: models stay swappable).
_Avoid_: prompt template (broader; this includes the few-shot contract); system message (this is the full contract, not just one message); persona (this is a structured contract, not a character)

**SessionState**:
The bounded state held for the duration of one user task (typical 30s-5min, ADR 0009). `{task: TaskSpec, scenario_progress: ChunkIndex, recent_atomics: VecDeque<AtomicRecord>, selector_cache: HashMap<String, ResolvedTarget>, foreground_app_history: VecDeque<ForegroundApp>, deadline: Instant}`. **All state lives on-device only**; never crosses the boundary to cloud (`recent_atomics` is excluded from EgressPolicy per ADR 0008). Session boundary signal: "user stops sending new instructions for 30s" OR "scenario abort / success". `selector_cache` is invalidated on `selector_miss` (UI drift recovery per ADR 0001). bounded `recent_atomics` (typical last 20 records) bounds session memory regardless of scenario length.
_Avoid_: agent state (overloaded); context (LLM context is per-call; this is per-session); memory (this is bounded working memory, not long-term); session (overloaded — this is the *state* held during one session)

**Observability**:
The contract for production debugging — `{audit_log: JSONL, metrics: MetricsRegistry, trace_id: UUID}`. `audit_log` writes one JSON line per atomic decision (timestamp, kind, session_id, chunk_id, atomic, atomic_result, decision_path), stored on-device only per ADR 0008. `metrics` exposes counters (`selector_miss_total`, `escalation_total`), gauges (`active_sessions`), histograms (`chunk_duration_ms`, `egress_payload_bytes`). `trace_id` is generated per session and propagates across kernel / selector / LLM calls (single UUID — lightweight, not full W3C distributed tracing). On-device storage rotates to bound disk usage. Compatible with the 06-12 "链式回退 + audit" pattern: each chain-recovery step is one audit log line.
_Avoid_: telemetry (usually implies remote); logging (this is structured, not plain text); tracing (this is lightweight single-process, not distributed); monitoring (this is the runtime export, not the storage shape)

**ScenarioLibrary**:
The on-device store of pre-built scenarios — `{templates: HashMap<TemplateId, Scenario>, recordings: HashMap<RecordingId, Scenario>, generated: Vec<GeneratedScenario>}`. `templates` cover generic flows (login / navigate / search / app_switch / 06-12 chain-recovery); `recordings` capture user-specific routines (ADR 0010). All entries are JSON-encoded `PlanMessage`, sharing wire format with LLM output (ADR 0006 alignment). LLM at chunk boundary decides: `use_template(id)`, `use_recording(id)`, or `plan_fresh` — this decision is itself a chunk-relative atomic, subject to `selector_miss` N=2 escalation (ADR 0008). `selector_cache` (ADR 0009) crosses template / recording / fresh-plan boundaries — same selector strings share cache entries. When a template or recording is available, `selector_miss` N=2 may be relaxed to N=3 (template retry is cheaper than LLM retry).
_Avoid_: macro library (overloaded); preset (vague); script library (these are semantic scenarios, not scripts); recipes (LLM is the cook, library is the cookbook); plugin (this is pre-built scenarios, not user-extensible plugins)

**ColdStartStrategy**:
The tiered approach to populating `ScenarioLibrary` from empty — `{ship_time_templates: Vec<TemplateId>, per_device_templates: Vec<TemplateId>, per_user_recordings: Vec<RecordingId>, fresh_plan_fallback: bool}` (ADR 0011). `ship_time_templates` are baked into the binary at build time (system apps: Settings / Launcher / Files / Camera / Dialer / Messages + 06-12 chain-recovery). `per_device_templates` accumulate per connected device via implicit recording during normal use. `per_user_recordings` are explicit user demos. `fresh_plan_fallback` is the escape valve for unknown apps. Cold-start path: `ship_time_templates → fresh_plan → cloud escalation (ADR 0008)`. Library self-evolves: high `selector_miss` rate templates get demoted to `deprecated`; high success-rate generated scenarios get promoted to `templates` (ADR 0010).
_Avoid_: bootstrap (LLM doesn't need bootstrap, it has a library); seeding (library is more than seed data); cold-start cache (this is library strategy, not cache); warmup (this is about *what library to use*, not perf warmup)