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
A pure single-shot primitive — either a write (tap / long_press / swipe / multi_touch_* / type / clear_text / key) or a read (observe(snapshot) / observe(ui_tree) / observe(video_frame, duration, every)). Atomic carries no retry, no wait, no branch semantics; those are Flow Control, owned by the LLM (see ADR 0001, 0002). Resolved by selector layer for writes that name a target, executed by kernel.
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