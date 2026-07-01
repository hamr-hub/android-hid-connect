# 0005 — Multi-app orchestration: Hybrid (app-agnostic default + `app_switch` atomic)

## Context

`/goal` 写明"**多app编排、跨app切换**"。ADR 0001 决定 LLM 输出 Intent(geometry-agnostic),ADR 0004 决定 AtomicResult 结构化。但 **LLM 怎么表达"目标 app"** 仍未定。

三个候选:

| 选项 | 表达方式 | Token 成本 | 跨 app 切换入口 | 与 text-based selector 对齐 |
|---|---|---|---|---|
| A. App-aware | 每个 intent 带 `app="X"` | 高(每 intent +5 token) | 隐式 | 不对齐(UI tree scope vs intent app 可能不一致) |
| B. App-agnostic | LLM 不指定,kernel 跟踪 | 最低 | 无显式入口(只能 deep link) | 对齐(UI tree 天然 app-scoped) |
| **C. Hybrid** | 默认 app-agnostic + `app_switch(X)` 显式 atomic | 中(切换时 1 atomic) | 显式 atomic | 对齐 |

## Decision

**Hybrid** —— LLM 在 app 内操作时,所有 atomic 不带 app 字段(默认 app-agnostic)。跨 app 切换通过 `app_switch(target: AppIdentifier)` atomic 显式完成,该 atomic 与其他 atomic 语法对等(都是普通 atomic),因此 selector/audit/chunk 边界都能看到切换点。

`ForegroundApp`(`{package, activity, focused, since_ms}`)是单 source of truth——observe atomic 返回它,LLM 用它做"现在在哪个 app"的判断,**不再在每个 intent 里携带 app 字段**。

## Why

1. **对齐 text-based selector**:UI tree 天然 app-scoped,app-agnostic intent 直接作用于 dump 出的 UI tree,无需 app 字段同步
2. **节省 token**:scenario 长(20 atomic)时,A 累积 ~100 token 冗余;C 仅在跨 app 时付出 1 个 atomic 的开销
3. **跨 app 切换有显式入口**:`/goal` 第一条就是多 app 编排,A 把 app 揉进每个 intent 太啰嗦,B 没有入口
4. **并行多 app 自然分解**:`/goal` "并行执行"在 C 下分解为"多个 LLM agent 各自有 foreground",atomics 仍 app-agnostic;无需引入 multi-app intent 复合类型
5. **与 chunk 边界天然对齐**:`app_switch` 是 chunk 的天然 re-plan 点(切完 app,新 UI tree 必变),LLM 在切完 app 后强制 observe 一次再决定下一步

## Considered Options

- A. App-aware — 否决:token 冗余、与 UI tree scope 易不一致
- B. App-agnostic without explicit switch — 否决:无显式跨 app 入口,违反 `/goal`
- **C. Hybrid — 选定**

## Consequences

- `app_switch` 必须支持多种切换方式(launcher tap / deep link / Android Intent / `am start`),由 kernel 内部路由(LLM 不感知)
- `ForegroundApp` 是 observe atomic 的必返回字段,LLM 永远能拿到
- 任何"切 app 时同时操作"的复合语义**禁止**——必须拆成 `app_switch` + 后续 atomic
- Phase 6.5 测得的 670ms adb 路径中,`adb shell am start` 占大头(launcher 启动 ~400ms);`app_switch` 的延迟必须被 selector cache / warm-start 优化(后续 ADR 跟进)
- Parallel multi-app = 多个 LLM agent 各自持有 foreground,kernel 端用 session id 隔离;一个 agent 不能动另一个 agent 的 app

## 后续 ADR 候选

- 0006 — Plan representation wire format(JSON / s-expr / DSL)— 见 Q7
- 0007 — `app_switch` 实现合约(launcher tap / deep link / `am start` 路由策略)— 等真机数据再开
- 0008 — Parallel multi-app session 隔离合约