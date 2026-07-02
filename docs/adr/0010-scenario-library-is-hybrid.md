# 0010 — Scenario library is Hybrid: templates + recordings + fresh-plan

## Context

LLM chunk-by-chunk 规划(ADR 0003)、session state bounded(ADR 0009)、egress filter(ADR 0008)。但**scenario 怎么复用?** 也就是同一个"打开微信 → 搜索联系人 → 发送消息"流程,每次让 LLM 从零生成?还是提前有 template / recording 可用?

| 选项 | 通用流程(login / navigate / search) | 用户特定流程 | LLM 自主 | token 成本 |
|---|---|---|---|---|
| A. 无 library | LLM 重 plan | LLM 重 plan | ✅ | 最高(每次重 plan) |
| B. Templates only | ✅ | ❌(LLM 每次重 plan) | 部分 | 中 |
| C. Recordings only | ❌(每次录制) | ✅ | 部分 | 中 |
| **D. Hybrid** | ✅ | ✅ | ✅ | 最低 |

## Decision

**`ScenarioLibrary = {templates, recordings, generated}`** 三段式(ADR 0010):

1. **`templates: HashMap<TemplateId, Scenario>`** —— 预定义 parameterized scenario,覆盖通用流程(`login_then_search(target)` / `navigate_to_app(app)` / 06-12 "链式回退"等)。可被 LLM 在 chunk 边界 `use_template(id)` 引用。
2. **`recordings: HashMap<RecordingId, Scenario>`** —— 用户录制的真人操作流程,作为 ground truth 与 imitation learning 素材。
3. **`generated: Vec<GeneratedScenario>`** —— LLM 自 plan 的 fresh scenario,可在 session 末被 promote 到 templates(如果 selector_miss rate 低)或 recordings(如果用户确认复用)。

**LLM 在 chunk 边界的决策也是 atomic**:决定 `use_template` / `use_recording` / `plan_fresh` 是 LLM 的 flow control 决策(ADR 0002),受 `selector_miss` N=2 升级阈值约束(ADR 0008);template / recording 可用时**阈值放宽到 N=3**(template retry 比 LLM retry 便宜)。

**selector cache(ADR 0009)跨 library 边界共享**:同一 selector string 在 template / recording / fresh-plan 间共享 cache entry。

## Why

1. **templates 节省 80% 通用流程 token**:login / navigate / search / app_switch 是最高频 scenario,每次让 LLM 重 plan 是浪费
2. **recordings 是真用户行为 ground truth**:06-12 偏好的"链式回退"可作为 templates 库一部分,selector_miss 升级阈值放宽直接降低 cloud escalation rate
3. **ADR 0006 已定 wire format 是 JSON**:templates / recordings 都是 JSON,落地无新基础设施
4. **generated promote 机制让 library 自演进**:LLM 自 plan 但 selector_miss rate 低的 scenario 自动晋升 templates,长期减少 LLM 推理负担

## Considered Options

- A. 无 library — 否决:长 scenario token 浪费严重,违反 `/goal` 效率优化
- B. Templates only — 否决:用户特定流程无复用路径,recordings 是 critical 偏好"self-contained"的自然延伸
- C. Recordings only — 否决:通用流程需要用户每次录制,不实用
- **D. Hybrid — 选定**

## Consequences

- **selector cache 跨 library 边界共享**:`search_button` selector 在 template / recording / fresh-plan 都指向同一 cache entry
- **escalation 阈值动态调整**:template / recording 可用时 N=2 → N=3;完全 fresh-plan 时 N=2
- **library on-device only**:per ADR 0008 history 不出域,library 本质是"已知成功 scenario",与 history 同等敏感度
- **promote 机制**:session 末 selector_miss rate < 5% 的 generated scenario 自动 promote 到 templates(节省未来 token);selector_miss rate > 50% 的 templates 降级到 `deprecated`
- 任何"templates 同步到 cloud"的探索不进 main branch(违反 ADR 0008);只能放 `experiments/`

## 关联

- 上游:ADR 0002(flow control 归 LLM), ADR 0006(wire format JSON), ADR 0008(egress), ADR 0009(session state)
- 下游:ADR 0011 — Cold-start strategy(首次连接 / 设备激活 / app 安装探测)— 见 Q18