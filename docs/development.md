# Development — `android-hid-connect`

> 本地开发循环 + 真机 E2E 步骤 + CI 矩阵 + 跑分对比。
>
> 提 PR 前自检见 [`../AGENTS.md`](../AGENTS.md) §5。

---

## 1. 前置依赖

| 工具 | 版本 | 用途 |
| ---- | ---- | ---- |
| Rust toolchain | stable + 1.87 (MSRV) | 编译 + 测试 |
| `cargo` | 自带 rustup | 构建 |
| `cargo-fmt` | 自带 | `cargo fmt` |
| `cargo-clippy` | `rustup component add clippy` | lint |
| `adb` | Android Platform Tools | 真机 E2E |
| Android device 或 emulator | API 23+ | 真机测试 |
| scrcpy-server v2.7 jar | 来自 [scrcpy release](https://github.com/Genymobile/scrcpy/releases) | 控制 socket 服务端 |

不需要的:

- ❌ Android SDK / NDK(本 crate 不编译 native)
- ❌ Java(不构建 scrcpy-server)
- ❌ `scrcpy` CLI(只用 server jar,不用 GUI)

---

## 2. 日常开发循环

### 2.1 一次性 setup

```bash
# clone
git clone https://github.com/hamr-hub/android-hid-connect.git
cd android-hid-connect

# 安装 stable + MSRV
rustup install stable
rustup install 1.87

# 装组件
rustup component add rustfmt clippy

# 装 criterion (可选,跑 bench 用)
# criterion 在 dev-dep,首次 build 自动拉
```

### 2.2 改代码 → 验证(本地)

```bash
# 1. 格式化
cargo fmt --all

# 2. Lint (CI 同命令)
cargo clippy --all-targets -- -D warnings

# 3. 单元 + 集成测试(标准)
cargo test

# 4. 单元 + 集成测试(tokio)
cargo test --features tokio

# 5. 跑全部 examples (不需要真机)
cargo run --example type_keys --no-connect   # 如果支持 no-connect 选项
# 或在 PC 上跑:examples/live_*.rs 需要真机,见 §3
```

### 2.3 改字节布局前必看

改 `src/control/message.rs` / `src/hid/` / `src/device.rs` 之前:

1. 读 [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md) §2 byte-exact 契约
2. 读 [`../ACCEPTANCE.md`](../ACCEPTANCE.md) §10 对照表
3. 改之前先加测试断言(测试先 fail),再改实现(测试变绿)
4. 不改字节布局 → 加 typed helper、batcher、facade,公开 API 不动

### 2.4 改公开 API

`src/lib.rs` 的 `pub use` 列表是 **公开 API 契约**。增删 re-export 视为 breaking change:

- 增 → minor version(release-please 自动 bump)
- 删 / 改签名 → major version(major bump 需要 issue 讨论)
- 加新变体到 enum → minor(release-please 自动)

加新公开类型时:

1. 在对应模块 `mod.rs` 加 `pub use ...::*;` 聚合
2. 在 `lib.rs` 加到对应 re-export 块
3. 在 `README.md` 模块表格加一行
4. 在 `docs/architecture.md` §6 扩展点加一行
5. 在 `CHANGELOG.md` Unreleased 段写一行

---

## 3. 真机 E2E 步骤

### 3.1 设备前置

```bash
# 1. 连接设备
adb devices   # 期望:R5CR70SRPSD  device

# 2. 推 scrcpy-server
adb push /tmp/scrcpy-server-v2.7 /data/local/tmp/scrcpy-server

# 3. 端口转发
adb forward tcp:27183 localabstract:scrcpy

# 4. 启动 server (后台)
adb shell 'nohup env CLASSPATH=/data/local/tmp/scrcpy-server \
  app_process / com.genymobile.scrcpy.Server 2.7 \
  video=false audio=false control=true clipboard_autosync=false \
  tunnel_forward=true send_dummy_byte=true \
  > /data/local/tmp/scrcpy.log 2>&1 &'

# 5. 验证启动
sleep 3 && adb shell 'cat /data/local/tmp/scrcpy.log' | head -1
# 期望:[server] INFO: Device: [samsung] samsung SM-G9910 (Android 11)
```

### 3.2 跑测试 example

```bash
# 30 项字节级 E2E
cargo run --example live_e2e
# 期望:pass: 30 / fail: 0

# 双向通信 (GET_CLIPBOARD 真实回包 + UHID lifecycle)
cargo run --example live_kbd
# 期望:DEVICE_MSG_CLIPBOARD text=... + DESTROY 写成功

# 真实打字
cargo run --example type_keys
# 期望:exit=0,无 panic;Settings 聚焦时注入 "Hello, world!"

# 10 指针 multitouch
cargo run --example multitouch_10
# 期望:exit=0,无 panic

# AI frame summary 演示 (需要 scrcpy-ai-server 部署)
cargo run --example ai_summary_demo
```

### 3.3 跑分

```bash
cargo bench --bench uhid_throughput
```

bench case 见 `README.md` "Benchmarks" 段。改 hot path 前先跑一次存档,改完再跑对比。

### 3.4 已知设备 caveat

**Samsung OneUI (SM-G9910) 8-slot UHID EINVAL**:

8 个 gamepad slot 一次性打开会触发 kernel UHID 限制(`UhidManager.open: write failed: EINVAL`)。**这是 kernel 限制,不是本 crate bug**。

workaround:
- 默认 `HidSession::open` 用 `1 kbd + 1 mouse + 1 gamepad` 不触发
- 单设备连续 open/destroy (`live_kbd` 流程) 不触发
- 生产场景需要 8 个手柄,等 scrcpy 上游加 slot 池复用

详细见 [`../ACCEPTANCE.md`](../ACCEPTANCE.md) §7.3。

---

## 4. CI 矩阵

`.github/workflows/ci.yml` 跑:

| Job | OS | 跑什么 | 期望时长 |
| --- | -- | ------ | -------- |
| `linux / stable` | ubuntu-latest | fmt + clippy + build + test | ~3-5 min |
| `test (macos / stable)` | macos-latest | build + test | ~5-8 min |
| `test (windows / stable)` | windows-latest | build + test | ~5-8 min |
| `MSRV (1.87 / ubuntu)` | ubuntu-latest | build only | ~2-3 min |

CI 不跑真机,真机回归由人在 `examples/live_*.rs` 跑后写入 `ACCEPTANCE.md` §7。

### 4.1 MSRV 跟踪

`Cargo.toml` `rust-version = "1.87"` 是最低支持 Rust 版本。三个原因:

1. `Cargo.lock` 是 format v4 → 需要 Cargo ≥ 1.78
2. dev-dep `clap_lex 1.1.0` 声明 `edition = "2024"` → 需要 Cargo ≥ 1.81
3. `examples/ai_summary_demo.rs` 用 `Integer::is_multiple_of` → 需要 Rust ≥ 1.87

**升 MSRV** 必须三处同步:

- `Cargo.toml` `rust-version`
- `.github/workflows/ci.yml` `msrv` job 的 `dtolnay/rust-toolchain@1.87`
- `ACCEPTANCE.md` 注释说明

---

## 5. 测试组织

| 类型 | 位置 | 跑法 |
| ---- | ---- | ---- |
| 单元测试 | 各模块 `#[cfg(test)] mod tests` | `cargo test` |
| 集成测试 | `tests/*.rs` | `cargo test` |
| 异步测试 | `tests/` + `src/async_device.rs`(feature gate) | `cargo test --features tokio` |
| Doc test | 源码 `///` 注释里的 `rust,no_run` 块 | `cargo test --doc` |
| Bench | `benches/uhid_throughput.rs` | `cargo bench` |
| 真机 E2E | `examples/live_*.rs` | 手动 + `ACCEPTANCE.md` 记录 |

### 5.1 命名规范

- 测试函数 snake_case,动词在前:`open_sends_full_uhid_create_chain`。
- 模块内单元测试一个 `#[cfg(test)] mod tests`,不要拆成多个。
- 集成测试按场景拆文件,不按被测模块拆。

### 5.2 字节级断言 helper

```rust
// tests/common/mod.rs(如果未来抽出来)
fn count_uhid_inputs(bytes: &[u8]) -> usize {
    bytes.windows(3)
        .filter(|w| w[0] == TAG_UHID_INPUT)  // 注意:这里也容易被误判,见下
        .count()
}
```

更稳的写法(见 `ACCEPTANCE.md` §12.4):

```rust
fn count_uhid_inputs(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            TAG_UHID_INPUT => {
                // skip id(2) + size(2)
                i += 5;
                if i < bytes.len() {
                    let size = u16::from_be_bytes([bytes[i-2], bytes[i-1]]);
                    i += size as usize;
                    count += 1;
                }
            }
            TAG_UHID_CREATE => { /* parse id+vid+pid+name+rd, skip */ }
            TAG_UHID_DESTROY => { i += 3; }
            _ => { i += 1; }
        }
    }
    count
}
```

按消息边界解析,**不** `bytes.iter().filter(|b| **b == 13).count()`(descriptor 里的 `0x0D` 会误判)。

---

## 6. 依赖管理

### 6.1 原则

- 默认零三方:只有 `thiserror` 必选。
- `tokio` 走 feature gate(`optional = true` + `tokio = ["dep:tokio"]`)。
- 其他 dep 加到 `dev-dependencies`,不进 `[dependencies]`。

### 6.2 升 dep

```bash
# 1. 手动改 Cargo.toml 或
cargo update -p <crate>

# 2. 重跑测试
cargo test
cargo test --features tokio

# 3. 跑 fmt + clippy
cargo fmt --all
cargo clippy --all-targets -- -D warnings

# 4. 提交
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): bump <crate>"
```

dependabot 每周自动开 PR,review + merge 即可,无需手动升。

### 6.3 加新 dep 流程

1. **issue 讨论** — 真的需要吗?std 写 20 行能不能解决?
2. **Cargo.toml 加 `[dependencies]`** OR `[dev-dependencies]` OR `optional = true` + feature
3. **CHANGELOG.md** 写一行
4. **AGENTS.md §2.2** 检查模块边界有没有破坏
5. **PR 描述** 解释为什么不能走 std

---

## 7. 跑分基准 (Bench)

```bash
cargo bench --bench uhid_throughput
```

| Bench | 关注场景 |
| ----- | -------- |
| `keyboard inject_key (no I/O)` | hid::keyboard 内部成本基线 |
| `uhid_input serialize` | control::serialize 成本 |
| `send_one into MockTransport` | 端到端序列化 + 写 mock |
| `gamepad frame pack` | 单帧 packed gamepad 成本 |
| `session set_frame_raw_* 512` | 大批量帧吞吐(coalesce on / off)|
| `client send_frame_unchecked` | dispatcher 单帧开销 |
| `client gamepad frame batcher unchecked 32` | fixed-stack batcher hot path |

改 hid/control/client 后**必跑**,对比 before/after,贴在 PR 描述里。

---

## 8. 调试技巧

### 8.1 看 device_msg 实际字节

```rust
use android_hid_connect::transport::{open_tcp, send_one};
use std::io::Read;

let mut sock = open_tcp("127.0.0.1", 27183)?;
let mut buf = vec![0u8; 4096];
let n = sock.read(&mut buf)?;
println!("got {} bytes: {:02x?}", n, &buf[..n]);
```

### 8.2 Mock transport 单步调试

`transport::MockTransport` 累积所有写入,可以在 unit test 里 `assert_eq!`:

```rust
let mut mock = MockTransport::new();
send_one(&mut mock, &msg)?;
assert_eq!(mock.bytes(), vec![12, 0, 1, ...]);  // UHID_CREATE 字节
```

### 8.3 plan preflight 离线估算

```rust
let actions = vec![/* huge plan */];
let summary = AgentPlanSummary::analyze(&actions);
println!("commands ~= {}", summary.estimated_run_dispatch_commands);
println!("blocking prefix = {}", summary.blocking_timing_prefix_len);
println!("structural error: {:?}", summary.structural_error_index);
```

不 dispatch,纯函数,可以在 CI 单测里覆盖。

---

## 9. 提 PR 前 checklist (摘自 AGENTS.md §5)

- [ ] `cargo fmt --all -- --check` 0 diff
- [ ] `cargo clippy --all-targets -- -D warnings` 0 issue
- [ ] `cargo test` 全 PASS
- [ ] `cargo test --features tokio` 全 PASS
- [ ] 字节布局改动 → `ACCEPTANCE.md` §10 同步
- [ ] 模块边界改动 → `AGENTS.md` §2.2 同步
- [ ] 公开 API 改动 → README + docs/INDEX.md + docs/architecture.md 同步
- [ ] 真机 E2E → `ACCEPTANCE.md` §7 加跑分记录
- [ ] 新 example → README 末尾 + docs/INDEX.md 同步
- [ ] 升 MSRV → Cargo.toml + CI + ACCEPTANCE.md 同步
- [ ] `CHANGELOG.md` Unreleased 段更新

---

## 10. 调试 SSH 真机 vs CI 假数据

- **真机 E2E**:`adb devices` 列出的 device,走 `examples/live_*.rs`。**只** 记录到 `ACCEPTANCE.md` §7。
- **CI 假数据**:所有 `tests/*.rs` 都用 `MockTransport` 或本地 `TcpStream::bind` + `connect`,**不**依赖真机。
- **写新 helper**:先在 `tests/` 加假数据用例(纯函数),再在 `examples/` 加真机用例(可选)。

---

## 11. 相关文档

- 目录规则 + 允许/禁止: [`../AGENTS.md`](../AGENTS.md)
- 验收点 + 真机回归: [`../ACCEPTANCE.md`](../ACCEPTANCE.md)
- 字节布局: [`wire-format.md`](wire-format.md)
- scrcpy 上游契约: [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md)
- 架构: [`architecture.md`](architecture.md)
- AI agent 集成: [`ai-agent-integration.md`](ai-agent-integration.md)
- 变更日志: [`../CHANGELOG.md`](../CHANGELOG.md)

最后更新: 2026-06-29。