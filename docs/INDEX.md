# Documentation Index — `android-hid-connect`

> 全部专题文档的导航。新增文档必须在这里登记。

---

## 入口

| 入口 | 何时读 | 何时跳过 |
| ---- | ------ | -------- |
| [`README.md`](../README.md) | 第一次接触本 crate,想跑通最小例子 | 已经会用 `HidSession` / `AgentControlSession` |
| [`AGENTS.md`](../AGENTS.md) | 准备提 PR / 改字节布局 / 加新 module | 只是 `cargo add` 拿来用 |

## 专题文档

| 文档 | 内容 | 读完后你会做什么 |
| ---- | ---- | ---------------- |
| [`architecture.md`](architecture.md) | 模块依赖图、线程/生命周期模型、纯度边界 | 知道在哪里加新功能不会破坏分层 |
| [`wire-format.md`](wire-format.md) | 22 control_msg + 3 AI + 3 HID report + 3 device_msg 字节布局速查 | 排查设备端拒收 / 对照 scrcpy C 端源码 |
| [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md) | 锁定的 scrcpy v2.7 版本 + 已知上游 caveat + 跟踪流程 | 升级 scrcpy 上游 / 报告 byte-exact 偏差 |
| [`ai-agent-integration.md`](ai-agent-integration.md) | LLM / agent runtime 怎么用 `AgentControlSession` | 把本 crate 嵌入 observe-plan-act 循环 |
| [`development.md`](development.md) | 本地开发循环 + 真机 E2E 步骤 + CI 矩阵 | 提 PR 前自检 / 跑分对比 |
| [`comparison-with-handsets.md`](comparison-with-handsets.md) | 本 crate vs `handsets` 仓库的多维度对比 | 选型决策(精度 HID vs a11y 自动化) |

## 配套文件

| 文件 | 内容 |
| ---- | ---- |
| [`../ACCEPTANCE.md`](../ACCEPTANCE.md) | AC-C/H/S/T/R 验收点表 + 真机回归记录 + 历史 bug 修复 |
| [`../CHANGELOG.md`](../CHANGELOG.md) | keep-a-changelog 格式的变更日志(release-please 会读) |
| [`../Cargo.toml`](../Cargo.toml) | crate 元数据 + 依赖 + feature flag |
| [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml) | CI 矩阵(ubuntu / macos / windows + MSRV) |

---

## 阅读路径 (建议)

按用途选路径:

### 路径 A — "我想用这个 crate"

1. [`README.md`](../README.md) → 协议概览 + 最小例子
2. [`docs/ai-agent-integration.md`](ai-agent-integration.md) → 如果你在写 LLM agent
3. [`docs/architecture.md`](architecture.md) → 想知道 `HidSession` / `AgentControlSession` / `HidClient` 怎么选

### 路径 B — "我想改这个 crate"

1. [`AGENTS.md`](../AGENTS.md) → 必读,目录规则 + 允许/禁止
2. [`docs/architecture.md`](architecture.md) → 模块分层 + 依赖方向
3. [`docs/wire-format.md`](wire-format.md) → 字节布局(改 hid/control 前必看)
4. [`docs/scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md) → 字节兼容契约
5. [`docs/development.md`](development.md) → 跑测试 / 真机 E2E

### 路径 C — "我在选 Android 自动化栈"

1. [`docs/comparison-with-handsets.md`](comparison-with-handsets.md) → 本 crate vs `handsets`
2. [`README.md`](../README.md) §"What this crate does" → 能力矩阵摘录
3. [`ACCEPTANCE.md`](../ACCEPTANCE.md) §9 → 真机回归 checklist

---

## 文档维护规则

- 新增专题文档 → 在本文件登记 + 在 `AGENTS.md` §8 加链接 + 在 `README.md` "Documentation" 段加链接。
- 删文档 → 先确认没有其它文档引用,再在本文件去掉条目。
- 改文档里的数字(测试数、scrcpy 版本号、跑分日期) → 全文 grep 同步更新。
- 文档代码示例用 ```` ```rust,no_run ```` 或 ```` ```text ````,**不要**在文档里贴真实命令输出(会过期)。
- 文档长度无硬上限,但单文件 > 1000 行要拆。

---

最后更新: 2026-06-29。