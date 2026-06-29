# scrcpy Protocol Compatibility — `android-hid-connect`

> 锁定的 scrcpy 上游版本、byte-exact 契约、已知 caveat、跟踪流程。
>
> 与 [`wire-format.md`](wire-format.md) 互补:那里讲"字节怎么排",这里讲"为什么这么排、什么时候可以变"。

---

## 1. 锁定的上游版本

| 组件 | 版本 | 来源 | 锁定方式 |
| ---- | ---- | ---- | -------- |
| scrcpy C 客户端 (控制协议定义) | v2.7 | `scrcpy/app/src/control_msg.c`、`scrcpy/app/src/hid/*.c` | 本 crate 字节级对齐 |
| scrcpy-server Java daemon | v2.7 | `scrcpy/server/src/main/java/com/genymobile/scrcpy/` | `examples/live_*.rs` 启动命令固定 |
| AI extension protocol | 自定义(0x80/0x81 等)| `scrcpy-ai-server` 上游(非 scrcpy 主线) | `docs/wire-format.md` §5 列出当前 layout |

**为什么不锁到 commit hash**:
scrcpy 客户端 v2.7 release 后的 commit 主要是 bug fix / 平台适配,control_msg.c 的 wire format 没动过。本 crate 用 release 标签对齐,这样 reviewer 看 `Cargo.toml` description / `ACCEPTANCE.md` §10 就能知道对应源码。

---

## 2. byte-exact 契约

下面是逐项的 byte-exact 保证,**对应 `ACCEPTANCE.md` §10 表格**:

| 维度 | scrcpy (C) | 本 crate (Rust) | 断言位置 |
| ---- | ---------- | --------------- | -------- |
| 22 control_msg 序列化 | `app/src/control_msg.c::sc_control_msg_serialize` | `src/control/message.rs::serialize_*` | `tests/integration.rs` + 各模块 `#[cfg(test)] mod tests` |
| 3 HID report descriptor | `app/src/hid/{keyboard,mouse,gamepad}.c` | `src/hid/descriptor.rs` | `hid::descriptor::tests::descs_have_content` |
| Keyboard 6KRO phantom state | `app/src/hid/keyboard.c` | `src/hid/keyboard.rs` | `hid::keyboard::tests::phantom_state_when_seven_keys_pressed` |
| Mouse scroll accumulator | `app/src/hid/mouse.c` | `src/hid/mouse.rs` | `hid::mouse::tests::scroll_emit_only_after_integer_accumulated` |
| Gamepad 8-slot 分配 (id 3..=10) | `app/src/uhid/uhid.cpp` | `src/hid/gamepad.rs::slot_hid_id` | `hid::gamepad::tests::slot_hid_id_roundtrip` |
| `sc_control_msg_is_droppable` | `app/src/control_msg.c` | `ControlMessage::is_critical` | `control::message::tests::critical_flag_matches_scrcpy` |
| 3 device_msg 反序列化 | `app/src/device_msg.c` + `DeviceMessageWriter.java` | `src/device.rs::read_device_message` | `device::tests::clipboard_message_uses_text_length_prefix` 等 |
| BE 字节序 | 网络字节序 | `to_be_bytes()` | 所有 serialize_into 测试 |
| Gamepad stick i16→u16 重映射 | `app/src/hid/gamepad.c` | `hid::gamepad::axis_event_rescales` | `hid::gamepad::tests::axis_event_rescales` |

### 2.1 字节兼容是单向的

本 crate 可以被新设备 / 新 server 接受;反过来,**任何用本 crate 写的客户端不能跑去连旧版本 scrcpy-server**,因为:
- v2.6 之前的 INJECT_SCROLL_EVENT 字段不全
- v2.4 之前 UHID_INPUT size 上限是 8 而非 15
- v2.0 之前没有 AI extension tag

只在 server ≥ v2.7 时连,否则用对应老版本客户端。

---

## 3. 已知上游 caveat (NOT a library bug)

### 3.1 Samsung OneUI 8-slot UHID EINVAL

**症状**:在 SM-G9910 (Android 11, OneUI) 上,**一次性打开 8 个 gamepad slot** 会触发 kernel UHID 限制
(`UhidManager.open: write failed: EINVAL` in `/dev/uhid`)。服务端收到 8 次 CREATE 后,内核在第 N 个 UHID 设备上返回 `EINVAL`。

**本 crate 行为**:字节级照常 PASS,`HidSession::open` 不报错 — 我们只验证字节布局和 server 接收顺序,kernel 拒绝发生在 server 端 Java 的 `Os.write`,与本 crate 序列化逻辑无关。

**workaround**:

- 单设备连续 open/destroy (`examples/live_kbd` 流程) 不触发。
- `HidSession::open` 默认 `1 keyboard + 1 mouse + 1 gamepad` 组合不触发。
- 生产场景需要 8 个手柄,等 scrcpy 上游加 gamepad slot 池复用 — 超出本 crate 范围。

**追踪**:`ACCEPTANCE.md` §7.3。

### 3.2 scrcpy-server 启动前缀

**症状**:server 启动后第一帧是 dummy byte (`0x00`) + 64 字节 NUL-padded device name,不是 control_msg。漏读这个 prefix 会把 prefix 误解析成 device_msg。

**本 crate 行为**:`AgentControlSession::connect_tcp` 自动消费 prefix(`read_scrcpy_control_prefix`)。手动用 `transport::open_tcp` 时,记得先调一次 prefix reader。

**追踪**:`ACCEPTANCE.md` §6 AC-R6 + `docs/wire-format.md` §6。

### 3.3 device_msg 没有统一 envelope

**症状**:scrcpy v2.7 的 3 种 device_msg **不是统一 envelope**。CLIPBOARD 带 `u32` 长度前缀,ACK / UHID_OUTPUT 各自定长 / 自描述长度。如果按"统一 envelope"假设去解析,会在 ACK 后接 UHID_OUTPUT 时 byte-desync。

**本 crate 行为**:`device::read_device_message` 按 type tag 分支解析,保持 byte-aligned。

**追踪**:`ACCEPTANCE.md` §6 AC-R1..AC-R4。

### 3.4 INJECT_TEXT > 300 字符截断

**症状**:`INJECT_TEXT` payload 超过 300 字符会被截断(scrcpy 故意限制,避免反压)。

**本 crate 行为**:不报错,只截断。超过 300 字符想完整输入,拆成多次 + small delay。

**追踪**:`ACCEPTANCE.md` §1 AC-C2。

### 3.5 SET_CLIPBOARD 截断到 CONTROL_MSG_MAX_SIZE

**症状**:`SET_CLIPBOARD` 总长超 256 KiB 会被截断。

**本 crate 行为**:`serialize_into` 把 `set_clipboard` payload 截断到上限,而不是返回 `Error::ControlMessageTooLarge`(那是 UHID_INPUT 的行为)。

**追踪**:`ACCEPTANCE.md` §1 AC-C22 + `docs/wire-format.md` §2 通用约束。

---

## 4. 跟踪上游变更

### 4.1 常规流程

1. dependabot weekly 跑 → 升 Rust dep / CI 工具链(`Cargo.toml` + `.github/dependabot.yml`)。
2. scrcpy 上游 release → 在 issue 里讨论本次 release 是否影响 control_msg.c / device_msg.c:
   - **不影响**(纯 bug fix / 平台适配)→ 不动 `Cargo.toml`,在 `CHANGELOG.md` 写一句"tracked scrcpy v2.8 release, no wire format impact"。
   - **影响**(新增 control_msg / device_msg type / 字段变化)→ 走 §4.2 RFC 流程。

### 4.2 wire format 变更 RFC 流程

**禁止**在 PR 里悄悄改字节布局。任何 byte layout 改动必须按以下顺序:

1. **开 issue** 描述 scrcpy 上游哪个 commit 改了 wire format,贴 C 端 diff。
2. **在 PR 之前**:
   - 更新 `docs/wire-format.md` 新 layout
   - 在 `ACCEPTANCE.md` §1 加新 AC 项 + 引用 issue 编号
3. **改实现**:
   - 先加测试断言新 layout(测试先 fail)
   - 再改 `src/control/message.rs` / `src/hid/*` / `src/device.rs`
   - 测试变绿
4. **真机回归**:
   - 在 `examples/live_e2e.rs` 跑新 AC 项
   - 更新 `ACCEPTANCE.md` §7 跑分记录 + 时间戳 + 设备
5. **CI 跑过**:
   - 3-OS 矩阵 + MSRV + clippy + fmt
6. **merge 前**:
   - `CHANGELOG.md` 写 BREAKING CHANGE 段(如果旧版本 client 不能连新 server,或反过来)
   - 同步 `Cargo.toml` version bump(release-please 会处理,但 PR 描述里点出来)

### 4.3 AI extension protocol 升级

`scrcpy-ai-server` 升级时:

- 新增 `FrameSummary` 字段 → minor-compatible,直接加到 typed view,旧字段标 `#[serde(default)]` 同等行为(本 crate 不 serde,只在 `agent::FrameSummary` 上加 default 构造)。
- 新增 envelope type → 加到 `device::DeviceEvent` enum 变体,旧 variant 保留。
- 删除字段 → 不允许在本 crate 内发生,跟 scrcpy-ai-server 提 issue 拒绝。

---

## 5. 反向:本 crate 不做什么

下面这些**不属于**本 crate 的兼容范围,即使 scrcpy 上游做了,本 crate 也不会跟:

- ❌ 视频流解码(H.264/H.265)。本 crate 是控制平面,不走 video socket。
- ❌ 音频流(scrcpy v2.x 加了 audio forwarding,本 crate 不管)。
- ❌ AOA / USB 传输(本 crate 只走 `adb forward tcp:27183`,不走 AOA)。
- ❌ 录屏 / 截图(由 scrcpy 主线 / `handsets` 做)。
- ❌ 多设备并行(本 crate 一个 `HidSession` 对应一个 device;多设备开多个 session,各自 transport)。

---

## 6. CI 矩阵 vs 兼容性

`.github/workflows/ci.yml` 跑的是:

- ubuntu-latest + stable: fmt + clippy + build + test
- macos-latest + windows-latest: build + test
- MSRV 1.87: build only

CI **不跑真机**,真机回归由人在 `examples/live_*.rs` 跑后写入 `ACCEPTANCE.md` §7。

CI 的字节兼容保证来自:

- `tests/integration.rs` — TCP + 全部 22 control_msg 字节级断言
- `tests/ai_intents.rs` — AI extension 字节级断言
- 各模块 `#[cfg(test)] mod tests` — HID descriptor / scancode / button / hat / scroll 行为

CI 失败 = 字节兼容破 = 立即 block。

---

## 7. 兼容性破坏红线 (Red lines)

下面任何一条被违反,即使 PR review 通过也必须 revert:

1. ❌ 改 `CONTROL_MSG_MAX_SIZE = 1 << 18`。
2. ❌ 改 `UHID_INPUT` payload size 上限 15。
3. ❌ 改 8-slot 上限(改 `GamepadHid` slot 分配逻辑)。
4. ❌ 改 BE 字节序为 LE 或 host order。
5. ❌ 改 22 control_msg 中任何一个 tag 数值。
6. ❌ 改 `is_critical()` 对 UHID_CREATE/DESTROY 的 true 判定。
7. ❌ 改 HID report descriptor(`src/hid/descriptor.rs`)任何字节。
8. ❌ 改 device_msg type tag(0/1/2)的数值。
9. ❌ 改 scrcpy prefix 长度(1 dummy byte + 64 name)。

详见 [`AGENTS.md`](../AGENTS.md) §4.1。

---

## 8. 历史兼容事故

| 时间 | scrcpy 上游 | 本 crate 影响 | 修复 |
| ---- | ----------- | ------------- | ---- |
| (无重大事故) | — | — | — |

本 crate 自 0.1.0 起 byte-exact 与 scrcpy v2.7 对齐,未发生过兼容破坏事故。

如未来发生,在本节加一行,记录:

- scrcpy commit hash
- 本 crate PR 编号
- 破坏的具体字节
- 修复方式
- 受影响的 callers(下游 `handsets` 等)

---

## 9. 相关文档

- 字节布局速查: [`wire-format.md`](wire-format.md)
- AC 验收点: [`../ACCEPTANCE.md`](../ACCEPTANCE.md) §10
- 真机回归记录: [`../ACCEPTANCE.md`](../ACCEPTANCE.md) §7
- 目录规则: [`../AGENTS.md`](../AGENTS.md) §4.1
- 模块分层: [`architecture.md`](architecture.md) §2

最后更新: 2026-06-29。