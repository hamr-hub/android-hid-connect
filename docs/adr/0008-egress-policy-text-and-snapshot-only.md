# 0008 — Egress policy for cloud path: text + snapshot allowed, history denied

## Context

ADR 0007 (Hybrid) + EscalationPolicy(默认 selector_miss N=2 升级)已定。cloud 路径的隐私边界未固定——具体说,**升级触发时,device 出域给 cloud LLM 的 Observation / state 包含什么?**

| 选项 | text(ui_tree + foreground_app)| snapshot(PNG) | history(recent atomics) | 隐私风险 | 能力保留 |
|---|---|---|---|---|---|
| A. 全量 | ✅ | ✅ | ✅ | 最高(操作轨迹泄漏) | 最高 |
| **B. text + snapshot** | ✅ | ✅ | ❌ | 中(截图泄漏,history 不泄漏) | 高(vision fallback 可用) |
| C. 仅 text | ✅ | ❌ | ❌ | 低 | 低(vision fallback 在 cloud 失效) |
| D. per-session opt-in | ✅ | opt-in | opt-in | 最低 | 用户驱动 |

## Decision

**EgressPolicy = `{allow_text: true, allow_snapshot: true, allow_history: false}`**(默认)。

- `text` — 结构化 Observation 子集(`ui_tree`, `foreground_app`)。**总是**出域(text 不含操作轨迹,体积小)。
- `snapshot` — PNG 视觉。**默认**出域(cloud vision reasoning 的必要输入)。
- `history` — 最近 atomic 执行 log。**默认禁出域**(操作轨迹是比截图更敏感的隐私面:它泄漏"用户做了什么",不仅"屏幕显示什么")。

**全局 escape valve**:`LlmDeployment.kind = on_device_only` —— 完全不走 cloud 路径,**强制 0 出域**。

**per-chunk override**:`PlanMessage.force_escalate.allow_snapshot: false` —— 极少数场景下用户希望"即使升级也不传 PNG",per-chunk opt-out。

## Why

1. **escalation 本身已 opt-in** —— ADR 0007/0008 默认 selector_miss N=2 才升级,**只在 hard case 才付隐私成本**,fast path 完全不出域
2. **vision fallback 是 cloud 升级的关键目的** —— 选 C / D 让 cloud 升级**等于无效**(cloud 拿不到 vision,selector_miss 必然复发 → abort 或循环),违反 escalation 设计目的
3. **history 是更敏感的隐私面** —— 操作轨迹(用户做了什么)比 UI 截图(屏幕上显示什么)更敏感;B 把 history 一刀切禁出域,**正确地隔离了最敏感数据**
4. **escape valve 存在** —— 用户可以全局禁 cloud,**默认不必 over-restrict**;给追求极致隐私的用户留有退路

## Considered Options

- A. 全量(text + snapshot + history)— 否决:history 泄漏操作轨迹,隐私风险最高
- **B. text + snapshot — 选定**
- C. 仅 text — 否决:cloud vision fallback 失效,escalation 等于无效
- D. per-session opt-in — 否决:每次 session 启动用户认知负担,违反"少配置多智能";opt-in 路径下能力与 C 同(因为多数用户不 opt-in)

## Consequences

- **隐私默认 acceptable**:snapshot 是最敏感的可见部分,但**被 escalation 频率封顶**(只有 selector_miss N=2 时才付)
- **history 永远不出域**,即使 escalation
- **per-chunk override**:罕见场景(如录屏工具内部使用,不希望录屏内容出域)可 per-chunk 关闭 snapshot
- simulator 必须实现 mock egress filter + 真实 egress filter 两套实现,**测试矩阵增加 1 维**
- 任何"把 history 加密后出域"的探索不进 main branch(history 禁出域是 hard rule);只能放 `experiments/`

## 关联

- 上游:ADR 0007(Hybrid),EscalationPolicy(selector_miss N=2)
- 下游:ADR 0009 — Simulator 验证策略(覆盖 hybrid + egress)— 见 Q13