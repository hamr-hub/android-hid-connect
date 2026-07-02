# 0009 — Session state is bounded, on-device only, single-task scope

## Context

ADR 0002 已定 kernel 是 stateless executor;ADR 0003 已定 LLM 在 chunk 边界 re-plan。但 re-plan 需要 context——**context 怎么保留?** 也就是 atomic 执行历史、selector cache、scenario 进度这些信息,**保留多久 / 多广 / 在哪?**

| 选项 | 范围 | 持久位置 | selector cache | 隐私 | 复杂度 |
|---|---|---|---|---|---|
| A. Stateless | 无 | n/a | 无(每次重 dump UI tree) | 完美 | 最低 |
| **B. Session-based** | 一次用户任务(30s-5min) | on-device | 有(同 session 复用) | on-device 不出域 | 中 |
| C. Cross-session | 跨用户任务 | on-device 持久化 | 有 | 跨 session 累积(违反 critical 偏好) | 高 |
| D. Explicit state passing | LLM 显式管理 | 任意 | 由 LLM 决定 | 由 LLM 决定 | 高(LLM 推理负担) |

## Decision

**SessionState = `{task, scenario_progress, recent_atomics, selector_cache, foreground_app_history, deadline}`,持有范围 = 一次用户任务(典型 30s-5min)。**

具体规则:
- `task` —— 用户的 task description(string),session 起点注入
- `scenario_progress` —— 当前 ChunkIndex,LLM 知道"还剩多少"
- `recent_atomics` —— bounded `VecDeque<AtomicRecord>`(典型 20 条),FIFO 淘汰
- `selector_cache` —— `HashMap<selector_string, ResolvedTarget>`,**`selector_miss` 时失效对应 entry**
- `foreground_app_history` —— bounded `VecDeque<ForegroundApp>`(典型 10 条),用于多 app 编排的 app_switch 决策
- `deadline` —— session 超时(默认 5min,防 runaway)

**session 边界信号**:
1. 用户停止发新指令 30s
2. scenario abort / success
3. deadline 到期

**state 物理位置**:**on-device only**——ADR 0008 已定 history 不出域,SessionState 是 history 的逻辑容器,**自然合规**。

## Why

1. **避免 A 的 token 浪费**:LLM 多次 chunk re-plan 是常态,A 让 LLM 每次重 plan 整个 scenario;B 的 `recent_atomics` + `selector_cache` 让 re-plan 局部化
2. **selector cache 跨 chunk 复用**:同 session 内同一 selector 可能被 reference 多次(典型 `tap(search) → type → tap(search)`),cache 避免重复 dump UI tree
3. **避免 C 的 critical 偏好冲突**:跨 session state 持久化 = 把 user data 留在 agent 进程,违反 "self-contained + in-house simulator"
4. **session 边界判定简单**:典型信号 = "用户停止 30s" 或 "scenario 终止"——不依赖 LLM 推理
5. **ADR 0008 自然合规**:`recent_atomics` 是 history 的结构化形态,EgressPolicy 已 deny history 出域——B 不需要新 egress 合约

## Considered Options

- A. Stateless — 否决:token 浪费严重,selector cache 无法复用,违反 `/goal` "效率优化"
- **B. Session-based — 选定**
- C. Cross-session — 否决:违反 critical 偏好"self-contained",state 同步 / 失效是分布式系统难题
- D. Explicit state passing — 否决:LLM 推理负担过重;违反 ADR 0002 的"kernel stateless"延伸语义(LLM 也不应显式管理 state schema)

## Consequences

- **selector cache stale 风险**:UI 漂移 → `selector_miss` → cache 失效对应 entry,自然恢复
- **session 边界判定**:30s idle 或 scenario 终止;不依赖 LLM 推理
- **state 物理位置**:on-device only;cloud 路径不接触 SessionState
- **bounded recent_atomics**(20 条):无论 scenario 多长,session 内存有上限
- **deadline**(5min):防止 runaway scenario / 用户遗忘
- 任何"持久化 SessionState 到 cloud"或"跨 device sync state"的探索不进 main branch;只能放 `experiments/`

## 关联

- 上游:ADR 0002(flow control 归 LLM), ADR 0003(chunk 边界 re-plan), ADR 0008(history 不出域)
- 下游:ADR 0010 — Observability(audit log / metrics / debugging)— 见 Q16