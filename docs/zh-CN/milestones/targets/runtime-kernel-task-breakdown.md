# Code Agent Evolution 任务拆分 v0.1-v0.5

## 文档状态

本文是 Lattecode 从基础 code agent 演进到 harness-native runtime 的独立任务拆分文档。文件名保留 `runtime-kernel-task-breakdown` 是为了维持现有索引稳定；内容已调整为渐进式实现排期。

英文对应文档：[`docs/en-US/milestones/targets/runtime-kernel-task-breakdown.md`](../../../en-US/milestones/targets/runtime-kernel-task-breakdown.md)。

## 1. 总体依赖

```text
v0.1 Basic working code agent
  -> v0.2 Structured trace and tool discipline
  -> v0.3 Evidence, facts, and context projection
  -> v0.4 Controlled effects, transactions, and recovery
  -> v0.5 Harness-native runtime hardening
```

全版本共同前提：Lattecode externally 是 code agent `Data Plane`；内部 `Control Plane Authority` 仅表示 Lattecode internal runtime authority，且是逐步形成的目标。

## 2. `v0.1`: Basic Working Code Agent

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-cli-config-contract` | 锁定 CLI、config 和 `TaskSpec` contract：`run` / resume / show / list 最小入口，project-local JSONC config，命令行参数进入 `TaskSpec` | 无 | CLI / config 能生成结构化任务输入，不依赖 TUI 或全局 secrets |
| `v0.1-agents-loader` | 实现最小 `AGENTS.md` loader：repo root / cwd 边界、snapshot/hash、pinned constraints | `v0.1-cli-config-contract` | `AGENTS.md` 约束进入 context snapshot，可追溯且不可读时有明确语义 |
| `v0.1-session-management` | 建立 local session management：create / list / show / resume、stable session id、固定 cwd / repo root、`TaskRunState.status` | `v0.1-cli-config-contract` | 支持 `queued` / `running` / `waiting_permission` / `blocked` / `failed` / `completed`，resume 语义明确 |
| `v0.1-phase-gated-react` | 在现有 query loop 上实现 phase-gated ReAct：phase 内部允许工具循环，phase 结束校验结构化 artifact | `v0.1-cli-config-contract`, `v0.1-session-management` | `Understand`、`Plan`、`Edit`、`Verify` 能使用 ReAct，但必须产出 schema 合法对象 |
| `v0.1-built-in-tools` | 建立 built-in tools 最小集合：read/search/edit/write/shell/manifest/minimal diff summary，并保留 tool contract | `v0.1-phase-gated-react` | 工具声明 schema、read-only / mutating、权限需求、风险等级、结果摘要；P0 diff 只输出 changed files / diff summary，危险工具不能裸执行 |
| `v0.1-permission-pipeline` | 建立 allow / deny / ask 权限管线，并把结果写入事件和 trace | `v0.1-built-in-tools` | 未授权命令或路径访问会 block / ask，而不是继续执行 |
| `v0.1-minimal-mcp-bridge` | 实现最小 MCP bridge：config-defined servers、list/call tools、默认 disabled 或 explicit enabled | `v0.1-built-in-tools`, `v0.1-permission-pipeline` | MCP tool 统一映射到 Lattecode tool contract，进入 permission / evidence / trace / session，不能绕过权限 |
| `v0.1-local-skill-loader` | 实现 local skill loader：加载本地 instruction / workflow / command bundle，注入 context / prompt registry | `v0.1-agents-loader`, `v0.1-session-management` | skill 不能直接执行 side effects；不做 hub、install、publish、marketplace |
| `v0.1-local-command-specs` | 实现 built-in / local command specs：command -> `TaskSpec` / phase event / session | `v0.1-cli-config-contract`, `v0.1-session-management` | command 不绕过 agent-loop、permission 或 session |
| `v0.1-evidence-trace-binding` | 绑定 `StepTrace`、`Evidence`、tool invocation、shell output summary、file edit summary、verification result | `v0.1-permission-pipeline`, `v0.1-minimal-mcp-bridge`, `v0.1-local-skill-loader`, `v0.1-local-command-specs` | 最终汇报可追溯每个关键行动，工具、文件修改和验证结果有证据引用 |
| `v0.1-output-contract` | 重新建立 output contract：`--output json` / `--output text` 保留，release-critical path 是 headless `AgentHandoff` | `v0.1-evidence-trace-binding` | TUI 不进入 `v0.1` release acceptance；non-TTY / CI / redirect 可稳定获得 JSON / text handoff |
| `v0.1-agent-handoff` | 输出结构化 `AgentHandoff`：变更摘要、验证结果、风险、阻塞、required user decisions、trace / evidence refs | `v0.1-output-contract`, `v0.1-evidence-trace-binding` | 用户能判断是否接受本次修改；失败、阻塞、跳过验证不会被报告为成功 |

### 非目标

- 完整 runtime kernel。
- 完整 `ActionGraph` / `StateStore` / `Scheduler`。
- 完整 OS sandbox。
- 多 agent 并行协作。
- 自动 PR、发布流水线或远程 server。
- 完整 MCP platform、marketplace、resource / prompt ecosystem、skill hub、command marketplace、IDE、正式 TUI / cockpit、telemetry 或 git automation 对齐。
- cloud sync、多设备、多用户 session、完整 branch / fork graph、long-term memory 或 full `ActionGraph` persistence。
- TUI 作为唯一输出通道，或 TUI 绕过 schema、permission、evidence、trace、handoff。

## 3. `v0.2`: Structured Trace and Tool Discipline

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-trace-schema` | 扩展 `StepTrace`：id、parent、status、inputs、outputs、artifacts | `v0.1-evidence-trace-binding` | trace 能映射到未来 `ActionNode` |
| `v0.2-capability-descriptor` | 定义基础 capability descriptor | `v0.1-built-in-tools`, `v0.1-evidence-trace-binding` | 文件、搜索、shell、Git、LSP、model 能力有输入输出和风险说明 |
| `v0.2-policy-guard` | 建立最小 guard：路径范围、命令 allowlist、写入范围、危险操作确认 | `v0.2-capability-descriptor` | 越权操作被 reject / ask，而不是继续执行 |
| `v0.2-node-executor-lite` | 将线性步骤执行封装为早期 `NodeExecutor` | `v0.2-trace-schema` | 执行步骤统一产出结构化结果 |
| `v0.2-action-node-seed` | 引入轻量 `ActionNode` 概念作为 trace 的结构化别名 | `v0.2-node-executor-lite` | 不引入复杂 graph，也能表达步骤依赖和状态 |
| `v0.2-tui-view-model-contract` | 定义 `runtime event stream -> stable TuiViewModel -> renderer adapters` 契约 | `v0.1-output-contract`, `v0.2-trace-schema` | TUI view model 只读消费 runtime events / handoff，不驱动 permission、session 或 runtime mutation |
| `v0.2-plaintext-renderer` | 实现或规范 `PlainTextRenderer` fallback | `v0.2-tui-view-model-contract` | non-TTY、CI、snapshot、crash fallback 不依赖 TUI dependency |
| `v0.2-ink-experimental-poc` | 建立 Ink experimental PoC gate：optional / lazy / experimental package，必要时 Node gate | `v0.2-tui-view-model-contract`, `v0.2-plaintext-renderer` | Node20 / Node22 matrix、non-TTY fallback、streaming / backpressure、1k / 10k events、resize、stdout / stderr mixed output、Ctrl+C / crash restore、snapshot tests 通过后才可称为 PoC 可用 |

### 非目标

- 复杂 DAG 调度。
- 完整 policy engine。
- capability marketplace。
- 为了抽象而拆分过多服务。
- 在 runtime core 中 import `react`、`ink` 或 `@opentui/*`。

## 4. `v0.3`: Evidence, Facts, and Context Projection

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-observation-evidence` | 定义 `Observation` 和 `Evidence` 最小模型 | `v0.2-trace-schema` | 工具输出有来源、时间、边界和 artifact 引用 |
| `v0.3-fact-lite` | 定义最小 `Fact` 和 lifecycle | `v0.3-observation-evidence` | verified / user-confirmed claim 才能成为 fact |
| `v0.3-promotion-rule` | 建立基础 promotion rule | `v0.3-fact-lite` | LLM hypothesis 不能直接成为 active fact |
| `v0.3-context-projection` | 用 `ContextProjection` 组织 LLM 输入 | `v0.3-fact-lite` | prompt 中关键材料可追溯到 fact / evidence / hypothesis |
| `v0.3-stale-marking` | 在文件、验证结果或本地编辑记录变化时标记相关 fact stale | `v0.3-context-projection` | stale fact 不作为强事实进入 projection |

### 非目标

- 完整知识图谱。
- 把 recall 信号当 verify 结果。
- 为所有文本都建立强事实。

## 5. `v0.4`: Controlled Effects, Transactions, and Recovery

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-effect-record` | 定义 `EffectRecord`：planned / observed / effective effect、状态、补偿可能性和 action 关联 | `v0.2-capability-descriptor`, `v0.3-observation-evidence` | 文件、shell、Git、外部 API 等 mutating action 执行前有 planned effect，执行后有 observed effect |
| `v0.4-transaction-lite` | 定义 `OverlayRevision` / transaction lite：patch refs、effect ids、verification ids、rollback handle、transaction status | `v0.4-effect-record`, `v0.3-stale-marking` | patch、effect 和验证新鲜度能绑定到同一 transaction boundary |
| `v0.4-transaction-gate` | 建立 commit 前 gate：验证新鲜度、overlay 状态、不可补偿 effect approval、rollback 条件 | `v0.4-transaction-lite`, `v0.2-policy-guard` | stale verification、invalid overlay 或缺少 approval 的不可补偿 effect 会阻断 commit |
| `v0.4-recovery-handoff` | 定义 partial / failed effect、rollback handle 缺失、stale fact 的 blocked / human handoff 语义 | `v0.4-transaction-gate` | 失败不只写入自然语言日志；用户能看到恢复选项和接管原因 |
| `v0.4-reconcile-lite` | 建立 effect、transaction、fact stale 三类最小 reconcile 入口 | `v0.4-recovery-handoff` | partial effect、invalid overlay、stale fact 能进入 needs_reconcile，而不是继续自动执行 |
| `v0.4-extension-boundary-side-lane` | 将 MCP / plugin / skills / hooks / LSP 保留为 compatibility / extension boundary，转换为 `CapabilityDescriptor` 并接入同一 validation、permission、evidence、trace、effect、transaction 管线 | `v0.4-effect-record`, `v0.4-transaction-gate` | 外部 capability 不能绕过 runtime v0.4 effects / transactions / recovery 主线；只读 LSP 可保留，code action 写入后置 |
| `v0.4-opentui-adapter-evaluation-gate` | 以 `v0.4+` side gate 评估 `OpenTUI` 作为 future cockpit / `ActionGraph` surface adapter candidate 的条件 | `v0.2-tui-view-model-contract` | 只形成评估结论：cockpit density、安装负担、native build 可靠性、fallback behavior；不得把 `OpenTUI` 写入 `v0.4` 主 runtime 必交付、默认依赖或 release deliverable |

### 非目标

- marketplace。
- remote execution。
- 聊天式 multi-agent fan-out。
- 无约束任意插件代码执行。
- 完整 IDE/TUI 产品形态。
- 主 runtime 引入 Bun / Zig / native build chain。
- 用 ecosystem / MCP / plugin / skills / hooks / LSP 替代 effects / transactions / recovery 主线。

## 6. `v0.5`: Harness-native Runtime Hardening

### 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-action-graph` | 将 trace / action node 收敛为正式 `ActionGraph` | `v0.2-action-node-seed`, `v0.4-reconcile-lite` | graph 可表达依赖、阻塞、验证、外部 capability 和恢复关系 |
| `v0.5-state-store` | 将 evidence / fact lite 收敛为正式 `StateStore` | `v0.3-fact-lite` | fact lifecycle、coverage、confidence 和 evidence refs 可审计 |
| `v0.5-scheduler` | 从线性 runner 演进为 dependency-aware `Scheduler` | `v0.5-action-graph` | guard 未通过或依赖 stale fact 的 node 不会执行 |
| `v0.5-effect-transaction-hardening` | 将 `v0.4` 的 `EffectRecord` / `OverlayRevision` / transaction gate 强化为 `EffectLedger` 和 `TransactionManager` 不变量 | `v0.4-effect-record`, `v0.4-transaction-lite`, `v0.4-transaction-gate`, `v0.4-extension-boundary-side-lane` | 文件、shell、Git、外部 capability 的 mutating action 无法绕过 effect / transaction boundary |
| `v0.5-reconciler` | 覆盖 graph、fact、effect、transaction 四类 reconcile；extension adapter 问题按既有 graph / effect / transaction 分类，或作为 extension hardening 输入 | `v0.5-state-store`, `v0.5-effect-transaction-hardening`, `v0.4-reconcile-lite` | partial effect、stale fact、invalid overlay 能进入 reconcile；extension failure 不新增 `ReconcileDecision.kind` |
| `v0.5-invariant-eval` | 建立 runtime invariant 测试和架构 demo | `v0.5-reconciler` | benchmark 报告同时展示任务结果和 runtime invariant 结果 |

### 非目标

- 以 benchmark 通过率替代 runtime invariant。
- 把 Lattecode 描述为外部 CI / review / deployment gate。
- 让外部协议直接写入内部 `StateStore`、`EffectLedger` 或 `TransactionManager`。
- 在 `ActionGraph` 成为实际 UX surface 前，把 cockpit hardening 或 `OpenTUI` 设为 `v0.5` 默认依赖。

## 7. 跨版本验收清单

- 所有当前正式设计文档保持中英文结构对齐。
- 所有 `Control Plane Authority` 表述限定为 internal runtime authority。
- v0.1 任务能端到端跑通，而不是只完成抽象类型。
- 每个阶段只引入解决当前问题所需的最小 runtime 概念。
- README 只指向当前维护的正式设计文档或明确非设计类调研文档。
