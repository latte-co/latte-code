# Code Agent Evolution Roadmap v0.1-v0.5

## 文档状态

本文定义 Fluxcode `v0.1` 至 `v0.5` 的渐进路线。文件名保留 `runtime-kernel-roadmap` 是为了维持现有索引稳定；内容已调整为 code-agent-first：先做基础可工作的 local-first code agent，再逐步演进到 harness-native runtime。

英文对应文档：[`docs/en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../../../en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md)。

## 1. 路线总览

```text
v0.1 Basic Working Code Agent
  -> v0.2 Structured Trace and Tool Discipline
  -> v0.3 Evidence, Facts, and Context Projection
  -> v0.4 Effects, Transactions, and Recovery
  -> v0.5 Harness-native Runtime Hardening
```

参考系：Fluxcode externally 是 code agent `Data Plane`；`Control Plane Authority` 仅指 Fluxcode internal runtime authority，并且只在 runtime 结构逐步形成后成立。

## 2. 核心原则

- 先证明 Fluxcode 能作为 code agent 完成真实代码任务，再沉淀 runtime 抽象。
- v0.1 不要求完整 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager` 或 `Reconciler`。
- 从第一天起保留可追溯执行记录，避免未来无法演进。
- 工具调用、文件修改和验证结果必须能被复盘。
- LLM 可以参与理解、计划和修改，但不应成为长期事实来源或未受约束的工具执行者。
- Harness-native runtime 是演进方向，不是 MVP 的复杂度起点。

## 3. 版本表

| Version | Theme | 主要目标 | 不做什么 |
| --- | --- | --- | --- |
| `v0.1` | Basic Working Code Agent | 完成任务输入、仓库理解、编辑、验证、交付的最小闭环 | 不做完整 runtime kernel、并行调度、复杂事实系统 |
| `v0.2` | Structured Trace and Tool Discipline | 把执行步骤结构化为可追溯 task trace，建立基础 capability 边界 | 不把 trace 过早复杂化为完整 graph 平台 |
| `v0.3` | Evidence, Facts, and Context Projection | 区分 observation、evidence、fact，引入最小 context projection | 不把模型推断直接当事实 |
| `v0.4` | Effects, Transactions, and Recovery | 管理 mutating effect、patch transaction、验证新鲜度和失败恢复 | 不追求无界自治或复杂 multi-agent |
| `v0.5` | Harness-native Runtime Hardening | 收敛 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler` 的 runtime 不变量 | 不以 benchmark 通过率替代架构不变量 |

## 4. `v0.1`: Basic Working Code Agent

### 目标

`v0.1` 应证明 Fluxcode 能在本地仓库中完成一个真实代码任务：理解上下文、修改文件、运行验证、汇报结果。

实现形态应以 Claude Code 类系统的 conversation-native query loop 为基础，但增加 Fluxcode 自己的 phase artifact boundary。也就是说，模型仍通过 ReAct 与工具交互；阶段完成必须提交 `TaskSpec`、`ContextPack`、`ChangePlan`、`PatchSummary`、`VerificationResult` 或 `AgentHandoff` 等结构化对象。

### 必须建立的能力

- `TaskSpec`：记录用户目标、范围、验收条件和非目标。
- Phase-gated ReAct：每个阶段内部保留 model / tool / observation 循环，但受预算、工具 allowlist 和 artifact schema 约束。
- Tool contract：工具声明 schema、read-only / mutating、权限需求、风险等级和结果摘要。
- Permission pipeline：工具执行前完成 allow / deny / ask 判定，并记录决策。
- 仓库搜索与读取：能定位相关文件、测试和文档。
- 编辑能力：能生成并应用小范围 patch。
- 验证能力：能运行已声明且用户允许的命令，例如 `npm test`。
- `StepTrace`：记录关键步骤、工具调用、修改摘要和验证结果。
- Handoff：输出变更摘要、验证证据、风险和阻塞。

### 验收方向

- 能完成至少一个端到端代码修改任务。
- `fluxcode run "实现一个贪吃蛇游戏"` 这类命令能够进入真实 agent loop；如果目标仓库具备应用和测试基础，应产出代码、测试和验证结果。
- 如果仓库缺少实现目标所需的框架、测试或依赖决策，应明确请求用户确认，不能静默 scaffold 或安装依赖。
- 每次工具调用和文件修改有可读记录。
- 验证命令、结果和失败信息被记录。
- 用户能从最终汇报判断改了什么、为什么改、是否验证。

## 5. `v0.2`: Structured Trace and Tool Discipline

`v0.2` 把 v0.1 的执行日志结构化，使其可以自然演进为 `ActionGraph` 和 `ActionNode`。

### 必须建立的能力

- `StepTrace` 增加 step id、parent、status、inputs、outputs。
- 基础 `CapabilityDescriptor`：声明文件、搜索、shell、Git、LSP、模型调用等能力的输入输出和风险。
- 简单 `PolicyGuard`：阻止越权路径、未声明命令、无关大范围修改。
- `NodeExecutor` 的早期形态：线性执行 task steps。

### 验收方向

- 每个重要步骤能映射到未来 `ActionNode`。
- 工具调用不再只是自由文本日志。
- 失败步骤有明确 status 和原因。

## 6. `v0.3`: Evidence, Facts, and Context Projection

`v0.3` 解决上下文可信度问题，让 agent 不再把所有 transcript 内容都当成事实。

### 必须建立的能力

- `Observation`：保存工具、用户或环境产生的原始观察。
- `Evidence`：保存带来源、时间、范围和 artifact 引用的证据。
- 最小 `Fact`：只表达经过验证或用户确认的 claim。
- `ContextProjection`：为当前步骤选择最小上下文，并标记来源和不确定性。

### 验收方向

- 工具输出不会自动成为 `Fact`。
- LLM hypothesis 与 verified fact 能被区分。
- stale 或不确定材料不会作为强事实进入 prompt。

## 7. `v0.4`: Effects, Transactions, and Recovery

`v0.4` 处理真实工程修改中的副作用和恢复问题。

### 必须建立的能力

- `EffectRecord`：记录文件写入、shell、Git、外部 API 等 mutating action。
- `OverlayRevision` / transaction：把一组 patch 和验证结果绑定起来。
- transaction gate：验证过期、overlay 失效或不可补偿 effect 时阻止提交。
- 轻量 `Reconciler`：处理 failed step、partial effect、stale fact、invalidated patch。

### 验收方向

- mutating action 有 effect 记录。
- patch 提交前能解释验证是否仍新鲜。
- 失败不能只靠 prompt retry 掩盖。

## 8. `v0.5`: Harness-native Runtime Hardening

`v0.5` 将前面阶段沉淀的对象收敛为 harness-native runtime。

### 必须建立的能力

- `ActionGraph` 成为执行账本、调度表面、恢复入口和 UX 表面。
- `StateStore` 管理 `Observation`、`Evidence`、versioned `Fact`。
- `Scheduler` 基于依赖、gate、预算和恢复状态运行节点。
- `EffectLedger` 与 `TransactionManager` 管理副作用和事务。
- `Reconciler` 覆盖 graph、fact、effect、transaction 四类失配。

### 验收方向

- runtime invariant 可被测试和解释。
- 用户能理解 blocked reason、风险和恢复选项。
- 外部系统信号只能通过 adapter / evidence / gate 进入 runtime。

## 9. 跨版本不变量

- Fluxcode externally remains a code-agent `Data Plane`。
- `Control Plane Authority` 必须限定为 internal runtime authority。
- v0.1 优先交付基础可工作 agent，不追求完整 runtime。
- 每个阶段只引入当前问题所需的最小抽象。
- 执行记录、工具调用、文件修改、验证结果必须可追溯。
- 代码实现 runtime 概念时必须同步更新相应设计文档；正式中英文文档必须保持结构和语义对齐。
