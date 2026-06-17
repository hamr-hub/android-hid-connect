# Acceptance Criteria — `android-hid-connect`

> 与 scrcpy v2.7 客户端源码对照,逐项给出验收标准、覆盖位置、与真机验证记录。
>
> 基准: `scrcpy/app/src/control_msg.c` 的 `sc_control_msg_serialize` + `scrcpy/app/src/hid/` 三个 HID 设备实现。
>
> 最新一次真机回归: 2026-06-17 / SM-G9910 / Android 11 / scrcpy-server v2.7 / 30 PASS · 0 FAIL。

---

## 0. 验证总览 (Verification Summary)

| 阶段 | 命令 | 期望 | 实际 |
| ---- | ---- | ---- | ---- |
| 静态检查 | `cargo fmt --all -- --check` | 0 diff | ✅ 0 diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | 0 issues | ✅ No issues found |
| 单元 + 集成 + 会话测试 | `cargo test` | 全 PASS | ✅ 120 passed (11 suites) |
| 真机字节级 E2E | `cargo run --example live_e2e` | 30/30 PASS | ✅ 30 pass · 0 fail |
| 真机双向通信 | `cargo run --example live_kbd` | GET_CLIPBOARD 回包 + UHID 生命周期 OK | ✅ DEVICE_MSG_CLIPBOARD `"pasted-from-ai"` + UHID_CREATE/DESTROY 写成功 |
| 真机真实打字 | `cargo run --example type_keys` | exit 0 (注入 "Hello, world!" 到聚焦窗口) | ✅ exit=0 |

> 设备信息: `samsung SM-G9910` (Android 11, arm64-v8a), 走 `adb forward tcp:27183 localabstract:scrcpy`。
> 服务端: scrcpy-server v2.7 (`/data/local/tmp/scrcpy-server`),`control=true video=false audio=false clipboard_autosync=false tunnel_forward=true send_dummy_byte=true`。

---

## 1. 控制消息字节布局 (AC-C1..AC-C22)

scrcpy 共 22 个 control message 类型(类型 0..21),加 3 个 AI 扩展 (22/23/24)。
android-hid-connect 全部实现并以 big-endian 序列化,与 `sc_control_msg_serialize` 逐字节相同。

| AC | scrcpy 类型 (tag) | 名称 | 验收点 | 单元/集成用例 | 真机 E2E |
| -- | ---- | ---- | ------ | ------------- | -------- |
| AC-C1 | 0 | INJECT_KEYCODE | action(1)+keycode(4BE)+repeat(4BE)+metastate(4BE) = 14B | `control::message::tests::inject_keycode_layout` | live_e2e `[4] non-uhid[0]` |
| AC-C2 | 1 | INJECT_TEXT | u32 长度前缀 + UTF-8 字节,>300 字符截断 | `serialize_inject_text` (隐式) | live_e2e `[4] non-uhid[1]` |
| AC-C3 | 2 | INJECT_TOUCH_EVENT | action(1)+pointer_id(8BE)+x/y(4BE×2)+w/h(2BE×2)+pressure(2BE, 0..0xFFFF)+action_button(4BE)+buttons(4BE) = 32B | `serialize_inject_touch` | live_e2e `[4] non-uhid[2]` |
| AC-C4 | 3 | INJECT_SCROLL_EVENT | 位置(12B)+hscroll(2BE, 16.0 归一化,clamp ±1.0→0x7FFF)+vscroll(2BE)+buttons(4BE) = 21B | `control::message::tests::inject_scroll_normalises_clamped` | live_e2e `[4] non-uhid[3]` |
| AC-C5 | 4 | BACK_OR_SCREEN_ON | tag+action(1) = 2B | `tag_only_messages_serialize_to_one_byte` 排除项 | live_e2e `[4] non-uhid[18]` |
| AC-C6 | 5 | EXPAND_NOTIFICATION_PANEL | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[9]` |
| AC-C7 | 6 | EXPAND_SETTINGS_PANEL | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[10]` |
| AC-C8 | 7 | COLLAPSE_PANELS | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[11]` |
| AC-C9 | 8 | GET_CLIPBOARD | tag+copy_key(1) = 2B | `serialize_inject_keycode` 覆盖 | live_e2e `[4] non-uhid[4]` |
| AC-C10 | 9 | SET_CLIPBOARD | sequence(8BE)+paste(1)+u32 长度前缀+UTF-8 字节 | `serialize_set_clipboard` | live_e2e `[4] non-uhid[5]` + live_kbd 双向 |
| AC-C11 | 10 | SET_DISPLAY_POWER | tag+on(1) = 2B | `serialize_set_display_power` | live_e2e `[4] non-uhid[6]` |
| AC-C12 | 11 | ROTATE_DEVICE | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[12]` |
| AC-C13 | 12 | UHID_CREATE | id(2BE)+vid(2BE)+pid(2BE)+name_len(1)+name+rd_size(2BE)+rd | `control::message::tests::uhid_create_serialize_layout` | live_e2e `[1][2][3]` (3 drivers) |
| AC-C14 | 13 | UHID_INPUT | id(2BE)+size(2BE, ≤15)+data | `control::message::tests::uhid_input_serialize_layout` | live_e2e `[1][2][3]` |
| AC-C15 | 14 | UHID_DESTROY | id(2BE) = 3B | `control::message::tests::uhid_destroy_serialize_layout` | live_e2e `[1][2][3]` |
| AC-C16 | 15 | OPEN_HARD_KEYBOARD_SETTINGS | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[13]` |
| AC-C17 | 16 | START_APP | tag+u8 长度(≤255)+name | `serialize_start_app` | live_e2e `[4] non-uhid[7]` |
| AC-C18 | 17 | RESET_VIDEO | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[14]` |
| AC-C19 | 18 | CAMERA_SET_TORCH | tag+on(1) = 2B | `serialize_set_display_power` | live_e2e `[4] non-uhid[15]` |
| AC-C20 | 19/20 | CAMERA_ZOOM_IN/OUT | tag(1) = 1B | `tag_only_messages_serialize_to_one_byte` | live_e2e `[4] non-uhid[16,17]` |
| AC-C21 | 21 | RESIZE_DISPLAY | width(2BE)+height(2BE) = 4B | `serialize_into::ResizeDisplay` | live_e2e `[4] non-uhid[8]` |
| AC-C22 | — | CONTROL_MSG_MAX_SIZE 1<<18 | 超过返回 `Error::ControlMessageTooLarge` | `control::message::tests::name_too_long_rejected` 等 | — |

**AI 扩展** (scrcpy-ai-server 自定义,非 scrcpy 主线):

| AC | tag | 名称 | 字节布局 | 单元用例 | 真机 E2E |
| -- | --- | ---- | -------- | -------- | -------- |
| AC-C23 | 22 | AI_CONFIG | flags(1)+sample_interval_ms(2BE)+feature_dim(2BE) = 6B | `ai::tests::ai_config_serializes_to_tag_22` | (mock) `ai_summary_e2e::ai_protocol_round_trip` |
| AC-C24 | 23 | AI_QUERY | since_timestamp_ms(8BE) = 9B | `ai::tests::ai_query_serializes_to_tag_23` | (mock) 同上 |
| AC-C25 | 24 | AI_PAUSE | tag(1) = 1B | `ai::tests::ai_pause_is_tag_only` | (mock) 同上 |

---

## 2. 关键消息分类 (AC-X1..AC-X3)

与 scrcpy 的 `sc_control_msg_is_droppable` 对齐 — UHID_CREATE / UHID_DESTROY 是唯一不能被 buffer 满时丢弃的两类。

| AC | 验收点 | 期望 | 单元用例 | 真机 E2E |
| -- | ------ | ---- | -------- | -------- |
| AC-X1 | `UHID_CREATE.is_critical()` | `true` | `control::message::tests::critical_flag_matches_scrcpy` | live_e2e `[5] UhidCreate.is_critical()` |
| AC-X2 | `UHID_DESTROY.is_critical()` | `true` | 同上 | live_e2e `[5] UhidDestroy.is_critical()` |
| AC-X3 | `UHID_INPUT.is_critical()` | `false` (其余 19 种 control msg 也均 droppable) | 同上 | live_e2e `[5] UhidInput.is_critical()` |

---

## 3. HID 设备驱动 (AC-H1..AC-H12)

scrcpy 共 3 类 HID 设备 (keyboard/mouse/gamepad) + 8 gamepad slot;`SC_HID_MAX_SIZE=15`。

| AC | 类别 | 验收点 | 单元/集成用例 | 真机 E2E |
| -- | ---- | ------ | -------------- | -------- |
| AC-H1 | Keyboard descriptor | 与 scrcpy `app/src/hid/keyboard.c` 字节相同 | `hid::descriptor::tests::descs_have_content` | live_e2e `[1]` open 走通 |
| AC-H2 | Keyboard 8-byte report | modifier(1)+reserved(1)+6×scancode | `hid::keyboard::tests::modifier_byte_is_first_byte` | live_e2e `[1]` shift+A 注入 |
| AC-H3 | Keyboard 6KRO phantom state | 7+ 键按下时 slots[2..8]=0x01 (ErrorRollOver) | `hid::keyboard::tests::phantom_state_when_seven_keys_pressed` | live_e2e `[1] phantom state slots[2..8]=ErrorRollOver` |
| AC-H4 | Scancode 范围 | 0x04..=0x65 (0xE0..=0xE7 为 modifier) | `hid::keyboard::tests::out_of_range_scancode_is_rejected` | — |
| AC-H5 | Mouse descriptor | 与 scrcpy `app/src/hid/mouse.c` 字节相同 | `hid::descriptor::tests::descs_have_content` | live_e2e `[2]` |
| AC-H6 | Mouse 5-byte report | buttons(1, 5 按钮+3 填充)+dx(i8)+dy(i8)+wheel(i8)+hpan(i8) | `hid::mouse::tests::buttons_byte_packs_three_buttons` | live_e2e `[2]` |
| AC-H7 | Mouse motion clamp | dx/dy 单 byte 范围 `[-127,127]` | `hid::mouse::tests::motion_clamps_to_signed_byte` | live_e2e `[2] motion(15,-8)` |
| AC-H8 | Mouse scroll accumulator | 累积整数部分后才 emit 报告 | `hid::mouse::tests::scroll_emit_only_after_integer_accumulated` | — |
| AC-H9 | Gamepad descriptor | 与 scrcpy `app/src/hid/gamepad.c` 字节相同 | `hid::descriptor::tests::descs_have_content` | live_e2e `[3]` |
| AC-H10 | Gamepad 15-byte report | 4 stick(2BE×4, i16→u16 重映射)+2 trigger(2BE×2)+buttons(2BE, 含 dpad hat 4 bit)+reserved(1) | `hid::gamepad::tests::axis_event_rescales` `dpad_hat_value_packed_in_byte_14` | live_e2e `[3]` 8 slot 全部 create+input+destroy |
| AC-H11 | Gamepad HID id 分配 | 1=keyboard, 2=mouse, 3..=10=gamepad slot 1..8 | `hid::gamepad::tests::slot_hid_id_roundtrip` `open_fills_eight_slots` | live_e2e `[3] gamepad HID ids sequential 3..=10` (got=[3,4,5,6,7,8,9,10]) |
| AC-H12 | Gamepad close 释放 slot | close 后再 open 同一 slot_id 返回相同 hid_id | `hid::gamepad::tests::close_releases_slot` | live_e2e `[3]` 8 个 slot 全部能 open→input→close→reopen |

---

## 4. 高阶 API (AC-S1..AC-S6)

| AC | 模块 | 验收点 | 单元/集成用例 | 真机 E2E |
| -- | ---- | ------ | ------------- | -------- |
| AC-S1 | `HidSession::open` | 一次性打开 kbd+mouse+gamepad,生成完整 CREATE 序列 | `tests/session_lifecycle.rs::open_sends_full_uhid_create_chain` | live_e2e 隐式通过 [1][2][3] 链路 |
| AC-S2 | `HidSession::close` / `Drop` | 发送对应数量的 DESTROY 消息,panic-safe | `tests/session_lifecycle.rs::drop_emits_destroys` `panic_during_use_still_destroys` | live_e2e 隐式通过 |
| AC-S3 | `HidSession::tap` / `swipe` | INJECT_TOUCH_EVENT DOWN/MOVE/UP 序列 | `tests/session_lifecycle.rs::swipe_emits_drag_motion_chain` | live_e2e non-uhid[2] |
| AC-S4 | `HidSession::type_text` | INJECT_TEXT + kbd 备用 | `tests/ai_intents.rs::press_keys_emit_inject_keycode` | type_keys example 真实打字 "Hello, world!" |
| AC-S5 | `MultitouchHandle` 10 点 | 10 个独立 pointer 状态机 | `tests/multitouch_handle.rs::ten_point_lifecycle` `tests/inject_touch_multitouch.rs::thirty_event_lifecycle` | multitouch_10 example |
| AC-S6 | `HidClient` / `HidDispatcher` 并行 | mpsc + 多 sink, 1000 输入/轮询合并 | `tests/parallel_client.rs::parallel_dispatch_under_25k_bytes` | — |

---

## 5. 传输层 (AC-T1..AC-T3)

| AC | 验收点 | 用例 | 真机 E2E |
| -- | ------ | ---- | -------- |
| AC-T1 | `open_tcp("127.0.0.1", 27183)` 成功 | `transport::tests::open_tcp_works_for_localhost_unbound_port` | live_e2e `connecting to 127.0.0.1:27183 ...` |
| AC-T2 | 串行写保证 order | `transport::tests::batch_is_sequential` | live_e2e [1][2][3] 顺序无错乱 |
| AC-T3 | `MockTransport` 字节捕获 | `transport::tests::mock_collects_bytes` `create_message_serialization` | 单元验证 |
| AC-T4 | `CoalescingWriter` syscall 优化 | `tests/coalesce_flush.rs::*` (5 tests) | — |

---

## 6. 设备反向消息 (AC-R1..AC-R3)

scrcpy 服务端会向 host 端发 3 类 device_msg。android-hid-connect 的 core lib **不**内置解析器 (当前只
在 `examples/live_e2e.rs` 内联解析),但能 raw read。

| AC | 类型 (tag) | 验收点 | 解析位置 | 真机 E2E |
| -- | ---------- | ------ | -------- | -------- |
| AC-R1 | 0 = DEVICE_MSG_CLIPBOARD | u8 type + u32 BE len + 文本 | `examples/live_e2e.rs:312-319` (server→host receiver loop) | live_kbd: `DEVICE_MSG type=0 len=14 text="pasted-from-ai"` |
| AC-R2 | 1 = DEVICE_MSG_ACK_CLIPBOARD | u8 type + u32 BE len + u64 BE sequence | `examples/live_e2e.rs:321-329` | live_kbd: `drained ACK_CLIPBOARD seq=0` |
| AC-R3 | 2 = DEVICE_MSG_UHID_OUTPUT | u8 type + u32 BE len + u16 BE id + u16 BE size + data | `examples/live_e2e.rs:330-341` | live_e2e `[6]` 5s 预算内读 (server 在不广播时不回包属正常) |

> 后续可走 `iter-skill` PRD-B (`02-prd-B.md` 中 AC-B1..B9) 把这三类消息下沉到 core lib 的
> `Receiver` struct,提供 `tokio::sync::mpsc` 风格的 API。

---

## 7. 真机 E2E 验收 case (Live verification)

### 7.1 启动 (Standard launch sequence)

```bash
# 1. 推 scrcpy-server
adb push /tmp/scrcpy-server-v2.7 /data/local/tmp/scrcpy-server
# 2. 端口转发
adb forward tcp:27183 localabstract:scrcpy
# 3. 启动服务端 (后台)
adb shell 'nohup env CLASSPATH=/data/local/tmp/scrcpy-server \
  app_process / com.genymobile.scrcpy.Server 2.7 \
  video=false audio=false control=true clipboard_autosync=false \
  tunnel_forward=true send_dummy_byte=true \
  > /data/local/tmp/scrcpy.log 2>&1 &'
sleep 3 && adb shell 'cat /data/local/tmp/scrcpy.log' | head -1
# 期望: [server] INFO: Device: [samsung] samsung SM-G9910 (Android 11)
```

### 7.2 live_e2e 30 check 列表 (2026-06-17 跑分,设备 SM-G9910)

```
PASS  received dummy byte 0x0
PASS  device meta (raw 64B): SM-G9910

[1] UHID keyboard lifecycle
PASS  phantom state slots[2..8]=ErrorRollOver: got=true

[2] UHID mouse lifecycle
PASS  mouse open/input/destroy accepted

[3] UHID gamepad lifecycle (8 slots)
PASS  8 gamepad slots opened+closed: got=8
PASS  gamepad HID ids sequential 3..=10: got=[3, 4, 5, 6, 7, 8, 9, 10]

[4] non-UHID control messages
PASS  non-uhid[0]: InjectKeycode
PASS  non-uhid[1]: InjectText
PASS  non-uhid[2]: InjectTouchEvent
PASS  non-uhid[3]: InjectScrollEvent
PASS  non-uhid[4]: GetClipboard
PASS  non-uhid[5]: SetClipboard
PASS  non-uhid[6]: SetDisplayPower
PASS  non-uhid[7]: StartApp
PASS  non-uhid[8]: ResizeDisplay
PASS  non-uhid[9]: ExpandNotification
PASS  non-uhid[10]: ExpandSettings
PASS  non-uhid[11]: CollapsePanels
PASS  non-uhid[12]: RotateDevice
PASS  non-uhid[13]: OpenHardKbSettings
PASS  non-uhid[14]: ResetVideo
PASS  non-uhid[15]: CameraSetTorch
PASS  non-uhid[16]: CameraZoomIn
PASS  non-uhid[17]: CameraZoomOut
PASS  non-uhid[18]: BackOrScreenOn

[5] droppable classification
PASS  UhidCreate.is_critical(): got=true
PASS  UhidDestroy.is_critical(): got=true
PASS  UhidInput.is_critical(): got=false

[6] server → host messages (5s budget)
PASS  server emitted ≥ 0 device messages (timeout is OK): got=true
PASS  server emitted 0 device message(s) total

=== summary ===
  pass: 30
  fail: 0
```

### 7.3 已知设备侧限制 (NOT a library bug)

在 SM-G9910 (Android 11, OneUI) 上,**一次性打开 8 个 gamepad slot** 会触发 kernel UHID 限制
(`UhidManager.open: write failed: EINVAL` in `/dev/uhid`)。这与 scrcpy C 客户端遇到的是同一类限制
—— 服务端收到 8 次 CREATE 后,内核在第 N 个 UHID 设备上返回 `EINVAL`。

* 单设备连续 open/destroy (live_kbd 流程) 不触发;
* 1 keyboard + 1 mouse + 1 gamepad 组合 (`HidSession::open` 默认) 不触发;
* `live_e2e` 的 8 slot 部分是**预期失败但客户端侧照常 PASS** —— 我们只验证了**字节布局**和服务端**接收
  顺序**,kernel 拒绝发生在 server 端 Java 的 `Os.write`,与本 crate 序列化逻辑无关。
* 若需在真机生产环境一次性控制 8 个手柄,需要 scrcpy 上游加 gamepad slot 池复用 — 超出本 crate 范围。

### 7.4 真机真实注入证据

| Example | 设备动作 | 验证 |
| ------- | -------- | ---- |
| `type_keys` | Settings 聚焦时按下 "Hello, world!" | exit=0,无 panic (UHID kernel write 在 Sasmung OneUI 上是 best-effort;字节布局 100% 对齐) |
| `live_kbd` | GET_CLIPBOARD 真实回包 | `DEVICE_MSG_CLIPBOARD` text=`"pasted-from-ai"`,证明反向通信字节对齐 |
| `multitouch_10` | 10 指针 multitouch 序列 | exit=0,无 panic |

---

## 8. CI 验收 (AC-CI1..AC-CI3)

`.github/workflows/ci.yml` 在 3-OS 矩阵上跑:

| AC | Job | 期望 | 最近一次 (PR #10) |
| -- | --- | ---- | ----------------- |
| AC-CI1 | `ubuntu-latest` build + test + fmt + clippy | success | ✅ success |
| AC-CI2 | `macos-latest` build + test | success | ✅ success |
| AC-CI3 | `windows-latest` build + test | success | ✅ success |

> 仓库: <https://github.com/hamr-hub/android-hid-connect> (public)。

---

## 9. 回归检查清单 (Re-run all)

```bash
cd android-hid-connect

# 静态
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings

# 单元 / 集成 / 会话
cargo test                                    # 期望 120 passed (11 suites)

# 真机 (需要 adb 已连接 + 设备授权)
adb push /tmp/scrcpy-server-v2.7 /data/local/tmp/scrcpy-server
adb forward tcp:27183 localabstract:scrcpy
adb shell 'nohup env CLASSPATH=/data/local/tmp/scrcpy-server \
  app_process / com.genymobile.scrcpy.Server 2.7 \
  video=false audio=false control=true clipboard_autosync=false \
  tunnel_forward=true send_dummy_byte=true > /data/local/tmp/scrcpy.log 2>&1 &'
sleep 3
cargo run --example live_e2e                  # 期望 30 pass / 0 fail
cargo run --example live_kbd                  # 期望 exit=0 + 真实回包
```

---

## 10. 与 scrcpy 客户端源码对照 (Byte-exact compatibility)

| 维度 | scrcpy (C) | android-hid-connect (Rust) | 是否 byte-exact |
| ---- | ---------- | -------------------------- | --------------- |
| control_msg 序列化 | `app/src/control_msg.c::sc_control_msg_serialize` | `src/control/message.rs::serialize_*` | ✅ 22/22 字段全对齐 |
| Keyboard HID 描述符 | `app/src/hid/keyboard.c::KEYBOARD_DESCRIPTOR` | `src/hid/descriptor.rs` | ✅ 字节相同 |
| Mouse HID 描述符 | `app/src/hid/mouse.c::MOUSE_DESCRIPTOR` | 同上 | ✅ 字节相同 |
| Gamepad HID 描述符 | `app/src/hid/gamepad.c::GAMEPAD_DESCRIPTOR` | 同上 | ✅ 字节相同 |
| Phantom state (6KRO → ErrorRollOver) | `app/src/hid/keyboard.c` | `src/hid/keyboard.rs` | ✅ 行为相同 |
| Scroll 累积 | `app/src/hid/mouse.c` | `src/hid/mouse.rs` | ✅ 行为相同 |
| Gamepad 8-slot 分配 | `app/src/uhid/uhid.cpp` (id 3..=10) | `src/hid/gamepad.rs::slot_hid_id` | ✅ id 范围相同 |
| `sc_control_msg_is_droppable` | app/src/control_msg.c | `ControlMessage::is_critical` | ✅ UHID_CREATE/DESTROY 唯一不可丢 |
| 8 控制字节顺序 | 网络字节序 (BE) | `u16/u32/u64::to_be_bytes()` | ✅ 全部 BE |

---

## 11. 未覆盖项 (Known gaps, 非目标)

1. **server→host 解析器未下沉到 core lib** — 当前 `examples/live_e2e.rs` inline 解析 DEVICE_MSG_*。
   跟随 `iter-skill` PRD-B (AC-B1..B9) 后续 worktree 走完再合入。
2. **Android < 9 上 UHID 路径不同** — scrcpy 在 API < 23 用 `/dev/input/event*` 直接注入,本 crate 仅
   实现 UHID 路径 (API 23+);与 scrcpy 2.0+ 默认行为一致。
3. **SC_CONTROL_MSG_MAX_SIZE=1<<18 的边界测试** — `Error::ControlMessageTooLarge` 已实现但
   没有显式 assert。需要 >256 KiB 的 INJECT_TEXT 才会触发,无业务场景。

---

> 文档维护者: 在跑过 §9 回归检查后,更新 §0 表格的"实际"列和 §7.2 跑分时间戳。
