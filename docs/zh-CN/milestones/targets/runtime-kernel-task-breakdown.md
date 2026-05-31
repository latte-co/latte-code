# Code Agent Evolution 任务拆分 v0.1-v0.5

## 文档状态

本文是 Fluxcode 从基础 code agent 演进到 harness-native runtime 的独立任务拆分文档。文件名保留 `runtime-kernel-task-breakdown` 是为了维持现有索引稳定；内容已调整为渐进式实现排期。

英文对应文档：[`docs/en-US/milestones/targets/runtime-kernel-task-breakdown.md`](../../../en-US/milestones/targets/runtime-kernel-task-breakdown.md)。

## 1. 总体依赖

```text
v0.1 Basic working code agent
  -> v0.2 Structured trace and tool discipline
  -> v0.3 Evidence, facts, and context projection
  -> v0.4 Effects, transactions, and recovery
  -> v0.5 Harness-native runtime hardening
```

全版本共同前提：Fluxcode externally 是 code agent `Data Plane`；内部 `Control Plane Authority` 仅表示 Fluxcode internal runtime authority，且是逐步形成的目标。

## 2. `v0.1`: Basic Working Code Agent

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-task-spec` | 定义最小 `TaskSpec`：目标、范围、验收条件、非目标 | 无 | 用户任务能被保存为结构化输入 |
| `v0.1-phase-gated-react` | 在现有 query loop 上实现 phase-gated ReAct：phase 内部允许工具循环，phase 结束校验结构化 artifact | `v0.1-task-spec` | `Understand`、`Plan`、`Edit`、`Verify` 能使用 ReAct，但必须产出 schema 合法对象 |
| `v0.1-tool-contract` | 建立基础 tool contract：schema、read-only / mutating、权限需求、风险等级、结果摘要 | `v0.1-phase-gated-react` | 工具调用前可校验，工具结果可记录，危险工具不能裸执行 |
| `v0.1-permission-pipeline` | 建立 allow / deny / ask 权限管线，并把结果写入事件和 trace | `v0.1-tool-contract` | 未授权命令或路径访问会 block / ask，而不是继续执行 |
| `v0.1-repo-read` | 支持文件搜索、读取和上下文摘要 | `v0.1-task-spec` | agent 能定位相关代码、测试和文档 |
| `v0.1-edit-loop` | 支持小范围 patch 生成和应用 | `v0.1-repo-read` | diff 可解释且只覆盖任务相关文件 |
| `v0.1-verification` | 支持运行已声明且允许的验证命令 | `v0.1-edit-loop` | 验证命令、退出码、关键输出被记录 |
| `v0.1-step-trace` | 记录关键步骤、工具调用、修改摘要和验证结果 | `v0.1-repo-read`, `v0.1-verification` | 最终汇报可追溯每个关键行动 |
| `v0.1-handoff` | 输出变更摘要、验证结果、风险和阻塞 | `v0.1-step-trace` | 用户能判断是否接受本次修改 |

### 非目标

- 完整 runtime kernel。
- 完整 `ActionGraph` / `StateStore` / `Scheduler`。
- 完整 OS sandbox。
- 多 agent 并行协作。
- 自动 PR、发布流水线或远程 server。
- 完整 MCP、IDE、TUI、telemetry 或 git automation 对齐。

## 3. `v0.2`: Structured Trace and Tool Discipline

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-trace-schema` | 扩展 `StepTrace`：id、parent、status、inputs、outputs、artifacts | `v0.1-step-trace` | trace 能映射到未来 `ActionNode` |
| `v0.2-capability-descriptor` | 定义基础 capability descriptor | `v0.1-repo-read`, `v0.1-verification` | 文件、搜索、shell、Git、LSP、model 能力有输入输出和风险说明 |
| `v0.2-policy-guard` | 建立最小 guard：路径范围、命令 allowlist、写入范围、危险操作确认 | `v0.2-capability-descriptor` | 越权操作被 reject / ask，而不是继续执行 |
| `v0.2-node-executor-lite` | 将线性步骤执行封装为早期 `NodeExecutor` | `v0.2-trace-schema` | 执行步骤统一产出结构化结果 |
| `v0.2-action-node-seed` | 引入轻量 `ActionNode` 概念作为 trace 的结构化别名 | `v0.2-node-executor-lite` | 不引入复杂 graph，也能表达步骤依赖和状态 |

### 非目标

- 复杂 DAG 调度。
- 完整 policy engine。
- capability marketplace。
- 为了抽象而拆分过多服务。

## 4. `v0.3`: Evidence, Facts, and Context Projection

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-observation-evidence` | 定义 `Observation` 和 `Evidence` 最小模型 | `v0.2-trace-schema` | 工具输出有来源、时间、边界和 artifact 引用 |
| `v0.3-fact-lite` | 定义最小 `Fact` 和 lifecycle | `v0.3-observation-evidence` | verified / user-confirmed claim 才能成为 fact |
| `v0.3-promotion-rule` | 建立基础 promotion rule | `v0.3-fact-lite` | LLM hypothesis 不能直接成为 active fact |
| `v0.3-context-projection` | 用 `ContextProjection` 组织 LLM 输入 | `v0.3-fact-lite` | prompt 中关键材料可追溯到 fact / evidence / hypothesis |
| `v0.3-stale-marking` | 在文件或 overlay 变化时标记相关 fact stale | `v0.3-context-projection` | stale fact 不作为强事实进入 projection |

### 非目标

- 完整知识图谱。
- 把 recall 信号当 verify 结果。
- 为所有文本都建立强事实。

## 5. `v0.4`: Effects, Transactions, and Recovery

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-effect-record` | 定义 `EffectRecord` 并记录 mutating action | `v0.2-capability-descriptor` | 文件写入、shell、Git、外部 API 有 effect 记录 |
| `v0.4-overlay-transaction` | 建立 `OverlayRevision` / transaction 边界 | `v0.4-effect-record` | patch 批次、验证结果和提交状态可关联 |
| `v0.4-transaction-gate` | 提交前检查验证新鲜度、overlay 状态和不可补偿 effect | `v0.4-overlay-transaction`, `v0.3-stale-marking` | stale verification 或 invalid overlay 阻止 commit |
| `v0.4-light-reconciler` | 处理 failed step、partial effect、stale fact、invalidated patch | `v0.4-transaction-gate` | 失败进入 block / repair / ask，而不是静默 retry |
| `v0.4-human-handoff` | 标准化风险、不可恢复副作用和用户确认请求 | `v0.4-light-reconciler` | 用户确认成为可审计 evidence / gate record |

### 非目标

- 无界自治。
- 聊天式 multi-agent fan-out。
- 替代 Git、CI 或 code review。

## 6. `v0.5`: Harness-native Runtime Hardening

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-action-graph` | 将 trace / action node 收敛为正式 `ActionGraph` | `v0.2-action-node-seed`, `v0.4-light-reconciler` | graph 可表达依赖、阻塞、验证和恢复关系 |
| `v0.5-state-store` | 将 evidence / fact lite 收敛为正式 `StateStore` | `v0.3-fact-lite` | fact lifecycle、coverage、confidence 和 evidence refs 可审计 |
| `v0.5-scheduler` | 从线性 runner 演进为 dependency-aware `Scheduler` | `v0.5-action-graph` | guard 未通过或依赖 stale fact 的 node 不会执行 |
| `v0.5-effect-transaction-hardening` | 强化 `EffectLedger` 和 `TransactionManager` 不变量 | `v0.4-overlay-transaction` | mutating action 无法绕过 effect / transaction boundary |
| `v0.5-reconciler` | 覆盖 graph、fact、effect、transaction 四类 reconcile | `v0.5-state-store`, `v0.5-effect-transaction-hardening` | partial effect、stale fact、invalid overlay 都能进入 reconcile |
| `v0.5-invariant-eval` | 建立 runtime invariant 测试和架构 demo | `v0.5-reconciler` | benchmark 报告同时展示任务结果和 runtime invariant 结果 |

### 非目标

- 以 benchmark 通过率替代 runtime invariant。
- 把 Fluxcode 描述为外部 CI / review / deployment gate。
- 让外部协议直接写入内部 `StateStore`、`EffectLedger` 或 `TransactionManager`。

## 7. 跨版本验收清单

- 所有当前正式设计文档保持中英文结构对齐。
- 所有 `Control Plane Authority` 表述限定为 internal runtime authority。
- v0.1 任务能端到端跑通，而不是只完成抽象类型。
- 每个阶段只引入解决当前问题所需的最小 runtime 概念。
- README 只指向当前维护的正式设计文档或明确非设计类调研文档。
