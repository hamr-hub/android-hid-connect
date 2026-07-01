# 0003 — LLM 以 Chunk 为粒度规划(per-chunk planning, default N=4)

## Context

ADR 0001 决定了 LLM 最小输出是 Intent,selector 是 text-based;ADR 0002(Q3) 决定了 flow control 归 LLM。一个自然的后续问题:**LLM 跟观察循环怎么耦合?** 也就是 LLM 在 scenario 内多久"看一眼屏幕"再决定下一步。

候选粒度有三个极端 + 一个中间方案:

| 选项 | LLM 调用频率 | UI 漂移容忍 | Token 成本 |
|---|---|---|---|
| A. Macro-plan | 每 scenario 一次 | 差(中途失败 = 从头重规划) | 最低 |
| B. Per-atomic | 每个 atomic 一次 | 极强(每步 re-plan) | 最高(~N×) |
| C. Per-chunk | 每 N 个 atomic 一次 | 强(chunk 边界 re-plan) | 中(典型 N=3-5) |

## Decision

**LLM 以 Chunk 为粒度规划。** 一个 chunk = N 个 atomic 的有序序列(N 默认 4,屏幕级),chunk 末尾必须包含至少一个 observe atomic。chunk 执行完后,kernel 返回结果给 LLM,LLM 决定:

- **continue**:发下一个 chunk(场景按计划推进)
- **re-plan**:发不同的 chunk(selector_miss 恢复、UI 漂移吸收)
- **abort**:停止并上抛错误

kernel 不在 chunk 内部做"等 UI 稳定"之类的隐式重试——chunk 是 LLM 视角的"原子"。

## Why

1. **效率 vs 鲁棒性平衡**:`/goal` 同时要求"效率优化"和"多 app 编排"。B 违反效率;A 在多 app 下 UI 漂移反复从头重规划,实际延迟比 C 高(per Phase 6.5 测得每次 adb 重规划 ~670ms,这是要避免的)。
2. **与实战 pattern 对齐**:06-12 session 的"链式回退 + audit"是 **scenario/chunk 级** 回退,正好天然适配 chunk 边界,不需要 kernel 介入。
3. **Industry baseline**:Sierra / Apple Intelligence 等 on-device agent 的 production pattern 就是 chunked planning(N=3-5,屏幕级),N=4 是 screen-level 中位数。

## Considered Options

- A. Macro-plan — 否决:多 app 编排下 UI 漂移反复触发"从头重规划"
- B. Per-atomic — 否决:token 翻 N 倍,违反 `/goal` 效率优化
- **C. Per-chunk — 选定**,default N=4
- (Implicit D. Streaming non-chunked — 否决:等价于 macro-plan 但延迟更差,未单列)

## Consequences

- **Selector miss 的传播**:kernel 在 atomic fail 时**立即返回 LLM**(不等到 chunk 末尾),防止 UI 状态已漂移后还执行剩余 atomic。
- **Token 成本**:per-chunk 比 per-atomic 节省 ~4×(N=4);per-chunk 比 macro-plan 多 ~10%(chunk 边界多一次 LLM 调用,但换来鲁棒性)。
- **Kernel 不持有 plan 状态**:chunks 之间无依赖,kernel 是 stateless executor(只持有最近 chunk 的执行进度)。
- **N 是 tunable**:可以根据 app UI 复杂度在 adapter 层调整(简单表单 N=2,复杂动态页面 N=6);默认值 N=4 由 ADK harness 注入。

## 后续 ADR 候选

- 0004 — Error contract(atomic fail 时 kernel 返回什么给 LLM)— 见 Q5
- 0005 — Multi-app orchestration(跨 app 切换时的 chunk 边界与 state handoff)