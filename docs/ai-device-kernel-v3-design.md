# AI Device Kernel — 重新设计 `android-hid` 架构

> Date: 2026-06-30
> Status: **Proposal v2** (更新: 加入 2026-06-30 deep research 调研结论 — LiteRT 端侧算力集成 + 端侧视觉服务 + Memory 层 + AutoDroid 借鉴; 取代 v1 的 4 phase plan)
> Goal: **让 AI 精准快速地操控 Android 设备**。基于 `comparison-with-handsets.md`、
> `agent-v2-design.md` (v2) 和 2026-06-30 deep research (6 角度, 24 sources, 18 confirmed + 7 killed claims) 重写。

## Changelog

| Date | Version | Changes |
|---|---|---|
| 2026-06-30 | v2 | 加入 §10 调研集成: 端侧算力迁移到 LiteRT (NNAPI Android 15 deprecated); §3.2 4 件套 → 5 件套 (加 Memory); §3.6 端侧算力分层 + LiteRT 集成; §3.7 Hybrid AI 架构; §11 References; 实施路径 6 phase → 8 phase; P0 必须做加 UHID 兜底; 借鉴 AutoDroid functionality-aware UI representation |
| 2026-06-30 | v1 | 初版: 4 件套 + 4 verb + 6 phase |

---

## 0. 一句话定位

**当前 `android-hid`** = 字节级 scrcpy UHID 移植 + 半成品的 agent + 半成品的 daemon。
三个 crate, 三个协议, 一堆 in-flight 设计, 没有任何一个端到端跑通。
**真要 LLM 操控 Android, 得重做。**

**新架构** = **AI 设备内核 (AI Device Kernel, ADK)**:
一个 on-device Rust 守护进程, 对 LLM agent 提供 **typed Action + ground truth
feedback + plan execution + streaming observation + on-device intelligence
(Memory + 端侧视觉服务)** 五件套, 端到端 < 5ms (本地 action) / < 50ms (端侧视觉)。

---

## 1. 现状诊断 — 为什么"差劲"

### 1.1 5 个真问题

| # | 问题 | 后果 |
|---|---|---|
| **P1** | **三栈架构**: scrcpy-server (HID) + hs.jar (a11y) + 自家 daemon (通信), 三个进程三个协议 | 启动慢 (200-500ms scrcpy 启动), 状态分裂 (scrcpy 不知道 daemon 做了什么, 反之亦然), 维护成本 ×3 |
| **P2** | **没有 ground truth**: tap → "ok" → agent 必须再发 1 条 `dump_active` 才能看到结果 | 单步 2 RTT, LLM 循环步进 10-15ms, 浪费 |
| **P3** | **协议是 verb-centric**, 70+ wire verb, agent 写 plan 时要在脑子里做 verb→动作的映射 | 复杂, 容易写错, LLM token 浪费 |
| **P4** | **16ms tap sleep 是 hack**: `Input.java:53` 硬编码 16ms `Thread.sleep`, 为了让 gesture detector "看起来像真 tap" | Android 14+ 不需要这 16ms; 我们测试过 |
| **P5** | **scrcpy "byte-exact" 约束**: AGENTS.md §6 锁死不能改 scrcpy 协议对齐 | 优化空间被砍: 不能改 frame 格式, 不能加新 verb, 不能改 control msg 时序 |

### 1.2 隐性问题 (P6-P10)

| # | 问题 |
|---|---|
| **P6** | **没有 idempotency**: tap 是 fire-and-forget, 网络断了不知道是不是发了 |
| **P7** | **没有真正的 state model**: `~/.handsets/state-<port>.json` 是文件读写, 不是真正的 stream |
| **P8** | **没有 typed Plan**: `AgentPlan` 是 20K 行 enum + 验证, 但下发到 daemon 还是变回 verb list, 跨进程丢类型 |
| **P9** | **没有 streaming observation**: 每次 `dump_active` 是 1 RTT, 大树 4.58ms, 频繁轮询 = 浪费 |
| **P10** | **没有可恢复性**: agent 断线重连后, daemon 不知道 agent 之前发到哪了, 一切从头来 |

### 1.3 当前架构图 (问题视觉化)

```
                host                                          device
   ┌─────────────────────────────┐                ┌──────────────────────────────┐
   │ LLM agent                    │                │                              │
   │      │                       │                │  ┌──────────────────────┐    │
   │      ▼                       │                │  │  scrcpy-server       │    │
   │ AgentControlSession ────────►│── TCP:27183 ──│─►│  (Java, scrcpy 协议)  │    │
   │   │                          │                │  │   - HID via /dev/uhid │    │
   │   │ mpsc (单 dispatcher)     │                │  │   - 视频流 H.264     │    │
   │   ▼                          │                │  └──────────────────────┘    │
   │ HidClient                    │                │  ┌──────────────────────┐    │
   │   │                          │                │  │  hs.jar (Java)       │    │
   │   ▼                          │                │  │   - a11y UiAutomation │    │
   │ DaemonBackend ──────────────►│── TCP:9008 ────│─►│  - 70+ verb           │    │
   │                              │                │  │   - 16ms tap sleep   │    │
   │                              │                │  │   - 截图 + H.264 stream│   │
   │                              │                │  └──────────────────────┘    │
   └─────────────────────────────┘                └──────────────────────────────┘

   问题:
   - 两条 socket, 两套 handshake, 两套错误码
   - scrcpy 不知道 hs.jar 做了什么, 反之亦然
   - agent 必须知道 "tap 走 scrcpy, dump 走 daemon"
   - 启动 200-500ms (scrcpy 启动)
```

**结论: 这不是"两个互补的项目", 这是"两个半成品, 互相不信任, agent 是和稀泥的胶水"**。

---

## 2. 设计目标 (围绕"AI 精准快速")

| 目标 | 指标 | 当前 | 目标 |
|---|---|---|---|
| **G1** 单步输入延迟 (tap+observe) | p50 | 25-50ms (2 RTT + 16ms sleep) | **< 5ms** (1 RTT + 0ms sleep) |
| **G2** LLM 循环步进 (1 plan 步) | p50 | 50-100ms | **< 10ms** |
| **G3** 地面真值 (ground truth) | 必返回 | ❌ | ✅ 每 action 必返回 (changed_nodes, frame_diff) |
| **G4** Plan 原子性 | 1 RTT 多步 | ❌ 1 RTT 1 步 | ✅ 1 RTT N 步 (typed) |
| **G5** 流式 observation | subscribe | ❌ poll | ✅ server push, 多 subscriber |
| **G6** 设备端依赖 | 0 个第三方 daemon | 2 个 (scrcpy + hs) | **1 个 (自研 Rust daemon)** |
| **G7** 协议 | AI-friendly | ❌ verb-centric ASCII | ✅ **action-typed + binary + 紧凑** |
| **G8** 启动延迟 | 冷启 | 200-500ms (scrcpy 启动) | **< 50ms** (单个 5MB Rust daemon) |
| **G9** 跨语言 | host SDK | Rust + Python + 任何 JSON | ✅ Rust + Python + 任何 binary |
| **G10** 可恢复 | 断线重连 | ❌ 一切从头 | ✅ idempotent + resume |

---

## 3. 新架构 — AI 设备内核 (AI Device Kernel)

### 3.1 三层模型 (从下到上)

```
┌──────────────────────────────────────────────────────────────────┐
│ Layer 0: LLM Agent                                              │
│   - Python / Rust / Node / 任何能调 RPC 的东西                  │
│   - 用 typed Action 写 plan, 拿到 typed Result                  │
│   - 不直接接触字节、socket、ADB                                 │
└──────────────────────────────────────────────────────────────────┘
                          ▲
                          │  Layer 0↔1 边界: typed Action/Plan/Result (跨语言)
                          │
┌──────────────────────────────────────────────────────────────────┐
│ Layer 1: AI Device Kernel (本项目核心)                          │
│                                                                  │
│   ┌───────────────────────────────────────────────────────┐    │
│   │  Rust 守护进程 (on-device) — 一个二进制 = 一个端口     │    │
│   │                                                       │    │
│   │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  │    │
│   │  │ Action      │  │ Observation │  │ Predicate   │  │    │
│   │  │ executor    │  │ stream      │  │ engine      │  │    │
│   │  │             │  │             │  │             │  │    │
│   │  │ - tap       │  │ - a11y tree │  │ - text-match│  │    │
│   │  │ - type      │  │ - frame     │  │ - scene-    │  │    │
│   │  │ - key       │  │ - events    │  │   change    │  │    │
│   │  │ - launch    │  │ - state     │  │ - activity  │  │    │
│   │  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  │    │
│   │         │                │                │         │    │
│   │  ┌──────┴────────────────┴────────────────┴──────┐  │    │
│   │  │  State model (single source of truth)         │  │    │
│   │  │  - last_input                                │  │    │
│   │  │  - last_observation                          │  │    │
│   │  │  - predicate set (wait conditions)           │  │    │
│   │  │  - event queue (Activity/lifecycle/a11y)      │  │    │
│   │  └───────────────────┬──────────────────────────┘  │    │
│   │                      │                             │    │
│   │  ┌───────────────────┴──────────────────────────┐  │    │
│   │  │  Input engine (no 16ms sleep, UHID+          │  │    │
│   │  │  MotionEvent 两条路径)                        │  │    │
│   │  └───────────────────┬──────────────────────────┘  │    │
│   │                      │                             │    │
│   │  ┌───────────────────┴──────────────────────────┐  │    │
│   │  │  Frame pipeline (H.265 keyframe on demand,    │  │    │
│   │  │  scene change detection)                      │  │    │
│   │  └───────────────────┬──────────────────────────┘  │    │
│   │                      │                             │    │
│   │  ┌───────────────────┴──────────────────────────┐  │    │
│   │  │  Capability surface (pm/am/wm/settings/...)   │  │    │
│   │  └──────────────────────────────────────────────┘  │    │
│   │                                                       │    │
│   │  端口 9008, 16 MiB 帧, length-prefix binary,         │    │
│   │  1 个连接, 1 个 thread-pool (default 4 worker)       │    │
│   └───────────────────────────────────────────────────────┘    │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
                          ▲
                          │  Layer 1↔2 边界: file descriptor (adb forward / USB)
                          │
┌──────────────────────────────────────────────────────────────────┐
│ Layer 2: 传输 (Transport)                                       │
│   - adb forward tcp:9008 localabstract:ahdk                     │
│   - 可选: USB 桥接 (厂商定制)                                  │
│   - 可选: 网络直连 (开发模式)                                  │
└──────────────────────────────────────────────────────────────────┘
```

**关键变化**:
- **1 个守护进程, 1 个端口, 1 个协议** (替换 scrcpy-server + hs.jar)
- **State model 是 single source of truth** (替换 `~/.handsets/state-*.json`)
- **Action 返回 ground truth** (替换 tap→ok + dump→2 RTT)
- **Predicate 引擎** (替换 sleep 轮询)
- **Observation stream** (替换每次 poll dump)

### 3.2 核心抽象 — 5 件套 (v2 新增 Memory)

> 2026-06-30 update: 借鉴 AutoDroid (arXiv 2308.15272) 的 `functionality-aware UI
> representation` + `offline UTG pre-exploration` 思路, 增加第 5 件套 `Memory`。
> AutoDroid 用 UTG (UI Transition Graph) 离线记忆 app 行为, +36.4% 任务成功率;
> v3 用 Memory 实时累积 (online), 跨 session 复用。

#### 3.2.0 Memory (cross-session 记忆层) — v2 新增

```rust
/// Screen fingerprint → 成功 action 序列。online 累积, 跨 session 复用。
pub struct Memory {
    /// Screen 指纹: a11y tree hash + frame pHash + package:activity
    pub screen_id: ScreenId,
    /// 屏对应的简化 UI 表示 (借鉴 AutoDroid HTML-tagged 风格)
    pub ui_repr: UiReprHtml,  // 500B vs 完整 a11y 50KB
    /// 之前在这个屏成功的 action 序列
    pub success_actions: Vec<ActionSequence>,
    /// 跨屏转换: (screen_id, action) -> next_screen_id
    pub transitions: HashMap<(ScreenId, ActionFingerprint), ScreenId>,
}

pub struct ScreenId(pub [u8; 16]);  // blake3(a11y-hash + frame-phash + pkg:activity)
```

**关键设计**:
- **Online 累积**: 不需要 offline pre-exploration (不像 AutoDroid), daemon 实时记录
- **跨 session 复用**: agent 重启后, daemon 还在, memory 还在
- **UI 简化表示**: HTML-tagged 风格 (e.g. `<btn id=login text="Login" clickable/>`),
  LLM 友好 (token 100× less than raw a11y)
- **Screen fingerprint**: 16-byte blake3, 跨设备/跨时间稳定

**借鉴 AutoDroid 但不同**:
- AutoDroid: 离线 random walk + UTG, 一次预跑, 之后所有任务复用
- v3: online 累积, 边用边学, **不需要预跑** (冷启直接用, 越用越准)
- AutoDroid: UTG 是全局图; v3: Memory 是 per-app/per-screen 局部缓存

#### 3.2.1 Action (typed, atomic, ground-truth)

```rust
/// 一次原子操作, daemon 端执行并返回 ground truth。
pub enum Action {
    /// Tap at (x, y). 0 sleep, 0 fake delay.
    Tap { x: i32, y: i32, deadline_ms: u32 },
    /// Tap on a11y node matched by selector.
    /// Daemon resolves selector on latest a11y snapshot, taps center.
    TapSelector { selector: String, deadline_ms: u32 },
    /// Type ASCII text into focused field.
    TypeText { text: String, deadline_ms: u32 },
    /// Press Android key code (typed AndroidKeycode).
    Key { code: u32, deadline_ms: u32 },
    /// Swipe from (x1, y1) to (x2, y2).
    Swipe { x1: i32, y1: i32, x2: i32, y2: i32, dur_ms: u32, deadline_ms: u32 },
    /// Gamepad input — full 15-byte HID report.
    GamepadFrame { report: [u8; 15], deadline_ms: u32 },
    /// Launch by component or by package name.
    Launch { target: String, by: LaunchBy },
    /// Set clipboard (with optional paste).
    SetClipboard { text: String, paste: bool },
    /// Inject raw UHID bytes (escape hatch).
    InjectRaw { bytes: Vec<u8> },
    /// ~12 个 typed action, 替换 70+ verb
}

pub struct ActionResult {
    /// Server-assigned unique ID (idempotency).
    pub id: ActionId,
    /// Did the action actually land?
    pub landed: bool,
    /// Ground truth: what changed.
    pub ground_truth: GroundTruth,
    /// How long (server-measured) the action took.
    pub elapsed_ms: u32,
}

pub struct GroundTruth {
    /// a11y nodes that changed (added/removed/text-changed).
    pub a11y_diff: Vec<A11yNodeDiff>,
    /// Frame diff summary (if frame was sampled).
    pub frame_diff: Option<FrameDiff>,
    /// New focused window.
    pub focus: Option<u32>,
    /// Scene change score [0, 1].
    pub scene_change: f32,
    /// Event log since the action started.
    pub events: Vec<DeviceEvent>,
}
```

**关键设计**:
- 每个 Action 自带 `deadline_ms` — daemon 知道什么时候放弃 (P10 部分解)
- Action 必返回 `ground_truth` (P3 解: 省 1 RTT)
- Action ID 支持 idempotent retry (P6 解)
- 12 个 Action enum 覆盖 80% 用例; escape hatch `InjectRaw` 给剩下 20%

#### 3.2.2 Plan (typed, atomic, resumable)

```rust
/// 多步操作, 1 RTT, atomic 语义。
pub struct Plan {
    pub id: PlanId,
    pub steps: Vec<PlanStep>,
    /// Stop on first step failure.
    pub abort_on_error: bool,
    /// Optional checkpoint — 每 N 步返回 ground truth 快照。
    pub checkpoint_every: u32,
}

pub struct PlanStep {
    pub id: StepId,
    pub action: Action,
    /// Optional predicate — daemon 等到满足再执行。
    pub wait_before: Option<Predicate>,
    /// Optional postcondition — daemon 验证动作落点正确。
    pub verify_after: Option<Predicate>,
}

pub struct PlanResult {
    pub plan_id: PlanId,
    pub steps: Vec<StepResult>,
    /// Final observation snapshot.
    pub final_observation: Observation,
    /// Total elapsed.
    pub total_elapsed_ms: u32,
}
```

**关键设计**:
- 1 plan = 1 frame = 1 reply (P2 解)
- `verify_after` 让 daemon 自检"我点了 login 吗?" — 如果点了 1 个不同按钮, 立刻报错, agent 不需要再 dump (P3 强化)
- 1 RTT = N step, 替换 N 个 verb round-trip

#### 3.2.3 Observation (streamed, not polled)

```rust
/// 一次 observation 快照, 由 daemon push 或被 fetch。
pub struct Observation {
    /// Monotonic sequence, 防止 race.
    pub seq: u64,
    /// Wall-clock ms since daemon start.
    pub timestamp_ms: u64,
    /// Optional a11y tree snapshot.
    pub a11y: Option<A11yTree>,
    /// Optional H.265 frame snapshot.
    pub frame: Option<FrameSnapshot>,
    /// Latest device state.
    pub state: DeviceState,
    /// Events since the previous observation.
    pub events: Vec<DeviceEvent>,
}

pub enum DeviceEvent {
    ActivityResumed { component: String },
    ActivityPaused { component: String },
    WindowFocusChanged { window_id: u32 },
    PackageAdded { pkg: String },
    PackageRemoved { pkg: String },
    ConfigurationChanged,
    SceneChangeDetected { score: f32 },
    NotificationPosted { key: String },
    ClipboardChanged,
    Uptime,
}
```

**关键设计**:
- 1 个 observation stream, 多个 subscriber, server-side push (P7/P9 解)
- LLM 可以 `observe(since_seq=N)` 拿到自 N 之后的所有事件
- `seq` 严格递增, 防止 observe-then-act race

#### 3.2.4 Predicate (declarative, no polling)

```rust
pub enum Predicate {
    /// Wait for text to appear in any a11y node.
    TextAppears { text: String, timeout_ms: u32 },
    /// Wait for activity to resume.
    Activity { component: String, timeout_ms: u32 },
    /// Wait for frame to stabilize (no scene change for N consecutive frames).
    SceneStable { duration_ms: u32, timeout_ms: u32 },
    /// Wait for a11y tree to be unchanged for N ms.
    A11yIdle { duration_ms: u32, timeout_ms: u32 },
    /// Wait for custom predicate (e.g. specific a11y node present).
    SelectorMatches { selector: String, timeout_ms: u32 },
    /// Wait for an event to fire.
    EventFires { kind: EventKind, timeout_ms: u32 },
}
```

**关键设计**:
- daemon 维护 predicate 集合, 事件触发时检查
- **0 polling, 0 CPU 浪费** (P4 间接解)
- timeout 是 backstop, 不是常规等待手段

### 3.3 协议 — 4 个核心 verb, 替换 70+

**当前**: 70+ wire verb, 每 verb 1 RTT, agent 写 plan 时要在脑子里做 verb 映射。

**新**: **4 个核心 verb**, AI-friendly:

```
┌────────┬───────────────────────────────────────────────────────────┐
│ verb   │ 用途                                                       │
├────────┼───────────────────────────────────────────────────────────┤
│ action │ 1 个 action + 1 个 reply, ground truth 包含在 reply       │
│ plan   │ 1 个 plan (多 action) + 1 个 reply, 所有 step 的 ground   │
│        │ truth + final observation                                  │
│ observe│ subscribe 到 observation stream, server-push 增量事件     │
│ query  │ 1-shot 拉一次 observation (用于 idle 时 fallback)          │
└────────┴───────────────────────────────────────────────────────────┘
```

**70+ verb 怎么办**: 保留, 但**降级为 internal verb**, 通过 typed Action 暴露:
- `pm_list` → `Action::ListPackages` → 结果返回 `Vec<PackageInfo>` (typed)
- `dumpsys window` → `Action::DumpService { name: "window" }` → 返回 raw text
- `key BACK` → `Action::Key { code: KEYCODE_BACK }`
- `clip_get` → `Action::Query::Clipboard` (in observe stream)

LLM agent 写 plan 时用 typed Action, 不需要碰 verb 名字。70+ verb 只是 daemon 内部的实现细节。

### 3.4 协议字节布局 (binary, 紧凑)

```
┌────────────────────────────────────────────────────────────────┐
│ wire frame                                                      │
│ ┌──────────┬──────────┬──────────────────────────────┐         │
│ │ type (1) │ flags(1) │ payload (varint length)        │         │
│ └──────────┴──────────┴──────────────────────────────┘         │
└────────────────────────────────────────────────────────────────┘

Action frame (host → device):
  type=0x01 (action)
  flags=0x80 (idempotent) | 0x40 (wait ground truth)
  payload = typed Action (postcard-serialised binary)
  reply = ActionResult with ground_truth

Plan frame (host → device):
  type=0x02 (plan)
  flags=0x80 (idempotent) | 0x20 (checkpoint every N)
  payload = typed Plan (postcard-serialised binary)
  reply = PlanResult (1 frame, all step results + final observation)

Observe frame (host → device):
  type=0x03 (observe)
  payload = ObserveRequest { since_seq: u64, filter: Vec<EventKind> }
  reply = server-stream of Observation (multi-frame, terminated by seq N or timeout)

Query frame (host → device):
  type=0x04 (query)
  payload = Query { a11y: bool, frame: bool, state: bool }
  reply = Observation
```

**关键决策**:
- **binary (postcard) 不是 JSON**: parser 5-15μs vs postcard 0.5-1μs, 大 payload 差异 10×
- **varint 长度前缀 不是 u32 BE**: 小帧省 2-3 字节
- **typed Action 不是 verb-string**: LLM-friendly, 编译期校验, 自动文档
- **4 个核心 verb 不是 70+**: 极简 API, 70+ 内部化
- **ground truth 必返回**: 1 RTT = input + observe
- **idempotent + retry**: 网络断开不丢, 复现可

### 3.5 Capability surface (内部, 70+ verb)

70+ 现有 verb 不再是 wire-level API, 而是 **internal capability** 通过 typed Action
暴露。这样:

- agent 写 plan 简单 (typed enum)
- daemon 内部实现可以演化 (verb 增删, agent 无感)
- 测试可以 mock 单个 capability
- 跨版本兼容 (capability 找不到 → typed error, agent 重试或 fallback)

```rust
// 内部 capability trait (daemon-side)
trait Capability {
    fn name(&self) -> &'static str;
    fn execute(&self, ctx: &CapabilityContext) -> Result<CapabilityOutput>;
}

// 注册表
struct CapabilityRegistry {
    caps: HashMap<&'static str, Box<dyn Capability>>,
}

// typed Action 映射到 1+ capability
impl Action {
    fn capabilities(&self) -> Vec<&'static str> {
        match self {
            Self::Tap { .. } => vec!["input.motion_event"],
            Self::TapSelector { .. } => vec!["a11y.resolve", "input.motion_event"],
            Self::TypeText { .. } => vec!["input.key_event", "shell.ime"],
            Self::Key { .. } => vec!["input.key_event"],
            // ...
        }
    }
}
```

### 3.6 端侧算力集成 (v2 新增) — LiteRT + GPU delegate

> 2026-06-30 deep research update: **NNAPI 在 Android 15 (API 35) deprecated**,
> Google 官方迁移路径是 **TensorFlow Lite in Google Play Services + TFLite GPU
> delegate**。v3 daemon 必须按这条路径集成, 而不是再调 NNAPI。

#### 3.6.1 NNAPI 已弃用, 迁移到 LiteRT

**关键事实** (来源: https://developer.android.com/ndk/guides/neuralnetworks/migration-guide,
updated 2026-03-06, 3-vote verified):

- NNAPI 在 Android 15 起 deprecated
- 官方推荐迁移: **TFLite in Play Services + TFLite GPU delegate**
- v3 daemon 集成 ML 模型走 LiteRT 路径, 不调 NNAPI

**LiteRT 两条集成路径** (来源: https://ai.google.dev/edge/litert/android/delegates/gpu):

| 路径 | 适用 | 依赖 |
|---|---|---|
| **Play services bundled** (推荐) | 大多数设备 (有 Play services) | `play-services-tflite-java` + `play-services-tflite-gpu 16.5.0` |
| **Standalone Maven** | 非 Play 设备 (中国 OEM, 鸿蒙 fork) | `litert` + `litert-gpu` + `litert-gpu-api` |

**GPU delegate 关键约束** (2-vote verified):
> "The GPU delegate must be created on the same thread that runs it."

**对 v3 daemon 的影响**:
- daemon 集成 LiteRT 时, **GPU delegate 必须在主线程初始化 + 推理**
- 多线程模型需要 `GpuDelegateFactory` + thread-local delegate
- Rust daemon 通过 JNI 调, 避免 NDK 跨语言内存问题

#### 3.6.2 端侧算力分层 (推荐架构)

```
┌──────────────────────────────────────────────────────────┐
│ Layer 0: LLM Agent (host) — 云端或本地                │
│   - GPT-4 / Claude (云端) 推理 plan, 200-2000ms        │
│   - 直接调 v3 daemon TCP                                │
└──────────────────────────────────────────────────────────┘
                          ▲
┌──────────────────────────────────────────────────────────┐
│ Layer 1: v3 Daemon (on-device Rust)                     │
│   - typed Action / Plan / Observation / Memory          │
│   - **端侧视觉 / 语义服务 (v2 新增) — Phase 4-5**  │
│     - OCR (ML Kit v2)                                   │
│     - UI detection (YOLOv8n-int8 TFLite)                │
│     - grounding (Florence-2-base ONNX/TFLite)          │
│     - 端侧 VLM (GUI-Owl-1.5 sub-7B, 待验证)          │
│   - 通过 JNI 调 LiteRT (Play services 或 standalone)    │
│   - GPU delegate 必须在主线程初始化                    │
└──────────────────────────────────────────────────────────┘
                          ▲
┌──────────────────────────────────────────────────────────┐
│ Layer 2: Android Runtime                                │
│   - LiteRT runtime (Play services or Maven)            │
│   - GPU delegate (OpenGL ES / Vulkan)                  │
│   - NPU vendor driver (Snapdragon Hexagon, MTK APU)    │
│   - DSP (Qualcomm Hexagon)                             │
└──────────────────────────────────────────────────────────┘
                          ▲
┌──────────────────────────────────────────────────────────┐
│ Layer 3: 硬件 (端侧算力)                              │
│   - Snapdragon 8 Gen 3/4 (NPU 45-50 TOPS, Hexagon V73)│
│   - MediaTek Dimensity 9000+ (APU 60 TOPS)            │
│   - Tensor G3/G4 (TPU)                                 │
│   - GPU: Adreno 740+, Mali-G715+                       │
└──────────────────────────────────────────────────────────┘
```

#### 3.6.3 模型选型 + 集成阶段 (4 阶段)

**Stage 1: 视觉锚定 (Phase 4) — 最低成本, 必做**

| 模型 | 用途 | 大小 (INT8) | 端侧延迟 (1080p) | 集成方式 |
|---|---|---|---|---|
| **ML Kit Text Recognition v2** | OCR 文字识别 | ~10MB (含 lang model) | < 50ms | Google Play services, 0 dep |
| **YOLOv8n-int8** | UI element detection | ~3MB (TFLite) | < 30ms | TFLite GPU delegate, 输入 640×640 |

v3 typed Action:
```rust
pub enum Action {
    // ... 既有 12 个

    /// 端侧 OCR: 找到 query 文字, 返回 BBox 列表
    /// daemon 端通过 ML Kit v2 实现
    LocalizeText { query: String, region: Option<Rect>, deadline_ms: u32 },

    /// 端侧 UI element detection: 找到 class_name 元素
    /// daemon 端通过 YOLOv8n-int8 实现
    DetectElement { class_name: String, confidence_min: u8, deadline_ms: u32 },
}
```

**Stage 2: 场景理解 (Phase 5) — 中等成本**

| 模型 | 用途 | 大小 (INT8) | 端侧延迟 |
|---|---|---|---|
| **Florence-2-base** | 多任务 (caption/grounding/OCR) | ~150MB | ~200ms (GPU) |

v3 typed Action:
```rust
/// Grounding: 找 "ok 按钮" 在图里位置
/// daemon 端通过 Florence-2 grounding task 实现
Ground {
    text: String,            // "ok button"
    image: FrameId,
    deadline_ms: u32,
},
```

**Stage 3: 端侧 VLM (Phase 6) — 高端设备**

| 模型 | 用途 | 大小 (INT8) | 备注 |
|---|---|---|---|
| **GUI-Owl-1.5 (Alibaba, 2025-2026)** | 屏 understanding | 2B/4B/8B | **未 primary-source 验证 sub-7B** (research openQuestion #1) |

v3 typed Action:
```rust
/// 多模态问答: "这个屏幕在做什么?"
/// daemon 端通过 GUI-Owl 推理
AskVisual {
    question: String,
    image: FrameId,
    deadline_ms: u32,
},
```

**Stage 4: 端侧 LLM (Phase 8 长期) — 终极**

- 集成 GUI-Owl-7B-INT4 (待验证量化后质量)
- 配合云端 GPT-4V/Claude, 决策权在 agent

#### 3.6.4 端侧 vs 云端 trade-off

| 维度 | 端侧 (LiteRT) | 云端 (GPT-4V/Claude) |
|---|---|---|
| 延迟 | < 100ms (UI detection) | 500-2000ms (LLM) |
| 隐私 | ✅ 屏内容不出设备 | ❌ 必须上云 |
| 模型能力 | 弱 (sub-10B, 量化) | 强 (100B+) |
| 成本 | 一次性模型 license | per-token API 费 |
| 离线 | ✅ 完全离线 | ❌ 需网 |
| 电耗 | 持续 NPU 高负载 | 无线传输 + 服务器 |
| 适合场景 | **频繁, 视觉锚定** | **稀少, 复杂 reasoning** |

**v3 决策: 端云混合**
- 端侧: 视觉锚定 (OCR + UI detection + grounding) — 频繁且低延迟
- 云端: LLM 推理 (plan / 决策 / 复杂 reasoning) — 慢但强
- 决策层在 host agent: **端侧能算就算, 不能算再上云**

### 3.7 Hybrid AI 架构 (v2 新增) — 端云分工

> 2026-06-30 research update: 借鉴 Google [Hybrid AI](https://developer.android.com/ai/hybrid)
> 和 [Computer Control](https://developer.android.com/ai/computer-control) 思路,
> 端云协同而非端云二选一。

```
┌──────────────────────────────────────────────────────┐
│ Agent (host/cloud)                                    │
│                                                      │
│  step 1: 收到屏内容, 决定下一步                       │
│    ↓ (本地缓存的 memory 检查: 之前有类似 plan 吗?)   │
│    ↓ 命中: 直接复用 typed Action 序列                  │
│    ↓ miss: 上云让 GPT-4V 看帧, 决定 typed Action 序列  │
│                                                      │
│  step 2: 发送 plan 到 v3 daemon                       │
│    daemon 端:                                        │
│    a) typed Action 执行 (毫秒级)                      │
│    b) 端侧视觉服务 (有需要时, e.g. 找坐标) < 100ms │
│    c) ground truth 返回                              │
│    ↓                                                │
│  step 3: ground truth 决定下一步                       │
│    ↓ (ground truth 含 changed_nodes, frame_diff)   │
│    ↓ agent 在内存里推理 (0 RTT)                     │
│    ↓ 决定下一步 action                               │
│                                                      │
│  闭环: 端云分工, 端侧做 90% 视觉锚定, 云端做 10% reasoning│
└──────────────────────────────────────────────────────┘
```

**关键 trade-off 表**:

| 任务 | 端侧 (LiteRT) | 云端 (GPT-4V) | 选哪个 |
|---|---|---|---|
| OCR (找文字坐标) | ML Kit v2 < 50ms | GPT-4V ~1000ms | **端侧** |
| UI element detection (找按钮) | YOLOv8n < 30ms | GPT-4V ~1000ms | **端侧** |
| Grounding (text → BBox) | Florence-2 ~200ms | GPT-4V ~1000ms | **端侧** (高频时) / **云端** (复杂时) |
| Plan decision (下一步做什么) | 弱 (sub-7B) | GPT-4 ~500ms | **云端** |
| 异常诊断 (为什么没点中) | 弱 (sub-7B) | GPT-4 ~500ms | **云端** |
| 跨 app workflow (5 步以上) | 太弱 | GPT-4 ~500ms | **云端** |

**总原则**: 凡是"频繁 + 视觉感知" 走端侧, 凡是"稀少 + reasoning" 走云端。

### 3.8 Functionality-aware UI representation (v2 新增, 借鉴 AutoDroid)

> 2026-06-30 research update: AutoDroid (arXiv 2308.15272) 提的 `functionality-aware UI
> representation` — 把 50KB a11y 树简化成 500B HTML-tagged 表示, LLM 友好。

**对比**:

| 表示 | 大小 | LLM token | 信息保留 |
|---|---|---|---|
| 完整 a11y XML/JSON | 50KB (典型 Setting 页面) | ~12,500 tokens | 100% |
| AutoDroid HTML-tagged | 500B | ~125 tokens | 90% (丢掉 bound/textsize/style) |
| v3 UiReprHtml (新) | 500B | ~125 tokens | 95% (保留 interactive 标志) |

**v3 UiReprHtml 格式**:
```html
<screen pkg="com.android.settings" activity="Settings">
  <node id="search" type="EditText" hint="Search settings" clickable/>
  <node id="network" type="TextView" text="Network & internet" clickable focused/>
  <node id="bluetooth" type="TextView" text="Bluetooth" clickable/>
  <node id="apps" type="TextView" text="Apps" clickable/>
  <node id="display" type="TextView" text="Display" clickable/>
  <node id="about" type="TextView" text="About phone" clickable/>
</screen>
```

**v3 typed Action**:
```rust
pub enum Action {
    /// 返回 UiReprHtml (500B), LLM 友好
    GetUiRepr { screen_id: Option<ScreenId>, deadline_ms: u32 },
}
```

**与 full a11y dump 的关系**:
- LLM plan 阶段: 拿 UiReprHtml (125 tokens, 5-10x token 节省)
- LLM 验证阶段 (verify_after): 拿 full a11y (用于边界条件)
- 端侧视觉阶段 (grounding): 拿 frame (Florence-2 / YOLOv8n)

---

## 4. 性能分析 — 每个 ms 去哪了

### 4.1 当前路径: 1 LLM 步进 (见 + 决策 + 行动 + 验证)

| 阶段 | 当前 (ms) | 原因 |
|---|---|---|
| 1. Agent 看到上一帧 (polled screenshot) | 8.02 | 768px JPEG p50 |
| 2. LLM 思考 (云端) | 200-2000 | 不可控 (LLM 推理) |
| 3. Agent 决定 tap login | < 1 | 内存 |
| 4. 写 tap 命令 | 5 | tap verb + daemon round-trip + 16ms sleep |
| 5. **不知道有没有点中** | 0 | fire-and-forget, 必须自己再 dump |
| 6. 写 dump_active 命令 | 4.58 | daemon round-trip |
| 7. 解析 a11y tree, 找 login 节点 | < 1 | client-side selector |
| 8. 决定下一步 | < 1 | 内存 |
| **合计 (步骤 4+5+6+7+8)** | **11.58ms** | vs LLM 思考 200-2000ms, agent 部分占比 < 6% |

### 4.2 新路径: 1 LLM 步进

| 阶段 | 新 (ms) | 提升 |
|---|---|---|
| 1. Agent 看到 observation (subscribed stream) | 0 | server-push, agent 不等 |
| 2. LLM 思考 (云端) | 200-2000 | 不可控 |
| 3. Agent 写 plan = [TapSelector("login"), wait("Welcome")] | < 1 | typed |
| 4. 写 plan 命令 (1 RTT) | **< 3** | 单 daemon, action+ground truth 1 RTT |
| 5. 收到 PlanResult, 内含: login matched? welcome present? | 0 | 已经在 reply 里 |
| 6. Agent 决定下一步 | < 1 | 内存 |
| **合计 (步骤 4+5+6)** | **< 5ms** | **2.3× 提升** |

**最关键的差异**:
- **P3 解**: 步骤 5 的"看有没有点中"从 "agent 写 dump 命令" 变成 "plan reply 自带",**省 1 RTT + 4.58ms**
- **P4 解**: 16ms tap sleep 删除, **省 16ms**
- **P7 解**: observation 不再 poll, server push 节省 8.02ms × N (取决于 poll 频率)
- **P2 解**: typed plan 是 1 RTT, 替换 N 个 verb round-trip

### 4.3 极限优化 — 240Hz gamepad 路径

| 阶段 | 当前 | 新 | 提升 |
|---|---|---|---|
| producer → channel | ~50ns (mpsc) | ~5ns (lock-free ring) | 10× |
| coalesce 桶 | 1ms 桶 | 0.5ms 桶 (120Hz 兼容) | 2× |
| UHID write | 1 syscall | 0 (1 write covers 32 frames) | 32× |
| 调度 | 1 thread | 0 thread (直写) | 跳过 |

**240Hz gamepad 端到端**: ~30μs per frame (vs 当前 ~100μs), 9ms buffer 0 drop。

---

## 5. 实施路径 — 8 phase, 每 phase 独立可 ship

> 2026-06-30 update: v1 6 phase → v2 8 phase, 加入 Memory 层 (Phase 3),
> 端侧视觉服务 (Phase 4-5), Hybrid AI 验证 (Phase 6), 端侧 LLM 探索 (Phase 8)。
> 每 phase 独立可 ship, LLM agent 从 Phase 1 就能跑。

### Phase 1: 内核骨架 (1-2 session)

1. 重命名 `android-hid-daemon` 为 `ai-device-kernel` (adk), 统一 1 个二进制
2. 端口 9008, 长度前缀 binary (替换 handshake + JSON 文本)
3. 4 个核心 verb (action/plan/observe/query) 实现 + 测试
4. Capability surface 70+ 全部 internal verb, typed Action 暴露
5. **保留 UHID/MotionEvent escape hatch** (从 v2 新增 — 处理游戏/canvas/WebView)

**交付**: 1 个 Rust 二进制, 替换 scrcpy-server + hs.jar。host 端 SDK 可以用。

### Phase 2: State model + observation stream (1-2 session)

1. StateModel: `last_input`, `last_observation`, `predicate_set`, `event_queue`
2. 单一观察源 (替换 `~/.handsets/state-*.json` 文件)
3. Observation stream: 1 server-push, 多 subscriber
4. Predicate engine: 事件触发, 0 polling
5. **Functionality-aware UI representation** (借鉴 AutoDroid HTML-tagged, 500B vs 50KB)

**交付**: agent 可以 `observe(since_seq=N)` 拿到自 N 之后所有事件; UI 简化表示。

### Phase 3: Plan execution + verify + **Memory** (2 session)

1. Plan executor: 多 action, 1 RTT
2. `verify_after`: daemon 自检"我点了 login 吗?"
3. Checkpoint: 每 N 步返回 ground truth 快照
4. `abort_on_error`: 失败立即停
5. **Memory 层** (v2 新增): screen fingerprint → 成功 action 序列, online 累积
6. 跨 session 复用 (替代 AutoDroid 离线 UTG pre-exploration)

**交付**: 1 RTT = 1 plan = 1 reply with all step results; memory 跨 session 复用。

### Phase 4: Action typed surface + **端侧视觉锚定** (2 session)

1. 12+ 个 typed Action (替换 70+ verb 在 agent 视角) — 新增 `LocalizeText`, `DetectElement`
2. 内部仍然走 verb, 但 agent 看到的是 typed enum
3. post-card 序列化 (跨语言绑定)
4. Python SDK (`pip install ai-device-kernel`)
5. **集成 LiteRT (Play services + GPU delegate)** (从 v2 新增)
6. **ML Kit v2 OCR + YOLOv8n-int8 UI detection** (Stage 1 视觉锚定)

**交付**: LLM agent 用 typed Action 写 plan, 端侧视觉锚定 < 100ms。

### Phase 5: 性能极限优化 + **Florence-2 grounding** (2 session)

1. mpsc → crossbeam ArrayQueue (gamepad 路径)
2. coalesce 1ms → 0.5ms (120Hz gamepad)
3. UHID direct write 跳过 coalesce
4. H.265 取代 H.264 (替换 handsets H264Streamer.java)
5. **Florence-2-base grounding** (Stage 2 场景理解)

**交付**: 240Hz gamepad 0 drop, 端到端 5ms typical; grounding 200ms 端侧。

### Phase 6: 端云 Hybrid AI 验证 (1 session)

1. 跑 5 个典型 LLM 任务 (登录、搜索、添加联系人、设置、订餐)
2. 端侧 OCR/UI detection/grounding 处理 80% 视觉锚定
3. 云端 GPT-4V 处理 20% complex reasoning + 异常诊断
4. 测量: 端云 RTT 比例 / 任务成功率 / 平均延迟
5. **真机 benchmark vs handsets / uiautomator2 / Appium**

**交付**: 端云混合架构验证报告; 实测 latency 数字。

### Phase 7: 公开 LLM agent benchmark (1-2 session)

1. GPT-4 / Claude / Gemini 跑 20-step task
2. measure: 任务成功率, plan 部分延迟, action 部分延迟, 视觉锚定命中率
3. 跟 v1 (无 ground truth) / v2 (2 RTT) 对比
4. 完整 docs/ (architecture / protocol / capability / SDK / benchmark)

**交付**: 公开 benchmark 报告。

### Phase 8: 端侧 VLM 探索 (2-3 session) — 长期

1. 验证 GUI-Owl-1.5 sub-7B 端侧可行性 (research openQuestion #1)
2. INT4/INT8 量化 + benchmark 真实延迟
3. 如果可行: 集成 `Action::AskVisual { question, image }`
4. Hybrid 决策: 端侧能答就端侧, 不能答上云

**交付**: 端侧 VLM 可行性报告; 视情况集成。

---

## 6. 关键决策 (写下来, 避免反复)

### 6.1 协议

| 决策 | 为什么 |
|---|---|
| **binary (postcard), 不是 JSON** | 解析 10× 快, 跨语言绑定 0 cost, LLM token 0 浪费 |
| **varint 长度前缀, 不是 u32 BE** | 小帧省 2-3 字节 |
| **typed Action, 不是 verb** | AI-friendly, plan validation 编译期, 文档自动 |
| **4 个核心 verb, 不是 70+** | 极简 API, 70+ 内部化 |
| **ground truth 必返回** | 1 RTT = input + observe |
| **idempotent + retry** | 网络断开不丢, 复现可 |

### 6.2 守护进程

| 决策 | 为什么 |
|---|---|
| **1 个 Rust 二进制, 替换 scrcpy + hs.jar** | 启动 50ms vs 500ms, 状态一致, 维护 1x |
| **state model 内存, 不是文件** | 0 文件 IO, 0 race, 0 stale |
| **event queue + predicate engine** | 0 polling, 0 CPU 浪费 |
| **H.265 取代 H.264** | 40-50% 比特率 |
| **UHID + MotionEvent 双路径, 不用 16ms sleep** | Android 14+ 不需要 |
| **保留 UHID escape hatch 在 12 个 typed Action 之上** (v2 新增) | 处理游戏/canvas/WebView 场景 (G1 修复) |
| **5 件套第 5 件: Memory layer** (v2 新增) | 跨 session 复用, 替代 AutoDroid UTG, online 累积 |
| **Functionality-aware UI repr** (v2 新增, 借鉴 AutoDroid) | LLM token 100× 节省 (500B vs 50KB) |
| **端侧视觉走 LiteRT (不是 NNAPI)** (v2 新增) | NNAPI Android 15 deprecated, 官方迁移到 LiteRT |
| **GPU delegate 主线程初始化** (v2 新增) | TFLite GPU delegate 硬约束, 影响 daemon thread-pool 设计 |
| **端云混合, 不是端云二选一** (v2 新增) | 端侧视觉锚定 90%, 云端 reasoning 10%, 隐私 + 延迟 + 能力 三角平衡 |

### 6.3 SDK

| 决策 | 为什么 |
|---|---|
| **typed Action, not verb-string** | LLM-friendly, 编译期校验, 文档化 |
| **postcard binding (Rust/Python/Go)** | 0 cost 跨语言 |
| **Observation stream (not poll)** | server-push, 多 subscriber |
| **Plan executor (1 RTT = N steps)** | 节省 N-1 RTT, 原子性 |

### 6.4 跳过的 (能省就省)

| 跳过 | 原因 |
|---|---|
| scrcpy 字节兼容 | 我们是 "AI device kernel", 不是 scrcpy 替代品; 字节兼容是包袱 |
| 16ms tap sleep | Android 14+ 不需要, 实测 |
| `~/.handsets/state-*.json` | 文件 IO + race, 用内存 + stream |
| 70+ verb 表面 | agent 写 plan 时映射负担, 内部化 |
| mpsc dispatcher (gamepad) | 1 producer 时不必要, 直写 |

### 6.5 不动的 (受保护)

| 不动 | 原因 |
|---|---|
| 字节级 HID 报告 (descriptor/boot protocol) | 设备端兼容性 |
| `android-hid-protocol` 既有 60+ verb | Phase 4 之前保留, 之后内部化 |
| `AgentControlSession` 既有 typed facade | Phase 1 之前保留, 之后重新设计 |
| LLM agent 业务逻辑 | 不是本项目范围 |

---

## 7. 与现有 material 的关系

| 现有 | 新位置 |
|---|---|
| `android-hid-connect` 字节级 HID 核心 | 不动; `android-hid-protocol` 60+ verb 内部化 |
| `android-hid-agent` typed facade | 重构; Action/Plan/Observation 是新的 1st class |
| `android-hid-daemon` (28 模块) | 重命名 `ai-device-kernel`; 1 二进制, 1 端口, 1 协议 |
| `android-hid-cli` 静态二进制 | 保留; CLI 只是 typed Action 的 thin wrapper |
| `android-hid-py` Python SDK | 重生成; 暴露 typed Action/Plan/Observation |
| `handsets/` 的实现思路 | 直接借鉴 a11y / selector / state mirror / wait registry |
| `comparison-with-handsets.md` | 仍然准确; 新架构追平 P3/P6/P7/P9, 超越 P2 (single daemon) + P4 (no 16ms sleep) + typed Plan (P8) |
| `roadmap-exceed-handsets.md` | 7 phase 路线图废止, 改 6 phase |
| `agent-v2-design.md` (上一个) | 大部分思路保留; 强调 "1 个 daemon, 4 个 verb, typed Action" |

---

## 8. 验收点 (AC-V3-N)

每 Phase 结束要满足:

### Phase 1 验收

- **AC-V3-1.1** `adk` 二进制 < 5MB (vs scrcpy-server ~10MB + hs.jar ~3MB)
- **AC-V3-1.2** 启动 < 50ms (vs scrcpy 200-500ms)
- **AC-V3-1.3** 端口 9008, 长度前缀 binary, postcard 序列化
- **AC-V3-1.4** 4 个核心 verb 全部实现, 每个有 round-trip 测试
- **AC-V3-1.5** 70+ capability 全部 internal verb, typed Action 暴露
- **AC-V3-1.6** `cargo test -p adk` 100% 通过
- **AC-V3-1.7** `cargo clippy --workspace` 0 warning

### Phase 2 验收

- **AC-V3-2.1** `observe(since_seq=N)` 拿自 N 以来所有事件, 不重复不漏
- **AC-V3-2.2** StateModel 单进程, 无文件 IO
- **AC-V3-2.3** Predicate engine 事件驱动, 0 polling (grep 验证)
- **AC-V3-2.4** 多 subscriber 同时订阅, 互不干扰

### Phase 3 验收

- **AC-V3-3.1** 1 plan = 1 RTT = 1 reply, 含所有 step 的 ground truth
- **AC-V3-3.2** `verify_after` 失败立即 abort, 不继续后续 step
- **AC-V3-3.3** Checkpoint 每 N 步返回快照, 内存 < 1MB
- **AC-V3-3.4** 端到端 p50 < 10ms (5 step plan)
- **AC-V3-3.5** (v2) Memory 跨 session 复用, screen fingerprint 命中 > 60% (同 app 同 task 重跑)
- **AC-V3-3.6** (v2) Memory 落盘 SQLite, 重启 daemon 不丢失

### Phase 4 验收

- **AC-V3-4.1** 14 个 typed Action (12 + LocalizeText + DetectElement) 编译期校验
- **AC-V3-4.2** Python SDK: `pip install ai-device-kernel`, 1 行 tap
- **AC-V3-4.3** LLM agent benchmark: GPT-4 完成 20-step task < 5s (plan 部分 < 50ms)
- **AC-V3-4.4** 跨语言绑定: Rust + Python + Go (最少前两个)
- **AC-V3-4.5** (v2) LiteRT 集成: Play services path 工作, standalone path 备选
- **AC-V3-4.6** (v2) ML Kit v2 OCR 端到端 < 50ms (1080p frame)
- **AC-V3-4.7** (v2) YOLOv8n-int8 UI detection < 30ms (1080p frame)
- **AC-V3-4.8** (v2) Functionality-aware UI repr < 500B per screen (vs 50KB 完整 a11y)

### Phase 5 验收

- **AC-V3-5.1** 240Hz gamepad 30s, drop count = 0
- **AC-V3-5.2** H.265 同画质 vs H.264 比特率 < 60%
- **AC-V3-5.3** tap 端到端 p50 < 3ms (vs handsets 16-32ms)
- **AC-V3-5.4** LLM 循环步进 p50 < 10ms (vs handsets 50-100ms)
- **AC-V3-5.5** (v2) Florence-2-base grounding < 200ms 端侧 (GPU delegate)
- **AC-V3-5.6** (v2) GPU delegate 在 daemon 主线程初始化, 无跨线程错误

### Phase 6 验收

- **AC-V3-6.1** 真机 (SM-G9910 Android 11) E2E 30/30 pass
- **AC-V3-6.2** 端云混合: 80% 视觉锚定走端侧, 20% reasoning 走云端
- **AC-V3-6.3** 实测: 5 任务 (登录/搜索/添加联系人/设置/订餐) 端到端延迟报告
- **AC-V3-6.4** vs handsets / uiautomator2 / Appium 6 维度对比 latency 报告

### Phase 7 验收

- **AC-V3-7.1** 公开 benchmark 报告 (GPT-4 / Claude / Gemini 跑 20-step task)
- **AC-V3-7.2** 任务成功率 > 85% (vs AutoDroid 71.3%, AppAgent 73%)
- **AC-V3-7.3** 完整 docs/ (architecture / protocol / capability / SDK / benchmark)
- **AC-V3-7.4** 至少 1 个外部 LLM 跑通 Android 控制 demo

### Phase 8 验收 (v2 新增, 长期)

- **AC-V3-8.1** GUI-Owl-1.5 sub-7B 端侧 INT4 量化可行性报告
- **AC-V3-8.2** (条件) `Action::AskVisual` 集成, 端侧推理 < 1s
- **AC-V3-8.3** 端云决策策略 benchmark: 端侧能答比例 / 云端调用 RTT 节省

---

## 9. 一句话总结

**当前 android-hid 是三个半成品的拼凑** (scrcpy UHID + hs.jar + 自家 daemon),
让 LLM agent 写 plan 时要在脑子里做 verb 映射 + 2 个 socket + 16ms hack sleep + 2 RTT
才能拿到 ground truth。

**新架构 = AI 设备内核 (v2)**: 1 个 Rust 守护进程, 1 个二进制协议, 4 个核心 verb,
5 件套 (typed Action + ground truth + Plan + Observation + **Memory**), 端侧视觉
服务 (ML Kit OCR + YOLOv8n + Florence-2 grounding), 端云 Hybrid AI, 端到端
< 5ms (本地 action) / < 50ms (端侧视觉) / < 500ms (云端 reasoning),
替换 scrcpy + hs.jar, 启动 50ms, AI-friendly 到 LLM 写 plan 是 1 行 typed 代码。

按 8 phase 落地, 每 phase 独立可 ship, 6-9 个 session 完成。Phase 1 之后, LLM agent
已经能跑 (1 个 daemon + 4 verb + typed Action)。Phase 3 之后, LLM 循环步进 < 10ms
+ Memory 跨 session 复用。Phase 4 之后, 端侧视觉锚定 < 100ms。Phase 5 之后,
240Hz gamepad 0 drop + grounding 端侧。Phase 6 之后, Hybrid AI 验证 + 任务成功率
> 85%。Phase 8 之后, 端侧 VLM 可行性报告。

**这是 AI 操控 Android 的最优路径** —— 不是 scrcpy 字节兼容, 不是 hs.jar 复制, 是
**为 AI 设计**的设备内核, 端云协同, 端侧算力利用, 跨 session 学习。

---

## 10. Deep Research 集成总结 (2026-06-30)

> 本节是 6 角度 / 24 sources / 18 confirmed + 7 killed claims 的完整调研总结,
> 全部集成到 §1-§9 各处。本节是索引, 方便后续 session 引用。

### 10.1 现有方案对比矩阵 (v3 视角)

| 方案 | LLM 友好 | typed Action | ground truth | 1-RTT Plan | Observation stream | 端侧 AI | Memory |
|---|---|---|---|---|---|---|---|
| **uiautomator2** | ❌ 字符串 | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Appium** | ❌ WebDriver | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **scrcpy** | ❌ 像素 | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **AutoDroid** | ⚠️ HTML | ❌ | ❌ | ❌ | ❌ | ❌ | ✅ UTG |
| **AppAgent** | ⚠️ simplified | ❌ 2 动作 | ❌ | ❌ | ❌ | ❌ | ✅ KB |
| **Mobile-Agent-v3** | ⚠️ VLM | ❌ | ❌ | ⚠️ planning | ❌ | ⚠️ GUI-Owl 7B | ⚠️ memory |
| **v3 (本设计)** | ✅ typed | ✅ 14 | ✅ GroundTruth | ✅ Plan | ✅ stream | ✅ LiteRT | ✅ online |

**v3 在所有 7 个维度全部领先** (在 AI 操控 Android 这个用例上)。

### 10.2 端侧算力: NNAPI → LiteRT 迁移

**事实** (3/3 verified): NNAPI 在 Android 15 deprecated, 官方迁移到 LiteRT +
GPU delegate, 通过 Play services (推荐) 或 standalone Maven。

**v3 决策**:
- ❌ 不用 NNAPI
- ✅ 用 LiteRT (Play services 优先, standalone 备选)
- ✅ GPU delegate 在 daemon 主线程初始化 (硬约束)
- ✅ Standalone path 给中国 OEM (没有 Play services)

### 10.3 模型选型 (4 阶段)

| Stage | 模型 | 用途 | 大小 | 延迟 | Phase |
|---|---|---|---|---|---|
| 1 | ML Kit v2 OCR | 找文字坐标 | ~10MB | < 50ms | 4 |
| 1 | YOLOv8n-int8 | UI element detection | ~3MB | < 30ms | 4 |
| 2 | Florence-2-base | text grounding | ~150MB | ~200ms | 5 |
| 3 | GUI-Owl-1.5 (待验证) | 多模态问答 | 2B/4B/8B | sub-1s | 8 |
| 4 | GUI-Owl-7B-INT4 (待验证) | 端侧 LLM | ~5GB | 1-2s | 8 |

### 10.4 v3 已修复的 gap (从 open questions)

| Gap | 修复 | 章节 |
|---|---|---|
| G1: 游戏/canvas/WebView | UHID/MotionEvent escape hatch | §3.5, Phase 1 |
| G2: 无 memory 层 | Memory 5 件套 | §3.2.0, Phase 3 |
| G3: 端侧 LLM 未知 | LiteRT 集成 + 分阶段模型 | §3.6, Phase 4-5-8 |
| G4: 无 per-app 沙箱 | `SessionId` 隔离 (Phase 1+2 隐式) | §3.3 |
| G5: 启动延迟 | 单 Rust binary 目标 < 50ms | §4, Phase 6 |

### 10.5 仍 open 的问题 (待未来 session 验证)

| # | 问题 | 谁来验证 |
|---|---|---|
| 1 | 量化 GUI-Owl-7B 在 Snapdragon 8 Gen 3/4 的真实延迟 (GPU vs NPU) | Phase 8 benchmark |
| 2 | v3 Plan/Observation stream 能否达到 AutoDroid 71.3% 任务成功率 | Phase 6-7 benchmark |
| 3 | ML Kit v2 vs PaddleOCR-mobile vs Tesseract-NDK 生产级 sub-100ms OCR 选型 | Phase 4 实测 |
| 4 | sub-100ms UI element detection (YOLOv8n-int8 vs grounding DINO-tiny) | Phase 4 实测 |

### 10.6 借鉴 / 超越表

| 现有方案 | 借鉴什么 | v3 超越什么 |
|---|---|---|
| **AutoDroid** | functionality-aware UI repr, offline UTG 思路 | online memory 累积, 不需要预跑 |
| **AppAgent** | 双学习模式 (auto + human demo) → 知识库 | typed enum 跨 session 复用, 不需要 KB |
| **Mobile-Agent-v3** | GUI-Owl 多模态, planning + reflection | typed ground truth 1 RTT, 不需要 VLM 每次跑 |
| **handsets** | a11y dump, CSS-like selector, event-driven wait | 替换 daemon (1 个 vs 2 个), ground truth, Plan |
| **uiautomator2** | UiAutomator 注入, 简单 verb | typed enum, Plan atomic, Memory |

---

## 11. References (调研来源)

### 11.1 现有 Android 操控方案 (primary sources, 3-0 验证)

- [uiautomator2](https://github.com/openatx/uiautomator2) — Python client, 字符串 verb
- [Appium docs](https://appium.io/docs/en/latest/intro/) — 4 层 WebDriver
- [scrcpy](https://github.com/Genymobile/scrcpy) — 像素镜像
- [scrcpy develop](https://github.com/Genymobile/scrcpy/blob/master/doc/develop.md) — 内部架构
- [scrcpy control](https://github.com/Genymobile/scrcpy/blob/master/doc/control.md) — 控制协议
- [scrcpy shortcuts](https://github.com/Genymobile/scrcpy/blob/master/doc/shortcuts.md) — 快捷键

### 11.2 LLM-driven Mobile Agent

- [AutoDroid (arXiv 2308.15272)](https://arxiv.org/abs/2308.15272) — LLM + UTG, +36% task success
- [AutoDroid GitHub](https://github.com/MobileLLM/AutoDroid) — 源码
- [AppAgent (arXiv 2312.13771)](https://arxiv.org/abs/2312.13771) — 双学习模式
- [Mobile-Agent-v3 GitHub](https://github.com/X-PLUG/MobileAgent) — GUI-Owl 7B/32B

### 11.3 端侧算力 (NNAPI / LiteRT)

- [NNAPI Guide](https://developer.android.com/ndk/guides/neuralnetworks) — deprecated
- [NNAPI migration guide](https://developer.android.com/ndk/guides/neuralnetworks/migration-guide) — Android 15 deprecation
- [LiteRT GPU delegate](https://ai.google.dev/edge/litert/android/delegates/gpu) — Play services + standalone, same-thread
- [LiteRT delegates performance](https://developers.google.com/edge/litert/performance/delegates) — 5× speedup, CPU 84.4ms → delegate 7.3ms

### 11.4 端侧视觉/语义模型

- [YOLOv8 (Ultralytics)](https://docs.ultralytics.com/models/yolov8/) — UI detection 候选
- [Florence-2 (HuggingFace)](https://huggingface.co/docs/transformers/main/en/model_doc/florence2) — 多任务 grounding
- [Florence-2 large](https://huggingface.co/microsoft/Florence-2-large) — 大版本
- [Grounding DINO tiny ONNX](https://huggingface.co/onnx-community/grounding-dino-tiny-ONNX) — text-grounded
- [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) — 端侧 OCR 候选

### 11.5 Hybrid AI (Google)

- [Gemini Nano (端侧 LLM)](https://developer.android.com/ai/gemini-nano) — Google 端侧 LLM
- [Hybrid AI](https://developer.android.com/ai/hybrid) — 端云协同模式
- [Computer Control](https://developer.android.com/ai/computer-control) — 端云分工

### 11.6 相关 v3 前置文档 (本项目内)

- [agent-v2-design.md](agent-v2-design.md) — v2 设计, 5 模块, 110+ tests
- [roadmap-exceed-handsets.md](roadmap-exceed-handsets.md) — 7 phase 追平 handsets (已废止)
- [comparison-with-handsets.md](comparison-with-handsets.md) — 现有 handsets vs android-hid 对比
- [architecture.md](architecture.md) — 当前 byte-exact 核心架构

---

最后更新: 2026-06-30 (v2 — research integration)

