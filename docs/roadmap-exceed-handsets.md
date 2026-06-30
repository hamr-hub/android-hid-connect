# Roadmap: Exceed handsets (超越 handsets)

> Date: 2026-06-29
> Goal: 在 `android-hid-connect` workspace 内,通过新增 sibling crate,实现 handsets 的全部能力并超过它。
> 不修改既有 `android-hid-connect` crate 的字节级 / 模块边界(由 AGENTS.md §6 保护)。

---

## 0. 战略决策(已确认)

| 维度 | 决策 |
| ---- | ---- |
| 组织 | **新增 sibling crates** 组成 workspace(不动 `android-hid-connect` 字节级核心) |
| 设备端 70+ 能力 | **纯 Rust daemon**(完全重写 hs.jar,不依赖 Java) |

---

## 1. Workspace 布局

```
android-hid-connect/                          ← Cargo workspace 根
├── Cargo.toml                                ← [workspace] members = [...]
│
├── android-hid-connect/                      ← 现有:字节级 HID 核心(不动)
│   └── ... (lib, 16K 行)
│
├── android-hid-protocol/                     ← 共享 wire format + 错误码
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── frame.rs        ← length-prefixed frame 编解码
│       ├── error.rs        ← ErrorCode enum(NOT_FOUND, TIMEOUT, AMBIGUOUS, ...)
│       ├── verb.rs         ← Verb 枚举(70+)
│       ├── kvs.rs          ← k=v parser
│       └── version.rs      ← 协议版本握手
│
├── android-hid-daemon/                       ← 设备端 Rust 守护(替代 hs.jar)
│   ├── Cargo.toml
│   ├── build.sh           ← cross 编译到 aarch64-linux-android / armv7-linux-androideabi
│   └── src/
│       ├── main.rs         ← app_process-style bootstrap(反射 hidden API)
│       ├── server.rs       ← TCP 长度前缀 frame 分发器(类似 hs Server.java)
│       ├── binder.rs       ← ServiceManager 反射 + IActivityManager 等
│       ├── uiautomation.rs ← UiAutomation 包装(屏幕输入 + a11y 读取)
│       ├── display.rs      ← VirtualDisplay + ImageReader + MediaCodec
│       ├── input.rs        ← MotionEvent / KeyEvent 注入
│       ├── screencap.rs    ← SurfaceFlinger / ImageReader 截图
│       ├── a11y.rs         ← AccessibilityNodeInfo dump
│       ├── state.rs        ← 状态镜像(电池 / top activity / ...)
│       ├── providers/      ← SMS / calls / contacts / calendar ContentProvider
│       ├── am.rs, pm.rs, wm.rs, settings.rs, props.rs, clipboard.rs
│       ├── files.rs        ← pull/push
│       ├── stream.rs       ← JPEG / H.264 / TileJPEG streamer
│       └── waits.rs        ← event-driven wait registry
│
├── android-hid-agent/                       ← 主机侧 Rust agent SDK(本 crate 的 "工具" 层)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── session.rs      ← DaemonSession(单设备)+ FanoutSession(多设备)
│       ├── backend/
│       │   ├── mod.rs
│       │   ├── daemon.rs   ← 连接 android-hid-daemon 的 TCP socket
│       │   ├── scrcpy.rs   ← 复用 android-hid-connect 的字节级 HID
│       │   └── unified.rs  ← 跨 daemon 原子操作(tap 走 scrcpy + wait 走 daemon)
│       ├── verbs/          ← 70+ verb 的 typed Rust 包装
│       ├── selectors.rs    ← CSS-like a11y selector parser
│       ├── geometry.rs     ← AgentPoint / AgentRect / basis points
│       ├── plan.rs         ← AgentPlan typed 多步计划(typed)
│       └── errors.rs
│
├── android-hid-cli/                          ← `ah` 二进制(对应 `hs`)
│   ├── Cargo.toml           ← 零三方依赖(or 最小)
│   └── src/
│       ├── main.rs
│       ├── args.rs         ← CLI 解析(std)
│       ├── json.rs         ← `--json` 输出
│       ├── commands/       ← 每个子命令一个文件
│       └── install.rs      ← adb push + adb forward + adb shell start
│
├── android-hid-py/                          ← Python SDK(PyO3 + subprocess 双模式)
│   ├── Cargo.toml
│   ├── pyproject.toml
│   └── src/lib.rs
│
└── docs/
    ├── roadmap-exceed-handsets.md             ← 本文件
    ├── workspace.md
    ├── wire-protocol.md                       ← 70+ verb 的 wire 布局
    └── daemon-internals.md                    ← 反射 / binder 细节
```

---

## 2. 7 个阶段, 每个阶段可独立 ship

### Phase 1: Workspace + 骨架 (Task #6) — 1 session
- 新建 `Cargo.toml` 根 `[workspace]` + 4 个子 crate `Cargo.toml` + 占位 `lib.rs/main.rs`
- 写根 `README.md` 描述 workspace 结构
- 在 `docs/workspace.md` 写成员说明 + 依赖方向
- CI 矩阵加 build all crates

### Phase 2: 协议 + 传输 + 生命周期 (Task #7) — 2-3 sessions
- `android-hid-protocol`: frame 编码/解码、ErrorCode、Verb enum (70+)、k=v parser、版本握手
- `android-hid-daemon`:
  - `binder.rs`: ServiceManager.getService 反射、IActivityManager$Stub.asInterface、disableHiddenApiExemptions
  - `server.rs`: 长度前缀 frame 分发器(类似 handsets Server.java),连接线程、daemon 线程池
  - `main.rs`: app_process 启动路径
- `android-hid-agent`:
  - `backend/daemon.rs`: TCP socket + frame parser + typed result
  - `backend/scrcpy.rs`: 复用 `android-hid-connect::HidSession`
  - `backend/unified.rs`: 跨 backend 决策表
- 集成测试: 端到端 dial-tone 几个最简 verb (ping/info/quit)

### Phase 3: 核心 verb (Task #8) — 3-4 sessions
- input: tap / swipe / down / move / up / scroll / key / text / swipe_dir
- clipboard: clip_get / clip_set / clip_watch
- files: pull / push (256KB chunk, drain on error)
- pm: list / path / uninstall / grant / revoke
- am: start / force_stop / kill / broadcast
- settings: get / put
- props: get / set
- shell: ARGV 透传 + `__exit__ N` trailer
- wm: info / rotation
- dumpsys / logcat / monitor: 流式响应
- installer: install / install_multi / deeplinks (binary AndroidManifest.xml 解析)

### Phase 4: a11y + 状态 + 等待 (Task #9) — 3 sessions
- a11y dump: Traverse 递归 JsonOut, FLAG_PREFETCH_DESCENDANTS_HYBRID
- node actions: click / long_click / set_text / scroll / focus / submit / paste
- selector parser: CSS-like (`EditText[hint~=Email]`, `:visible :clickable :has-text("x") :near(SEL, PX) :below() :right-of() :in() :text-is() :focused :checked`)
- state mirror: 4 个事件源(a11y, PACKAGE_*, DisplayListener, ACTION_CONFIGURATION_CHANGED) + 重计算线程 + COW 订阅
- wait registry: event-driven, no poll
- UiEvents: 单 listener 槽 + COW fan-out

### Phase 5: 截图 + 视频流 + providers (Task #10) — 4 sessions
- screencap: ImageReader + VirtualDisplay 优先,fallback `ua.takeScreenshot`
- JPEG / WebP / PNG 编码
- H.264 streamer: MediaCodec + InputSurface + SPS/PPS 注入 + keyframe API
- TileStreamer: tile JPEG, low bandwidth
- Streamer: basic JPEG stream
- content providers: IActivityManager.getContentProviderExternal + shell UID
- notifications: dumpsys notification --noredact
- location: dumpsys location
- manifest: AssetManager.addAssetPath + XmlResourceParser

### Phase 6: 超越 handsets (Task #11) — 3-4 sessions
- FanoutSession: 多设备并发 dispatch
- AI frame summary ↔ a11y selector 对齐(把 detection box 投影到 a11y node)
- 跨 daemon 原子操作:`tap_and_dump` (scrcpy tap + daemon dump) 一次 round-trip
- 编译期 feature flag(`a11y`, `h264`, `fanout`, `python`, ...)
- 性能优势: HID gamepad 240Hz + 10 multitouch(从 scrcpy backend 借用)
- typed async first (Tokio)
- 离线 plan preflight

### Phase 7: CLI + Python + 文档 + 测试 (Task #12) — 2 sessions
- `ah` CLI: 零三方依赖 / 静态二进制
- Python SDK: PyO3 native + subprocess fallback
- 真机 E2E (SM-G9910 Android 11)
- 基准测试 + 比较 vs hs
- CI: ubuntu/macos/windows 矩阵 + 真机集成 job(可选 self-hosted runner)
- 完整 docs/: architecture.md, wire-protocol.md, daemon-internals.md

---

## 3. 与 handsets 能力矩阵对照

| 能力 | handsets | android-hid-* (新) | 超越点 |
| ---- | -------- | ------------------ | ------ |
| Tap/Swipe/Key/Text | ✅ UiAutomation | ✅ MotionEvent + KeyEvent (Rust) | 同样 + Rust 类型安全 |
| **8 slot HID gamepad** | ❌ | ✅ 通过 scrcpy backend | **handsets 没有** |
| **10 点 multitouch** | ❌ | ✅ 通过 scrcpy backend | **handsets 没有** |
| **AI 帧摘要 + a11y 对齐** | ❌ | ✅ | **handsets 没有** |
| 多点设备 fan-out | ⚠️ CLI 级 | ✅ typed session | 类型安全 |
| 截图 | ✅ VirtualDisplay | ✅ 同样 | 同样 |
| 视频流 H.264 | ✅ MediaCodec | ✅ 同样 | 同样 |
| a11y dump | ✅ | ✅ | 同样 |
| CSS-like selector | ✅ | ✅ | 同样 |
| 节点动作 | ✅ | ✅ | 同样 |
| 事件等待 | ✅ | ✅ | 同样 |
| 包管理 | ✅ | ✅ | 同样 |
| ContentProvider | ✅ | ✅ | 同样 |
| 通知 | ✅ | ✅ | 同样 |
| 文件 push/pull | ✅ | ✅ | 同样 |
| dumpsys/logcat | ✅ | ✅ | 同样 |
| shell exec | ✅ | ✅ | 同样 |
| 状态镜像 | ✅ | ✅ | 同样 |
| Settings/Props | ✅ | ✅ | 同样 |
| **typed Rust API** | ❌ (CLI/Python) | ✅ | **native first-class** |
| **async / Tokio** | ❌ | ✅ | **native first-class** |
| **编译期 feature flag** | ❌ (whole jar) | ✅ (per feature) | **更小二进制** |
| **零 Java / 纯 Rust** | ❌ (jar) | ✅ | **单栈** |
| **字节级 HID byte-exact** | ❌ | ✅ (从 scrcpy 借) | **scrcpy 互操作** |
| **plan-level preflight** | ❌ | ✅ | **可重放** |

---

## 4. 每个 subagent 任务模板

每次 subagent 启动,传入:

1. **目标文件清单** (本 phase 涉及的所有 Rust 源文件路径)
2. **输入**:
   - wire format 文档(`/mnt/ssd/codespace/tool/android-control/handsets/docs/wire.md`)
   - 对应 Java 源(`/mnt/ssd/codespace/tool/android-control/handsets/src/dev/handsets/daemon/<File>.java`) 全文
   - 对应 handsets-cli handler(`/mnt/ssd/codespace/tool/android-control/handsets/handsets-cli/src/<file>.rs`) 全文(如有)
3. **约束**:
   - 纯 std(无 java-style reflection 是不可能的 — 但用 Rust 反射 `nix` + `android-bindgen` 或纯 unsafe)
   - 写测试 (`#[cfg(test)] mod tests`)
   - 字节级 wire format 必须与 handsets 兼容(让我们能 0 改动调用 handsets 的 hs.jar)
4. **输出**:
   - Rust 源文件 + Cargo.toml 改动
   - 测试代码
   - 在 `docs/daemon-internals.md` 写一段 (java → rust 翻译说明)
   - 集成测试: tcp simulator + 端到端 verb round-trip

---

## 5. 风险 & 缓解

| 风险 | 缓解 |
| ---- | ---- |
| MediaCodec/H.264 跨 Android 版本 ABI 不稳 | 复用 handsets 已知可用参数(BITRATE_MODE_VBR, MAX_B_FRAMES=0, KEY_PREPEND_HEADER off); 在 docs/daemon-internals.md 记录 vendor quirks |
| UiAutomation 在某些 OEM ROM 拒绝 app_process | 沿用 handsets 经验: `Main.connectWithRetry` + `setRunAsMonkey` |
| Settings provider 拒绝 app_process | 走 `IActivityManager.getContentProviderExternal` (沿用 handsets 经验) |
| `am start` 必须 `callingPackage="com.android.shell"` | 硬编码 + 沿用 |
| 反射 hidden API 在新 SDK 改名 | `Binders` 用 `getMethod` 模糊匹配,fallback 多 overload |
| Rust 端 NDK 编译慢 | cache sccache + CI 矩阵先验证 ubuntu/macos/windows std-only crate |
| 现有 `android-hid-connect` 测试可能受 workspace 改动影响 | 先把 `android-hid-connect` 提升为 workspace member 但保留其独立 build path, 验证 `cargo test` 通过后再加新 crate |

---

## 6. 立即可派工的 Phase 1 子任务

Phase 1 任务列表(顺序执行):

1. **创建根 `Cargo.toml` workspace** — 1 个 PR
2. **创建 `android-hid-protocol` crate** — 1 个 PR
3. **创建 `android-hid-daemon` crate** — 1 个 PR
4. **创建 `android-hid-agent` crate** — 1 个 PR
5. **创建 `android-hid-cli` crate** — 1 个 PR
6. **创建 `android-hid-py` crate** — 1 个 PR(stub, 等 Phase 7 完善)
7. **更新 `AGENTS.md` §1** — 加 "sibling crates in workspace" 说明
8. **更新 `docs/INDEX.md`** — 加 workspace.md
9. **更新根 `README.md`** — 加 workspace overview

每项 PR 都需要 `cargo test` + `cargo clippy` 全过。

---

## 7. 验收标准

- 全部 70+ handsets verb 在 `android-hid-daemon` 上有对应 Rust 实现,wire format 兼容
- `android-hid-agent` 提供 typed Rust API, 编译期类型安全
- `ah` CLI 提供 handsets `hs` 风格短命令 + `--json` 输出
- 至少 1 个真实设备(目标 SM-G9910 Android 11)的 E2E 跑过
- 字节级 HID gamepad + multitouch 跑过(从 scrcpy backend 借)
- AI frame summary + a11y dump 在同一 session 内 1-call 对齐
- 性能: bench 数据 ≥ handsets 1-8ms p50
- 文档: `docs/wire-protocol.md` + `docs/daemon-internals.md` + `docs/workspace.md` 全部完成
- 全部 Rust 公开 API 用 `cargo doc --no-deps` 无 warning

---

最后更新: 2026-06-29
