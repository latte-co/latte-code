# Code Agent Evolution Roadmap v0.1-v0.5

## 文档状态

本文定义 Lattecode `v0.1` 至 `v0.5` 的渐进路线。文件名保留 `runtime-kernel-roadmap` 是为了维持现有索引稳定；内容已调整为 code-agent-first：先做面向本地代码仓库工作流的基础可工作 code agent，再让 runtime 结构从 trace、evidence、permission、effect 和 recovery 需求中逐步长出来。

英文对应文档：[`docs/en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../../../en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md)。

## 1. 路线总览

```text
v0.1 Basic Working Code Agent
  -> v0.2 Structured Trace and Tool Discipline
  -> v0.3 Evidence, Facts, and Context Projection
  -> v0.4 Controlled Effects, Transactions, and Recovery
  -> v0.5 Internal Runtime Hardening
```

参考系：Lattecode externally 是 code agent `Data Plane`；`Control Plane Authority` 仅指 Lattecode internal runtime authority，并且只在 runtime 结构逐步形成后成立。

## 2. 核心原则

- 先证明 Lattecode 能作为 code agent 完成真实代码任务，再沉淀 runtime 抽象。
- v0.1 不要求完整 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager` 或 `Reconciler`。
- 从第一天起保留可追溯执行记录，避免未来无法演进。
- 工具调用、文件修改和验证结果必须能被复盘。
- LLM 可以参与理解、计划和修改，但不应成为长期事实来源或未受约束的工具执行者。
- 完整内部 runtime 结构是长期演进方向，不是 MVP 的复杂度起点。

## 3. 版本表

| Version | Theme | 主要目标 | 不做什么 |
| --- | --- | --- | --- |
| `v0.1` | Basic Working Code Agent | 完成 contract-first code agent 最小闭环：CLI、config、`AGENTS.md`、session、agent-loop、tools、minimal MCP、local skills、local commands、permission、evidence / trace、handoff | 不做完整 runtime kernel、并行调度、复杂事实系统或完整生态平台 |
| `v0.2` | Structured Trace and Tool Discipline | 把执行步骤结构化为可追溯 task trace，建立基础 capability 边界 | 不把 trace 过早复杂化为完整 graph 平台 |
| `v0.3` | Evidence, Facts, and Context Projection | 区分 observation、evidence、fact，引入最小 context projection | 不把模型推断直接当事实 |
| `v0.4` | Controlled Effects, Transactions, and Recovery | 强化 extension / effect / transaction 边界，引入 `EffectRecord`、overlay / transaction lite、transaction gate 和恢复 / reconcile 语义，使 mutating action 不再只靠自然语言记录 | 不把 ecosystem、MCP、plugin、skills、hooks、LSP 或 TUI 替代为 runtime 主线；MCP / skills 不是本阶段首次出现 |
| `v0.5` | Internal Runtime Hardening | 收敛 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler` 的 runtime 不变量 | 不以 benchmark 通过率替代架构不变量 |

## 3.1 TUI / renderer 演进线

本文重新建立 TUI / output decisions，作为 roadmap 中的 renderer 演进线。该演进线不扩大 `v0.1` release acceptance，也不表示当前 `src/` 已实现这些能力。

| Version | TUI / renderer posture |
| --- | --- |
| `v0.1` | release-critical path 仍是 headless JSON / text `AgentHandoff`；不交付正式 TUI / IDE cockpit；保留 `--output json` / `--output text`；如出现 `--ui tui` 或 experimental command，只能 opt-in，且不能改变 handoff schema。 |
| `v0.2` | 定义 renderer-neutral `TuiViewModel` 与 `PlainTextRenderer` fallback；可启动 Ink experimental PoC，但 UI 只消费 runtime events / handoff，不驱动 permission、session 或 runtime mutation。 |
| `v0.3` | 在 structured trace、evidence refs 和 context projection 稳定后，扩展 TUI view model 的 trace / evidence 展示能力；增加 streaming、backpressure、1k / 10k events、resize、stdout / stderr mixed output、Ctrl+C / crash terminal restore、snapshot fallback 等 PoC 验收。 |
| `v0.4+` | 只设置 `OpenTUI` adapter evaluation gate，评估 cockpit density、install burden、native build reliability 和 fallback behavior；不把 `OpenTUI` 写成 `v0.4` 必交付项、默认依赖或 release deliverable。 |
| `v0.5+` | 如果 `ActionGraph` 成为实际 UX surface，再考虑 cockpit hardening candidate；仍保留 headless JSON / text 和 `PlainTextRenderer` recovery path。 |

通用边界：runtime core 不 import `react`、`ink`、`@opentui/*`；主 runtime 不引入 Bun / Zig / native build chain；TUI 不成为唯一输出通道，也不能绕过 schema、permission、evidence、trace 或 handoff。

## 4. `v0.1`: Basic Working Code Agent

### 目标

`v0.1` 应证明 Lattecode 能在本地仓库中完成一个真实代码任务：理解上下文、修改文件、运行验证、汇报结果。

实现形态应以 Claude Code 类系统的 conversation-native query loop 为基础，但增加 Lattecode 自己的 phase artifact boundary。也就是说，模型仍通过 ReAct 与工具交互；阶段完成必须提交 `TaskSpec`、`ContextPack`、`ChangePlan`、`PatchSummary`、`VerificationResult` 或 `AgentHandoff` 等结构化对象。

### 必须建立的能力

- CLI：`lattecode run` / resume / show / list 等最小 headless 入口。
- Config：project-local JSONC 配置覆盖 models、runtime、tools、permissions、session、commands、skills、MCP，且不存 secrets。
- `AGENTS.md` loader：读取 repo root / cwd 边界内约束，记录 snapshot/hash，进入 context snapshot。
- Session lifecycle：create / list / show / resume；stable session id；cwd / repo root 固定；`TaskRunState.status` 仅使用 `queued`、`running`、`waiting_permission`、`blocked`、`failed`、`completed`。
- `TaskSpec`：记录用户目标、范围、验收条件和非目标。
- Phase-gated ReAct：每个阶段内部保留 model / tool / observation 循环，但受预算、工具 allowlist 和 artifact schema 约束。
- Agent loop / phase runner：主链路为 `CLI / local command / local skill / minimal MCP bridge / built-in tools -> TaskSpec -> Session / TaskRunState -> ContextPack -> AgentLoop / PhaseRunner -> PermissionDecision -> Evidence / StepTrace -> AgentHandoff`。
- Tool contract：工具声明 schema、read-only / mutating、权限需求、风险等级和结果摘要。
- Built-in tools：read/search/edit/write/shell/manifest/minimal diff summary 的最小可用集合；`v0.1` 的 diff 只覆盖 changed files / diff summary，用于 `AgentHandoff` 和安全回顾。
- Minimal MCP bridge：config-defined servers；list/call tools；统一走 permission、evidence、trace、session；默认 disabled 或 explicit enabled；不做 marketplace、resource / prompt platform 或 server management UI；不能绕过权限。
- Local skill loader：本地 instruction / workflow / command bundle；注入 context / prompt registry；不做 hub、install、publish、marketplace；不能直接执行 side effects。
- Local command specs：built-in / local command 统一 route through `TaskSpec`、phase event 和 session，不绕过 agent-loop / permission / session。
- Permission pipeline：工具执行前完成 allow / deny / ask 判定，并记录决策。
- 仓库搜索与读取：能定位相关文件、测试和文档。
- 编辑能力：能生成并应用小范围 patch。
- 验证能力：能运行已声明且用户允许的命令，例如 `npm test`。
- `StepTrace`：记录关键步骤、工具调用、修改摘要和验证结果。
- Handoff：输出变更摘要、验证证据、风险和阻塞。
- Output contract：`AgentHandoff` 通过 JSON / text headless 输出保持稳定，TUI 不进入 `v0.1` 验收。

### 验收方向

- 能完成至少一个端到端代码修改任务。
- `lattecode run "实现一个贪吃蛇游戏"` 这类命令能够进入真实 agent loop；如果目标仓库具备应用和测试基础，应产出代码、测试和验证结果。
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
- `TuiViewModel` / `PlainTextRenderer` 的 renderer-neutral contract：为后续 TUI PoC 提供只读投影与 fallback，不改变 runtime 权限。

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
- TUI PoC 验收：若存在 Ink experimental path，必须覆盖 Node20 / Node22 matrix、non-TTY fallback、streaming / backpressure、1k / 10k events、resize、stdout / stderr 混合输出、Ctrl+C / crash terminal restore 和 snapshot tests。

### 验收方向

- 工具输出不会自动成为 `Fact`。
- LLM hypothesis 与 verified fact 能被区分。
- stale 或不确定材料不会作为强事实进入 prompt。

## 7. `v0.4`: Controlled Effects, Transactions, and Recovery

`v0.4` 的 runtime 主线与正式架构和 runtime-evolution 模块保持一致：在 `v0.1` 至 `v0.3` 的 tool、trace、evidence、fact、context 和 permission 基础上，引入副作用声明、overlay / transaction lite、transaction gate 和基础恢复 / reconcile 语义。目标是让 mutating action 可审计、可阻断、可恢复或可交给人，而不是把扩展生态作为本阶段主交付。

MCP、skills、commands 在 `v0.1` 已作为最小入口或桥接能力出现。`v0.4` 不把它们作为首次交付项，而是 harden extension / effect / transaction boundaries：任何 MCP、plugin、skills、hooks、LSP 或类似能力都必须进入同一 capability schema、permission、evidence、trace、effect 和 transaction gate 管线，不能绕过 runtime 主线，也不能取代 `EffectLedger`、`TransactionManager` 或 `Reconciler` 的演进。

### 必须建立的能力

- `EffectRecord`：mutating action 执行前有 planned effect，执行后记录 observed effect、状态和补偿可能性。
- `OverlayRevision` / transaction lite：把 patch 批次、effect ids、验证新鲜度和 rollback handle 绑定到同一 transaction boundary。
- `transaction_gate`：commit 前检查验证新鲜度、overlay 状态、不可补偿 effect 的 approval 状态和 rollback 条件。
- Recovery / reconcile boundary：partial effect、failed effect、stale fact、invalid overlay 或 rollback handle 缺失时，进入 blocked / needs_reconcile / human handoff，而不是继续自动执行。
- Compatibility / extension boundary：MCP、plugin、skills、hooks、LSP 等外部 capability 如被引入，必须转换为 Lattecode `CapabilityDescriptor`，并进入 validation、permission、evidence、trace、effect 和 transaction 管线；只读 LSP 可作为低风险 compatibility lane，code action 写入后置。
- `OpenTUI` adapter evaluation gate：仅作为 `v0.4+` side gate 评估 future cockpit / `ActionGraph` surface 需求、安装负担、native build 可靠性和 fallback 行为；不作为 `v0.4` release 必交付项、默认依赖或 renderer 选择。

### 验收方向

- 文件、shell、Git 和外部 API 等 mutating action 执行前有 effect declaration，执行后有可审计状态。
- stale verification、invalid overlay、缺少 approval 的不可补偿 effect 会阻断 commit。
- partial / failed effect 能进入 recover、reconcile 或 human handoff，而不是只写入自然语言日志。
- 外部 capability 不能绕过 schema、permission、evidence、trace、effect 或 transaction gate；被禁用的外部工具对模型不可见，且不能通过名称调用。
- 外部结果必须有截断、引用和 evidence 记录，不能直接成为 `Fact`。

## 8. `v0.5`: Internal Runtime Hardening

`v0.5` 将前面阶段沉淀的 trace、evidence、context、effect、transaction、recovery 和受控 extension boundary 收敛为完整内部 runtime。副作用、事务和恢复在 `v0.4` 已进入主线；本阶段负责把它们与 `ActionGraph`、`StateStore`、`Scheduler` 和 `Reconciler` 一起硬化为 runtime 不变量。

### 必须建立的能力

- `ActionGraph` 成为执行账本、调度表面、恢复入口和 UX 表面。
- `StateStore` 管理 `Observation`、`Evidence`、versioned `Fact`。
- `Scheduler` 基于依赖、gate、预算和恢复状态运行节点。
- `EffectRecord`、`OverlayRevision`、`EffectLedger` 与 `TransactionManager` 管理文件写入、shell、Git、外部 capability 等副作用和事务。
- `Reconciler` 覆盖 graph、fact、effect、transaction 四类失配；extension adapter 问题按既有 graph / effect / transaction reconcile 分类处理，或作为 `v0.4+` extension hardening 输入，不新增 `ReconcileDecision.kind`。
- 如果 `ActionGraph` 成为实际 UX surface，再评估 cockpit hardening candidate；`OpenTUI` 仍不是默认依赖或 release-critical output path。

### 验收方向

- runtime invariant 可被测试和解释。
- 用户能理解 blocked reason、风险和恢复选项。
- 外部系统信号只能通过 adapter / evidence / gate 进入 runtime。
- mutating effect 无法绕过 effect ledger、transaction gate 和 permission record。

## 9. 跨版本不变量

- Lattecode externally remains a code-agent `Data Plane`。
- `Control Plane Authority` 必须限定为 internal runtime authority。
- v0.1 优先交付基础可工作 agent，不追求完整 runtime。
- 每个阶段只引入当前问题所需的最小抽象。
- 执行记录、工具调用、文件修改、验证结果必须可追溯。
- `v0.1` 的 MCP、skills、commands 只交付最小桥接 / 本地加载 / 本地规格；完整 MCP platform、marketplace、resource / prompt ecosystem、skill hub 和 command marketplace 不进入 `v0.1`。
- Ecosystem、MCP、plugin、skills、hooks、LSP 和 TUI / cockpit 后续只能作为 compatibility / extension / renderer side lane，不得替代 runtime v0.4 的 effects / transactions / recovery 主线。
- 代码实现 runtime 概念时必须同步更新相应设计文档；正式中英文文档必须保持结构和语义对齐。
