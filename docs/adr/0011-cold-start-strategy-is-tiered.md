# 0011 — Cold-start strategy is Tiered (ship-time + per-device + per-user + fresh-plan)

## Context

Scenario library (ADR 0010) 已覆盖已成熟流程,但**首次启动怎么办?** —— 用户首次连接 device / 首次安装新 app / LLM cold-start 时,ScenarioLibrary 是空的或部分空的。

| 选项 | ship-time templates | per-device templates | per-user recordings | LLM fresh-plan | 冷启动 latency |
|---|---|---|---|---|---|
| A. 全 cold | ❌ | ❌ | ❌ | ✅ | 极差(每次全 LLM 重 plan) |
| B. 预装 templates only | ✅ | ❌ | ❌ | 部分 | 中(覆盖 system app,第三方 app 差) |
| C. 全 hot | ❌ | ❌ | ✅(需累积) | ✅ | 极差(等用户用一段时间) |
| **D. Tiered** | ✅ | ✅(自动累积) | ✅(用户主动) | ✅(escape valve) | 最佳 |

## Decision

**Tiered cold-start** —— `ColdStartStrategy = {ship_time_templates, per_device_templates, per_user_recordings, fresh_plan_fallback}`:

1. **`ship_time_templates`** —— build time 内置到 binary 的 templates 集合:
   - Android system apps: Settings / Launcher / Files / Camera / Dialer / Messages
   - 06-12 chain-recovery 模式(ANR → force-stop → cold start → 重走 navigation)
   - Head-3 第三方 app(TikTok / WeChat / Alipay —— ship-time 由产品决策选)
2. **`per_device_templates`** —— per connected device 自动累积的 templates,通过**隐式 recording**(用户正常使用时,高 success-rate chunk 自动 promote,per ADR 0010)生成。
3. **`per_user_recordings`** —— 用户主动录制的真人操作流程(显式 recording)。
4. **`fresh_plan_fallback`** —— 未知 app / 罕见场景的 escape valve,LLM 自己 plan。

**cold-start 路径**:`ship_time_templates → fresh_plan → cloud escalation (ADR 0008)`。Library 自演进:high selector_miss rate templates 自动 demote 到 `deprecated`,high success-rate generated scenarios 自动 promote 到 `templates`(per ADR 0010)。

## Why

1. **cold-start latency 必须可接受**(`/goal` 效率优化):A / C 在 cold-start 期让 LLM 重 plan 全 scenario,延迟数十倍于 hot path,违反 `/goal`
2. **ship-time templates 是 deterministic baseline**:Settings / Launcher / Files / Camera / Dialer / Messages **每个 Android 设备都有**,ship-time 提供"零推理即可用"的 baseline
3. **per-device templates 通过隐式 recording 自动构建**:stale 风险通过 selector_miss escalation 自动修复(ADR 0008)
4. **06-12 链式回退已验证**:可作为 ship-time templates 的子集(所有 Android 设备 ANR/crash 处理逻辑相似)

## Considered Options

- A. 全 cold — 否决:cold-start latency 极差,违反 `/goal` 效率优化
- B. 预装 templates only — 否决:第三方 app 不覆盖,per-device / per-user 累积路径缺失
- C. 全 hot — 否决:cold-start latency 极差,需等用户用一段时间才能复用
- **D. Tiered — 选定**

## Consequences

- **ship-time templates 选 app 列表是产品决策**:哪个 head-3 第三方 app 入选需要产品 + 用户调研
- **per-device templates 通过 selector_miss rate 自动 promote / demote**:无需人工维护
- **per-user recordings 显式录制**:用户主动"演示"一次后,future session 可 replay
- **fresh_plan fallback 是 escape valve**:任何 library miss 都可以回退到 LLM 自 plan,不会卡死
- ship-time templates 失效时(Android 版本更新 → UI 变化)→ selector_miss rate 上升 → 自动 demote → fresh_plan 接管
- 任何"ship-time templates 远程 OTA 更新"的探索不进 main branch(违反 ADR 0008);只能放 `experiments/`

## 关联

- 上游:ADR 0008(egress), ADR 0010(scenario library)
- 下游:本 ADR 是 master grill 收尾决策;后续 ADR 候选见 grill 完成报告