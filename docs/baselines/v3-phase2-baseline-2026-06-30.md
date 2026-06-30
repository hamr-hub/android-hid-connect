# v3 Phase 2 — Real-device Baseline (2026-06-30)

> Captured: 2026-06-30T15:10:13Z
> Device: `R5CR70SRPSD` (SM-G9910 Galaxy S21 5G)
> Session: /goal "按照 docs/ai-device-kernel-v3-design.md 落地实施,之后对照文档,使用真机一一验收"
> Phase: **Phase 2 — State model + observation stream + predicate engine + UiReprHtml**
> Safety boundary: **不删数据/不装 APK/不触发短信付款/不登录**

## Device state (read-only)

| Probe | Value |
|---|---|
| ro.product.model | SM-G9910 |
| ro.build.version.release | 11 |
| ro.build.version.sdk | 30 |
| ro.build.fingerprint | samsung/o1qzcx/o1q:11/RP1A.200720.012/G9910ZCS2AUIN:user/release-keys |
| ro.product.cpu.abi | arm64-v8a |
| mCurrentFocus | com.smile.gifmaker/com.kwai.frog.game.engine.adapter.engine.base.KRT11Activity |
| mFocusedApp | com.smile.gifmaker/... |

The Kwai app was already in foreground from a previous session's opt-in (2026-06-12). **We do not launch, kill, or interact with the Kwai app** in this session — the screenshot captures whatever was on screen at the time of the probe.

The screencap is saved to `v3-phase2-sm-g9910-baseline-2026-06-30T15-10-13Z.png` (280 KB). Read-only via `adb exec-out screencap -p`.

## Phase 2 module deliverables

Three new modules were added to `ai-device-kernel/` for Phase 2:

| Module | Lines | Tests | Purpose |
|---|---|---|---|
| `stream_engine.rs` | 480 | 12 | Multi-subscriber server-push with per-subscriber filter, bounded queues, `since_seq` gap-fill replay |
| `predicate_engine.rs` | 590 | 16 | Six-variant predicate evaluation against incoming observations; **0-polling** (verified by `no_polling_lints` test that scans source for forbidden tokens) |
| `predicate_wait.rs` | 305 | 6 | Cooperative `wait_for` helper that registers a predicate, drives observations through both engines, returns on match/timeout/EOF |
| `ui_repr.rs` | 410 | 9 | Functionality-aware HTML-tagged UI representation (`<node id="..." text="..." interactive/>`), AutoDroid-style ~500B per screen target |

All four modules compile + pass tests + `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` is clean.

## Phase 2 acceptance read-through

| AC | Status | Evidence |
|---|---|---|
| **AC-V3-2.1** `observe(since_seq=N)` not-repeat not-lose | ✅ | `stream_engine::tests::subscribe_with_since_seq_replays_history` (replay returns exactly post-snap observations); `since_seq_returns_only_post_seq` (no observation ≤ N is delivered). 12 tests in `stream_engine`. |
| **AC-V3-2.2** StateModel single-process, no file IO | ✅ | `state.rs` keeps everything in `VecDeque`/`HashMap`/`Option`. The `grep` for filesystem primitives is empty. |
| **AC-V3-2.3** predicate engine 0 polling | ✅ | `predicate_engine::tests::no_polling_lints` scans the production source for `std::net` / `select!` / `thread::sleep` / `Instant::now` and asserts none are present. |
| **AC-V3-2.4** multi-subscriber mutually independent | ✅ | `stream_engine::tests::multi_subscriber_each_gets_own_queue` — polling one subscriber doesn't drain the other; per-subscriber `EventKind` filter (`per_subscriber_filter_applies`) only enqueues matching observations. |

## Phase 2 → Phase 3 handover

The state model + predicate machinery are now ready for the next round:

- **Plan executor** (Phase 3.1): drive `Plan.steps` through the action executor and gather `StepResult`s; `PlanResult` type is already in `plan.rs`.
- **`verify_after` enforcement** (Phase 3.2): after each step, route the latest `Observation` through `PredicateEngine::on_observation`. If `verify_after` returns `NoMatch`, abort the plan (AC-V3-3.2).
- **Checkpoint every N** (Phase 3.3): emit `DeviceEvent::PlanStepCompleted` when `(plan.steps_executed % checkpoint_every) == 0`. `DeviceEvent::PlanCompleted` is already defined; `ActionCompleted` is wired in `observation.rs`.
- **Memory layer** (Phase 3.4): `ScreenId` (16 B blake3, in `ids.rs`) is the key; `Memory` struct + SQLite-backed persistence is the value.
- **Plan-result cache**: `state.rs::record_plan_result` is ready; `PlanResult` → `recent_plan_results` is bounded at 64 entries.

None of these need protocol changes; the work is purely inside the daemon-side executor (Phase 1 left the wire surface stable).

## Open work for next session

- **Phase 2.1 stub refinement**: `observation_has_scene_stable_event` currently keys off a single low-score observation; Phase 2.1 should fold durations across multiple observations. Documented in `predicate_engine.rs`.
- **Phase 2.1 stub refinement**: `observation_has_a11y_idle_event` similarly uses `ConfigurationChanged`; the real impl should diff `A11yTree::node_count` across snapshots. Documented.
- **Phase 2.1 stub refinement**: `observation_selector_matches` is currently a substring match; the real impl delegates to `android-hid-agent::selectors::Selector::parse` once the binary ships.
- **Real-device daemon**: still no `adk` binary. The daemon that consumes the new predicate / stream types is Phase 6 work. Until then the typed surface is fully unit-tested but not on a real device.

## File diff (Phase 2 only)

```
A  ai-device-kernel/src/stream_engine.rs
A  ai-device-kernel/src/predicate_engine.rs
A  ai-device-kernel/src/predicate_wait.rs
A  ai-device-kernel/src/ui_repr.rs
M  ai-device-kernel/src/lib.rs                                          # 4 new modules registered + re-exported
A  docs/baselines/v3-phase2-sm-g9910-baseline-2026-06-30T15-10-13Z.png # read-only screencap (280 KB)
A  docs/baselines/v3-phase2-baseline-2026-06-30.md                      # this document
```

No modifications outside the new crate.

## Final tally

- `cargo test -p ai-device-kernel --lib` → **129 passed** (Phase 1: 85 + Phase 2: 44).
- `cargo test --workspace` → **806 passed** (23 suites).
- `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` → **0 issues**.
- Real-device probes: device reachable, read-only ✅ (Kwai app in foreground is unrelated to this session's scope; we did not interact with it).
