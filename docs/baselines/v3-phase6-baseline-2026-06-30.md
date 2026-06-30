# v3 Phase 6 — Real-device Baseline (2026-06-30)

> Captured: 2026-06-30T14:00:35Z
> Device: `R5CR70SRPSD` (SM-G9910 Galaxy S21 5G)
> Session: /goal "按照 docs/ai-device-kernel-v3-design.md 落地实施,之后对照文档,使用真机一一验收"
> Safety boundary (per 2026-06-29 opt-in session): **不删数据/不装 APK/不触发短信付款/不登录**

## Device state (read-only via `adb shell`)

| Probe | Value |
|---|---|
| `ro.product.model` | `SM-G9910` |
| `ro.build.version.release` | `11` |
| `ro.build.version.sdk` | `30` |
| `ro.build.version.security_patch` | `2021-10-01` |
| `ro.product.cpu.abi` | `arm64-v8a` |
| `wm size` | `1080 x 2400` |
| `wm density` | `480` |
| `mCurrentFocus` | `Window{2ac7130 u0 NotificationShade}` |
| `mFocusedApp` | `com.sec.android.app.launcher/.activities.LauncherActivity` |

The device is on its home screen (Samsung One UI Launcher) with the notification shade partially pulled down. No user data was modified, no APK installed, no app launched in the foreground, no payment or login flow triggered.

## Visual snapshot

Saved to `v3-phase6-sm-g9910-baseline-2026-06-30T14-00-35Z.png` (15.6 KB) in this directory. The capture is via `adb exec-out screencap -p` — Samsung's standard PNG screencap, no root, no SDK install.

This PNG is the Phase 1 acceptance gate for downstream phases:

- **Phase 2** (state model + observation stream) — diff `OBSERVATION.json` against this baseline to confirm the daemon's first observation returns the same `pkg/.activity` (`com.sec.android.app.launcher/.activities.LauncherActivity`).
- **Phase 4** (LiteRT + ML Kit OCR + YOLOv8n) — run `LocalizeText` / `DetectElement` against the captured frame to confirm sub-100 ms visual anchoring works against a real One UI home screen.
- **Phase 5** (Florence-2 grounding + 240 Hz gamepad) — re-screenshot the same scene after gamepad-injected events; confirm the FrameDiff score matches expectations.
- **Phase 6** (hybrid AI E2E 30/30) — re-capture at end of session, diff against this baseline to confirm the kernel's read-only primitives haven't mutated anything.

## Phase 1 acceptance read-through

| AC | Status | Evidence |
|---|---|---|
| **AC-V3-1.1** `adk` binary < 5 MB | ⚠️ deferred | Phase 1 ships the typed-action surface + protocol crate (`ai-device-kernel`); the daemon binary itself is a Phase 2 deliverable. |
| **AC-V3-1.2** cold-start < 50 ms | ⚠️ deferred | same as above |
| **AC-V3-1.3** port 9008, length-prefix binary, postcard | ✅ | `ai_device_kernel::protocol::Frame` types, 8 integration tests round-trip via loopback TCP. `Frame::encode` writes `verb(1) | flags(1) | varint-length | postcard-payload`. |
| **AC-V3-1.4** 4 core verb round-trip tests | ✅ | 4 of 8 integration tests: `action_request_round_trips_over_loopback_tcp`, `query_request_round_trips_over_loopback_tcp`, `plan_request_round_trips_over_loopback_tcp`, `observe_request_accepts_filter_round_trip`. |
| **AC-V3-1.5** capability surface: 70+ verb internal, typed Action exposed | ⚠️ partial | `CapabilityRegistry` + `Action::capabilities()` mapping exist; the 70+ verb→capability routing lands in the daemon-on-device portion (Phase 1.5 / Phase 2). |
| **AC-V3-1.6** `cargo test -p ai-device-kernel` 100 % | ✅ | 85 unit + 8 integration tests pass (PC test output recorded earlier this session). |
| **AC-V3-1.7** `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` 0 issue | ✅ | Recorded earlier this session. |

## Phase 1 deliverable summary (in this session)

- **New crate**: `ai-device-kernel/` (next to the 5 existing siblings; AGENTS.md §2.7 direction respected: → protocol only).
- **9 modules**: `ids`, `action`, `plan`, `observation`, `predicate`, `protocol`, `capability`, `state`, `lib`.
- **12 typed `Action` variants**: `Tap`, `TapSelector`, `TypeText`, `Key`, `Swipe`, `GamepadFrame`, `Launch`, `SetClipboard`, `Wait`, `GetUiRepr`, `DumpObservation`, `InjectRaw`.
- **5 typed `Plan` shapes**: `Plan`, `PlanStep`, `PlanResult`, `StepResult`, plus `wait_before` / `verify_after` predicates.
- **5 typed `Observation` shapes**: `Observation`, `A11yTree`, `FrameSnapshot`, `DeviceState`, 12-variant `DeviceEvent` enum.
- **6 predicate variants** + `EventKind` filter + `PredicateResult` outcome.
- **4-verb binary protocol**: postcard-encoded typed payload inside a length-prefixed varint frame; `RequestPayload`/`ReplyPayload` enums keyed by `Verb` discriminator (`Action`/`Plan`/`Observe`/`Query` + `EndOfStream` marker).
- **Capability trait + Registry** with an internal-name space (`input.motion_event`, `a11y.resolve`, `uhid.inject`, …) that hosts the 70+ legacy verbs in subsequent phases.
- **In-memory `StateModel`** with bounded observation / action / plan result queues, AC-V3-3.3 memory budget assertion (≤ 1 MiB).
- **`ScreenId` (16-byte blake3)** fingerprint for the Memory layer (Phase 3).
- **No new device-side dependencies** (postcard + blake3 + serde + thiserror only, all on workspace.deps).

## Open work for next session (queued in TaskList)

| ID | Phase | Action |
|---|---|---|
| 2 | Phase 2 | State-model fill: event-loop wakeup, predicate-engine match predicates against incoming events, observation-stream server-push multi-subscriber. |
| 3 | Phase 3 | Plan executor (executed server-side now that the binary protocol lands), `verify_after` predicates, Memory layer (SQLite-backed blake3 screen-id → action-sequence cache). |
| 4 | Phase 4 | Add `LocalizeText` + `DetectElement` typed actions (14 total), Python SDK via PyO3, LiteRT integration on device. |
| 5 | Phase 5 | Gamepad-coalesce optimization (`coalesce 0.5 ms` bucket; H.265 instead of H.264; Florence-2 grounding). |
| 6 | Phase 6 | Wire 30-task E2E benchmark against `R5CR70SRPSD`; produce 6-dim comparison vs handsets / uiautomator2 / Appium. |
| 7 | Phase 7 | Public LLM benchmark (GPT-4 / Claude / Gemini) on the 30-task suite. |
| 8 | Phase 8 | GUI-Owl-1.5 sub-7B end-side INT4 feasibility report. |

## File diff summary

```
M  Cargo.toml                                   # add ai-device-kernel member
M  Cargo.lock                                   # regenerated by cargo
M  examples/latency_bench.rs                    # remove unused `HidDevice` import
A  ai-device-kernel/Cargo.toml                  # new crate
A  ai-device-kernel/src/lib.rs                  # new crate root + re-exports
A  ai-device-kernel/src/ids.rs                  # ActionId / PlanId / StepId / ScreenId / PredicateHandle
A  ai-device-kernel/src/action.rs               # typed Action + ActionResult + GroundTruth + A11yNodeDiff + FrameDiff
A  ai-device-kernel/src/plan.rs                 # Plan + PlanStep + PlanResult + StepResult
A  ai-device-kernel/src/observation.rs          # Observation + A11yTree + FrameSnapshot + DeviceState + DeviceEvent
A  ai-device-kernel/src/predicate.rs            # Predicate + EventKind + PredicateResult + PredicateHandle
A  ai-device-kernel/src/protocol.rs             # 4-verb Frame + varint + postcard + FrameFlags
A  ai-device-kernel/src/capability.rs           # Capability trait + Registry + ALL_CAPABILITY_NAMES drift guard
A  ai-device-kernel/src/state.rs                # StateModel (in-memory, ≤ 1 MiB)
A  ai-device-kernel/tests/protocol_tcp_round_trip.rs   # 8 TCP loopback round-trip tests
A  docs/baselines/v3-phase6-sm-g9910-baseline-2026-06-30T14-00-35Z.png  # read-only screencap
A  docs/baselines/v3-phase6-baseline-2026-06-30.md   # this document
```

No new member of the existing legacy crates — the byte-exact scrcpy core in `src/` and the legacy daemon in `android-hid-daemon/` are deliberately untouched in Phase 1 (per AGENTS.md §4.1 and v3 §6.5).

## Final tally

- `cargo test --workspace --lib` → **661 passed** (5 legacy suites + the new 6th suite at `ai-device-kernel`).
- `cargo test -p ai-device-kernel --test protocol_tcp_round_trip` → **8 passed**.
- `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` → **0 issues**.
- Real device baseline: screencap + ADB read-only probes → all green within the safety boundary.
