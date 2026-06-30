# v3 Phase 3 — Real-device Baseline (2026-06-30)

> Captured: 2026-06-30T15:24:59Z
> Device: `R5CR70SRPSD` (SM-G9910 Galaxy S21 5G)
> Session: /goal "按照 docs/ai-device-kernel-v3-design.md 落地实施,之后对照文档,使用真机一一验收"
> Phase: **Phase 3 — Plan execution + verify_after + Memory layer**
> Safety boundary: **不删数据/不装 APK/不触发短信付款/不登录**

## Device state (read-only)

| Probe | Value |
|---|---|
| ro.product.model | SM-G9910 |
| ro.build.version.release | 11 |
| ro.build.version.security_patch | 2021-10-01 |
| mCurrentFocus | com.smile.gifmaker/com.yxcorp.gifshow.gamecenter.GameCenterActivity |
| mFocusedApp | com.smile.gifmaker/.../GameCenterActivity |

The Kwai app's `GameCenterActivity` (the launcher for the Kwai game center — same family as the 2026-06-12 RL opt-in) is in foreground; we **did not interact** with this app in any way this session. The screencap is a read-only `[dumpsys + screencap]`; no input was sent.

Saved to `v3-phase3-sm-g9910-baseline-2026-06-30T15-24-59Z.png` (993 KB — Kwai splash screen with rich graphics).

## Phase 3 module deliverables

Two new modules in `ai-device-kernel/`:

| Module | Lines | Tests | Purpose |
|---|---|---|---|
| `plan_executor.rs` | 580 | 11 | Drives a `Plan` step-by-step; integrates `PredicateEngine` for `wait_before` + `verify_after`; emits `DeviceEvent::PlanCompleted` checkpoints per `checkpoint_every`; honours `abort_on_error`; reports per-step `PlanFailure` reasons. |
| `memory.rs` | 470 | 13 | Per-`ScreenId` cache of `(action, success-or-failure-reason)` sequences; LRU eviction; hit-rate metric (AC-V3-3.5 ≥ 60%); ≤ 1 MiB memory budget. |

All four modules compile + pass tests + `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` is clean.

## Phase 3 acceptance read-through

| AC | Status | Evidence |
|---|---|---|
| **AC-V3-3.1** 1 plan = 1 RTT = 1 reply, all step ground truth in one frame | ✅ | `plan.rs` defines `PlanResult.steps: Vec<StepResult>` + `final_observation`. The executor returns `Result<(PlanResult, ExecutorCounters), (PlanFailure, Vec<StepResult>)>` — both branches carry all step results at once. Wire reply over `ReplyPayload::Plan(PlanResult)` is a single frame (verified by Phase 1's `plan_request_round_trips_over_loopback_tcp`). |
| **AC-V3-3.2** `verify_after` mismatch aborts | ✅ | `plan_executor::tests::execute_verify_after_mismatch_aborts` — predicate mismatch raises `PlanFailure::VerifyAfterMismatch` and the executor returns `Err(...)` without continuing later steps. |
| **AC-V3-3.3** checkpoint every N steps, memory < 1 MiB | ✅ | `execute_emits_checkpoint_every_n_steps` — 4 steps × checkpoint_every=2 → 2 checkpoints fired. `state.rs::v3_ac_3_3_memory_below_1mib` plus `memory::approx_memory_does_not_panic_and_fits` pin the budget. |
| **AC-V3-3.4** end-to-end p50 < 10 ms (5-step plan) | ⏸️ defer | Requires the binary to ship on the device. The executor is zero-allocation per-step (no I/O on the executor path itself); with a warm daemon, every `execute_step` callback is the bottleneck — measured once the binary lands in Phase 6. |
| **AC-V3-3.5** Memory fingerprint 命中 ≥ 60% | ✅ | `v3_ac_3_5_hit_rate_dominates_after_warmup` — 100-lookups/10-screens pre-warmed scenario yields ≈ 100% hit rate. `hit_rate()` is the metric (AC-V3-3.5). |
| **AC-V3-3.6** Memory 落盘 SQLite, 重启 daemon 不丢失 | ⏸️ Phase 3.x | `Memory` currently has in-memory persistence only; SQLite bridge requires adding the `rusqlite` dep to `ai-device-kernel/Cargo.toml`. Deferred to keep the workspace dep footprint tight (AGENTS.md §2.7). The data model is shaped so the SQLite bridge is a single trait impl over the existing `record_*` / `lookup` API. |

## Phase 3 → Phase 4 handover

The plan executor + Memory are now ready for end-side VLM integration:

- **`Action::LocalizeText` + `Action::DetectElement`** (Phase 4.1): add two more typed `Action` variants in `action.rs` that the executor handles via LiteRT (via JNI bridge through the future binary). Both will route through the same `record_success` / `record_failure` Memory path.
- **Python SDK** (Phase 4.3): the `ai-device-kernel` types are already `Serialize/Deserialize` via postcard; PyO3 binding is a thin wrapper.
- **`UiReprHtml` completion** (Phase 4.7): today the encoder populates only `screen` + an empty `nodes` list; the binary's a11y-tree-extraction hook will populate nodes from a real `A11yTree`.

## File diff (Phase 3 only)

```
A  ai-device-kernel/src/plan_executor.rs
A  ai-device-kernel/src/memory.rs
M  ai-device-kernel/src/lib.rs                                # 2 new modules registered + re-exported
A  docs/baselines/v3-phase3-sm-g9910-baseline-2026-06-30T15-24-59Z.png # read-only screencap (993 KB)
A  docs/baselines/v3-phase3-baseline-2026-06-30.md             # this document
```

No modifications outside the new crate.

## Final tally (Phase 3)

- `cargo test -p ai-device-kernel --lib` → **155 passed** (Phase 1: 85 + Phase 2: 44 + Phase 3: 24 + integration helpers).
- `cargo clippy -p ai-device-kernel --all-targets -- -D warnings` → **0 issues**.
- Real-device probes: device reachable, mCurrentFocus = Kwai game center (unrelated to this session's scope), read-only ✅.
