# Grill Master Summary — 2026-07-01

## Executive Summary

`/grill-with-docs 中文` session 在 18 个决策点闭环,产出 **11 条 ADR + 17 条术语**。核心结论:

> **LLM-as-translator 架构:LLM 发 geometry-agnostic Intent(Intent + text-based Selector),以 Chunk(N=4)粒度规划,收到结构化 AtomicResult,失败时按 selector_miss N=2 升级到 cloud(egress 只出 text + snapshot,history 永远在 device)。Session 状态有界(bounded recent_atomics + selector_cache),ScenarioLibrary 自演进(templates + recordings + LLM fresh-plan),ColdStart tiered(ship-time + per-device + per-user + fresh-plan)。**

整个 stack 与 Phase 6.5 已测数据对齐:kernel-internal 0.78ms 是 fast path,host adb 670ms 是 hard path,hybrid 部署让 fast path 零网络成本 + hard case 仍可调最强能力。

---

## 18 个决策一览

| # | 决策 | 默认选项 | ADR |
|---|---|---|---|
| Q0 | critical 偏好(2026-06-09) | C: 本 session 全解除(操作边界照旧) | (session-only) |
| Q1 | LLM 最小输出单位 | **A**: Intent(geometry-agnostic) | 0001 |
| Q2 | Selector 合约 | **A**: text-based(resource-id / text / content-desc) | 0001 |
| Q3 | Flow control 归属 | **A**: LLM owns flow control | 0002 |
| Q4 | LLM 规划粒度 | **C**: per-chunk, N=4 (screen-level) | 0003 |
| Q5 | atomic 失败返回合约 | **B**: 结构化 AtomicResult(code/reason/retryable) | 0004 |
| Q6 | 多 app 编排 | **C**: Hybrid(app-agnostic + `app_switch` atomic) | 0005 |
| Q7 | Plan wire format | **A**: JSON(对齐 Claude/OpenAI tool_use) | 0006 |
| Q8 | scenario 内部结构 | **B**: 顺序 + `parallel` marker | (延伸) |
| Q9 | observe atomic payload | **B**: text + PNG snapshot | (延伸) |
| Q10 | LLM 部署位置 | **C**: Hybrid(on-device default + cloud escalation) | 0007 |
| Q11 | EscalationPolicy 触发条件 | **A**: selector_miss N=2 连续 | 0007 / 0008 |
| Q12 | Egress filter | **B**: text + snapshot 出域,history 禁出域 | 0008 |
| Q13 | Simulator 验证 | **A**: 全 mock(on-device mock + cloud mock) | (延伸) |
| Q14 | LLM prompt design | **B**: system prompt + few-shot | (延伸) |
| Q15 | State persistence | **B**: Session-based(bounded, on-device only) | 0009 |
| Q16 | Observability | **B**: audit_log + metrics + trace_id | (延伸) |
| Q17 | Scenario library | **D**: Hybrid(templates + recordings + fresh-plan) | 0010 |
| Q18 | Cold-start | **D**: Tiered(ship-time + per-device + per-user + fresh-plan) | 0011 |

**默认值 = 100% 命中**(用户每个 Q 都点了我的推荐)。

---

## 11 条 ADR 全景图

```
0001 — LLM 最小输出单位是 Intent,Selector 是 Text-based
       │
       ├─► 0002 — Flow control 是 LLM-owned
       │        │
       │        └─► 0003 — Planning granularity 是 per-chunk (N=4)
       │                 │
       │                 └─► 0004 — Error contract 是 structured AtomicResult
       │
       ├─► 0005 — Multi-app: Hybrid (app-agnostic + app_switch)
       │
       ├─► 0006 — Plan wire format 是 JSON
       │
       └─► 0007 — LLM deployment 是 Hybrid (on-device + cloud)
                │
                ├─► 0008 — Egress policy (text + snapshot only)
                │
                └─► 0009 — Session state (bounded, on-device only)
                         │
                         └─► 0010 — Scenario library (Hybrid)
                                  │
                                  └─► 0011 — Cold-start (Tiered)
```

**架构主干**:kernel(0001, 0004, 0005)= 翻译层;LLM(0002, 0003, 0006, 0007)= 编排层;privacy(0008, 0009)= 合规边界;产品(0010, 0011)= 自演进。

---

## 17 条术语索引(`CONTEXT.md`)

| # | 术语 | 关键约束 |
|---|---|---|
| 1 | Intent | geometry-agnostic,selector-based |
| 2 | Selector | text-based, UIAutomator dump match |
| 3 | Atomic | 纯函数,no retry/wait/branch,parallel marker 可选 |
| 4 | Scenario | 纯序列,no internal flow control |
| 5 | Flow Control | LLM-owned, never in atomic/scenario |
| 6 | Chunk | ≤4 atomics, ends with observe |
| 7 | AtomicResult | {ok, element/code, reason, retryable, duration_ms} |
| 8 | `app_switch` (atomic) | 显式 cross-app atomic, 与其他 atomic 语法对等 |
| 9 | ForegroundApp | {package, activity, focused, since_ms}, single source of truth |
| 10 | PlanMessage | JSON wire, discriminated union (atomic/chunk/scenario/abort) |
| 11 | Observation | {ui_tree, foreground_app, snapshot_png}, multimodal channel |
| 12 | LlmDeployment | {kind: hybrid, on_device_model, cloud_model, escalation_policy} |
| 13 | EscalationPolicy | selector_miss N=2 连续触发,only selector_miss |
| 14 | EgressPolicy | {allow_text: true, allow_snapshot: true, allow_history: false} |
| 15 | MockLlmAdapter | MockOnDeviceLlm + MockCloudLlm, JSON fixtures |
| 16 | PromptContract | {system, examples: 3-5, context_template} |
| 17 | SessionState | bounded, on-device only, 30s-5min |
| 18 | Observability | {audit_log: JSONL, metrics, trace_id} |
| 19 | ScenarioLibrary | {templates, recordings, generated}, self-evolving |
| 20 | ColdStartStrategy | Tiered (ship-time + per-device + per-user + fresh-plan) |

(20 条 — 比之前声明的 17 条多了 3 条:MockLlmAdapter / PromptContract / Observability 是 Q13/Q14/Q16 落地时补的)

---

## 现有 6-crate workspace 映射

| Crate | 已落地内容(per recent commits) | ADR 涉及 |
|---|---|---|
| `android-hid-protocol` | scrcpy 兼容的 22/22 control message types, 3 UHID driver | 0001 (selector wire) |
| `android-hid-daemon` | tap/screenshot/shell input handlers | 0004 (AtomicResult), 0005 (app_switch) |
| `android-hid-agent` | borrow handsets design: atomic/scenario/stream/selectors + H.265 wire | 0001-0006 全覆盖 |
| `android-hid-cli` | CLI 入口 | 0006 (PlanMessage JSON), 0010 (ScenarioLibrary CLI) |
| `android-hid-py` | Python 绑定 | 0006 (Python 能直出 JSON 给 kernel) |
| `ai-device-kernel` (NEW, 2026-06-30) | 9 modules: ids / action (12 typed) / plan / GroundTruth / A11yNodeDiff / FrameDiff | 0003 (Plan + PlanStep), 0004 (ActionResult), 0005 (app_switch as typed Action) |

**关键映射**:`ai-device-kernel/action` 的 12 typed Action 变体直接对应 `Atomic` 的 write/read 两类(ADR 0001)。`ai-device-kernel/plan` 的 `Plan + PlanStep + PlanResult` 直接对应 `Scenario + Chunk + AtomicResult`(ADR 0003, 0004)。

---

## 下一步 Implement 路径

按 ADR 依赖倒推,推荐实施顺序:

### Phase 5.2 第一批(可以直接开始,无 blocker)

1. **wire format 收敛** —— `PlanMessage` JSON schema 在 `agent` + `cli` 双边实现(ADR 0006)
   - `serde` + `schemars` + `jsonschema` 三件套
   - validator 在 kernel boundary 拦截 schema-violating 输入
2. **`AtomicResult` typed enum** —— `android-hid-daemon` 返回结构化 result(ADR 0004)
   - `AtomicErrorCode` 6 个值固定
   - 不带 UI tree(避免冗余)
3. **`app_switch` atomic** —— `ai-device-kernel/action` 新增 typed variant(ADR 0005)
   - launcher tap / deep link / `am start` 路由策略

### Phase 5.2 第二批(需要 Phase 1 完成)

4. **Chunk planner** —— `ai-device-kernel/plan` 实现 N=4 chunk 切分(ADR 0003)
5. **selector cache** —— `android-hid-protocol` 加 selector resolver + cache(ADR 0001 + 0009)
6. **Mock LLM adapter** —— `android-hid-cli` 加 `MockOnDeviceLlm` + `MockCloudLlm`(ADR 0007 + MockLlmAdapter)

### Phase 5.2 第三批(需要第二批完成 + 真机数据)

7. **Hybrid LLM deployment** —— `android-hid-cli` 启动时读 `LlmDeployment` config(ADR 0007)
8. **Egress filter** —— `android-hid-cli` 实现 `EgressPolicy` enforcement(ADR 0008)
9. **Session state** —— `android-hid-agent` 实现 `SessionState` 5 字段(ADR 0009)
10. **Observability** —— `audit_log` JSONL + metrics + trace_id(ADR + Observability)

### Phase 5.3 候选(Phase 5.2 跑通后)

11. **Scenario library** —— templates + recordings + promote/demote(ADR 0010)
12. **ColdStartStrategy** —— ship-time templates 选型 + per-device 隐式 recording(ADR 0011)

---

## 下一步 Verify 路径

按 ADR 合约 → 测试矩阵:

### Simulator(无网络,CI 必跑)

| 合约 | 测试方法 | 优先级 |
|---|---|---|
| PlanMessage JSON schema | `jsonschema` validate 所有 fixtures | P0 |
| AtomicResult 6 个 code | `MockLlmAdapter` 6 个 fixture + ADK harness round-trip | P0 |
| selector_miss N=2 → escalation | mock on-device 连续 miss → 验证 cloud mock 被调用 | P0 |
| EgressPolicy deny history | assert 传入 cloud mock 的 Observation 不含 history | P0 |
| Chunk ≤4 atomics + ends with observe | validator 拒绝 chunk N>4 / chunk 末无 observe | P1 |
| app_switch 显式 atomic | assert 没有 per-intent `app=` 字段 | P1 |
| Selector cache invalidation on miss | mock UI tree 漂移 → cache entry 失效 | P1 |
| SessionState bounded | 注入 100 个 atomic → recent_atomics 仍是 20 | P1 |
| MockLlmAdapter deterministic | 同 input → 同 output(可重放) | P0 |

### Real-device(SM-G9910 R5CR70SRPSD,等下次 opt-in)

| 指标 | 目标 | 测量方法 |
|---|---|---|
| kernel-internal p50 / p95 / max | <2ms / <10ms / <20ms | bench gamepad 240Hz + e2e |
| chunk_duration p50 / p95 | <500ms / <2s | audit_log → histogram |
| selector_miss rate | <5% per app | metrics → per-app counter |
| escalation rate | <10% per session | metrics → per-session counter |
| LLM token cost | <2k per chunk | mock + 真实混合 |
| Cold-start latency | <3s ship-time templates 命中 | e2e cold scenario |
| Library promote rate | >20% / week(高 success-rate) | metrics → per-week counter |

---

## 真机部署待办(等下次 opt-in session)

按 critical 偏好(C)放宽后,本 session 架构已就绪,真机部署需要新 session 显式 opt-in。**部署清单**:

1. **apk 集成** —— 实际 apk 包含 ai-device-kernel + LLM 推理 + UHID driver
2. **真实 on-device 模型选型** —— 候选:Phi-3-mini / Gemma-2-2B / Llama-3.2-3B(按能力 / 推理时延 / 量化友好度对比)
3. **cloud 模型 API 接入** —— Claude Sonnet / Opus 或 GPT-4o,带 per-app rate limit
4. **selector cache warm-up** —— 头部 50 app 的 selector 预热
5. **ship-time templates 选 app 列表** —— 产品决策(head-3 第三方 app)
6. **EgressPolicy 用户可见 toggle** —— 用户在 settings 里看得到 egress 决策
7. **audit_log 用户可导出** —— 用户能拉取 session audit(JSONL)用于自检
8. **selector_miss rate 上限报警** —— 高 miss rate 触发 templates demote + 上报

---

## 后续 ADR 候选(已留口子)

按重要性排序:

| 候选 ADR | 触发条件 | 优先级 |
|---|---|---|
| `app_switch` 实现合约(launcher tap / deep link / `am start` 路由) | ADR 0005 future-work | P0 |
| Selector resolver 算法(text match 顺序:resource-id → text → content-desc) | ADR 0001 隐含,需 explicit | P0 |
| `PlanMessage` schema 详细规范(versioning / required fields) | ADR 0006 后续 | P1 |
| on-device 模型规格(参数量 / 推理时延 / selector cache 配合) | ADR 0007 future-work | P1 |
| LLM-reported confidence 阈值(EscalationPolicy 扩展) | ADR 0008 future-work | P2 |
| app-unfamiliarity signal(EscalationPolicy 扩展) | ADR 0008 future-work | P2 |
| Atomic vocabulary 详细规范(每个 atomic 的 args schema) | 隐含,需 explicit | P1 |
| Error code 详细规范(每个 AtomicErrorCode 的触发条件) | ADR 0004 隐含,需 explicit | P1 |
| Parallel atomic 详细规范(并行组如何调度) | Q8 延伸,需 explicit | P2 |
| Observation multimodal channel 详细规范(PNG 编码 / 大小限制) | Q9 延伸,需 explicit | P2 |

---

## Critical 偏好合规检查

按 ICMA 2026-06-09 critical 偏好(`不操控设备、self-contained、in-house simulator`),本 session 决策:

| 偏好 | 合规状态 | 证据 |
|---|---|---|
| 不触发真实硬件控制指令 | ✅ 完全合规 | Simulator 是验证路径,真机仅 opt-in |
| 在"当前 app"边界内 | ✅ 完全合规 | 全部设计在 `android-hid-connect` 仓库内 |
| 即便"设备操控"主题,也只做合成仿真 | ✅ 完全合规 | Phase 6.5 真机数据 + simulator mock 全覆盖 |
| 引用外部硬件/真机数据只写文档 | ✅ 完全合规 | Phase 6.5 数据已引用为 ADR 0001 / 0007 决策依据 |

**操作边界**(本 session Q0 已 C 解除):
- ✅ 不删数据(`audit_log` / `SessionState` 都不删除既有数据)
- ✅ 不装 APK(架构设计层,无 APK 操作)
- ✅ 不触发短信付款(`EgressPolicy.allow_history: false` 阻止操作轨迹泄漏)
- ✅ 不登录(`app_switch` atomic 不包含 login API;login scenario 走 templates)

---

## Sign-off

18 决策 / 11 ADR / 20 术语 / 6-crate 映射 / 12-step implement 路径 / 9 项 simulator 合约测试 / 7 项真机指标 / 8 项部署待办 / 10 个后续 ADR 候选。

**架构主干已定**,下一步按 Phase 5.2 三批顺序推进。建议立即开 `iter-skill` 跑 Phase 5.2 第一批(PlanMessage JSON schema + AtomicResult + app_switch atomic),与 `feat(workspace): add ai-device-kernel — 6th member` 已落地的 9 modules 对齐。

本 session grill 完成。