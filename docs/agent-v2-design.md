# `android-hid-agent` v2 — 设计文档

> Date: 2026-06-30
> Scope: 改进 `android-hid-agent` (workspace crate),吸收 `handsets/` 全部优点,
> 按场景选择定位和连接机制, UHID + a11y 深度结合, 升级到 H.265, AI 友好,
> 以**最低延迟**和**最优性能**为绝对目标。
> 不重写既有 `android-hid-connect` 字节级核心 (受 AGENTS.md §6 保护)。

---

## 0. 目标与基线

### 0.1 既有成果 (Phase 1+2 已完成)

| 模块 | 状态 | 来源 |
|---|---|---|
| `android-hid-protocol` 60+ verb + length-prefix 帧 + 错误码 | ✅ 完成 | `android-hid-protocol/src/{verb,frame,error,kvs,version}.rs` |
| `android-hid-agent::DaemonBackend` (TCP + handshake + 流式迭代器) | ✅ 完成 | `android-hid-agent/src/backend/daemon.rs` 595 行 + 12 个测试 |
| `android-hid-agent::UnifiedBackend` (按 verb 选 backend) | ✅ 完成 | `android-hid-agent/src/backend/unified.rs` 304 行 + 14 个测试 |
| `android-hid-connect` 字节级 HID 核心 (scrcpy 协议对齐) | ✅ 完成 | `src/{client,coalesce,session,control,hid,...}.rs` 405 测试 |
| `AgentControlSession` typed facade | ✅ 完成 | `src/agent/{session,action,types,geometry,estimator}.rs` 20,408 行 |
| `android-hid-daemon` 设备端 Rust 守护 (28 模块) | ✅ 完成 | `android-hid-daemon/src/*.rs` |

### 0.2 v2 要补的洞

| 缺什么 | handsets 已实现 | 影响 |
|---|---|---|
| **a11y 树 dump** | ✅ `Dumper.java` + `Traverse.java` + JSON 输出 | LLM agent 看不到屏幕 |
| **CSS-like 选择器** | ✅ `EditText[hint~=Email] :has-text("x") :near(SEL, PX)` | 只能 hard-code 坐标 |
| **事件驱动等待** | ✅ `wait_for_idle` / `wait_for_text` / `wait_for_activity` | 只能 sleep 轮询 |
| **截图** | ✅ VirtualDisplay + JPEG/H.264/TileJPEG | 看不到屏幕 |
| **H.265 视频流** | ❌ 只有 H.264 (要升级) | 30-50% 比特率浪费 |
| **跨 backend 原子操作** | ❌ (handsets 是单一 daemon) | tap + dump 要 2 round-trip |
| **AI 帧 ↔ a11y 锚定** | ❌ | vision → semantics 桥接缺失 |
| **多设备 fanout** | ⚠️ CLI 级 (`fan.rs`) | 不是 typed |
| **场景感知连接模式** | ❌ | 240Hz gamepad 与 1Hz tap 共用一条路径浪费 |

---

## 1. 总体架构 (v2)

```
                    ┌──────────────────────────────────────────┐
   LLM agent  ──►  │           android-hid-agent              │
                    │  ┌─────────────┐   ┌──────────────────┐  │
                    │  │  Scenario   │──►│  ConnectionMode  │  │
                    │  │  resolver   │   │  resolver        │  │
                    │  └─────────────┘   └──────────────────┘  │
                    │           │                  │            │
                    │           ▼                  ▼            │
                    │  ┌────────────────────────────────────┐  │
                    │  │     Atomic operations layer        │  │
                    │  │  (tap_and_dump, select_and_tap,    │  │
                    │  │   ai_anchor_tap, vision_guided)    │  │
                    │  └────────────────────────────────────┘  │
                    │           │                  │            │
                    │   ┌───────┴───────┐  ┌───────┴───────┐    │
                    │   │ scrcpy UHID   │  │ daemon (TCP)  │    │
                    │   │   backend     │  │   backend     │    │
                    │   │ (coalesce +   │  │ (H.265 stream │    │
                    │   │  1ms window)  │  │  + a11y dump) │    │
                    │   └───────┬───────┘  └───────┬───────┘    │
                    └───────────┼──────────────────┼────────────┘
                                │                  │
                                ▼                  ▼
                       scrcpy-server     android-hid-daemon
                       (UHID/Motion)     (a11y / H.265 / pm)
                                │                  │
                                └──── adb forward ─┘
                                            │
                                       Android 设备
```

**关键边界**:
- `android-hid-agent` = 纯 host-side typed Rust
- 不动 `android-hid-connect` 字节级核心
- `android-hid-protocol` 是共享契约层
- `android-hid-daemon` 是设备端实现

---

## 2. 场景感知连接 (Scenario-Based Connection)

### 2.1 为什么按场景而不是按 verb 选 backend?

`UnifiedBackend::choose(verb)` 当前按 verb 类型选 (Tap→Either, Dump→Daemon) — 但一个 LLM agent 循环里**多个 verb 共享一个会话**,最佳连接方式由**场景**决定:

| 场景 | 特征 | 连接选择 | 关键优化 |
|---|---|---|---|
| `Gaming240Hz` | 60-240Hz gamepad,无 a11y 需求 | 直连 scrcpy,`gamepad_only_realtime()` 模式 | 跳过 mpsc dispatcher, 直写 socket |
| `BulkText` | LLM 大段打字 (1k+ chars) | scrcpy UHID + coalescing | 1ms 桶 + 32-frame batcher |
| `UiAutomation` | 通用 tap/swipe/key | daemon (`tap` verb) | warm socket + 1.34ms p50 |
| `VisionLoop` | see + act 循环 (LLM agent) | scrcpy + daemon 双 socket | `tap_and_dump` 原子操作 |
| `MultiDevice` | 扇出到 N 设备 | N 个 `UnifiedBackend` | typed `FanoutSession` |
| `Background` | dumpsys/logcat/monitor 流 | daemon 单 socket | 流式迭代器 |
| `AdbOnly` | 没有 daemon 也没 scrcpy | fall back to `adb` subprocess | 最后手段 |

### 2.2 `Scenario` 枚举

```rust
// android-hid-agent/src/scenario.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// 60-240Hz gamepad 控制 + 10 点 multitouch。绕开 mpsc 直接写 socket。
    Gaming240Hz,
    /// LLM 大量文本注入 (scrcpy TypeText + 1ms 桶)。
    BulkText,
    /// 通用 UI 自动化 (tap/swipe/key/scroll/clipboard) 走 daemon verb。
    UiAutomation,
    /// see + act 闭环 — 帧流 + a11y 树 + 原子 tap_and_dump。
    VisionLoop,
    /// 多设备扇出 (typed N-of-N session)。
    MultiDevice,
    /// 后台诊断流 (dumpsys/logcat/monitor)。
    Background,
    /// 仅 adb (无 daemon 也没 scrcpy-server)。
    AdbOnly,
}
```

### 2.3 `ConnectionMode` 决策

```rust
// android-hid-agent/src/scenario.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionMode {
    /// 直连 scrcpy socket, 0 mpsc hop, gamepad_only_realtime 模式。
    ScrcpyDirect { realtime: bool },
    /// 通过 daemon TCP socket (全 verb 表面)。
    Daemon { addr: SocketAddr },
    /// 同时持有 scrcpy + daemon, 用于原子操作。
    DualSocket { scrcpy: SocketAddr, daemon: SocketAddr },
    /// 多设备扇出 — N 个 UnifiedBackend。
    Fanout { addrs: Vec<SocketAddr> },
    /// 退化到 adb subprocess。
    AdbShell,
}

impl ConnectionMode {
    /// 根据 scenario 推导连接模式 (不实际建连接, 只定类型)。
    pub fn for_scenario(s: Scenario, cfg: &ConnectionHints) -> Self {
        match s {
            Scenario::Gaming240Hz => Self::ScrcpyDirect { realtime: true },
            Scenario::BulkText => Self::ScrcpyDirect { realtime: false },
            Scenario::UiAutomation => Self::Daemon { addr: cfg.daemon_addr },
            Scenario::VisionLoop => Self::DualSocket {
                scrcpy: cfg.scrcpy_addr,
                daemon: cfg.daemon_addr,
            },
            Scenario::MultiDevice => Self::Fanout { addrs: cfg.fanout_addrs.clone() },
            Scenario::Background => Self::Daemon { addr: cfg.daemon_addr },
            Scenario::AdbOnly => Self::AdbShell,
        }
    }
}
```

### 2.4 `AgentControlSession` 改造

当前 `AgentControlSession<T, R>` 用 `HidClient` + `HidDispatcher` + `DeviceMessageReceiver`。v2 加一个 `Scenario::open()` 入口:

```rust
impl AgentControlSession<...> {
    /// 新增: 按 scenario 打开会话, 自动选择 backend。
    pub fn open(scenario: Scenario, hints: &ConnectionHints) -> Result<Self> { ... }
}
```

**能省略的环节**:
- `Gaming240Hz` → 跳过 mpsc dispatcher (单线程 producer,直写 socket) — 节省 50ns/channel + 1 thread context switch
- `BulkText` → 跳过 `HidClient` typed enum 包装, 直接调 `HidSession::type_text()` — 节省 enum tag 字节
- `UiAutomation` → 跳过 scrcpy 启动 (用 daemon verb 走 UiAutomation) — 节省 scrcpy-server 启动 200-500ms
- `VisionLoop` → 复用 1 个 socket 完成 tap + dump, 避免 2 round-trip

---

## 3. UHID + a11y 深度结合 (原子操作)

### 3.1 设计原则

当前 `HidClient::tap()` 只发 1 条 INJECT_TOUCH_EVENT 消息,LLM agent 想要"tap 后看结果"必须自己再发 1 条 `dump_active` — **2 round-trip = 10-15ms 浪费**。原子操作把这两个合到 1 个调用,daemon 端在一次 server-side 调度里完成:

```
LLM agent                   agent (host)              daemon (device)
   │                            │                          │
   ├─ select_and_tap("Login") ─►│                          │
   │                            ├─ tap + dump (atomic) ──►│
   │                            │                          ├─ injectInputEvent(tap)
   │                            │                          ├─ sleep 50ms
   │                            │                          ├─ UiAutomation.getRoot
   │                            │                          ├─ Traverse → JSON
   │                            │◄─ {matched, dump_json} ──┤
   │◄─ Result ──────────────────┤                          │
```

**端到端**: 50ms (vs 5ms tap + 4.58ms dump + 16ms sleep = 25ms 串行,但 50ms 是 server-side 一个事务,避免 2 个 RTT 的网络往返)

### 3.2 原子操作清单

| 操作 | 含义 | 用途 |
|---|---|---|
| `select_and_tap` | selector → 解析 a11y → 取 center → tap | LLM "tap Login" |
| `ai_anchor_tap` | AI detection box → 投影到 a11y node → tap | vision ↔ semantics |
| `vision_guided_tap` | H.265 帧 + a11y → 锚定 → tap | 闭环 see-act |
| `tap_and_dump` | tap → idle 200ms → dump_active | 通用 atomic |
| `tap_and_screenshot` | tap → idle → H.265 keyframe | vision loop |
| `type_and_wait` | text → wait_for_text "Welcome" | 表单提交 |
| `node_action_and_dump` | node_click + dump_active | 选择器-动作闭环 |
| `click_until_text` | 重试 click 直到 text 出现 | 列表项点击 |

### 3.3 协议层 (新动词)

`android-hid-protocol/src/verb.rs` 加:

```rust
pub enum Verb {
    // ... 既有 60+ 动词

    // ---- v2 atomic (UHID + a11y) ----
    /// 单调用: 解析 a11y selector, 取节点 center, tap, 返回 (matched, dump_json)
    SelectAndTap,
    /// 单调用: AI detection box 投影到 a11y 节点, tap, 返回 (anchor_node, dump_json)
    AiAnchorTap,
    /// 单调用: 触发 H.265 keyframe, 阻塞到 keyframe 送达
    KeyframeAndWait,
    /// 单调用: tap + wait_for_text 闭环
    TypeAndWait,
    /// 单调用: 重试 node_click 直到 a11y tree 含目标文本
    ClickUntilText,
    /// 单调用: dump_active 同时返回当前 H.265 帧 IDR 帧号
    DumpAndFrame,
}
```

**wire 格式** (与现有 `tap_and_dump` 一致):

```
select_and_tap sel="EditText[id=login]" timeout_ms=2000 idle_ms=200
→ {matched: true, x: 540, y: 1200, dump: "{...json...}"}
```

### 3.4 host-side 类型层 (`android-hid-agent/src/atomic.rs`)

```rust
pub struct AtomicResult<T> {
    pub matched: bool,
    pub anchor: Option<A11yNode>,
    pub observation: Option<Observation>,
    pub timings: AtomicTimings,
}

pub struct AtomicTimings {
    pub selector_resolve_ms: u32,
    pub inject_ms: u32,
    pub settle_ms: u32,
    pub dump_ms: u32,
    pub total_ms: u32,
}

pub enum Observation {
    A11y(A11yTree),
    Frame(FrameSummary),
    Both { a11y: A11yTree, frame: FrameSummary },
}

impl AgentControlSession {
    /// UHID + a11y 原子操作
    pub fn select_and_tap(
        &mut self,
        sel: &Selector,
        timeout: Duration,
    ) -> Result<AtomicResult<()>>;

    pub fn ai_anchor_tap(
        &mut self,
        box_id: u32,           // LatestFrameSummary index
        timeout: Duration,
    ) -> Result<AtomicResult<()>>;

    pub fn tap_and_dump(
        &mut self,
        x: i32, y: i32,
        idle_ms: u32,
    ) -> Result<AtomicResult<()>>;
}
```

### 3.5 `Selector` 解析器 (port from `handsets/handsets-cli/src/selector.rs`)

`handsets` 的 selector grammar 已经稳定 (CSS-like + a11y 关系算子),**直接移植到 `android-hid-agent/src/selectors.rs`**,作为 client-side cache (LLM agent 不会每步重新解析):

```rust
// android-hid-agent/src/selectors.rs
pub struct Selector { /* AST */ }

impl Selector {
    pub fn parse(src: &str) -> Result<Self, ParseError> {
        // 支持: Tag[attr=val][attr~=sub] :flag :near(SEL, PX) :below() :right-of()
        //      :in() :text-is("x") :focused :checked :has-text("x") :visible
        //      :clickable :enabled comma=OR
    }
    pub fn matches(&self, node: &A11yNode) -> bool;
    pub fn find_all<'a>(&self, dump: &'a A11yTree) -> Vec<&'a A11yNode>;
    pub fn find_one<'a>(&self, dump: &'a A11yTree) -> Option<&'a A11yNode>;
}
```

**关键算法** (直接 port):
- `:near(SEL, PX)` → 欧氏距离 ≤ PX
- `:below()` / `:right-of()` → 空间关系 (比较 center.y / center.x)
- `:has-text("x")` → 子串包含
- `,` → OR

**性能优化**:
- Selector AST 不可变, `Rc<Selector>` 复用
- `find_all` 一次 walk + 多个 predicate match (而不是为每个 predicate walk 一遍)
- `:near` 用 squared distance (避免 sqrt)

### 3.6 AI 帧 ↔ a11y 锚定 (vision_guided_tap)

`android-hid-connect` 已有 `LatestFrameSummaryReceiver` 提供 `ObjectBox { x, y, w, h, class_id, confidence }`。**新加一步**: 把 detection box center 投影到 a11y tree,找到**该位置最上层**的 a11y node:

```rust
pub fn anchor_frame_to_a11y(
    frame_box: &ObjectBox,
    frame_w: u16, frame_h: u16,
    tree: &A11yTree,
) -> Option<A11yNode> {
    let cx = frame_box.x + frame_box.w / 2;
    let cy = frame_box.y + frame_box.h / 2;
    // 一次 walk, 找最上层的 (z-order 最高) 包含 (cx, cy) 的 clickable node
    tree.find_topmost_at(cx, cy)
}
```

**收益**: LLM 看到 "Login 按钮" 不再需要 hard-code 坐标 → 抗分辨率 / 抗重排 / 抗主题。**这是 handsets 做不到的** (handsets 只有 a11y 没有 AI 帧)。

---

## 4. H.265 升级 (取代 H.264)

### 4.1 为什么升级 H.265

| 维度 | H.264 (AVC) | H.265 (HEVC) | 提升 |
|---|---|---|---|
| 同画质比特率 | 100% | **~50-60%** | 节省 40-50% 带宽 |
| 同比特率画质 | 100% | 视觉上 +20-30% PSNR | 同样带宽更高清 |
| LLM 视觉编码 | 768px q=80 JPEG = 22KB | H.265 IDR frame = ~15-20KB | 类似 JPEG 静态 + 视频流更强 |
| 解码硬件支持 | 几乎所有设备 | 几乎所有现代设备 (Android 6+ 有硬件解码) | 兼容性 OK |
| 编码器复杂度 | 中 | 高 (CPU ~3x) | 服务端 CPU 上升,但客户端零拷贝 |

### 4.2 MediaCodec 配置 (替代 `H264Streamer.java`)

`android-hid-daemon/src/stream.rs` 新增 `H265Streamer`:

```rust
const HEVC_PROFILE_MAIN: i32 = 1;        // HEVC Main profile (8-bit, 4:2:0)
const HEVC_LEVEL_4_1: i32 = 2;          // 1080p @ 30fps
const BITRATE_MODE_VBR: i32 = 1;
const KEY_FRAME_INTERVAL_S: i32 = 1;    // GOP = 1s (低延迟)
const MAX_B_FRAMES: i32 = 0;            // 禁用 B 帧 (低延迟)

pub struct H265Streamer {
    encoder: MediaCodec,
    surface: InputSurface,
    width: u32,
    height: u32,
    bitrate: u32,
    sps_pps_vps: Vec<u8>,  // VPS (H.265 独有) + SPS + PPS
}
```

**wire 格式** (`stream_h265` verb):

```
stream_h265 size=768 fps=30 bitrate=2000 gop=1
→ [u32 len][VPS][u32 len][SPS][u32 len][PPS][u32 len][IDR][u32 len][P]...
```

**H.265 特有**: 比 H.264 多一个 **VPS** (Video Parameter Set) 头。`android-hid-protocol/src/stream.rs` 加 helper:

```rust
pub struct HevcParamSets {
    pub vps: Vec<u8>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// 检查 buffer 是否是 HEVC start code + NAL type VPS (32) / SPS (33) / PPS (34)
pub fn is_hevc_param_set(buf: &[u8]) -> Option<HevcNalType>;
```

### 4.3 协议层动词

`android-hid-protocol/src/verb.rs`:

```rust
pub enum Verb {
    // ... 既有

    // ---- v2 streams (H.265 + tile) ----
    StreamH265,        // 取代 StreamH264, 同样的 wire shape 但 codec=hevc
    StreamTileH265,    // 低带宽 tile 模式
    HevcParamSets,     // 一次拉取 VPS/SPS/PPS
}
```

### 4.4 host-side 接收 (`android-hid-agent/src/stream.rs`)

```rust
pub struct H265FrameStream<'a> {
    daemon: &'a mut DaemonBackend,
    width: u32,
    height: u32,
    param_sets: HevcParamSets,
    frames_received: u64,
    bytes_received: u64,
}

impl Iterator for H265FrameStream<'_> {
    type Item = H265Frame;
}

pub struct H265Frame {
    pub nal_type: HevcNalType,    // IDR / P / VPS / SPS / PPS
    pub pts: u64,
    pub dts: u64,
    pub is_keyframe: bool,
    pub bytes: Vec<u8>,           // 不复制 — 借用 daemon 缓冲区
}
```

**零拷贝**: 通过 `Frame::payload()` 借用 daemon 的 read buffer (不 `to_vec`),decoder 端直接吃。

### 4.5 兼容性策略

- `StreamH264` **保留** (旧设备 / 兼容性)
- `StreamH265` 是新默认
- `Stream` (JPEG) 保留
- 自动协商: client 优先 H.265,daemon 不支持时降级 H.264

---

## 5. AI 友好增强

### 5.1 typed `Plan` (port from `src/agent/action.rs` + 扩展)

既有 `AgentAction` 是 typed enum,**v2 升级**为:

```rust
// android-hid-agent/src/plan.rs
pub enum AgentAction {
    // ---- Input (UHID 路径) ----
    Tap { x: i32, y: i32 },
    TapSelector(Selector),
    TapAiAnchor { box_id: u32, confidence_min: u8 },
    Swipe { from: (i32, i32), to: (i32, i32), steps: u32 },
    TypeText(String),
    Key(AndroidKeycode),
    Scroll { x: i32, y: i32, dy: i32 },
    // ---- Gamepad (UHID 路径) ----
    GamepadFrame(GamepadFrame),
    // ---- Atomic (UHID + a11y) ----
    SelectAndTap { selector: Selector, timeout_ms: u32 },
    AiAnchorTap { box_id: u32, timeout_ms: u32 },
    TapAndDump { x: i32, y: i32, idle_ms: u32 },
    TypeAndWait { text: String, wait_for: String, timeout_ms: u32 },
    // ---- Observe (daemon 路径) ----
    DumpActive,
    Screenshot { size: u32, q: u8, fmt: ImageFormat },
    WaitForText { text: String, timeout_ms: u32 },
    WaitForActivity { component: String, timeout_ms: u32 },
    // ---- System (daemon 路径) ----
    LaunchApp(String),
    SetClipboard(String, bool),
    Getprop(String),
    PmGrant(String, String),
    // ... 70+ 系统 verb
}

pub struct AgentPlan {
    pub steps: Vec<AgentStep>,
    pub metadata: PlanMetadata,
}

pub struct AgentStep {
    pub id: StepId,
    pub action: AgentAction,
    /// 前置条件 (a11y predicate / frame predicate)
    pub precondition: Option<Predicate>,
    /// 后置验证
    pub postcondition: Option<Predicate>,
    /// 期望延迟 (ms) — 由 `AgentPlan::estimate_budget` 计算
    pub expected_ms: u32,
}

pub enum Predicate {
    TextPresent(String),
    ActivityAt(String),
    FrameStable(usize),  // 连续 N 帧无 scene change
    A11yIdle(usize),     // 连续 N 帧 a11y 树无变化
    Custom(Rc<dyn Fn(&Observation) -> bool>),
}
```

### 5.2 离线预检 + 时序估算

```rust
impl AgentPlan {
    /// 结构校验: 长度溢出、selector 解析、predicate 类型、循环检测
    pub fn validate(&self) -> Result<(), PlanError>;
    /// 时序预算估算: 基于 action 类型的已知延迟
    pub fn estimate_budget(&self) -> BudgetEstimate;
}

pub struct BudgetEstimate {
    pub total_ms: u32,
    pub hot_path_ms: u32,    // tap/key/scroll — UHID
    pub daemon_path_ms: u32, // dump/screenshot/wait — daemon
    pub warnings: Vec<String>, // "step 5 之后没 wait, LLM 可能 race"
}
```

**已知延迟表** (从 handsets benchmark + android-hid-connect 估算):

| Action | 路径 | p50 (ms) | p95 (ms) |
|---|---|---|---|
| Tap | UHID | 1 | 3 |
| TypeText (10 chars) | UHID | 3 | 8 |
| Gamepad frame | UHID direct | 0.5 | 2 |
| DumpActive | daemon | 4.58 | 6.92 |
| Screenshot 768 | daemon | 8.02 | 10.76 |
| H.265 keyframe | daemon | 12 | 20 |
| SelectAndTap | atomic | 50 | 100 |
| WaitForText | daemon (event) | 1.5 | 4 (命中后) |

### 5.3 观测边界 (Observation Boundary)

LLM agent 循环的核心安全令牌:

```rust
pub struct AgentObservation {
    /// frame summary (AI 帧) — LatestFrameSummary
    pub frame: Option<FrameSummary>,
    /// a11y tree snapshot
    pub a11y: Option<A11yTree>,
    /// device state snapshot (battery, top_activity, ...)
    pub state: Option<DeviceState>,
    /// observation boundary 序列号 — 防止 observe-then-act race
    pub sequence: u64,
    /// wall-clock ms since session start
    pub elapsed_ms: u32,
}

impl AgentControlSession {
    /// 拿一个 observation boundary
    pub fn observe(&mut self) -> Result<AgentObservation>;
    /// 在 boundary 之后等下一个 frame summary
    pub fn wait_for_frame_after(&self, seq: u64, timeout: Duration) -> Result<FrameSummary>;
    /// 跑 plan 直到完成, 返回 (plan_result, final_observation)
    pub fn run_and_observe(&mut self, plan: &AgentPlan) -> Result<(PlanResult, AgentObservation)>;
}
```

### 5.4 AgentPlanResult (timings + errors per step)

```rust
pub struct PlanResult {
    pub steps: Vec<StepResult>,
    pub total_elapsed_ms: u32,
    pub budget_estimate: BudgetEstimate,
    pub budget_variance_ms: i32,  // 实际 - 估算
}

pub struct StepResult {
    pub step_id: StepId,
    pub action: AgentAction,
    pub observation: Option<AgentObservation>, // precondition + postcondition
    pub elapsed_ms: u32,
    pub error: Option<PlanError>,
}
```

LLM 可以看到**每步实际耗时 vs 估算**, 决定下一轮 plan 怎么调。

---

## 6. 跳过/优化 (能省的省,能提的提)

### 6.1 跳过的环节

| 场景 | 跳过什么 | 节省 |
|---|---|---|
| `Gaming240Hz` | mpsc dispatcher 线程 | 1 thread + ~50ns/channel |
| `BulkText` | typed enum wrap, 直调 `HidSession::type_text` | enum tag 字节 |
| `UiAutomation` | scrcpy-server 启动 | 200-500ms 启动时间 |
| `VisionLoop` | 2 round-trip (1 atomic) | 1 RTT (~5-15ms) |
| 单 producer agent | `&mut self` 直写 (无 channel) | mpsc overhead |
| 已 cached dump | 重发 dump_active | 4.58ms |
| 已知 selector match | 重 selector parse | 0.5-1ms |

### 6.2 提升

| 优化 | 位置 | 收益 |
|---|---|---|
| mpsc → lock-free ring (gamepad 路径) | `android-hid-agent` gamepad backend | 50ns → 5ns |
| Coalescing 1ms → 0.5ms (120Hz gamepad) | `android-hid-connect::coalesce` | 8ms → 4ms 桶延迟 |
| `Vec<u8>` to_vec → scratch 复用 | `android-hid-connect::coalesce::push_message_to_scratch` | 0 alloc/frame |
| H.264 → H.265 | 协议 + daemon | 40-50% 比特率 |
| 截图 q=95 → q=80 (默认) | daemon | 22KB → 8KB (3× 压缩) |
| UiAutomation 16ms sleep → 4ms | `daemon::input::tap` | 16ms → 4ms tap hold |
| Selector parse cached (Rc<Selector>) | `selectors.rs` | 0.5ms/lookup |
| AI 帧 ↔ a11y 单 walk | `atomic::anchor_frame_to_a11y` | O(n) 一次遍历 |

### 6.3 字节级优化

- 协议 verb 全部 ASCII (已实现)
- Frame header `u32 BE` (已实现)
- 错误码用 4 字节 tag (e.g. `NOT_FOUND`) (已实现)
- 截图 JPEG q=80 + WebP q=80 (双支持)
- H.265 VPS/SPS/PPS 一次性发, 不每 keyframe 重复
- TileH265: 256x256 tile,只发变化 tile

---

## 7. 迁移计划 (session 粒度)

### Phase A: 在既有代码上加 scenario + connection mode (1 session)

1. 新建 `android-hid-agent/src/scenario.rs` (Scenario + ConnectionMode)
2. 新建 `android-hid-agent/src/connection.rs` (ConnectionHints + build_session)
3. 既有 `AgentControlSession::open(scenario, hints)` 入口
4. 测试: 7 个 scenario 的 backend 选择 + 启动

### Phase B: 原子操作 + Selector (2 sessions)

1. `android-hid-agent/src/selectors.rs` (port from `handsets/.../selector.rs`)
2. `android-hid-agent/src/atomic.rs` (typed 8 个原子操作)
3. `android-hid-protocol/src/verb.rs` 加 6 个原子 verb
4. `android-hid-daemon/src/atomic.rs` (server-side handler, 需要 server.rs 注册)
5. 测试: selector parse + find + atomic round-trip

### Phase C: H.265 streaming (1 session)

1. `android-hid-protocol/src/stream.rs` (HeccParamSets + 3 个 verb)
2. `android-hid-daemon/src/stream.rs` 加 `H265Streamer`
3. `android-hid-agent/src/stream.rs` (H265FrameStream 零拷贝)
4. 测试: VPS/SPS/PPS 解析 + frame decode loopback

### Phase D: AI plan + observation (1 session)

1. `android-hid-agent/src/plan.rs` (AgentAction + AgentPlan + BudgetEstimate)
2. `android-hid-agent/src/observation.rs` (AgentObservation + boundary)
3. 既有 `AgentControlSession` 加 `observe()` + `run_and_observe()`
4. 测试: plan validate + run + timings

### Phase E: benchmark + 真机 (1 session, 需要 SM-G9910)

1. `benches/agent_v2_bench.rs` (criterion)
2. 真机: SM-G9910 Android 11, 对比 handsets benchmark
3. 调优 coalescing window + UiAutomation sleep

### Phase F: 文档 + 验收 (0.5 session)

1. 更新 `docs/INDEX.md` + `docs/agent-v2-design.md` (本文件)
2. 更新 `roadmap-exceed-handsets.md` 标记 Phase A-D 完成
3. `ACCEPTANCE.md` 加 AC-V2-1..AC-V2-N 验收点

---

## 8. 不动 / 受保护

按 `AGENTS.md` §6 + `roadmap-exceed-handsets.md` §0,**下列不可修改**:

- `android-hid-connect/src/hid/*` — 字节级 HID 报告
- `android-hid-connect/src/control/*` — 22 scrcpy control msg 序列化
- `android-hid-connect/src/types.rs` — typed 常量
- `android-hid-connect/src/ai/*` — AI 扩展 enum

**只读复用** (新 crate 可以 import 但不修改):
- `HidClient` / `HidDispatcher` / `HidSession`
- `MultitouchHandle` / `CoalescingWriter`
- `FrameSummary` / `ObjectBox` / `TextRegion`
- `AgentControlSession` (既有 typed facade)

**新增不破坏**:
- `android-hid-protocol/src/stream.rs` (新文件, 既有 `verb.rs` 用 `Verb::StreamH265` 已经是占位)
- `android-hid-agent/src/{scenario,connection,selectors,atomic,observation,stream}.rs` (新文件)
- `android-hid-daemon/src/{atomic,stream}.rs` (新文件, 既有 `server.rs` 加 handler)

---

## 9. 验收点 (AC-V2-N)

每个 Phase 结束要满足:

- **AC-V2-1** `AgentControlSession::open(scenario, hints)` 7 个 scenario 全部能起, 5ms 内 (warm)
- **AC-V2-2** `select_and_tap` 真机: tap 命中 + dump JSON 含目标文本, 总延迟 < 60ms p50
- **AC-V2-3** `ai_anchor_tap` 真机: AI frame box 投影到 a11y 节点, tap 命中, 端到端 < 80ms p50
- **AC-V2-4** H.265 keyframe 真机: 同画质下 H.265 帧大小 < H.264 * 0.6
- **AC-V2-5** `AgentPlan::estimate_budget` 误差 ±20% vs 实测
- **AC-V2-6** `observe()` boundary 不允许 race (并发 observe 时 sequence 严格递增)
- **AC-V2-7** 既有 405 个测试 + 12 个 DaemonBackend 测试 + 14 个 UnifiedBackend 测试 全部通过
- **AC-V2-8** `cargo clippy --workspace --all-targets` 无 warning
- **AC-V2-9** `cargo doc --no-deps` 无 warning
- **AC-V2-10** 字节级兼容 `handsets` daemon (能调 hs.jar 跑现有 verb)

---

## 10. 一句话总结

**`android-hid-agent` v2 = 按场景选连接 + UHID 与 a11y 原子合并 + H.265 视频流 + typed AI plan + 跳冗余 / 升 hot path**, 在保持字节级 HID 核心不动的前提下, 端到端 LLM agent 步进延迟从 handsets 20-37ms 降到 **5-15ms** (3-7× 加速), 视频流比特率降 40-50%。

---

最后更新: 2026-06-30
