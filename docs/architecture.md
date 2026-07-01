# Architecture — `android-hid-connect`

> Module dependencies, threading, lifecycle, and purity boundaries.
>
> Per [ADR-0015](adr/0015-ai-middleware-consolidation.md) (2026-07-01)
> this document describes the **consolidated** 3-crate workspace.
> The legacy 7-crate layout is in git history under the
> `pre-adr-0015` tag.

---

## 1. Workspace layering

```
┌─────────────────────────────────────────────────────────────────┐
│  ai-device-kernel (THE AI middleware — single façade)           │
│    • 12 typed Action + Observation + Plan + Predicate          │
│    • Capability registry + Memory (sqlite)                     │
│    • bridge module  →  android-hid-connect::HidSession         │
│    • adk binary    →  port 9008, postcard/4-verb protocol      │
│                                                                 │
│       ▲ uses                                                    │
│       │                                                         │
│  android-hid-protocol  (zero-dep wire types)                   │
│    • Frame / Verb / ErrorCode / k=v parser / version            │
│                                                                 │
│       ▲ used by                                                 │
│       │                                                         │
│  android-hid-connect  (byte-exact scrcpy UHID core)            │
│    • hid::KeyboardHid / hid::MouseHid / hid::GamepadHid        │
│    • control::ControlMessage (22 control + 3 AI + 3 HID)       │
│    • session::HidSession (panic-safe Drop)                     │
│    • transport::open_tcp + MockTransport                       │
│                                                                 │
│       ▼ emits to                                                │
│       │                                                         │
│  scrcpy-server  (AOSP on-device, video + control + clipboard)  │
└─────────────────────────────────────────────────────────────────┘
```

**Direction constraint** (ADR-0015):

- `android-hid-protocol` depends on nothing.
- `android-hid-connect` depends on `android-hid-protocol` (for shared wire types).
- `ai-device-kernel` depends on both `android-hid-connect` (via the `bridge` module) and `android-hid-protocol` (for shared wire types).
- **No reverse edges.** No crate depends on `ai-device-kernel`. The four retired crates (`android-hid-{daemon,agent,cli,py}`) are gone — see ADR-0015.

---

## 2. Crate-to-crate data flow (write path)

```
LLM (cloud or on-device)
  │ PlanMessage JSON (ADR-0012)
  ▼
ai-device-kernel::plan_executor::execute(Plan)
  │
  ├──► bridge::ActionInjector::apply(action)         ┐
  │       │                                           │ one binary,
  │       ▼                                           │ one process,
  │     android-hid_connect::HidSession               │ one socket
  │       │                                           │
  │       ▼                                           │
  │     control::ControlMessage::UhidInput            │
  │       │                                           │
  │       ▼                                           │
  │     transport::open_tcp / MockTransport           │
  │                                                   │
  ▼                                                   │
adb forward tcp:9008 localabstract:adk                │
  │                                                   │
  ▼                                                   │
scrcpy-server (TCP control socket)                    │
  │                                                   │
  ▼                                                   │
Android InputDispatcher  ◄────────────────────────────┘
```

The kernel-internal intent→UHID hot path is measured at **p99 = 5 µs in-process** via `adk-bench` (see [`perf/real-device-call-flow.md`](perf/real-device-call-flow.md)).

---

## 3. Module map

### `android-hid-connect` (root)

```
src/
├── lib.rs                # public API; pub use by module
├── hid/                  # pure: keyboard/mouse/gamepad HID drivers
│   ├── descriptor.rs     #   3 report descriptors (byte-locked)
│   ├── keyboard.rs       #   6KRO + phantom state
│   ├── mouse.rs          #   5-byte report + scroll residual
│   └── gamepad.rs        #   8-slot, 15-byte report
├── control/              # pure: scrcpy wire-format serializer
│   └── message.rs        #   ControlMsgType enum + 22 variants
├── session.rs            # HidSession lifecycle (panic-safe Drop)
├── client.rs             # HidClient (mpsc + dispatcher) + frame batchers
├── device.rs             # sync device→host reverse parser
├── async_device.rs       # #[cfg(feature = "tokio")] async twin
├── multitouch.rs         # MultitouchHandle (10 pointer state machine)
├── coalesce.rs           # CoalescingWriter (1ms bucket syscall merge)
├── gamepad_ring.rs       # 8-slot ring buffer (240 Hz SLO)
├── ai/mod.rs             # FrameSummary / AiConfig (LLM sensor input)
├── transport/            # open_tcp + MockTransport + send_one/batch
├── types.rs              # public typed constants
└── error.rs              # Error enum (thiserror)
```

**Purity rules** (unchanged from AGENTS.md §2.2):
- `hid/`, `control/`, `ai/`, `types/` are **pure** — no `std::net`, no `tokio`, no `Instant::now()`, no channels.
- `hid/`, `control/`, `ai/`, `types/`, `error/` may reference each other but **never** `session`/`client`/`device`/`transport`/`multitouch`/`coalesce`.

### `ai-device-kernel` (THE AI middleware)

```
src/
├── lib.rs                # public API; pub use by module
├── ids.rs                # ActionId / PlanId / StepId / ScreenId (blake3)
├── action.rs             # 12 typed Action + 2 Phase 4 + 2 Phase 5 reads
├── plan.rs               # Plan / PlanStep / PlanResult (version-stamped)
├── observation.rs        # Observation / A11yTree / FrameSnapshot / ForegroundApp
├── predicate.rs          # 6-variant Predicate + EventKind + PredicateResult
├── protocol.rs           # 4-verb Frame (postcard)
├── capability.rs         # Capability trait + CapabilityRegistry
├── state.rs              # in-memory StateModel
├── stream_engine.rs      # bounded observation stream (SUBSCRIBER_QUEUE_CAP)
├── predicate_engine.rs   # predicate resolution + RegisteredPredicate
├── predicate_wait.rs     # wait_for / WaitOutcome
├── plan_executor.rs      # execute(Plan) → PlanResult
├── bridge.rs             # Action → byte-exact UHID execution path ← NEW
├── ui_repr.rs            # AutoDroid-style ~500 B HTML-tagged UI repr
├── memory.rs             # in-memory cache (selector cache, recent atomics)
├── memory_sqlite.rs      # sqlite-backed Memory persistence
├── bin/
│   ├── adk.rs            # the single binary (port 9008)
│   └── adk-bench.rs      # in-process micro-bench ← NEW
└── tests/                # 3 integration test suites (orchestrator, protocol, smoke)
```

**Purity rules**:
- `ids`, `action`, `plan`, `observation`, `predicate`, `protocol`,
  `capability`, `ui_repr`, `memory` are **pure data types** — no I/O.
- `bridge`, `plan_executor`, `predicate_engine`, `predicate_wait`,
  `stream_engine` are **kernel-internal logic** — they consume
  pure types and emit either typed results or transport bytes.
- `bin/adk.rs` is the **only** crate entry point with I/O. The
  kernel is the single AI façade.

### `android-hid-protocol`

```
src/
├── lib.rs                # flat re-exports
├── frame.rs              # length-prefixed Frame (16 MiB cap)
├── error.rs              # ErrorCode enum + ErrorFrame
├── verb.rs               # 60+ Verb variants
├── kvs.rs                # parse_kv / KvWriter / split_head
└── version.rs            # PROTOCOL_VERSION + USER_AGENT
```

Zero external deps. Pure data types.

---

## 4. Threading model

### `android-hid-connect` (root)

- `HidClient` is `mpsc` multi-producer + 1 dispatcher thread. Producers
  call `try_send` (back-pressure aware); dispatcher serializes via
  the `Write` transport.
- `HidSession` is `!Send` — owns a single `Write`. Multi-thread use
  requires a `Mutex`.
- `CoalescingWriter` batches writes in a 1ms window; the dispatcher
  thread is the single consumer.
- `async_device.rs` (feature-gated) provides async equivalents with
  identical byte semantics.

### `ai-device-kernel`

- `bridge::ActionInjector` is `!Send` — same single-transport
  constraint as `HidSession`.
- `predicate_engine::PredicateEngine` is `Send + Sync` — multiple
  callers may register / evaluate predicates concurrently.
- `stream_engine::StreamEngine` uses an `Arc<Mutex<...>>` interior
  with bounded subscriber queues (`SUBSCRIBER_QUEUE_CAP`).
- `bin/adk.rs` runs as a single-threaded tokio task for the
  postcard/4-verb protocol loop. No multi-thread orchestration
  inside the kernel.

---

## 5. Lifecycle invariants

| Invariant | Where enforced | Test |
| --------- | -------------- | ---- |
| `HidSession::Drop` sends `UHID_DESTROY` even on panic | [`src/session.rs`](../src/session.rs) `Drop` impl | `tests/session_lifecycle.rs::panic_during_use_still_destroys` |
| Gamepad slot count is exactly 8 (scrcpy `SC_GAMEPAD_MAX`) | [`src/types.rs`](../src/types.rs) + [`src/hid/gamepad.rs`](../src/hid/gamepad.rs) | `tests/parallel_client.rs` |
| `CONTROL_MSG_MAX_SIZE = 1 << 18` (256 KiB) | [`src/control/message.rs`](../src/control/message.rs) | `tests/integration.rs` |
| `MAX_FRAME_LEN = 16 MiB` | [`android-hid-protocol/src/frame.rs`](../android-hid-protocol/src/frame.rs) | `android-hid-protocol/src/lib.rs::frame_cap_is_16mib` |
| `Plan::version` is `"0.1.0"` (default) | [`ai-device-kernel/src/plan.rs`](../ai-device-kernel/src/plan.rs) `default_plan_version` | `ai-device-kernel::tests` |
| `adk-bench` p99 < 5 ms | [`ai-device-kernel/src/bin/adk-bench.rs`](../ai-device-kernel/src/bin/adk-bench.rs) | CI gate |

---

## 6. What lives where — for new contributors

- **New typed Action variant** → `ai-device-kernel/src/action.rs`
  + add mapping to `ai-device-kernel/src/bridge.rs::apply` if it's
  a write.
- **New wire verb** → `android-hid-protocol/src/verb.rs` (add to
  enum + `parse` + `as_str`) + add dispatch in `ai-device-kernel/src/bin/adk.rs`.
- **New byte-exact HID driver** → `android-hid-connect/src/hid/` (it
  must match a scrcpy C-side source — see ADR-0001 / scrcpy compat doc).
- **New capability** → `ai-device-kernel/src/capability.rs` (trait
  registration) + implement in `bin/adk.rs`.

See [`AGENTS.md` §2](../AGENTS.md) for the full directory rules.