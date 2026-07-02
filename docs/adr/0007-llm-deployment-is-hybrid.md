# 0007 — LLM deployment topology is Hybrid (on-device default + cloud escalation)

## Context

`/goal` 写明"android-binder-agent **最终要部署在设备端侧**,作为 LLM 意图 ↔ 设备操控的翻译层"。"最终"是 long-term;immediate path 不固定。Phase 6.5 (2026-06-30) 测得:

- kernel-internal (on-device 直接 UHID): p50 0.78ms / max 9.51ms
- host adk + `adb shell input tap` (cloud-brain 路径): p50 670ms / p95 1087ms / max 1519ms

850× 差距由 seam 位置决定(ADR 0001)。但 LLM 部署位置本身是另一条独立维度:

| 选项 | 延迟 | 能力 | 隐私 | 离线可用 | 升级路径 |
|---|---|---|---|---|---|
| A. 仅 on-device | 极低 | 低 | 完美 | ✅ | 升级需 OTA |
| B. 仅 cloud | 高(网络 + adb) | 极高 | 差(数据出域) | ❌ | 即时 |
| **C. Hybrid** | fast path 极低 / hard case 高 | fast path 低 / hard case 极高 | fast path 完美 / hard case 差 | ✅ fast path / ❌ cloud | on-device 模型升级,cloud 升级率自然下降 |
| D. User choice | 视选 | 视选 | 视选 | 视选 | 配置驱动 |

## Decision

**Hybrid** —— `LlmDeployment = {kind: "hybrid", on_device_model, cloud_model, escalation_policy}`。默认 on-device 处理所有 chunk;**cloud 仅在 escalation policy 触发时介入**(默认:连续 selector_miss N=2 次,见 EscalationPolicy)。

**fast path 物理约束**:on-device LLM 与 kernel 同进程(per `/goal` 部署在设备端侧),走 0.78ms kernel-internal UHID 路径;cloud LLM 通过网络 + 当前 host adk 路径接入,付 670ms 量级成本,但只发生在 hard case。

**隐私边界**:on-device 路径下**任何观察数据不出设备**;cloud escalation 时,**只传 `Observation` 的结构化子集**(ui_tree + foreground_app),**PNG snapshot 默认不上云**(默认开启 `egress_filter.allow_snapshot = false`,需用户显式开启)。

## Why

1. **对齐 `/goal` long-term 路径**:"最终 on-device"是终态;C 是当前最佳 + 长期迁移路径(模型升级,无需架构变更)
2. **吸收 Phase 6.5 的 850× gap**:fast path 不付网络成本,hard case 不付能力税
3. **对齐 critical 偏好**:on-device 路径天然符合"不操控设备、self-contained";cloud 路径配 egress_filter 隔离敏感数据
4. **基础设施已就位**:`feat(adk): extend binary + real-device latency + mock LLM harness — 19 ACs literally verified` 的 mock adapter 接口直接对应"双 LLM 适配器"
5. **架构稳定 + 模型可替换**:on-device 模型每代升级,cloud 升级率自然下降,**架构无需随模型演进**

## Considered Options

- A. 仅 on-device — 否决:当前能力差距大,无法支撑 `/goal` "多app编排"
- B. 仅 cloud — 否决:与 `/goal` 部署方向冲突;网络依赖;670ms gap 在 fast path 上不可接受
- **C. Hybrid — 选定**
- D. User choice — 否决:用户认知负担;违反"少配置多智能"

## Consequences

- ADK harness 启动时读取 `LlmDeployment`,实例化两个 model adapter(on-device + cloud)
- Escalation policy 是 structured,默认值由 ADR 0007 + EscalationPolicy(后续 ADR)定义
- cloud 路径强制走 egress_filter,policy 默认 `deny snapshot_png`(用户可改)
- on-device 模型权重随 release 更新,cloud 模型由 provider 升级
- simulator 测试必须同时覆盖 on-device-only / cloud-only / hybrid-3 种拓扑(Q19 后续)

## 后续 ADR 候选

- 0008 — EscalationPolicy 详细合约(selector_miss N=2 + 用户显式 + 未来 confidence 阈值)— 见 Q11
- 0009 — Egress filter(cloud 路径的数据出域策略)— 见后续 Q
- 0010 — On-device 模型规格(参数量、推理时延、selector cache 配合)— 等真机数据再开

## 关联

- 上游:ADR 0001(Intent + selector), ADR 0006(wire format)
- 下游:ADR 0008(EscalationPolicy), ADR 0009(Egress filter)