# `android-hid-connect` vs `handsets` — 多维度对比

> 本仓库内的 `android-hid-connect` 与同级目录的 `handsets` 都在做"驱动 Android"这件事,
> 但处于完全不同的抽象层、解决不同的问题。本文按维度拆解两者的设计、原理、协议、
> 输入输出能力、并发模型、权限边界、可嵌入性、测试强度,最后给出选型建议。
>
> 数据采集时间: 2026-06-29 · 对照源码: `../android-hid-connect` 与 `../handsets`。

---

## 1. 一句话定位

| 项目 | 定位 | 一句话总结 |
| --- | --- | --- |
| **android-hid-connect** | 底层 Rust 库,字节级移植 scrcpy 的 UHID/控制协议 | 把 scrcpy 客户端的 C 代码用 Rust 重写,并加 AI 扩展 |
| **handsets** | 完整 CLI 套件 + 自带设备端守护进程 + 多种语言绑定 | 一行命令跑通 Android 自动化全链路 |

`handsets` 是"工具",`android-hid-connect` 是"零件"。

---

## 2. 形态与代码体量

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 形态 | 1 个 crate(`android_hid_connect` lib) | 3 个 crate + Java daemon:`handsets-cli` / `handsets-tui` / `handsets-viewer` + `hs.jar` |
| 代码体量 | ~13,203 行 Rust,26 文件 | ~10,972 行 Rust + 数千行 Java(35 个 .java) |
| License | MIT OR Apache-2.0 | MIT |
| 依赖 | 默认 `thiserror` + `std`;可选 `tokio` | `handsets-cli` **零三方依赖**(纯 std);TUI 走 ratatui+crossterm;Viewer 走 winit+Metal+VideoToolbox(macOS) |
| Rust 版本 | `rust-version = "1.87"` | `edition = "2021"` |
| 发布形态 | `cargo add android-hid-connect` 即可作为 Rust 库嵌入 | `curl … \| bash` 安装 CLI + Python `pip install handsets` |

---

## 3. 架构与原理

### 3.1 android-hid-connect

```
[你的 agent]─── mpsc / HidClient ───► [HidSession] ── TCP ──► adb forward tcp:27183 ──► [scrcpy-server] ──► Android InputManager / UHID 设备
                                              │
                                              └── CoalescingWriter (1ms 桶)
```

**核心原理**: 复用 scrcpy-server(`com.genymobile.scrcpy.Server`)作为 on-device 守护端。这个 server 在 Android 上以 shell UID 通过 `app_process` 启动,接收 22 种控制消息 + 3 种 AI 扩展消息(类型 22/23/24)。其中三种 UHID 消息(`UHID_CREATE/UHID_INPUT/UHID_DESTROY`)会让 Android 内核合成一个虚拟 USB 键盘/鼠标/手柄,后续的 HID 报告会被 InputDispatcher 当成真硬件派发到聚焦窗口。

**关键模块**:

- `hid::KeyboardHid/MouseHid/GamepadHid` — 三种 HID 设备的 report builder,descriptor 与 scrcpy C 端字节相同
- `control::ControlMessage` — 22 种 scrcpy 控制消息的序列化器,BE 字节序
- `session::HidSession` — 一键开 kbd+mouse+gamepad,Drop 时 panic-safe 关闭
- `client::HidClient` — `std::sync::mpsc` 多生产者 + 单 dispatcher 线程,4096 bounded channel
- `coalesce::CoalescingWriter` — 1ms 桶合并 syscall,优化 bursty 写入
- `agent::AgentControlSession` — 给 LLM agent 用的高层门面:plan/action/typed helpers/批处理/frame summary 等待
- `multitouch::MultitouchHandle` — 10 点独立 pointer 状态机

### 3.2 handsets

```
[hs CLI / Python SDK / 任何 subprocess]──► TCP:9008 ──► adb forward ──► [hs.jar (Java daemon, shell UID)]
                                                                                │
                                                                                ├─ UiAutomation  ──► getRootInActiveWindow / injectInputEvent
                                                                                ├─ IAccessibilityService 树 dump
                                                                                ├─ 反射 → IPackageManager / IActivityManager / IWindowManager / ISettings
                                                                                ├─ IContentProvider (via getContentProviderExternal)
                                                                                ├─ SurfaceFlinger screencap / H.264 streamer
                                                                                ├─ Push-cached state mirror (a11y + lifecycle)
                                                                                └─ WaitRegistry (wait_for_idle / text / activity)
```

**核心原理**: 自己写了一个 Java daemon(`hs.jar`,R8 压缩,minSdk 28),通过 `app_process` 以 shell UID 启动(`disableHiddenApiRestrictions` 关掉 hidden API 限制,反射访问 `IActivityManager`、`IPackageManager`、`ServiceManager` 等)。宿主侧用 `adb forward tcp:9008 localabstract:hsd` 转发。

**关键模块**:

- `Server.java` — 长度前缀二进制帧分发器,70+ verb
- `Dumper.java` + `Traverse.java` — 无障碍树 dump,输出 JSON
- `Input.java` — `UiAutomation.injectInputEvent` 注入 MotionEvent / KeyEvent(16ms tap hold,60Hz MOVE)
- `Screenshot.java` — screencap → JPEG/WebP/PNG;`H264Streamer.java` 推 H.264 流
- `Binders.java` — 反射拿到 system Context 和 binder 服务
- `Pm/Am/Wm/SettingsDirect/Providers/Location/Notifications/...` — 各种系统服务封装
- `NodeActions.java` — AccessibilityNodeInfo 上的 click/long_click/set_text/scroll
- `State.java` + `UiEvents.java` + `WaitRegistry.java` — 事件驱动状态镜像 + 等待原语
- 宿主侧 `handsets-cli` 纯 std(零三方依赖)、`handsets-tui` 是 ratatui+crossterm、`handsets-viewer` 是 winit+Metal+VideoToolbox

---

## 4. 通信协议对比

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 协议 | **scrcpy 原生字节协议** — 1 字节 type tag + 类型相关 payload(全部 BE) | **自研长度前缀二进制**:`[u32 BE len][payload]`,命令是 ASCII verb + k=v |
| 设备侧守护 | scrcpy-server(v2.7,需自带) | hs.jar(R8 压缩后约几百 KB,自研) |
| 端口 | 27183(`adb forward tcp:27183 localabstract:scrcpy`) | 9008(`adb forward tcp:9008 localabstract:hsd`) |
| 设备侧 IO 模型 | 1 socket 控制 + 1 socket 视频(分开);UHID 走控制 socket | 1 socket,流式响应(`ERR:` 错误码 + `[len=0]` 结束符) |
| 字节序 | 全部 BE | 长度前缀 BE,payload 内 ASCII |
| 命令条数 | 25 (22+3 AI) | 70+ (tap/swipe/dump/screenshot/stream/pm_*/am_*/settings_*/clip_*/sms/calls/calendar/contacts/install/pull/push/shell/monitor/logcat/dumpsys/state/...) |

---

## 5. 输入输出能力矩阵

| 能力 | android-hid-connect | handsets |
| --- | --- | --- |
| 触摸 / 滑动 | ✅ `INJECT_TOUCH_EVENT`(完整 DOWN/MOVE/UP/CANCEL) | ✅ `tap`/`swipe`/`swipe_dir`/`scroll`/`down`/`move`/`up`(用 UiAutomation,16ms hold) |
| 多点触控 | ✅ 10 点 MultitouchHandle + HidClient batcher | ❌(未提及,UiAutomation 路径) |
| 键盘(USB HID) | ✅ 完整 8 字节 report,6KRO + phantom state,scancode 校验,6 键 chord | ✅ `key NAME` / `key code=N` / `text STRING`(通过 KeyCharacterMap) |
| UHID Mouse(相对) | ✅ 5 字节 report,scroll residual accumulator | ❌(走 UiAutomation 的绝对坐标路径) |
| UHID Gamepad | ✅ 8 个 slot,15 字节 report,4 stick + 2 trigger + 16 按钮 + hat | ❌ |
| Android KeyEvent | ✅ `INJECT_KEYCODE` typed helpers(typed `AndroidKeyAction`/`AndroidKeycode`) | ✅ `key` verb,`text` verb |
| 剪贴板 | ✅ GET/SET + ACK 序列匹配 + reader | ✅ `clip_get`/`clip_set`/`clip_watch`(流式变更推送) |
| 屏幕开关/音量/电源 | ✅ SET_DISPLAY_POWER / ROTATE / BACK_OR_SCREEN_ON | ✅ 类似的 verb |
| 启动应用 | ✅ START_APP | ✅ `am_start n=COMPONENT [a=ACTION] [d=DATA] [f=FLAGS]` |
| 截图 | ❌(要走 scrcpy-server 视频流,本库不处理) | ✅ `screenshot size=N q=N fmt=jpeg\|webp\|png max=1 secure_check=1` |
| 视频流 | ❌ | ✅ `stream` / `stream_h264` / `stream_tilejpeg` + `keyframe` 强制 IDR |
| 无障碍树 dump | ❌ | ✅ `dump` / `dump_active` 输出 JSON 树 |
| CSS 选择器 | ❌ | ✅ `[a=v]` `[a~=sub]` `:visible :clickable :has-text("x") :near(SEL, PX) :below() :right-of() :in() :text-is() :focused :checked` |
| 节点动作 | ❌ | ✅ `node_click` / `node_long_click` / `node_set_text` / `node_scroll` / `node_focus` / `submit` |
| 事件驱动等待 | ❌(需自己 poll) | ✅ `wait_for_idle` / `wait_for_text` / `wait_for_activity`(事件触发,非轮询) |
| 包管理 | ❌ | ✅ `pm_list` / `pm_path` / `pm_uninstall` / `pm_grant` / `pm_revoke` / `install` / `install_multi`(streaming)/ `deeplinks`(从二进制 manifest 解析) |
| ContentProvider | ❌ | ✅ `sms` / `calls` / `contacts` / `calendar`(走 `getContentProviderExternal`,shell UID 读权限) |
| 通知 | ❌ | ✅ `notifications`(含 FLAG_SECURE 检测) |
| 文件 | ❌ | ✅ `pull` / `push` 流式块 |
| Dumpsys / Logcat | ❌ | ✅ 流式返回 |
| Shell exec | ❌ | ✅ `shell ARGV…` + `__exit__ N` trailer |
| 状态镜像 | ❌ | ✅ push-cached 状态:`state interactive\|battery_level\|battery_charging\|top\|procs\|device` + `state_watch` 流 |
| Settings | 仅 SET_DISPLAY_POWER / ROTATE | ✅ `settings_get/put NS KEY VALUE`(NS ∈ system/secure/global) |
| Props | ❌ | ✅ `getprop`/`setprop` |
| AI frame summary | ✅ AI extension tag 22/23/24,`DeviceEvent::FrameSummary`,`LatestFrameSummaryReceiver`,typed `AgentTargetSelector`,`AgentRect` 锚点 | ❌(但有 H.264 视频流可自己跑视觉) |

---

## 6. 并发、延迟、吞吐

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 并发模型 | `HidClient`:`std::sync::mpsc` 多生产者 + 单 dispatcher 线程,bounded 4096 | 短命令每调用开新 socket;`hs run`/`hs act` 内部用 warm socket,默认 channel TTL 短 |
| 输入批处理 | **强**:`TouchFrameBatcher`(24 帧 fixed stack) / `KeyboardFrameBatcher`(32) / `AndroidKeyFrameBatcher`(32) / `MouseFrameBatcher`(32) / `ScrollFrameBatcher`(32) / `GamepadFrameBatcher`(32 + vector 大批量);`PackedGamepadFrameBatcher` 走无分配 hot path | 无(`tap` 就是一条 verb) |
| 系统调用合并 | `CoalescingWriter` 1ms 桶 + hard_limit,burst 写入合并成单次 `write_all` | 无 |
| 专用低延迟 gamepad 路径 | `OpenRequest::gamepad_only_realtime()` coalescing=0,直接写到 socket | 无 |
| Plan 预检 | `AgentPlanSummary::analyze`、`bounded_try_queue_prefix`、`plan_summary` 离线估算 dispatcher command 压力 | 无 |
| 同步原语 | `flush_wait`(checked barrier,回传前序 command error)/ `try_flush_wait` / `close_wait` / `close_checked` | 无显式 barrier |
| 延迟数据 | bench:`uhid_throughput` criterion 套件 | benchmark.md:`ping` 1.61ms p50,`wm_info` 1.34ms p50,`state top` 2.54ms p50,`screenshot size=768` 8.02ms p50,`dump_active` 4.58ms p50 — vs `adb shell` 27–2105ms |
| LLM 友好度 | 高(`AgentAction` typed plan + 计划摘要 + 阻塞/非阻塞区分) | 高(`hs --json` 一行一对象,Python SDK,`Session` 缓存) |

---

## 7. 设备侧依赖与权限模型

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 设备端依赖 | **scrcpy-server** jar(scrcpy 自带,大约几 MB),`adb push` 到 `/data/local/tmp` | **hs.jar**(自研,R8 压缩,minSdk 28) |
| 运行身份 | scrcpy-server:shell UID(需 `--no-control --no-video --no-audio` 让 server 仅监听) | hs.jar:shell UID,`disableHiddenApiRestrictions` 关 hidden API |
| 输入来源 | `INJECT_TOUCH_EVENT`(MotionEvent)+ `UHID_CREATE`(合成 USB 设备,InputDispatcher 当真硬件处理) | `UiAutomation.injectInputEvent`(Espresso 同款,shell UID 注入) |
| 无障碍树访问 | 无 | `UiAutomation.getRootInActiveWindow` / `getWindows` |
| Hidden API | 不需要 | **大量反射**:`ServiceManager.getService`、`IActivityManager$Stub.asInterface`、`IActivityTaskManager.startActivityAsUser`、`getContentProviderExternal` |
| 已知坑 | UHID 8 slot 在某些 Samsung OneUI 上 kernel 报 EINVAL(同 scrcpy C 端一样) | 见 `../handsets/docs/sharp-edges.md`:Settings provider 拒绝 app_process 身份,所以走 `getContentProviderExternal`;`am start` 必须显式带 `callingPackage="com.android.shell"` |

---

## 8. 跨语言、可嵌入性、可观察性

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 用作库 | ✅ 直接 `cargo add android-hid-connect`,Rust API | ✅ Rust crate 内嵌 + Python SDK + 任何语言 subprocess 解析 `hs --json` |
| 用作 CLI | ❌(库不是二进制) | ✅ `hs` 单文件 std 二进制,`-z + fat-lto + strip` 编译 |
| 跨语言 | Rust crate;或者自己写协议客户端 | Python(`pip install handsets`)+ `hs --json` 一行一对象给任何语言 |
| 可观察性 | `LatestFrameSummaryReceiver`、`DeviceMessageReceiver`、`AgentPlanSummary` 离线估算、`wait_for_frame_summary_after_seq/timestamp` | `state_watch` 流、`hs show` 读 `~/.handsets/state-<port>.json` |
| 错误码 | `Error::AgentTimeout`、`Error::ControlMessageTooLarge` 等 typed | 退出码:`0/1/2/3/4` (ok/failure/NOT_FOUND/TIMEOUT/AMBIGUOUS),`--json` 携带 `error.code` |

---

## 9. 测试与验收强度

| 维度 | android-hid-connect | handsets |
| --- | --- | --- |
| 单元 + 集成 | **405 个测试**(11 suite),`tokio` feature **416** 个 | 内部模块测试(未在文档里给数字) |
| 字节级兼容 | 与 scrcpy C 客户端**逐字节**对齐(scrcpy v2.7 全部 22 种 control message + 3 AI + 3 device_msg + HID descriptor);`ACCEPTANCE.md` 列出 AC-C1..AC-C22 / AC-H1..AC-H12 / AC-S1..AC-S7 / AC-T1..AC-T4 / AC-R1..AC-R8 | 自有协议 |
| 真机回归 | **2026-06-18 SM-G9910 Android 11 scrcpy-server v2.7**:30/30 PASS `live_e2e` + `live_kbd` 双向 + `type_keys` 真实打字 + `multitouch_10` | bench 数据 + adb 对比表(可比性优先,正确性测试未量化) |
| CI | 3-OS 矩阵(ubuntu/macos/windows)+ clippy + fmt | 未公开 |
| 字节回归历史 | `ACCEPTANCE.md` §12 列出 2026-06-18 当天修了 5 个 flaky/clippy 测试 bug | — |

---

## 10. 抽象层级金字塔

如果用"金字塔"表示控制粒度:

```
            ┌───────────────────────────────────────┐
   顶层     │  LLM agent 循环(选择器/语义/事件等待)   │   handsets 全部覆盖
            ├───────────────────────────────────────┤
   中层     │  复合手势 / 多设备扇出 / 视觉锚点      │   handsets / android-hid-connect 各覆盖一半
            ├───────────────────────────────────────┤
   底层     │  HID 报告 / 触摸事件 / 帧 dispatch     │   android-hid-connect 全覆盖;handsets 走 UiAutomation 抽象
            └───────────────────────────────────────┘
```

---

## 11. 选型建议

这两个项目解决的问题不一样,**不存在"全方位更好"的赢家**,具体看你的用例。

### 11.1 选 **handsets** 的场景(大多数 LLM agent / shell 自动化)

- 你的核心是 **"找到控件 → 触发 → 等待 UI 响应"** 的循环,而不是像素坐标的精确注入
- 你需要 **看见屏幕**(截图、H.264 流)再决定下一步 — handsets 自带
- 你需要 **语义选择器**(`EditText[hint~=Email]`),不想硬编码坐标
- 你需要 **事件驱动等待**(`wait_for_text "Welcome"`)而不是轮询 sleep
- 你需要 **包管理 / 权限授予 / ContentProvider 读 SMS 通话记录 / 剪贴板 / 文件 push-pull** 这一整套
- 你的 agent 用 **Python / Node / Go** 等任何非 Rust 语言
- 你想 **curl install** 一行装好就开工,不想自己编译 scrcpy-server

**优势**: 端到端延迟 1–8ms(对 LLM 循环够用了,benchmark 数据公开可复现)、零三方 Rust 依赖、不需要装 APK、不需要 root、单 jar 就位。

### 11.2 选 **android-hid-connect** 的场景(精确 HID 注入 / 游戏 / 极限延迟)

- 你要做 **USB HID 手柄** 控制(8 个 slot、15 字节 report、hat switch)— handsets 没这条路
- 你要 **多点触控**(10 个 pointer 独立状态机)— handsets 没有显式支持
- 你需要 **60/120/240Hz 游戏循环** 最低延迟(`OpenRequest::gamepad_only_realtime()` + `set_frame_raw_packed_batch` + 无 `Vec` 分配的 hot path)
- 你要 **键盘宏 / 快捷键 / 6 键 chord**,需要 USB HID 真实上报
- 你已经有 **scrcpy-server 在跑**(比如视频流场景),想复用同一个 server,而不是再起一个 daemon
- 你需要 **AI 帧摘要**(类型 22/23/24 扩展消息)— 配套 scrcpy-ai-server,有 `AgentTargetSelector`、`AgentRect`、`LatestFrameSummaryReceiver`
- 你需要 **byte-exact** 与 scrcpy C 客户端互操作(比如作为 scrcpy 的 Rust 重写,做贡献)
- 你在写 **Rust agent**(不想再开 subprocess 解析 JSON)— 直接 `cargo add` 即可

**优势**: 字节级与 scrcpy 对齐、405+ 测试、30 个真机 E2E 项、HID 通路无替代、并行 mpsc + 多种 fixed-stack batcher + coalescing writer 给低延迟游戏控制用的全套优化、typed agent plan + 计划预检 + checked barrier。

---

## 12. 综合结论

| | 通用 agent 自动化(找按钮 / 读屏幕 / 装包 / 装 APK) | 极限 HID 注入(游戏 / 键盘宏 / 多点触控 / scrcpy 互操作) |
|---|---|---|
| **首选** | **handsets** | **android-hid-connect** |
| **数据依据** | 端到端 1–8 ms、a11y dump + 选择器、事件等待、自带 daemon | 405 测试 + 30 真机 E2E、8 gamepad slot、mpsc + batcher + coalescing |
| **替代方案** | uiautomator2 / Appium / 原生 `adb shell input` | 自己移植 scrcpy C / 写 ndk 走 `/dev/uhid` |

**总结一句话**: **handsets 是"工具",android-hid-connect 是"零件"**。

- 想"今天下午就要让 agent 能点 Android 屏幕,还要看得到点了什么"→ 装 handsets,一行 curl。
- 想"做一个跑在设备上的 LLM 操控中间层,要能像 scrcpy 一样发 UHID 报告,要 240Hz 手柄不掉帧"→ 用 android-hid-connect 做内核,然后考虑把 handsets 当可选 a11y 上层叠上去(a11y dump 给视觉,android-hid-connect 发 HID)。

这两个项目其实是**互补**而非竞争:理想架构里,android-hid-connect 提供 HID / 触摸底座 + AI 帧摘要;handsets 提供 a11y 树 dump + 选择器 + 截图 + 包管理 + 系统服务反射。两者都用 adb 转发 + 长度/类型前缀的紧凑协议,都是 Rust 优先 + 跨语言友好,都把延迟优化到毫秒级 — 只是分工不同。

---

## 附录 A. 关键文件清单(供进一步对照阅读)

### A.1 android-hid-connect

```
android-hid-connect/
├── Cargo.toml                      # crate 描述;仅 thiserror + 可选 tokio
├── README.md                       # 协议 + 高层 API
├── ACCEPTANCE.md                   # AC-C/H/S/T/R 130+ 验收点 + 2026-06-18 真机回归
├── src/
│   ├── lib.rs                      # 顶层模块导出
│   ├── hid/{keyboard,mouse,gamepad,descriptor}.rs
│   ├── control/message.rs          # 22 control msg + 3 AI msg 序列化
│   ├── session.rs                  # HidSession(panic-safe lifecycle)
│   ├── client.rs                   # HidClient(mpsc + 多 batcher)
│   ├── coalesce.rs                 # CoalescingWriter(1ms 桶)
│   ├── agent.rs                    # AgentControlSession / AgentAction / AgentPlanSummary
│   ├── multitouch.rs               # 10 pointer state machine
│   ├── device.rs / async_device.rs # scrcpy 反向 device_msg 解析
│   └── transport.rs                # open_tcp + MockTransport
├── examples/                       # type_keys / live_e2e / live_kbd / multitouch_10 / ai_*
├── tests/                          # session_lifecycle / coalesce_flush / ai_intents / ai_summary
└── benches/uhid_throughput.rs      # criterion 套件
```

### A.2 handsets

```
handsets/
├── README.md                       # 卖点 + benchmark 表
├── Cargo workspace:
│   ├── handsets-cli/               # 主 CLI,纯 std
│   ├── handsets-tui/               # ratatui+crossterm 交互式
│   └── handsets-viewer/            # macOS Metal+VideoToolbox GUI 镜像
├── bindings/python/                # pip install handsets
├── build.sh                        # javac → R8 → d8 → jar
├── install.sh                      # curl 装到 ~/.handsets
├── src/dev/handsets/daemon/        # 35 个 .java
│   ├── Main.java                   # disableHiddenApiRestrictions + UiAutomation 启动
│   ├── Server.java                 # 70+ verb 分发
│   ├── Dumper.java + Traverse.java # 无障碍树 dump
│   ├── Input.java                  # UiAutomation.injectInputEvent
│   ├── Screenshot.java + H264Streamer.java + TileStreamer.java
│   ├── Binders.java                # 反射拿 system context / binder 服务
│   ├── Pm.java + Am.java + Wm.java + SettingsApi.java + SettingsDirect.java
│   ├── Providers.java              # SMS / calls / contacts / calendar
│   ├── NodeActions.java            # AccessibilityNodeInfo 操作
│   ├── State.java + UiEvents.java + WaitRegistry.java
│   ├── Clipboard.java + Notifications.java + Location.java
│   └── Lifecycle.java + Dumpsys.java + Logcat.java + ShellExec.java
└── docs/
    ├── wire.md                     # 完整协议参考
    ├── architecture.md             # 架构图
    ├── benchmark.md                # 延迟数据
    ├── sharp-edges.md              # 已知坑
    └── cookbook.md                 # 实战食谱
```

---

## 附录 B. 参考文献

- scrcpy 官方: <https://github.com/Genymobile/scrcpy>
- handsets 仓库: <https://github.com/elliotgao2/handsets>
- android-hid-connect 仓库: <https://github.com/hamr-hub/android-hid-connect>
- Android UiAutomation: <https://developer.android.com/reference/android/app/UiAutomation>
- Android UHID / kernel input subsystem: `drivers/hid/uhid.c` (AOSP kernel)