# AI Agent Integration — `android-hid-connect`

> 怎么把本 crate 嵌入 LLM / agent runtime。本文档面向写 agent runtime 的工程师,假设你已经会用本 crate 的 `HidSession` / `HidClient`。
>
> 与 `README.md` 互补:那里讲 API 长什么样,这里讲怎么**用它**写出鲁棒、低延迟、不写炸真机设备的 agent 循环。

---

## 1. 为什么需要 Agent facade

裸 `HidSession` / `HidClient` 已经能完成 UHID 注入 + reverse 解析,但 LLM agent 通常需要:

- **typed plan**(LLM 输出 JSON → typed enum),不是裸 `ControlMessage`
- **结构预检**(typed enum 在 dispatch 前就能拒绝坏 plan)
- **统一等待原语**(等 frame summary / 等 scene change / 等 stable frames),而不是 poll sleep
- **latest-frame cache**(感知慢消费者 → 跳过旧帧 backlog,直接读最新)
- **observation → plan → dispatch → wait** 的原子化边界

这些都在 `AgentControlSession`(`src/agent/`)里。

---

## 2. 入口与三种 agent loop

### 2.1 单次 connect

```rust,no_run
use android_hid_connect::{AgentControlSession, OpenRequest};

let (prefix, mut agent) =
    AgentControlSession::connect_tcp("127.0.0.1", 27183, OpenRequest::all())?;
println!("connected to {}", prefix.device_name);
agent.set_screen_size(1080, 2400)?;
```

- 自动消费 scrcpy prefix (1 dummy byte + 64 设备名),返回 `ScrcpyControlPrefix`。
- 默认走 coalescing 1ms 桶,适合大多数 agent loop。
- 不需要 prefix 时用 `AgentControlSession::from_parts(...)` 手动组装。

### 2.2 推荐的低延迟循环:`observe → plan → dispatch → wait`

```rust,no_run
use std::time::Duration;
use android_hid_connect::{AgentAction, AgentTargetSelector, TouchPointerId};

let (latest, _pump) = agent.detach_latest_frame_summary_receiver()?;
let observation = latest.observe();

// 同一帧已有合适目标 → 0-wait 直接 dispatch
let tapped = agent.tap_latest_observation_target_at_pointer(
    &observation,
    AgentTargetSelector::best_object(),
    TouchPointerId::VIRTUAL_FINGER,
    5_000, 5_000,
)?;

// 没有目标 → 派 action + 等下一帧
let next = agent.run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
    &[AgentAction::query_ai(0)],
    &latest,
    &observation,
    Duration::from_millis(120),
)?;
```

要点:

- **先 `observe()` 一次**,得到 `LatestFrameSummaryObservation`(one-read boundary)。
- **优先用同一帧**:有目标就 `tap_latest_observation_*`,不浪费 LLM 推理时间。
- **没目标才 dispatch + 等下一帧**:`run_actions_and_wait_for_next_latest_frame_after_observation_timeout` 自动用 post-barrier cache version,等待的是 **action 之后** 的新帧,不是观察时的旧帧。

### 2.3 高并发 loop:多 producer + checked barrier

```rust,no_run
use android_hid_connect::{AgentAction, AgentPlanSummary};

let client = agent.clone_client();
let plan = vec![
    AgentAction::tap(540, 1200),
    AgentAction::type_text("hello"),
    AgentAction::query_ai(0),
    AgentAction::Flush,
];

let summary = AgentPlanSummary::analyze(&plan);
if !summary.try_run_dispatch_fits_bound(agent.command_bound()) {
    // 计划超出队列预算 → 拆分或重排
}

// try_run_actions:non-blocking enqueue + checked barrier(回传前序 command error)
agent.try_run_actions(&plan)?;
```

要点:

- `clone_client()` 拿 `HidClient`(内部 `Arc<Sender>`)给 worker 线程,共用一个 dispatcher。
- `try_run_actions` 比 `run_actions` 更适合高竞争调度器(channel 满时返回 `Error`,不阻塞)。
- `AgentPlanSummary::analyze` 离线估算 dispatcher command 压力,**不 dispatch**,纯函数。

---

## 3. 三种 dispatch 模式怎么选

| API | 阻塞 enqueue? | checked barrier? | 适用 |
| --- | ------------- | ---------------- | ---- |
| `queue_actions(&[…])` | ✅ 阻塞 | ❌ | 想吞满 back-pressure,不在意 error |
| `run_actions(&[…])` | ✅ 阻塞 | ✅ (1 次) | 通用 agent loop,简单可靠 |
| `try_queue_actions(&[…])` | ❌ 非阻塞 | ❌ | 满 channel 立刻放弃整批,**先 preflight** |
| `try_run_actions(&[…])` | ❌ 非阻塞 | ✅ (1 次,non-blocking)| 高竞争 scheduler,需要 error 又要 fast-fail |
| `try_queue_actions_prefix(&[…])` | ❌ | ❌ | 只派前缀(到第一个 blocking timing barrier)|
| `try_run_actions_prefix(&[…])` | ❌ | ✅ | 只派前缀 + checked barrier |
| `try_queue_actions_bounded_prefix(&[…], bound)` | ❌ | ❌ | 按 command 预算派最长前缀 |
| `try_run_actions_bounded_prefix(&[…], bound)` | ❌ | ✅ | 按 command 预算派最长前缀 + checked barrier |

### 3.1 preflight 必须先做

```rust,no_run
let actions = vec![
    AgentAction::tap(10, 20),
    AgentAction::type_text_strict("€uro"),  // unsupported char!
    AgentAction::Flush,
];

if let Some(err) = AgentAction::first_structural_error(&actions) {
    eprintln!("reject before dispatch: {err}");
    return Ok(());
}
if let Some(err) = AgentAction::first_try_queue_error(&actions) {
    eprintln!("reject before try_queue: {err}");
    return Ok(());
}

agent.run_actions(&actions)?;
```

`first_structural_error` 检查:

- fixed-buffer batch 超 32 帧
- keyboard chord > 6 键
- strict text 出现 unsupported char
- START_APP name > 255 字节
- rect basis-point anchor 越界

`first_try_queue_error` 检查同上 + 任何 blocking timing barrier(Wait / LongPress 不允许 try_queue)。

**为什么必须 preflight**:`queue_actions` / `try_queue_actions` 在 plan 中段发现错会**回滚已派发的前缀**? 不 — 会**继续派**直到结尾,出错时返回 `Err`,已派发的部分需要 caller 自己清理。所以**preflight 在派发前一次性拒绝**。

### 3.2 拆分 blocking 与 non-blocking 段

`Wait` 和 `LongPress` 必须用 `run_actions`(blocking timing barrier);`try_*` 路径会拒绝它们。

```rust,no_run
let plan = vec![
    AgentAction::tap(540, 1200),
    AgentAction::LongPress { x: 540, y: 1200, duration: Duration::from_millis(800) },
    AgentAction::type_text("done"),
];

let split = AgentAction::blocking_timing_prefix_len(&plan);  // 1
let (non_blocking, blocking) = plan.split_at(split);
agent.try_run_actions(non_blocking)?;  // fast non-blocking
agent.run_actions(blocking)?;          // blocking with checked barrier
```

---

## 4. 感知循环的三种模式

### 4.1 Latest-frame cache(默认)

```rust,no_run
let (latest, _pump) = agent.detach_latest_frame_summary_receiver()?;
let observation = latest.observe();   // one-read boundary

// 等下一帧(>= observation.boundary_version)
let next = agent.wait_next_after_observation(&latest, &observation)?;

// 或限时
let next = latest.wait_next_after_observation_timeout(&observation, Duration::from_millis(200))?;

// 或自定义 predicate
let match_f = latest.wait_matching(&observation, |s| {
    s.objects.iter().any(|o| o.class_id == 7 && o.confidence >= 200)
})?;
```

要点:

- **持续 drain mixed event stream,只保留最新 `FrameSummary`**。慢消费者不积压旧帧。
- `observe()` 返回 one-read boundary,后续 `wait_next_after_observation` 等的是 **新** 帧。
- 已 cached 的帧可以直接 `tap_latest_observation_target_at_pointer`,**0-wait dispatch**(在 §2.2 例子里)。

### 4.2 Ordered event stream

```rust,no_run
while let Some(event) = agent.recv_device_event()? {
    match event {
        DeviceEvent::FrameSummary(s) => { /* process */ }
        DeviceEvent::AiStats(s)      => { /* process */ }
        DeviceEvent::Native(DeviceMessage::Clipboard(text)) => { /* process */ }
        _ => {}
    }
}
```

要点:

- **不丢旧帧**,但消费者慢会让 dispatcher 阻塞。
- 适合 frame-by-frame 训练 / 调试,不适合生产 agent loop。

### 4.3 Background receiver

```rust,no_run
let (event_rx, pump) = spawn_default_device_event_receiver(reader)?;
std::thread::spawn(move || {
    while let Ok(Ok(event)) = event_rx.recv() {
        // process on worker thread
    }
    drop(event_rx);
    pump.join().unwrap();
});
```

要点:

- 后台线程 + bounded mpsc,caller 端 consumer drop 后 pump 自动 join 回收 reader。
- 与 latest-frame cache 不兼容(两个消费者同时读会 byte-desync)。要么用 §4.1,要么用 §4.3。

---

## 5. 目标选择 API 速查

### 5.1 单 frame 内部选目标(无 wait)

```rust,no_run
use android_hid_connect::{AgentObjectSelector, AgentRect, AgentTargetSelector};

let summary: &FrameSummary = ...;

// best object (按 confidence + area tie-break)
let r: AgentRect = summary.best_object().unwrap();

// class-filtered object
let r = summary.best_object_in_class(7).unwrap();   // 7 = button class_id
let r = AgentObjectSelector::class_min_confidence(7, 220)  // 7 = button, conf >= 0.86
    .select_from_frame(summary).unwrap();

// indexed text
let r = summary.text_region(0).unwrap();

// largest text region
let r = summary.largest_text_region().unwrap();

// unified
let r = AgentTargetSelector::best_object().select_from_frame(summary).unwrap();
```

### 5.2 等下一个匹配目标(wait)

```rust,no_run
let r = agent.wait_for_best_object_rect()?;                  // 默认 object frame
let r = agent.wait_for_object_selector_rect(&sel)?;          // class+conf filter
let r = agent.wait_for_largest_text_region_rect()?;          // text frame

// 限时版(TCP session 才支持)
let r = agent.wait_for_target_rect_timeout(&sel, Duration::from_millis(500))?;

// 帧预算版(超 budget 返回 Ok(None) 不 dispatch tap)
let r = agent.tap_next_object_selector_with_limit(&sel, 8)?;  // 最多看 8 帧
```

### 5.3 等 + 派发(action+wait+tap)

```rust,no_run
let r = agent.run_actions_and_tap_next_object_selector_at_pointer(
    &[AgentAction::tap(540, 1200)],
    &sel,
    TouchPointerId::VIRTUAL_FINGER,
    5_000, 5_000,   // basis-point anchor(中心点 50%, 50%)
)?;
```

要点:

- **一个 checked barrier**:先派 `actions`,等目标,再 tap。一气呵成,避免两次 dispatch 之间状态漂移。
- `at_pointer` 版保留 scrcpy 保留 pointer id(mouse / virtual-finger)。
- `at(anchor_x_bp, anchor_y_bp)` 在 rect 内部按 basis-point 选锚点(左上 0,0; 右下 10_000, 10_000)。

---

## 6. 等待场景速查

| 想等什么 | API |
| -------- | --- |
| 下一帧 (latest-frame cache) | `latest.wait_next_after_observation(...)` |
| 任意 predicate 满足 | `latest.wait_matching(...)` |
| Scene change (frame diff > threshold) | `agent.wait_for_scene_change()?` |
| Motion detected | `agent.wait_for_motion()?` |
| N 帧 stable(无变化) | `agent.wait_for_stable_frames(3)?` |
| Frame seq > X | `agent.wait_for_frame_summary_after_seq(seq)?` |
| Timestamp > X | `agent.wait_for_frame_summary_after_timestamp(ms)?` |
| 限定帧数 | `*.with_limit(n)` 后缀变体 |
| 限定 wall-clock (TCP) | `*_timeout(..., duration)` 后缀变体 |

---

## 7. Clipboard 与 AI stats 等待

```rust,no_run
// 设置剪贴板 + 等 ACK
let ack = agent.set_clipboard_and_wait_ack("copied", false)?;

// 读剪贴板 + 等响应
let text = agent.get_clipboard_and_wait_key(ClipboardCopyKey::COPY)?;

// AI stats
let stats = agent.query_ai_and_wait_stats(0)?;
println!("fps={:.1}", stats.current_fps);

// action + AI_QUERY + 读 stats(一个 checked barrier)
let stats = agent.run_actions_and_query_ai_and_wait_stats(
    &[AgentAction::tap(10, 20)],
    0,
)?;

// TCP bounded
let stats = agent.query_ai_and_wait_stats_timeout(0, Duration::from_millis(500))?;
```

要点:

- 等待过程中会**跳过不相关 native event**(不阻塞在 clipboard ACK 等 AI 帧)。
- TCP bounded 版会**临时设置 read timeout**,退出时恢复原值(见 `ACCEPTANCE.md` §6 AC-R5 / AC-R7)。

---

## 8. 关闭路径

```rust,no_run
// 推荐:回收 + 报告最后 command error
let report = agent.close_checked()?;
if let Some(err) = report.command_result {
    eprintln!("queued command failed before close: {err}");
}
let closed = report.closed;
let _stream = closed.transport;  // TcpStream

// 普通 close (fire-and-forget)
agent.close()?;
let _stream = agent.into_inner().transport;  // 不带 command error 检查
```

要点:

- `close_checked` 在 close 前**做一次 checked barrier**,回传 barrier 后第一个 command error。
- `detach_latest_frame_summary_receiver` 之后,**写侧**用 `close_transport_checked` 关闭。
- 任意 panic 路径都会走 `Drop` → `try_close_all` → `UHID_DESTROY × N`(catch_unwind 保证)。

---

## 9. typed plan 工厂速查

```rust,no_run
use android_hid_connect::{AgentAction, AgentPoint, AgentRect, TouchPointerId, Modifiers, Scancode};

// tap / swipe
AgentAction::tap(540, 1200)
AgentAction::tap_pointer(TouchPointerId::VIRTUAL_FINGER, 540, 1200)
AgentAction::tap_point(AgentPoint::CENTER)
AgentAction::swipe((100, 500), (900, 500), 4)

// rect 锚点
let text_box = AgentRect::try_from_pixels(120, 500, 841, 121, 1080, 2400)?;
AgentAction::tap_rect(text_box)
AgentAction::tap_rect_at(text_box, 1_000, 5_000)  // left-middle anchor
AgentAction::swipe_rect(text_box, (0, 5_000), (10_000, 5_000), 4)

// pinch
AgentAction::pinch_points(
    AgentPoint::try_from_basis_points(4_000, 5_000)?,
    AgentPoint::try_from_basis_points(3_000, 5_000)?,
    AgentPoint::try_from_basis_points(6_000, 5_000)?,
    AgentPoint::try_from_basis_points(7_000, 5_000)?,
    6,
)

// keyboard
AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT)
AgentAction::ctrl_scancode(Scancode::C)
AgentAction::try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL)?

// mouse
AgentAction::mouse_motion_buttons(12, -4, &[MouseButton::Left])
AgentAction::mouse_scroll(0, -1)

// AI
AgentAction::configure_ai(AI_FLAG_OBJECTS | AI_FLAG_TEXT, 16, 0)
AgentAction::query_ai(0)
AgentAction::pause_ai()

// clipboard
AgentAction::SetClipboard { sequence: 0, text: "x".into(), paste: false }

// control
AgentAction::SetScreenPower { on: true }
AgentAction::LaunchApp { name: "com.example".into() }

// boundary
AgentAction::Flush
```

---

## 10. 常见反模式

### 10.1 不用 plan,直接串 API

```rust
// ❌ 不用 plan:每次都 blocking
agent.tap(540, 1200)?;
agent.type_text("hello")?;
agent.query_ai(0)?;
```

```rust
// ✅ 用 plan:一次 checked barrier,channel pressure 降低
agent.run_actions(&[
    AgentAction::tap(540, 1200),
    AgentAction::type_text("hello"),
    AgentAction::query_ai(0),
])?;
```

### 10.2 用 sleep 等 UI

```rust
// ❌ 用 sleep 猜 UI 响应时间
agent.tap(540, 1200)?;
std::thread::sleep(Duration::from_millis(500));
agent.tap(700, 800)?;  // 不知道上一个 tap 是否生效
```

```rust
// ✅ 用 wait_for_scene_change / wait_for_stable_frames
agent.run_actions_and_wait_for_scene_change(&[AgentAction::tap(540, 1200)])?;
agent.run_actions_and_wait_for_stable_frames(&[AgentAction::tap(700, 800)], 2)?;
```

### 10.3 用裸 Thread + sleep

```rust
// ❌ 后台线程裸 sleep,channel 满就 panic
std::thread::spawn(move || loop {
    client.send_frame_unchecked(frame)?;
    std::thread::sleep(Duration::from_millis(16));
});
```

```rust
// ✅ 用 batcher + try_send
use android_hid_connect::client::GamepadFrameBatcher;
let mut batcher = GamepadFrameBatcher::unchecked(&client, 8);
loop {
    if batcher.try_push(frame).is_err() {
        // back-pressure:选择 skip 或 retry,不阻塞
        continue;
    }
    if frame_idx % 8 == 7 { batcher.flush()?; }
}
```

### 10.4 直接读 device_msg 自己 parse

```rust
// ❌ 自己拼 byte 解析 device_msg
let mut buf = [0u8; 5];
reader.read_exact(&mut buf)?;
let msg_type = buf[0];
let text_len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
```

```rust
// ✅ 用 device::read_device_message (字节级与 scrcpy 一致)
match android_hid_connect::read_device_message(&mut reader)? {
    DeviceMessage::Clipboard(text) => { /* UTF-8 already validated */ }
    DeviceMessage::AckClipboard { sequence } => { /* ... */ }
    DeviceMessage::UhidOutput { id, data } => { /* ... */ }
}
```

### 10.5 不用 AgentPlanSummary 估算

```rust
// ❌ 不预检,channel 满才发现超 budget
agent.run_actions(&huge_plan)?;  // Err(SessionLifecycle(...))
```

```rust
// ✅ 先 AgentPlanSummary 估算
let summary = AgentPlanSummary::analyze(&huge_plan);
if summary.estimated_run_dispatch_commands > agent.command_bound() {
    return Err(/* 拆分重排 */);
}
agent.run_actions(&huge_plan)?;
```

---

## 11. 错误处理模板

```rust,no_run
use android_hid_connect::Error;

match agent.run_actions(&plan) {
    Ok(()) => { /* dispatch + checked barrier 成功 */ }
    Err(Error::AgentTimeout("latest frame summary")) => {
        // post-barrier 等帧超时 → 重试或放弃
    }
    Err(Error::AgentTimeout("wait_for_clipboard")) => {
        // TCP read timeout → 恢复已自动处理,检查上层 retry 策略
    }
    Err(Error::SessionLifecycle("channel closed")) => {
        // dispatcher 挂了 → 重建 session
    }
    Err(Error::BufferFullCritical) => {
        // channel 满 + 不可丢消息 → 升级 bound 或降 producer 速率
    }
    Err(e) => { eprintln!("unexpected: {e}"); }
}
```

---

## 12. checklist: agent loop 接入前自检

- [ ] 选对了入口:`AgentControlSession::connect_tcp` vs `from_parts`
- [ ] `set_screen_size(width, height)` 在第一次 `tap_*` / `swipe_*` 之前调用
- [ ] `detach_latest_frame_summary_receiver` 在第一次 `wait_for_frame_*` 之前调用
- [ ] 用 `AgentPlanSummary::analyze` 在每次大 plan 派发前估算
- [ ] blocking / non-blocking 段按 `blocking_timing_prefix_len` 拆分
- [ ] 等 UI 用 `wait_for_*` 不用 `sleep`
- [ ] 关闭用 `close_checked` 回收 + 报告 command error
- [ ] panic-safe:用 `AgentControlSession` 而非裸 `HidSession`(前者 Drop 有 catch_unwind)

---

## 13. 相关文档

- API 总览: [`../README.md`](../README.md)
- 字节布局: [`wire-format.md`](wire-format.md)
- 模块分层: [`architecture.md`](architecture.md)
- AC 验收 + 真机回归: [`../ACCEPTANCE.md`](../ACCEPTANCE.md)
- 错误码速查: [`wire-format.md`](wire-format.md) §9

最后更新: 2026-06-29。