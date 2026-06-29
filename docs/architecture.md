# Architecture — `android-hid-connect`

> 模块依赖、线程模型、生命周期、纯度边界。
>
> 与 `AGENTS.md` §2.2 是同一份契约的两面:那里讲"目录长什么样",这里讲"为什么这么长"。

---

## 1. 一句话模型

```
[agent code]
   │
   ▼
AgentControlSession  (src/agent/)         ←─── 高层 facade,LLM 友好
   │
   ├── clone ──► HidClient + HidDispatcher  (src/client.rs)  ←─── mpsc + 多 batcher
   │                  │
   │                  ▼
   │             HidSession   (src/session.rs)   ←─── panic-safe lifecycle
   │                  │
   │                  ▼
   │             transport::Write  (src/transport/)   ←─── TcpStream / MockTransport
   │                  │
   │                  ▼
   │             control::ControlMessage  (src/control/)  ←─── BE 序列化
   │                  │
   │                  ▼
   │             hid::{KeyboardHid, MouseHid, GamepadHid}  (src/hid/)  ←─── 纯函数
   │
   └── reader ──► DeviceMessageReceiver / LatestFrameSummaryReceiver  (src/device.rs)
                         │
                         ▼
                   transport::Read   ←─── 同一 socket 的另一方向
                         │
                         ▼
                   device_msg 字节布局  ←─── 与 scrcpy-server 的 DeviceMessageWriter.java 对齐
```

---

## 2. 模块分层

```
            ┌─────────────────────────────────────────────────────────┐
   顶层     │  agent/                                                 │  高层 facade
            │   ├─ AgentControlSession   (typed plan + 命令 clone)    │  不直接接触 transport
            │   ├─ AgentAction           (枚举动作 + 元数据预检)      │
            │   ├─ AgentPoint/AgentRect  (归一化坐标 + basis-point)   │
            │   ├─ AgentPlanSummary      (transport-free 离线估算)     │
            │   └─ AgentPlanBoundedPrefix(预算前缀拆分 + 切分 helper)  │
            ├─────────────────────────────────────────────────────────┤
   中层     │  session      (HidSession 生命周期,UHID 驱动 + Drop)    │  持有 transport
            │  client       (HidClient + HidDispatcher + batcher)    │  多生产者 ↔ 1 dispatcher
            │  device       (DeviceMessageReceiver + FrameSummary)   │  反向 reader + 后台 pump
            │  async_device (同上,Tokio 异步适配)                     │  feature-gated
            │  multitouch   (MultitouchHandle 10-pointer 状态机)      │  不开 socket,但有状态
            │  coalesce     (CoalescingWriter 1ms 桶合并 syscall)    │  包装 Write
            │  transport    (open_tcp + MockTransport + send_*)      │  I/O 边界
            ├─────────────────────────────────────────────────────────┤
   底层     │  control      (22 control_msg + 3 AI 序列化)            │  纯函数
            │  hid          (3 HID 设备驱动 + descriptor)             │  纯函数
            │  ai           (AI 扩展 enum + typed flags)             │  纯数据
            │  types        (AndroidKeycode / Scancode / Modifiers …) │  typed 常量
            │  error        (Error enum,thiserror)                   │  数据
            └─────────────────────────────────────────────────────────┘
```

### 2.1 依赖方向约束

| from \ to | 底层 (hid/control/ai/types/error) | 中层 (session/client/device/...) | 顶层 (agent) |
| --------- | --- | --- | --- |
| **底层** | ✅ 互引 OK | ❌ 禁止 | ❌ 禁止 |
| **中层** | ✅ 可引 | ⚠️ 单向(client → session OK,session → client 禁止)| ❌ 禁止 |
| **顶层** | ✅ 可引 | ✅ 可引 | ✅ 内部 OK |

例外:

- `multitouch` 用 `hid` 和 `control`,但 `session` 内的 multitouch 走 `inject_touch`,不通过 `MultitouchHandle`。
- `device` 和 `async_device` 是**兄弟实现**,字节语义一致,**不互相依赖**。
- `client` 依赖 `session`(从 session 拿 client)和 `coalesce`(把 writer 包成桶合并)。
- `agent` 依赖 `client` + `session` + `device` + `multitouch` + `coalesce` + `transport`,以及所有底层。

### 2.2 纯度边界

| 模块 | 是否纯函数 | 允许的副作用 |
| ---- | ---------- | ------------ |
| `hid` | ✅ 100% | 无 |
| `control` | ✅ 100% | 无 |
| `ai` | ✅ 100% | 无 |
| `types` | ✅ 100% | 无 |
| `error` | ✅ 100% | 无 |
| `session` | ❌ | `Write` 调用、分配 `GamepadHid` slot、UHID lifecycle |
| `client` | ❌ | channel send、dispatcher thread、`CoalescingWriter::flush` |
| `device` | ❌ | `Read` 调用、`mpsc::channel` |
| `async_device` | ❌ | `AsyncRead` / `AsyncWrite` 调用、`tokio::sync` channel |
| `transport` | ❌ | `TcpStream::connect`、文件 `read`/`write`、Vec 累积 |
| `multitouch` | ✅ 100%(有内部状态)| 无 IO,纯状态机 |
| `coalesce` | ❌ | `Write::write_all`、内部定时器(可选)|
| `agent` | ❌ | 上述一切的编排 |

测试可以临时给纯函数模块加 `Instant::now()`,但**生产代码路径不允许**。

---

## 3. 线程与生命周期模型

### 3.1 同步路径

```
agent thread ──┬── clone client ──► producer thread A ──┐
               │                                          │
               ├── clone client ──► producer thread B ──┤
               │                                          ▼
               └── run_actions ──►                 bounded mpsc (4096 default)
                                                          │
                                                          ▼
                                                  HidDispatcher::run (单线程)
                                                          │
                                                          ▼
                                                  HidSession
                                                          │
                                                          ▼
                                                  transport::Write
                                                          │
                                                          ▼
                                                  adb forward → scrcpy-server
```

关键点:

- **HidDispatcher 是单线程**。所有控制消息都按入队顺序 serial 写 socket,保证 hid 上游语义不变。
- `HidClient` 是 `Clone`(内部 `Arc<Sender>`),多生产者共享一个 channel。
- `HidSession::Drop` 走 `catch_unwind`,panic 也会把 `UHID_DESTROY` 排队给 dispatcher。

### 3.2 反向 reader 路径

```
scrcpy-server ── adb forward ──► TcpStream::Read
                                      │
                                      ▼
                              AgentControlSession 持有 reader
                                      │
                            ┌─────────┼─────────────┐
                            ▼                       ▼
                  recv_device_event()      spawn_*_receiver()
                  (caller pull)            (bounded mpsc 后台读)
                            │                       │
                            ▼                       ▼
                      直接消费                caller 端 mpsc::Receiver
```

- **一个 socket 同时双向**:写侧由 `HidSession` / `HidClient` 拥有,读侧由 `AgentControlSession` 拥有(或 detach 给 `LatestFrameSummaryReceiver`)。
- 不能两个对象同时拥有 reader — 会 byte-desync。
- 后台 reader pump drop 时,**先 drop receiver 端 mpsc**,再 `join` pump,最后回收 reader(见 `ACCEPTANCE.md` §6 AC-R5)。

### 3.3 latest-frame 路径(感知优化)

```
reader → DeviceMessageReceiver → DeviceEvent 流
                                       │
                                       ▼
                          LatestFrameSummaryReceiver
                                       │
                                       ▼
                          watch<LatestFrameSummarySnapshot>
                                       │
                                       ▼
                          agent.observe() → LatestFrameSummaryObservation
                                       │
                                       ▼
                          agent.run_actions_and_wait_for_next_latest_frame(...)
```

- 持续 drain mixed event stream,**只保留最新** `FrameSummary`,慢消费者不会积压。
- `observe()` 返回 one-read boundary + 可选 snapshot,observe-plan-act 循环的安全令牌。

### 3.4 Tokio async 路径

`feature = "tokio"` 时:

- `read_device_message_async` / `read_device_event_async` — 字节级语义与同步版一致。
- `spawn_async_*_receiver` — 后台 tokio task,bounded mpsc 给 caller。
- `spawn_async_latest_frame_summary_receiver` — 用 `tokio::sync::watch` 而不是 std mpsc。
- **不重写** 写侧 — `HidSession` / `HidClient` 仍然 sync,async 适配只覆盖 reader 方向。

---

## 4. 关键设计决策

### 4.1 为什么 hid/control 是纯函数?

- 可单测:字节级断言不需要起 socket,见 `tests/integration.rs` 和各模块 `#[cfg(test)] mod tests`。
- 可 fuzz:`cargo fuzz` 直接打纯函数,覆盖率 100%。
- 可组合:`AgentAction::tap` = `INJECT_TOUCH_EVENT(DOWN) + MOVE + UP`,三个纯函数调用。

### 4.2 为什么 HidDispatcher 是单线程?

scrcpy-server Java 端按收到顺序处理 control msg;乱序会破坏 UHID 语义(slot 分配、CREATE/INPUT/DESTROY 顺序)。单线程 dispatcher 天然保证顺序,避免 channel 内部再加 `SequenceLock`。

### 4.3 为什么 batcher 都用 fixed-stack 默认?

- 32 frame fixed buffer = 0 堆分配,常见游戏循环每帧 1-2 个动作,远低于 32。
- 超出 32 走 `Vec` 备份路径,失败时 batch 自动回到 batcher,不会丢 payload。
- 公开 `KEYBOARD_BATCH_FRAMES` / `MOUSE_BATCH_FRAMES` / `SCROLL_BATCH_FRAMES` / `GAMEPAD_BATCH_FRAMES` / `TOUCH_BATCH_FRAMES` / `ANDROID_KEY_BATCH_FRAMES` 让 caller 知道上限,提前切片。

### 4.4 为什么 AgentAction 是 typed enum 而不是字符串?

- 编译期校验 keycode/axis/button 名字写错。
- `AgentPlanSummary::analyze` / `first_structural_error` 能在 dispatch 前发现长度溢出、chord 超 6 key、unsupported strict char 等问题。
- LLM agent 输出的"伪 plan"在 LLM 端先序列化成本 enum 再下发,出错路径短。

### 4.5 为什么 AgentControlSession 用 catch_unwind 的 Drop?

`HidSession::Drop` 必须保证 UHID_DESTROY 发出,即使调用方 `panic!()`。否则设备上残留一个虚拟 UHID 设备,下次连接 slot 错乱。

### 4.6 为什么 async_device 是单独模块而不是 trait 抽象?

- 同步 / 异步 parser 字节语义一致,但 trait 抽象会引入 `Pin<Box<dyn Future>>` 或 GAT,徒增复杂度。
- 异步调用方通常用 `tokio::spawn` 包一层,显式调用 `read_device_message_async` 比隐式 trait dispatch 更清晰。
- feature gate 让默认 build 不拉 tokio 依赖。

---

## 5. 典型调用链(对照表)

| 场景 | 调用链 |
| ---- | ------ |
| 单次 tap | `agent.tap(x, y)` → `AgentControlSession::tap` → `HidClient::send_*` → dispatcher → `INJECT_TOUCH_EVENT(DOWN/MOVE/UP)` |
| 60Hz 手柄 | agent loop → `client.send_frame_raw_packed_batch(&frames)` → dispatcher → `UHID_INPUT`(gamepad slot)|
| 240Hz 低延迟手柄 | `OpenRequest::gamepad_only_realtime()` → `session.set_frame_raw_packed_batch(&frames)` → direct write(绕过 coalescing)|
| LLM 观察 + 决策 | `latest.observe()` → plan against boundary → `agent.run_actions(&[...])` → `latest.wait_next_after_observation()` |
| 边界关闭 | `agent.close_checked()` → checked barrier → `UHID_DESTROY × N` → 回收 transport + reader,回传 shutdown 前 command error |
| TCP 超时读 | `agent.wait_for_target_rect_timeout(&sel, dur)` → 临时设置 read_timeout → `Error::AgentTimeout` → 恢复原 timeout |

---

## 6. 扩展点(给想加新功能的人)

按"加在哪个模块不会破坏分层"排序:

| 想加什么 | 加在哪 | 注意 |
| -------- | ------ | ---- |
| 新 control_msg 类型 | `src/control/message.rs` + `src/types.rs` | 先跟 scrcpy 上游对齐字节,见 `docs/scrcpy-protocol-compatibility.md` |
| 新 HID 设备驱动 | `src/hid/*.rs` + `src/hid/descriptor.rs` | descriptor 必须字节级对齐(目前只有 keyboard/mouse/gamepad)|
| 新 typed 常量 | `src/types.rs` | 暴露在 `lib.rs` re-export |
| 新 facade helper | `src/agent/session.rs` 或 `src/agent/action.rs` | 复用现有 batcher,避免开新通道 |
| 新 batcher | `src/client.rs` | 跟现有 `KeyboardFrameBatcher` 等同模板,fixed-stack 默认 |
| 新 async API | `src/async_device.rs`(整个 feature gate) | 必须与同步版字节级一致,加 `async_device::tests::*` |
| 新 example | `examples/*.rs` | 单职责,需真机时顶部写明前置步骤 |
| 新真机 E2E 项 | `examples/live_*.rs` + `ACCEPTANCE.md` §7 表格 | 跑过再写,不要"等下补" |

---

## 7. 常见反模式(改了就要打回的)

- ❌ 在 `hid::*` 引入 socket 或 channel → 破坏纯函数边界。
- ❌ 在 `control::serialize_*` 加 `std::time::Instant::now()` → 同上。
- ❌ 在 `AgentControlSession` 之外定义 `Agent*` 公开类型 → 破坏 facade 单点入口。
- ❌ 改 `HidDispatcher` 为多线程(加锁)→ 破坏 control msg 顺序保证。
- ❌ 把 `serde` 加到 `hid::*` / `control::*` → 增加无关依赖,本 crate 故意不序列化这些 enum 到 JSON。
- ❌ 把 `tokio` 加到 `[dependencies]` 而不是 `optional = true` → 破坏"默认零三方"原则。

---

## 8. 相关文档

- 目录规则 + 允许/禁止: [`../AGENTS.md`](../AGENTS.md)
- 字节布局速查: [`wire-format.md`](wire-format.md)
- AI agent 怎么用: [`ai-agent-integration.md`](ai-agent-integration.md)
- 验收点 + 真机回归: [`../ACCEPTANCE.md`](../ACCEPTANCE.md)
- scrcpy 上游契约: [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md)

最后更新: 2026-06-29。