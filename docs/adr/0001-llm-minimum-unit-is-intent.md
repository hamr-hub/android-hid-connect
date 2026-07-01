# 0001 — LLM 最小输出单位是 Intent,Selector 是 Text-based

## Context

`android-binder-agent` 是 LLM 意图 ↔ 设备操控的翻译层(`/goal` 2026-06-09)。Phase 6.5 (2026-06-30) 测得两条路径端到端差距 ~850×:

- host adk + `adb shell input tap`: p50 670ms / p95 1087ms / max 1519ms
- kernel-internal `--no-adb`: p50 0.78ms / max 9.51ms

差距本质是 seam 位置(adb 路径每跳一个 IO,kernel-internal 是零拷贝)。seam 在哪决定 LLM 的最小输出单位,直接决定三个子项目(Daemon 输入面 / ADK harness / UHID phase5.2)的接口形态。

## Decision

**LLM 输出的最小单位是 Intent(geometry-agnostic),不直接发 raw coordinates。**

Intent 形如 `tap(search_button)` / `scenario: login_then_search` / `stream: observe(every 200ms)`,由 kernel + selector 层解析为 UHID 字节流。

**Selector 是 text-based:`resource-id` / `text` / `content-desc` 字符串,匹配 UIAutomator dump。** UI tree 不可用时返回 `selector_miss(reason)`,由 LLM 决策下一步;vision 作为 non-blocking fallback 留给后续 ADR。

## Why

1. **与既有词汇一致**:`/goal` 已经写明 `atomic/scenario/stream/selectors`,intent 是与既有语料对齐的最小公分母。
2. **kernel hot path 可行**:Phase 6.5 测得 0.78ms 已验证"已解析坐标 → UHID"路径走得通,新设计只需在前面加 selector 层,不动 hot path。
3. **解耦屏幕几何**:UI 漂移时 selector 自动重选,LLM 不需要每帧重新推理坐标。
4. **on/off-device LLM 切换只需重写意图生成器,kernel 不动**——这是"翻译层"的本质。

## Considered Options

| 选项 | LLM 最小单位 | Selector | 结论 |
|---|---|---|---|
| A | Intent | text-based | **选定** |
| B | Action (raw coords) | n/a | 否决:UI 漂移 = 任务失败,跨设备零迁移 |
| C | Hybrid (intent 主,action 兜底) | text-based | 否决:接口语义最复杂,fallback 触发条件需要再开 ADR |
| D | Intent | semantic embedding | 否决:每选一次 ~200ms,压垮 0.78ms 承诺 |

## Consequences

- 三个子项目接口全部走 intent-based:daemon 输入面接收 atomic 名 + selector,转 selector→坐标→UHID;ADK harness 校验语义 + selector 命中;UHID phase5.2 走 SPSC ring + selector cache。
- selector_miss 路径由后续 ADR 定义(默认 LLM 决策下一步;vision 作为 non-blocking fallback)。
- 任何对 raw coordinates 的需求(真机 tap 调试 / 录屏重放)走"action 旁路",不进 LLM 主路径。