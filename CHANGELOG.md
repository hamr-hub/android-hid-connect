# Changelog — `android-hid-connect`

> All notable changes to this crate will be documented in this file.
>
> Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),版本遵循 [Semantic Versioning](https://semver.org/)。
>
> 自动 release 由 [release-please](https://github.com/googleapis/release-please) 处理。手动改本文件时,**只**改 `## [Unreleased]` 段。

---

## [Unreleased]

### Added

- `ai-device-kernel/src/bridge.rs` — typed `Action` → byte-exact UHID execution path (`ActionInjector::apply`)
- `ai-device-kernel/src/bin/adk-bench.rs` — in-process micro-bench (CI gate, p99 < 5 ms)
- `docs/adr/0015-ai-middleware-consolidation.md` — 2026-07-01 workspace consolidation decision

### Changed

- **AI middleware consolidation (ADR-0015)** — workspace reduced from 7 crates to 3 crates + 1 binary. `ai-device-kernel` is THE AI façade (single binary `adk`, single port 9008, single protocol postcard/4-verb). Root crate `Cargo.toml` description rewritten to reflect AI middleware positioning. `src/agent/` removed from root crate. `AGENTS.md` §1 + §2.1 + §2.6 + §2.7 + §4.2 updated to reflect the 3-crate workspace.
- `docs/architecture.md` — rewritten to show the consolidated 3-crate layering + bridge module.
- `docs/ai-agent-integration.md` — rewritten around `ActionInjector::apply` (replaces `AgentControlSession` examples).
- `docs/development.md` — added `adk-bench` section; cargo commands updated.
- `docs/INDEX.md` — rewritten around the 3-crate workspace + 15 ADRs.

### Removed

- `android-hid-daemon/` (entire crate, 1.7 kLOC)
- `android-hid-agent/` (entire crate, 3.3 kLOC)
- `android-hid-cli/` (entire crate, 42-LOC stub)
- `android-hid-py/` (entire crate, 33-LOC stub)
- `src/agent/` (8 files, 20.6 kLOC) — including public APIs `AgentControlSession`, `AgentAction`, `AgentPlanSummary`, `AgentControlClosed`, `AgentControlCloseReport`, `AgentControlCommandCloseReport`, `AgentObjectSelector`, `AgentTargetSelector`, `AgentTouchFrame`, `AgentScrollFrame`, `AgentRect`, `AgentPoint`, `AgentPlanBoundedPrefix`, `AgentPlanBoundedPrefixStop`, `DEFAULT_AGENT_COMMAND_BOUND`
- `docs/agent-v2-design.md`
- `docs/roadmap-exceed-handsets.md`
- `docs/comparison-with-handsets.md`
- `docs/adr/0006-plan-wire-format-is-json.md` (superseded by ADR-0012)
- `docs/baselines/v3-phase{2,3,4,5.1,5-6-7}-*` (intermediate baselines; phase 1 + phase 6 retained)

### Fixed

- `ai-device-kernel` `Plan`/`PlanStep`/`PlanResult`/`StepResult` now derive `JsonSchema` (was a hidden compile error in the existing uncommitted edits; ADR-0012 wires through `Plan::version`).

### Migration

- External users of `AgentControlSession` etc.: pin to the `pre-adr-0015` git tag, or migrate to `ai_device_kernel::ActionInjector`.
- External users of `android-hid-{daemon,agent,cli,py}`: same — these crates are deleted; git history retains the last pre-ADR-0015 commit under the `pre-adr-0015` tag.

---

## [0.1.0] - 2026-06-29

- **Convert to Cargo workspace** — root `Cargo.toml` 现在既是 root 也是 workspace member;`members = [".", "android-hid-protocol", "android-hid-daemon", "android-hid-agent", "android-hid-cli", "android-hid-py"]`,`resolver = "2"`。无 breaking change:既有 byte-exact HID core 的 `Cargo.toml` 字段、API、`pub use` 全部不动,453 个既有测试 + 35 个新测试全部通过。
- `AGENTS.md` §1(项目定位)+ §2.1(顶层布局)+ 新增 §2.7(兄弟 crate 协作约束),描述 workspace 布局和依赖方向(daemon → protocol;agent → protocol + 根 crate;cli → agent;py → agent)
- `Cargo.toml` `[dependencies]` / `[dev-dependencies]` 改用 `workspace = true` 共享;`edition` / `rust-version` / `license` / `repository` 全部 `.workspace = true`
- `Cargo.toml` 新增 `[profile.release]`(放在 root,所有 member 共享)

### Fixed

(none)

---

## [0.1.0] - 2026-06-29

### Added

- **Protocol**:全部 22 种 scrcpy control message + 3 种 AI extension(AC-C1..AC-C25,byte-exact)
- **HID 驱动**:
  - `hid::KeyboardHid` — 8-byte report, 6KRO + phantom state (ErrorRollOver on 7+ keys)
  - `hid::MouseHid` — 5-byte report, scroll residual accumulator
  - `hid::GamepadHid` — 15-byte report, 8 concurrent slots (UHID id 3..=10)
  - 三种 HID report descriptor 与 scrcpy v2.7 字节相同
- **生命周期**:
  - `session::HidSession` — panic-safe UHID lifecycle, `Drop` 通过 `catch_unwind` 保证 DESTROY 必发
  - `session::OpenRequest` — kbd/mouse/gamepad 自由组合 + `gamepad_only_realtime()` 240Hz 低延迟
- **并发**:
  - `client::HidClient` — `Arc<mpsc::Sender>` 多生产者, 单 dispatcher 线程
  - `client::HidDispatcher` — bounded 4096 channel, backpressure-safe
  - `client::CoalescingWriter` — 1ms 桶 syscall 合并
- **Batcher(全部 fixed-stack 默认 32 帧,零堆分配)**:
  - `client::KeyboardFrameBatcher` + `KeyboardChordFrame`(6-key chord)
  - `client::AndroidKeyFrameBatcher`
  - `client::MouseFrameBatcher`
  - `client::ScrollFrameBatcher`
  - `client::GamepadFrameBatcher` + `PackedGamepadFrameBatcher`
  - `client::TouchFrameBatcher`
- **Multitouch**:
  - `multitouch::MultitouchHandle` — 10 pointer 独立状态机
- **Device → Host reverse**:
  - `device::read_device_message` + `DeviceMessageReceiver` — 3 native msg
  - `device::read_device_event` + `spawn_device_event_receiver` — native + AI mixed stream
  - `device::spawn_latest_frame_summary_receiver` — newest-only cache
  - `device::LatestFrameSummaryObservation` / `LatestFrameSummaryBoundary` typed tokens
- **Agent facade**:
  - `agent::AgentControlSession` — 同步 facade, TCP + 内部 read timeout 恢复
  - `agent::AgentAction` — typed plan enum (tap / swipe / pinch / keyboard / mouse / scroll / gamepad / clipboard / control / AI)
  - `agent::AgentPoint` / `AgentRect` / `AgentObjectSelector` / `AgentTargetSelector` — normalized selectors
  - `agent::AgentTouchFrame` / `AgentScrollFrame` — typed batch helpers
  - `agent::AgentPlanSummary` — transport-free 离线估算(structural error index / blocking prefix / command pressure)
  - `agent::AgentPlanBoundedPrefix` — 按 command 预算拆分 + 切分 helper
  - `try_*` 家族(non-blocking enqueue + checked barrier)
- **Async adapter**(`feature = "tokio"`):
  - `async_device::read_device_message_async` / `read_device_event_async`
  - `spawn_async_device_message_receiver` / `spawn_async_device_event_receiver`
  - `spawn_async_latest_frame_summary_receiver`(`tokio::sync::watch`)
- **Typed constants** (`types.rs`):
  - `AndroidKeycode` / `AndroidKeyAction`
  - `Scancode` / `Modifiers` / `MouseButton`
  - `GamepadAxis` / `GamepadButton`
  - `TouchAction` / `TouchPointerId`(scrcpy 保留 pointer id)
  - `ClipboardCopyKey`
  - `HID_ID_KEYBOARD` / `HID_ID_MOUSE`
- **Examples**:
  - `examples/live_e2e.rs` — 30 项字节级真机 E2E
  - `examples/live_kbd.rs` — GET_CLIPBOARD 真实回包 + UHID 双向
  - `examples/type_keys.rs` — 真实打字 "Hello, world!"
  - `examples/multitouch_10.rs` — 10 pointer multitouch
  - `examples/gamepad_demo.rs` — 手柄演示
  - `examples/ai_summary_demo.rs` — AI frame summary
  - `examples/ai_phone_demo.rs` — AI 端到端 demo
- **Tests**: 405 个测试 (11 suite), 416 个 (`--features tokio`)
- **CI**: 3-OS 矩阵(ubuntu + macos + windows)+ MSRV 1.87 + clippy -D warnings + rustfmt

### Fixed

2026-06-18 真机回归发现并修复(详见 [`ACCEPTANCE.md`](ACCEPTANCE.md) §12):

- **§12.1**: `tests/coalesce_flush.rs::default_open_enables_coalescing` + `close_flushes_via_into_inner` 期望被 gamepad 状态机 dedup 吃掉 — 改用递增 / 漂移值。
- **§12.2**: `tests/parallel_client.rs::packed_gamepad_frame_batcher_try_push_backpressure` 线程时序 flaky — 改用 `uhid_inputs > 0` 宽断言;新增 `count_uhid_inputs(&[u8])` helper 走消息边界解析。
- **§12.3**: `benches/uhid_throughput.rs` clippy E0382(closure 移动 client)— 改 `black_box(client.clone())`。
- **§12.4**: `tests/parallel_client.rs::try_send_frame_batch_unchecked_backpressure` raw `0x0D` 计数误报 — 用 `count_uhid_inputs` helper 替换 raw byte filter。
- **§12.5**: `HidClient` 分散 `MultitouchDown/Move/Up` 命令丢失 active 状态 — dispatcher 改用 `HidSession::inject_touch` 直接调用,不依赖临时 handle 的 active 状态。

### Compatibility

- byte-exact 与 scrcpy v2.7 C 端 + scrcpy-server Java 端对齐(`ACCEPTANCE.md` §10)
- 已知 caveat:Samsung OneUI 8-slot UHID EINVAL(`ACCEPTANCE.md` §7.3)
- MSRV: Rust 1.87
- 默认 zero-dep(`thiserror` only),`tokio` feature 可选

---

## 版本号规则 (semver)

- **MAJOR**:任何 byte-exact 兼容破坏(scrcpy 上游 layout 变化 / `CONTROL_MSG_MAX_SIZE` 改 / 8-slot 上限改 / BE 字节序改)
- **MINOR**:新 typed enum 变体、新 facade helper、新 batcher、新 public type、新 example、新 feature flag
- **PATCH**:bug 修复(不动 byte layout + 不动公开 API)、文档更新、CI 调整、refactor 内部实现

release-please 会根据 Conventional Commits 自动判断版本 bump 级别:

- `feat:` → minor
- `fix:` → patch
- `feat!:` / `BREAKING CHANGE:` footer → major
- `chore:` / `docs:` / `style:` / `refactor:` / `test:` → 通常 patch

---

## 变更类型标签 (Conventional Commits)

本仓库采用 [Conventional Commits](https://www.conventionalcommits.org/) 风格:

| 前缀 | 用途 | 影响 |
| ---- | ---- | ---- |
| `feat:` | 新功能(新公开 API、新 example) | minor |
| `feat!:` | breaking change | major |
| `fix:` | bug 修复(不动 API) | patch |
| `refactor:` | 内部重构 | patch |
| `perf:` | 性能优化(不改语义) | patch |
| `test:` | 加 / 修测试 | patch |
| `docs:` | 文档 / 注释 | patch |
| `chore:` | 工具链 / CI / 依赖 bump | patch |
| `style:` | fmt / 命名调整 | patch |

模块 scope:`feat(agent): ...` / `fix(client): ...` / `docs(architecture): ...`。

---

## 历史 summary

| 版本 | 发布日期 | 主要变化 |
| ---- | -------- | -------- |
| 0.1.0 | 2026-06-29 | 首发:22 control_msg + 3 AI + 3 HID + typed agent facade + Tokio adapter + 405 测试 |

---

最后更新: 2026-06-29。