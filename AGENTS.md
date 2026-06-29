# AGENTS.md

> 协作约定 — 给在这个仓库里写代码、改代码、提 PR 的人和 AI agent 看。
>
> 核心精神: **byte-exact 与 scrcpy 对齐、零隐式 I/O、最小依赖、panic-safe 生命周期**。任何破坏这四条的改动都需要先在 PR 描述里解释清楚,再合入。

---

## 1. 项目定位 (在动手前先读完)

`android-hid-connect` 是一个 **Rust 库 crate**(不是 CLI,不是 service),目的是把 scrcpy 客户端 C 源码中控制协议相关部分 — 22 种 control message + 3 种 AI 扩展 + 3 种 HID 设备驱动 — 用 Rust 重写一遍。

四件事不能破:

| # | 不可破 | 由谁保证 |
| - | ------ | -------- |
| 1 | **byte-exact** 与 scrcpy v2.7 C 端 / scrcpy-server Java 端对齐 | `ACCEPTANCE.md` §10 + `tests/` 字节级断言 |
| 2 | **零隐式 I/O** — `hid`/`control`/`types` 必须是纯函数 | 模块边界(见 §2.2) |
| 3 | **最小依赖** — 默认只有 `thiserror`;`tokio` feature 才引入 `tokio` | `Cargo.toml` + CI |
| 4 | **panic-safe 生命周期** — `HidSession::Drop` 必发 UHID_DESTROY | `tests/session_lifecycle.rs::panic_during_use_still_destroys` |

---

## 2. 目录规则 (Directory rules)

### 2.1 顶层布局

```
android-hid-connect/
├── Cargo.toml                  # crate 描述;依赖必须最小
├── README.md                   # 协议概览 + 高级 API 演示 + 真实 E2E 接入步骤
├── ACCEPTANCE.md               # AC-C/H/S/T/R 验收点 + 真机回归记录 + 历史 bug
├── AGENTS.md                   # ← 本文件
├── CHANGELOG.md                # keep-a-changelog 格式,release-please 会读
├── LICENSE*                    # MIT OR Apache-2.0
├── src/                        # 库源码(见 §2.2)
├── examples/                   # 可运行示例,演示单一能力点(见 §2.3)
├── tests/                      # 集成测试(见 §2.4)
├── benches/                    # criterion 基准(见 §2.5)
├── docs/                       # 专题文档,见 docs/INDEX.md
│   ├── INDEX.md
│   ├── architecture.md
│   ├── wire-format.md
│   ├── scrcpy-protocol-compatibility.md
│   ├── ai-agent-integration.md
│   ├── development.md
│   └── comparison-with-handsets.md   # 已有,跨项目对比
└── .github/
    ├── workflows/ci.yml        # 3-OS 矩阵 + MSRV
    ├── dependabot.yml          # weekly cargo deps
    └── release-please.yml      # 自动 release
```

新增顶层目录前先在 PR 里说明 — 当前结构是有意的。

### 2.2 `src/` 模块边界 (重要)

```
src/
├── lib.rs              # 顶层 re-export;新类型按模块就近 re-export,不要全平铺到 root
├── hid/                # 纯函数:键盘/鼠标/手柄 HID 报告 + descriptor
│   ├── descriptor.rs   # 三种 HID 报告描述符(与 scrcpy 字节相同)
│   ├── keyboard.rs     # 6KRO + phantom state
│   ├── mouse.rs        # 5-byte report + scroll residual accumulator
│   └── gamepad.rs      # 8 slot,15-byte report,stick/trigger/button/hat
├── control/            # 纯函数:22 control msg + 3 AI 扩展 序列化
│   ├── message.rs      # sc_control_msg_serialize 对齐
│   └── mod.rs
├── session.rs          # HidSession 生命周期(panic-safe Drop,UHID_DESTROY)
├── client.rs           # HidClient(mpsc 多生产者 + 1 dispatcher)+ 各种 fixed-stack batcher
├── agent/              # AgentControlSession — 给 LLM 用的高层 facade
│   ├── mod.rs          # 入口 + 公开类型聚合
│   ├── session.rs      # 同步 session(Read+Write 传输)
│   ├── session_tcp.rs  # TCP session + read timeout 恢复
│   ├── action.rs       # AgentAction typed plan 枚举
│   ├── types.rs        # AgentPoint/AgentRect/AgentTouchFrame/AgentScrollFrame/...
│   ├── geometry.rs     # 坐标系转换(basis-point → pixel)
│   ├── estimator.rs    # AgentPlanSummary / AgentPlanBoundedPrefix
│   └── tests.rs        # 模块内单元测试
├── device.rs           # 同步 device→host reverse parser(3 native msg + AI envelope)
├── async_device.rs     # #[cfg(feature = "tokio")] 异步 adapter,与同步版字节语义一致
├── transport/          # open_tcp + MockTransport + send_one/send_batch
│   ├── mod.rs
├── multitouch.rs       # MultitouchHandle(10 pointer 状态机)
├── coalesce.rs         # CoalescingWriter(1ms 桶 syscall 合并)
├── ai/mod.rs           # AI 扩展 enum + typed flags
├── types.rs            # public typed 常量(AndroidKeycode/Scancode/Modifiers/TouchPointerId/...)
└── error.rs            # Error enum(thiserror)
```

#### 模块分层(谁可以依赖谁)

```
            ┌─────────────────────────────────────────────┐
   顶层     │  agent/                                     │  高层 facade
            ├─────────────────────────────────────────────┤
   中层     │  session / client / device / async_device / │  有 I/O / 有生命周期
            │  multitouch / coalesce / transport          │
            ├─────────────────────────────────────────────┤
   底层     │  control / hid / ai / types / error         │  纯函数 + 数据
            └─────────────────────────────────────────────┘
```

**规则**:

- **底层模块只能依赖底层模块**。`hid`/`control`/`ai`/`types`/`error` 之间可以互引,但**绝不**引用 `session`/`client`/`agent`/`device`/`async_device`/`transport`/`multitouch`/`coalesce`。
- **中层可以引用底层**,但**不互相反向引用**。例如 `session` 用 `hid`+`control`,`client` 用 `hid`+`control`+`coalesce`,`agent` 用 `client`+`session`+`device`+`types`。
- `device` 和 `async_device` 是**兄弟模块**,不互相依赖;async 版在 feature gate 下编译。
- `lib.rs` 的 `pub use` 列表是 **公开 API 契约**,新增类型时**默认**就 re-export,放到对应模块聚合(`agent::*` / `client::*` / `hid::*`),不要把内部细节漏到 root。

#### 纯度约束 (重要)

- `hid/`、`control/`、`ai/`、`types/` 这四个模块**不许出现**:
  - `std::net` / `tokio::net` / 任何 socket 类型
  - `std::fs` / 文件操作
  - `std::process` / 子进程
  - `std::time::Instant::now()` / 系统时间读取(`Duration` 参数可以)
  - `std::sync::mpsc` channel / Mutex / async runtime
- 单元测试可以读 `Instant::now()` 来测耗时,但生产代码路径必须 100% 可由输入 → 输出的纯函数 + 显式 `Write` 构成。
- 违反此规则的 PR 一律打回。

### 2.3 `examples/` 规则

- 每个 example **只演示一件事**(单 responsibility),文件名用 snake_case + 下划线动词。
- 必须 `cargo run --example NAME` 能直接跑,不能依赖外部文件 (除了 `./scrcpy-server` jar,运行前由文档提示)。
- 需要真机的 example 在文件顶部用 `//!` 写清楚:
  - 设备前置条件 (`adb push`、`adb forward`、scrcpy-server v2.7 启动命令)
  - 期望 stdout / exit code
  - 已知限制(参见 `ACCEPTANCE.md` §7.3 Samsung OneUI 8-slot UHID 限制)
- 不需要真机的 example 必须能在 CI(ubuntu)上跑通,或加 `#[cfg(...)]` 跳过。
- 已有的 example 不删,改名要连带更新 `README.md` 和 `docs/ai-agent-integration.md` 里的引用。

### 2.4 `tests/` 规则

集成测试目录,**只放跨模块协作**的测试。纯函数行为测试留在各模块的 `#[cfg(test)] mod tests`。

| 文件 | 范围 |
| ---- | ---- |
| `integration.rs` | TCP + control_msg + HID 端到端字节级 |
| `session_lifecycle.rs` | HidSession open / close / Drop / panic-safe |
| `parallel_client.rs` | HidClient 多生产者 + batcher + back-pressure |
| `coalesce_flush.rs` | CoalescingWriter 1ms 桶合并 |
| `multitouch_handle.rs` | 10 pointer 状态机 |
| `inject_touch_multitouch.rs` | INJECT_TOUCH_EVENT wire + multitouch |
| `ai_intents.rs` | AI 扩展 + Agent 高层 helper |
| `ai_summary.rs` | FrameSummary 解析 |
| `ai_summary_e2e.rs` | mock server 全链路 round-trip |

- 测试名用 snake_case,动词在前(`open_sends_full_uhid_create_chain`)。
- 字节级断言用 `count_uhid_inputs(&[u8])` 这类按消息边界解析的 helper,**不要** `bytes.iter().filter(|b| **b == TAG_UHID_INPUT).count()`(会被 descriptor 里的 `0x0D` 干扰,见 `ACCEPTANCE.md` §12.4)。
- 需要真机的测试不要写在 `tests/`,写到 `examples/live_*.rs` 里跑命令验证,然后把结果记到 `ACCEPTANCE.md` §7。

### 2.5 `benches/` 规则

- criterion bench,`harness = false`。
- bench case 名字简短,带稳定数据规模(`*_512`、`*_packed_batch_32`)。
- 任何 bench 改动都要在 PR 里贴 before/after 数字,目标是 1ms 桶游戏控制循环不退化。
- 不要为了优化改 hid/control 的字节布局 — 那是 scrcpy 兼容性锁死的。

### 2.6 `docs/` 规则

- 每个文档一个专题,文件名小写 + 连字符。
- 必有的文件:`INDEX.md`(目录索引,新文档必须更新)、`architecture.md`、`wire-format.md`、`scrcpy-protocol-compatibility.md`、`ai-agent-integration.md`、`development.md`。
- 跨项目对比(`comparison-with-handsets.md`)单独保留,不属于本 crate 内部。
- 文档代码示例用 ```rust,no_run 或 ```text,不要在文档里跑真实命令的输出。
- 文档里的数字(测试数、版本号、跑分日期)**要可复现**:改了测试数或 scrcpy 版本号,立刻同步更新所有引用它的文档。

---

## 3. 允许的事 (Allowed)

### 3.1 代码层

- ✅ 在 `hid` / `control` / `ai` / `types` 加**新的纯函数**,只要返回类型还是 `ControlMessage` / `HidReport` / typed enum。
- ✅ 在 `session` / `client` / `agent` 加**新的 facade helper**,语义清晰、不重复现有 API。
- ✅ 在 `device` / `async_device` 加新的 device_msg 类型解析,**前提是**和 scrcpy-server 的 `DeviceMessageWriter.java` 字节对齐。
- ✅ 加新 example,演示新能力;在 `README.md` 末尾的 example 列表里登记。
- ✅ 加新 typed 常量到 `types.rs`,并在 `hid`/`control` 里使用,避免 caller 写裸字面量。
- ✅ 修 bug / 重构内部实现,**保持公开 API 与 byte-exact 兼容**。
- ✅ 优化 batcher / dispatcher,前提是公开 API 不变且测试不退化。
- ✅ 加 `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]` 等常见 trait 给现有类型。
- ✅ 在公开 API 加 `#[must_use]` 提示(仅在不破坏现有 caller 的前提下)。
- ✅ 升级 dev-dep / 修 typo / 重命名 internal-only 符号。

### 3.2 文档层

- ✅ 修 typo、改表述、补一个例子、更新表格里的数字。
- ✅ 加新的 `docs/*.md`,同步更新 `docs/INDEX.md` 和 `README.md` 文档索引。
- ✅ 在 `ACCEPTANCE.md` 跑过真机 E2E 后填 §7 跑分时间戳和设备信息。
- ✅ 在 `CHANGELOG.md` 加 unreleased 段,描述变更(格式见该文件)。

### 3.3 工具链

- ✅ `cargo fmt --all`
- ✅ `cargo clippy --all-targets -- -D warnings`
- ✅ `cargo test` 和 `cargo test --features tokio`
- ✅ `cargo bench --bench uhid_throughput`(只在改 hot path 后跑)
- ✅ `cargo doc --no-deps`(公开 API 文档)

---

## 4. 不允许的事 (Forbidden)

### 4.1 协议层 (硬规则)

- ❌ **修改任何 control_msg / device_msg / HID report 的字节布局**。本 crate 是 scrcpy 的 Rust 重写,字节兼容是核心契约。
  - 例外:加 scrcpy 上游**新引入**的 message 类型时,先在 PR 里贴 scrcpy C 端源码,然后在 `tests/` 加字节对齐断言,再在 `ACCEPTANCE.md` §1 / §6 加 AC 项。
- ❌ 改 `src/control/message.rs` 的 BE 字节序为 LE 或 host order。
- ❌ 改 `src/hid/descriptor.rs` 的三种 HID report descriptor(任何字节改动都破坏 InputDispatcher 行为)。
- ❌ 改 `src/types.rs` 里 `AndroidKeycode` / `Scancode` 等常量的数值(它们的值是 Android framework / USB HID 规范锁死的)。
- ❌ 把 `src/control/message.rs` 的 `CONTROL_MSG_MAX_SIZE = 1<<18` 改小或改大(scrcpy-server 用同样的 cap)。
- ❌ 把 gamepad slot 上限从 8 改成别的数字(scrcpy `SC_GAMEPAD_MAX = 8`,server 端也 hard-code 8)。
- ❌ 在 `Cargo.toml` 加新的必选 dep — 想加 dep 就走 feature gate。

### 4.2 模块层 (硬规则)

- ❌ `hid` / `control` / `ai` / `types` 出现任何形式的 I/O(网络/文件/进程/系统时间)。
- ❌ `device.rs` 和 `async_device.rs` 互相 `use`(它们是并列实现,不是父子关系)。
- ❌ 在 `agent/` 模块外出现 `Agent*` 公开类型(保持 facade 单点入口)。
- ❌ 在 `lib.rs` re-export 任何 `pub(crate)` 或 internal 类型。

### 4.3 测试层

- ❌ 删测试用例 — 要么改、要么 `#[ignore]` + 说明、要么换成等价更强的断言。
- ❌ 把单元测试搬到 `tests/`、又把 `tests/` 集成测试搬到单元测试里 — 两者职责不同。
- ❌ 用 `unwrap()` 吞掉真机 E2E 错误(`examples/live_*.rs` 的错误必须打印 + 退出码非 0)。
- ❌ 用 `bytes.iter().filter(|b| **b == 13).count()` 数 UHID_INPUT(见 §2.4 提醒)。
- ❌ 把需要真机的断言塞到 CI 跑的 `tests/`(CI 没有 device)。

### 4.4 依赖 / 工具链

- ❌ 加必选 dep 到 `[dependencies]`(只能走 `[dev-dependencies]` 或 `optional = true` + feature)。
- ❌ 把 MSRV 升上去而不更新 `Cargo.toml` 的 `rust-version` 字段 + CI 的 `msrv` job + `ACCEPTANCE.md` 里的注释。
- ❌ 把 MSRV 降下去(目前 `rust-version = "1.87"`,降到 1.78/1.81 都会破坏 `clap_lex 1.1.0` 的 edition2024 或 `Integer::is_multiple_of`)。
- ❌ 引入 `unsafe`(本 crate 全部 safe Rust)。
- ❌ `#[allow(clippy::...)]` 不写原因注释。
- ❌ `cargo update` 不带 rationale — `Cargo.lock` 是被 release-please 用的。

### 4.5 仓库卫生

- ❌ 提交 `target/`、`*.swp`、`.DS_Store`、`scrcpy-server` jar、`adb` 设备日志。
- ❌ 提交调试 print `println!("DEBUG: ...")`(用 `tracing` 或 test fixture 替代)。
- ❌ 在公开 API 加 `pub fn foo() -> ()`(unit return 用 `pub fn foo()`)。
- ❌ 把 git config 的 email/name 留成别人的(每个 contributor 用自己的)。
- ❌ force-push 到 `main`(`release-please` + dependabot 依赖 linear history)。

---

## 5. 提 PR 前自检清单 (Pre-PR checklist)

逐条打勾,任何一条挂了都不要合:

- [ ] `cargo fmt --all -- --check` 0 diff
- [ ] `cargo clippy --all-targets -- -D warnings` 0 issue
- [ ] `cargo test` 全 PASS(标准测试套件)
- [ ] `cargo test --features tokio` 全 PASS(async 套件)
- [ ] 改动了字节布局 → `ACCEPTANCE.md` §10 同步更新对比表
- [ ] 改动了模块边界 → `AGENTS.md` §2.2 同步更新依赖图
- [ ] 改了公开 API → `README.md` / `docs/INDEX.md` / `docs/architecture.md` 同步更新
- [ ] 跑了真机 E2E → `ACCEPTANCE.md` §7 加新跑分记录 + 时间戳
- [ ] 加了新 example → `README.md` 末尾 example 列表 + `docs/INDEX.md` 同步
- [ ] 升了 MSRV → `Cargo.toml` + `.github/workflows/ci.yml` + `ACCEPTANCE.md` 同步
- [ ] `CHANGELOG.md` 加了 Unreleased 段(如果改动可见给下游用户)

---

## 6. 不在范围内 (Out of scope,不要扩散)

下面这些功能**不属于**本 crate,即使看起来"很自然"也不要加:

- ❌ 截图 / 录屏 / 视频解码(走 scrcpy 视频流,本 crate 只控不显)
- ❌ a11y 树 dump / 节点选择器 / 事件等待(`handsets` 做这事)
- ❌ 视频 AI 帧摘要推理(`scrcpy-ai-server` 做这事,本 crate 只定义 typed message)
- ❌ 包管理 / 权限授予 / ContentProvider(走 `handsets`)
- ❌ CLI / TUI / GUI viewer(本 crate 是 lib,不是 binary)
- ❌ Android 端 daemon / APK(scrcpy-server 是上游的事)
- ❌ Python / Node / Go SDK(留给下游 binding crate)
- ❌ 真机测试的硬件驱动 / 设备发现 / adb 库封装(只走 `transport::open_tcp`)

---

## 7. 行为契约(给所有 AI agent 的 meta-rule)

> 本节是给 **AI agent** 看的,人类 contributor 可以略过。

1. **不要在 `src/` 里跑 `adb` / `scrcpy` / 真机命令**。真机 E2E 写在 `examples/live_*.rs`,由人手动跑。
2. **不要改 protocol 字节布局**,即使有 PR review 通过也不行 — 走 RFC 流程,先在 issue 里讨论 → 加 ACCEPTANCE 项 → 加测试 → 改实现。
3. **不要"顺手重构"**别人刚合入的代码。`iter-skill` / `code-review` skill 是干这个的,但需要先看到失败的测试或具体的 issue,不是单纯的审美。
4. **不要新增 dep** 来解决"用 std 写 20 行就行"的问题。本 crate 故意依赖少。
5. **不要静默吞错**。`unwrap_or_default()` / `let _ = ...` 在 `hid` / `control` 模块里是 PR-rejected 信号。
6. **不要写"AI 风格注释"**(`// This function does X ////`)。本 crate 注释密度低,一句话能讲清楚就一句话。
7. **不要扩展 README** 超过 1500 行 — 长内容迁到 `docs/*.md`,README 只放 protocol 概览 + 入门示例。
8. **保存重要决策** 到 ICM(`decisions-android-hid-connect` topic),不写到 CLAUDE.md(那是全局的)。

---

## 8. 相关文档索引

- 协议概览:`README.md`
- 验收标准 + 真机回归:`ACCEPTANCE.md`
- 架构图 / 模块依赖:`docs/architecture.md`
- 字节级 wire format 参考:`docs/wire-format.md`
- 与 scrcpy 上游的版本契约:`docs/scrcpy-protocol-compatibility.md`
- LLM / agent 怎么用本 crate:`docs/ai-agent-integration.md`
- 开发循环 / 跑分 / CI:`docs/development.md`
- 跨项目对比(本 crate vs `handsets`):`docs/comparison-with-handsets.md`
- 全部文档导航:`docs/INDEX.md`
- 变更日志:`CHANGELOG.md`

---

最后更新: 2026-06-29 · 与 `Cargo.toml` version 0.1.0 / MSRV 1.87 / 405 测试(标准)/ 416 测试(tokio)同步。