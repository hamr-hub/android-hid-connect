# 0002 — Flow control is owned by the LLM, not the kernel

## Context

ADR 0001 决定 LLM 输出 Intent(geometry-agnostic),但**flow control(retry / wait / branch / loop)的责任归属**未定。三种划分:

| 选项 | atomic 语义 | scenario 语义 | kernel 责任 | LLM 责任 |
|---|---|---|---|---|
| A. LLM owns | 纯函数(单次 write/read) | 纯序列 | 翻译 + 执行 | 编排(retry/branch/wait/loop) |
| B. Kernel owns | 可含 `wait_for(ui_state, timeout=5s)` 等 | 内置 retry / timeout | 翻译 + 编排 + 执行 | 仅发 scenario |
| C. Hybrid | 默认纯,device-internal wait 是 kernel 原语 | scenario-level 编排 + device-internal 同步 | 翻译 + 部分编排 + 执行 | scenario 级编排 |

## Decision

**A. LLM owns flow control.** atomic 是纯函数,scenario 是纯序列,kernel 只翻译 + 执行。任何 retry / branch / wait / loop 都是 LLM 通过 scenario 组合表达,绝不烤进 atomic 或 scenario 内部。

具体边界:
- atomic fail → kernel 返回 `AtomicResult::fail` 立即,**不重试、不等待、不分支**
- LLM 看到 fail → 决策:retry(再发 atomic)、branch(发 recovery scenario)、wait(发 observe(snapshot) + sleep)、abort(发 scenario 终止信号)
- 没有任何 "wait_for_ui_idle" / "wait_for_app_launch" 之类的 kernel 原语;所有 device-internal 等待都通过 observe atomic 由 LLM 显式发起

## Why

1. **对齐 `/goal` "翻译层"语义**:translation ≠ orchestration,flow control 是 orchestration
2. **实战 pattern 对齐**:06-12 session 验证的"链式回退 + audit"是 **scenario/chunk 级** 回退,正是 LLM 拥有 flow control 时的自然 pattern
3. **simulator 验证成本最低**:纯函数 atomic + 纯序列 scenario,无状态机、无 timeout race,fixture 简单;B/C 让 simulator 必须模拟 UI 状态机,验证成本陡增
4. **LLM 全局 retry 优于 per-atomic retry**:kernel 局部 retry 看不到 LLM 的全局意图(LLM 可能想"换 selector 而不是同 selector 重试")
5. **Phase 6.5 数据支持**:0.78ms kernel-internal 路径假设 kernel 是 stateless executor;B/C 会引入 state,反而破坏这一性能特征

## Considered Options

- **A. LLM owns — 选定**
- B. Kernel owns — 否决:违反 `/goal` 翻译层语义、simulator 验证成本高、与实战 pattern 不对齐
- C. Hybrid — 否决:device-internal wait 与 LLM-level wait 的边界本身就是 ADR 难题,等于把 ADR 0002 拆成两个问题;先把所有权划清,再谈 hybrid

## Consequences

- atomic / scenario 的 schema **禁止**任何 retry / wait / branch / loop 字段(B/C 会引入);这是 review checklist 必查项
- kernel 代码不允许 spawn timer / state machine;实现永远是 stateless executor(只持有最近 chunk 的执行进度)
- LLM token 成本:scenario 长时每 chunk 一次 LLM 调用,但比 per-atomic 节省 ~4×(见 ADR 0003)
- 任何"等 UI 稳定"语义由 LLM 通过 `observe(snapshot)` + 自检 + 重发 atomic 表达;kernel 不参与

## 关联

- 上游:ADR 0001(Intent 是 geometry-agnostic)
- 下游:ADR 0003(chunk 边界 = LLM re-plan 点)、ADR 0004(AtomicResult 让 LLM 拿到 fail 信息后做决策)