# 0004 — Error contract is structured AtomicResult, not boolean

## Context

ADR 0003 已定 kernel 在 atomic fail 时**立即返回 LLM**(不等到 chunk 末尾)。问题是:return payload 是什么?

三个候选:

| 选项 | Payload 大小 | LLM 决策依据 | 冗余度 |
|---|---|---|---|
| A. Boolean | 1 byte | 无(LLM 盲) | n/a |
| B. Structured | ~50 bytes | code + reason + retryable | 低(UI tree 不冗余) |
| C. Full snapshot | ~50 KB | 完整 UI + history | 高(observe atomic 覆盖) |

## Decision

**Atomic 返回 `AtomicResult` 结构化类型**,定义见 `CONTEXT.md`。两条规则:

1. **成功路径**:`{ok: true, element: ResolvedTarget, duration_ms}`。`ResolvedTarget` 是 selector 解析后的最小信息(id 字符串 + 屏幕坐标 + bounds),LLM 用于审计与下一 chunk 计划。
2. **失败路径**:`{ok: false, code: AtomicErrorCode, reason, retryable}`。code 是固定枚举,不开放自由文本;reason 是 i18n 字符串;retryable 是 advisory bool。

**UI tree 状态永远不嵌入 AtomicResult**——LLM 通过 observe atomic 单独拿,避免冗余传输。

## Why

1. **不冗余 observe atomic**:ADR 0003 已定 chunk 末尾必有 observe,UI tree 由该 atomic 覆盖;选 C 重复劳动。
2. **code 枚举让 LLM 决策有据**:`selector_miss` → 换 selector 或 cold-start 重 dump UI tree;`permission_denied` → branch 到 recovery scenario;`app_not_foreground` → abort + 用户介入;`kernel_error` → retryable=true 时 retry。
3. **retryable 是 advisory**:LLM 可以忽略(例如 5 次 retry 后强制 abort);这是 LLM 策略,kernel 不强制。
4. **in-house simulator 验证最干净**:每个 code 一个 mock fixture,在 ADK harness 里 round-trip 跑通。

## Considered Options

- A. Boolean — 否决:LLM 拿不到"为什么 fail",无法决策 retry/branch/abort
- **B. Structured (`AtomicResult`)— 选定**
- C. Full snapshot — 否决:UI tree 冗余,token 浪费

## Consequences

- `AtomicErrorCode` 枚举在 kernel 和 ADK harness 之间**单 source of truth**(共享 `enum AtomicErrorCode { ... }`)
- `reason` 字符串可本地化(中/英/...),但 schema 一致
- Kernel 不抛异常(panic 不算),所有失败都转成 `AtomicResult::fail(...)`
- simulator 必须实现所有 code 的 mock fixture;真机跑通前 CI 必须过 simulator 全码
- 未来加新 code(例如 `biometric_required`)是 enum 扩展,需要 minor version bump

## 后续 ADR 候选

- 0005 — Multi-app orchestration(跨 app 切换合约)— 见 Q6
- 0006 — Plan representation(LLM 输出的 wire format:JSON / s-expression / typed DSL)