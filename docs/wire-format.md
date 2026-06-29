# Wire Format — `android-hid-connect`

> 字节级协议参考。所有 layout 与 scrcpy v2.7 C 端 / scrcpy-server Java 端逐字节相同。
>
> 对照代码:`scrcpy/app/src/control_msg.c::sc_control_msg_serialize`、`scrcpy/app/src/device_msg.c`、`scrcpy/app/src/hid/{keyboard,mouse,gamepad}.c`。
>
> 这里给的是 **速查表**;AC 项、断言位置、真机验证见 [`../ACCEPTANCE.md`](../ACCEPTANCE.md) §1-§6。

---

## 1. 控制方向总览

```
[host]                                       [device]
HidClient / HidSession                       scrcpy-server
   │                                              │
   │  control_msg (host → device)                  │
   │  ──── TCP (adb forward) ─────────────────►   │
   │                                              ▼
   │                                       ControlMessageReader
   │                                              │
   │  device_msg (device → host)                  │
   │  ◄──── TCP (same socket) ────────────────    │
   ▼                                       DeviceMessageWriter
```

**注意**:scrcpy v2.7 默认开 tunnel_forward 时,host 端先读一个 dummy byte (`0x00`) + 64 字节 NUL-padded device name,作为 out-of-band 前缀(由 `read_scrcpy_control_prefix` 解析)。prefix 之后才是 control_msg / device_msg 主协议。

---

## 2. Host → Device: 22 control messages

所有 message = 1 byte type tag (BE) + 类型相关 payload。

| Tag | Name | Payload | 总长 | 对应 AC |
| --- | ---- | ------- | ---- | ------ |
| 0 | INJECT_KEYCODE | action(1) + keycode(4 BE) + repeat(4 BE) + metastate(4 BE) | 14B | AC-C1 |
| 1 | INJECT_TEXT | u32 BE len + UTF-8 bytes (>300 字符截断) | 5+N | AC-C2 |
| 2 | INJECT_TOUCH_EVENT | action(1) + pointer_id(8 BE) + x(4 BE) + y(4 BE) + w(2 BE) + h(2 BE) + pressure(2 BE, 0..=0xFFFF) + action_button(4 BE) + buttons(4 BE) | 32B | AC-C3 |
| 3 | INJECT_SCROLL_EVENT | x(4 BE) + y(4 BE) + w(2 BE) + h(2 BE) + hscroll(2 BE, ±1.0→0x7FFF) + vscroll(2 BE) + buttons(4 BE) | 21B | AC-C4 |
| 4 | BACK_OR_SCREEN_ON | action(1) | 2B | AC-C5 |
| 5 | EXPAND_NOTIFICATION_PANEL | (无 payload) | 1B | AC-C6 |
| 6 | EXPAND_SETTINGS_PANEL | (无 payload) | 1B | AC-C7 |
| 7 | COLLAPSE_PANELS | (无 payload) | 1B | AC-C8 |
| 8 | GET_CLIPBOARD | copy_key(1, 0/1/2) | 2B | AC-C9 |
| 9 | SET_CLIPBOARD | sequence(8 BE) + paste(1) + u32 BE len + UTF-8 bytes | 14+N | AC-C10 |
| 10 | SET_DISPLAY_POWER | on(1, 0/1) | 2B | AC-C11 |
| 11 | ROTATE_DEVICE | (无 payload) | 1B | AC-C12 |
| 12 | UHID_CREATE | id(2 BE) + vid(2 BE) + pid(2 BE) + name_len(1, ≤127) + name + rd_size(2 BE) + report_desc bytes | 8+N+2+M | AC-C13 |
| 13 | UHID_INPUT | id(2 BE) + size(2 BE, ≤15) + report bytes (1..=15) | 5+N | AC-C14 |
| 14 | UHID_DESTROY | id(2 BE) | 3B | AC-C15 |
| 15 | OPEN_HARD_KEYBOARD_SETTINGS | (无 payload) | 1B | AC-C16 |
| 16 | START_APP | name_len(1, ≤255) + UTF-8 bytes | 2+N | AC-C17 |
| 17 | RESET_VIDEO | (无 payload) | 1B | AC-C18 |
| 18 | CAMERA_SET_TORCH | on(1, 0/1) | 2B | AC-C19 |
| 19 | CAMERA_ZOOM_IN | (无 payload) | 1B | AC-C20 |
| 20 | CAMERA_ZOOM_OUT | (无 payload) | 1B | AC-C20 |
| 21 | RESIZE_DISPLAY | width(2 BE) + height(2 BE) | 5B | AC-C21 |
| 22 | **AI_CONFIG (extension)** | flags(1) + sample_interval_ms(2 BE) + feature_dim(2 BE) | 6B | AC-C23 |
| 23 | **AI_QUERY (extension)** | since_timestamp_ms(8 BE) | 9B | AC-C24 |
| 24 | **AI_PAUSE (extension)** | (无 payload) | 1B | AC-C25 |

**通用约束**:

- `CONTROL_MSG_MAX_SIZE = 1 << 18` (256 KiB),超出会被服务端 reject(`Error::ControlMessageTooLarge`)。
- `SET_CLIPBOARD` 文本超长会被截断到上限,而不是 reject。
- `UHID_INPUT` 报告 size 上限 15 字节,HID payload 上限(`SC_HID_MAX_SIZE`)。
- `is_critical()` 只对 `UHID_CREATE` / `UHID_DESTROY` 返回 true — 与 scrcpy `sc_control_msg_is_droppable` 对齐。

### 2.1 INJECT_TOUCH_EVENT 字段细节

| 字段 | 取值 | 说明 |
| ---- | ---- | ---- |
| action | `0` DOWN / `1` UP / `2` MOVE / `3` CANCEL / ... | 见 `types::TouchAction` |
| pointer_id | u64 | scrcpy 保留: `POINTER_ID_MOUSE = -1i64` / `POINTER_ID_GENERIC_FINGER = -1i64`(同值)/ `POINTER_ID_VIRTUAL_FINGER = -2i64` |
| x, y | u32 pixel | 相对屏幕坐标 |
| w, h | u16 pixel | 屏幕尺寸元数据 |
| pressure | u16, 0..=0xFFFF | Android `MotionEvent` 压力,0 = 无压力 |
| action_button | u32 | 当前 action 关联的按钮(`MouseButton::Left` = `MotionEvent.BUTTON_PRIMARY` 等)|
| buttons | u32 | 当前按下的按钮位图 |

### 2.2 AI extension flags(`AI_CONFIG.flags` 位图)

```text
bit 0 (0x01): AI_FLAG_KEYFRAMES  - 关键帧检测(scene change)
bit 1 (0x02): AI_FLAG_MOTION     - 运动检测
bit 2 (0x04): AI_FLAG_OBJECTS    - 物体检测(class + bbox + conf)
bit 3 (0x08): AI_FLAG_TEXT       - 文字区域检测
bit 4 (0x10): AI_FLAG_FEATURES   - 视觉特征向量(feature_dim 维)
```

由 `scrcpy-ai-server` 在 on-device 侧实现,本 crate 仅序列化请求 + 解析响应。

---

## 3. Host → Device: HID reports

### 3.1 Keyboard (8 bytes, UHID id = 1)

```text
offset  size  field
0       1     modifier bitmap (bit 0..=7 = LCTRL..=RGUI)
1       1     reserved (must be 0)
2..8    6     up to 6 scancodes (USB HID Usage IDs 0x04..=0x65)
```

- 6KRO (6-key rollover)。
- 按下第 7 个非 modifier 键时,`slots[1..8] = 0x01` (ErrorRollOver) — phantom state。
- Modifier scancodes: `0xE0..=0xE7`(LCTRL..=RGUI) **不计入** 6 个 slot,只占 modifier byte 的对应 bit。
- Scancode 范围校验:仅接受 `0x04..=0x65` 与 `0xE0..=0xE7`,其他返回 `Error::ScancodeOutOfRange`。

### 3.2 Mouse (5 bytes, UHID id = 2)

```text
offset  size  field
0       1     button bitmap (bit 0 = Left, 1 = Right, 2 = Middle, 3..4 = X1/X2; bit 5..7 = padding)
1       1     signed dx [-127, 127]
2       1     signed dy [-127, 127]
3       1     signed vertical wheel (integer delta,累加后才发)
4       1     signed horizontal AC Pan
```

- dx/dy 单字节超界会 clamp 到 `[-127, 127]`,不报错。
- vertical wheel 走 residual accumulator — `MouseHid::scroll(dy)` 内部累加整数部分,只在跨过整数边界时 emit 报告(避免 1/120 滚轮信号被吞)。

### 3.3 Gamepad (15 bytes, UHID id = 3..=10)

```text
offset  size  field
0..2    2     left stick X (u16 LE, i16 → u16 重映射: u = i.max(-32767) as u16 ^ 0x8000)
2..4    2     left stick Y
4..6    2     right stick X
6..8    2     right stick Y
8..10   2     left trigger (u16 LE, 0..=0xFFFF)
10..12  2     right trigger
12..14  2     button bitmap (u16 LE, low 12 bits = 16 buttons,high 4 bits = dpad hat)
14      1     reserved (must be 0)
```

- 8 个 slot: UHID id `3, 4, 5, 6, 7, 8, 9, 10`。
- dpad 在 button bitmap 高 4 bit,内部转换为 hat switch 值 0..=8 (0 = centred)。
- `i16` → `u16` 重映射:`u = (i.max(-32767) as u16) ^ 0x8000`(避免 `-32768` 翻转出错)。

---

## 4. Device → Host: 3 device messages (反向)

**注意**: device_msg 不是统一 envelope,各类型**长度字段不一致**。

### 4.1 DEVICE_MSG_CLIPBOARD (type = 0)

```text
offset  size  field
0       1     type = 0
1..5    4     text_len (u32 BE)
5..     text_len  UTF-8 文本
```

有 `u32` 长度前缀。

### 4.2 DEVICE_MSG_ACK_CLIPBOARD (type = 1)

```text
offset  size  field
0       1     type = 1
1..9    8     sequence (u64 BE)
```

**没有** `u32` payload length 前缀。这与 `scrcpy/app/src/device_msg.c::sc_device_msg_deserialize` 对齐。

### 4.3 DEVICE_MSG_UHID_OUTPUT (type = 2)

```text
offset  size  field
0       1     type = 2
1..3    2     id (u16 BE)
3..5    2     size (u16 BE, ≤ 15)
5..     size  data bytes
```

**没有** `u32` payload length 前缀。size 字段自描述数据长度。

---

## 5. AI extension envelope (device → host)

由 `scrcpy-ai-server` 在 native device_msg 之后追加,解析时走 `device::read_device_event` / `device::DeviceEvent`。

### 5.1 通用 envelope

```text
offset  size  field
0       1     type tag (AI extension 用 type ≥ 0x80)
1..5    4     payload_len (u32 BE)
5..     payload_len  类型相关 payload
```

### 5.2 FrameSummary (type = 0x80)

按 AI extension 通用 envelope 解码后:

```text
offset      size    field
0           8       timestamp_ms (u64 BE)
8           4       frame_seq (u32 BE)
12          2       width (u16 BE)
14          2       height (u16 BE)
16          1       keyframe_flag (u8, 0/1)
17          1       objects_count (u8)
18..        ...      objects[count]:
                          class_id (u16 BE)
                          confidence (u8, 0..=255 → 0.0..=1.0)
                          x_min, y_min, x_max, y_max (u16 BE × 4,normalized 0..=0xFFFF)
                          reserved (u8)
...                     text_regions[variable]:
                          text_len (u16 BE)
                          UTF-8 bytes
...                     feature_vec (feature_dim × f32 LE)
```

具体布局随 `scrcpy-ai-server` 版本演进;本 crate 的 `FrameSummary` 结构跟随 `agent::FrameSummary` typed view,字段新增是 **minor-compatible**。

### 5.3 AiStats (type = 0x81)

```text
offset  size  field
0       8     window_start_ms (u64 BE)
8       8     window_end_ms (u64 BE)
16      4     processed_frames (u32 BE)
20      4     current_fps_milli (u32 BE, ÷1000)
24      4     dropped_frames (u32 BE)
```

---

## 6. scrcpy 控制前缀 (out-of-band)

`adb forward` + `tunnel_forward=true` 时,server 启动后会先发:

```text
offset  size  field
0       1     dummy_byte = 0x00
1..65   64    device_name (NUL-padded UTF-8)
```

解析:`read_scrcpy_control_prefix(reader) -> Result<ScrcpyControlPrefix>`。

```rust
pub struct ScrcpyControlPrefix {
    pub dummy_byte: u8,           // expected 0x00
    pub device_name: String,      // trim trailing NUL
    pub raw_name: [u8; 64],
}
```

之后才是 control_msg / device_msg 主协议。`AgentControlSession::connect_tcp` 自动消费这个 prefix。

---

## 7. Endianness 与字节序

| 字段类型 | 字节序 | 说明 |
| -------- | ------ | ---- |
| u16 / u32 / u64 (control_msg / device_msg) | **BE** (网络字节序)| `to_be_bytes()` |
| u16 (gamepad stick/trigger/button 内部表示)| **LE** | `to_le_bytes()` (与 scrcpy 一致) |
| f32 (feature vector) | **LE** | 单精度浮点 |
| 文本 (UTF-8) | n/a | 无字节序 |

---

## 8. 控制消息 droppable 分类 (AC-X1..AC-X3)

与 scrcpy `sc_control_msg_is_droppable` 对齐:

| 消息类型 | is_critical | 说明 |
| -------- | ----------- | ---- |
| `UHID_CREATE` | `true` | 不能丢,否则设备侧 UHID slot 错乱 |
| `UHID_DESTROY` | `true` | 不能丢,否则设备残留虚拟 HID 设备 |
| 其他 20 种 | `false` | channel 满时被允许丢弃,优先保留 CREATE/DESTROY |

---

## 9. 错误码速查

| 来源 | 错误 | 触发 |
| ---- | ---- | ---- |
| `Error::ScancodeOutOfRange(u16)` | `hid::keyboard` | scancode ∉ [0x04..=0x65] ∪ [0xE0..=0xE7] |
| `Error::ControlMessageTooLarge { size, max }` | `control::serialize_into` | 总长 > 256 KiB |
| `Error::NameTooLong { size }` | `control::UHID_CREATE` 序列化 | name > 127 bytes |
| `Error::ReportDescTooLong { size }` | `control::UHID_CREATE` 序列化 | report_desc > 65535 bytes |
| `Error::UnknownGamepad(u32)` | `hid::gamepad` | slot 未通过 `open()` 注册 |
| `Error::NoGamepadSlot` | `hid::gamepad` | 已开 8 个 slot |
| `Error::BufferFullCritical` | `session/client` | 通道满 + 不可丢消息 |
| `Error::SessionLifecycle(&'static str)` | `session` | open / close / Drop 失败 |
| `Error::AgentTimeout(&'static str)` | `agent` | TCP reader 超时 |
| `Error::MultitouchPointerIdOutOfRange { id, max }` | `multitouch` | pointer_id ∉ [0, MAX_POINTERS) |
| `Error::InvalidStrictText(char)` | `session::type_text_strict` | 字符不在 strict text 集 |
| `Error::Transport(String)` | 任何 IO 失败 | socket / mock 错误 |

---

## 10. 相关文档

- AC 验收点 + 真机回归: [`../ACCEPTANCE.md`](../ACCEPTANCE.md)
- scrcpy 上游契约: [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md)
- AI agent 怎么消费反向流: [`ai-agent-integration.md`](ai-agent-integration.md)
- 模块依赖: [`architecture.md`](architecture.md)

最后更新: 2026-06-29。