# Harness-native Code Agent 设计建议

## 设计目标

Harness-native code agent 的目标，是把 code agent 从 message-driven tool runner 演进为 graph-driven execution runtime。

它不只是“支持更多 agent”或“把 prompt 写进 harness”，而是让 graph 成为执行、恢复、审计和人工接管的主状态。

## 设计建议与调研事实的边界

- 调研事实见 [Code Agent 横向调研](../research/code-agent-survey.md)。
- 本文件是基于调研事实提出的目标架构、接口模型和设计约束，属于设计建议。
- MVP 的阶段切法、非目标和验收标准见 [MVP 切法与路线图](./mvp-roadmap.md)。

## 核心模型

| 模型 | 含义 |
| --- | --- |
| Graph as source of truth | graph 记录任务结构、依赖、状态、gate、evidence 和 reconcile 结果 |
| Agent as node executor | agent 负责执行一个 node，而不是拥有全局任务状态 |
| Tool result as evidence | 工具结果被规范化为 evidence，可引用、可审计、可恢复 |
| Gate as checkpoint | gate 是显式 checkpoint，可阻塞、放行或要求修复 |
| Reconcile as first-class lifecycle | reconcile 是生命周期阶段，不是下一轮 prompt 的隐式副作用 |
| Multi-agent as scheduling, not chat fan-out | multi-agent 是 scheduler 分配 node executor，不是多个对话并发发散 |

## 与传统 code agent 的差异

传统 code agent：

```text
用户消息 → 模型 → 工具 → 回复
```

Harness-native code agent：

```text
harness graph
→ scheduler 选择 ready node
→ agent executor 执行
→ tool 产出 evidence
→ node result 写回
→ reconciler 更新 graph/gate
→ TUI/API 展示和接管
```

关键变化：

- 顶层控制器从 query loop 变为 scheduler；
- agent 的职责从“处理整段对话”变为“执行 node contract”；
- tool result 从上下文材料变为 evidence record；
- gate/reconcile 从人工约定变为 runtime 状态；
- TUI/API 展示对象从消息流变为 graph cockpit。

## 推荐分层

### 1. Runtime Kernel

负责执行生命周期的最小内核：加载 GraphState、选择 ready node、调用 executor、写回结果、触发 reconcile。

不应包含具体工具实现、具体 agent prompt 或 UI 细节。它需要管理权限边界、取消、错误传播和资源清理，但这些职责都应围绕 graph lifecycle 展开。

### 2. Tool / MCP Capability Layer

统一内置工具、MCP 工具、插件能力和外部能力。

基础要求：

- schema；
- permission；
- read-only / mutating 标识；
- concurrency；
- output truncation；
- evidence mapping；
- audit metadata。

### 3. Skill / Agent Registry

声明可用 agent、skill、executor profile。

每个 executor 需要明确：

- 能处理的 node 类型；
- 输入 contract；
- 输出 node result 格式；
- 可用工具范围；
- 权限策略；
- evidence 生产规则。

### 4. Graph-driven Agent Loop

负责单个 node 的执行。它可以复用 conversation-native query loop，但输出必须回到 node result 与 evidence，而不是只追加 transcript。

### 5. Harness Graph / State

GraphState 是主状态，至少包含：

- pending / running / completed / failed / blocked nodes；
- node dependencies；
- node contract；
- node result；
- evidence records；
- gates；
- reconcile history；
- dispatch control；
- run metadata。

### 6. Scheduler

根据 GraphState 选择 ready node，并决定使用哪个 executor。

调度输入包括：

- dependency 是否满足；
- gate 是否放行；
- executor capability；
- permission / risk level；
- concurrency budget；
- human confirmation state。

Scheduler 根据 GraphState 决定下一步，而不是根据聊天上下文推断下一步。

### 7. Persistence / Recovery

持久化不只保存 transcript，而要保存 graph lifecycle。

至少需要：

- append-only event log；
- GraphState snapshot；
- evidence storage；
- resume cursor；
- partial failure recovery；
- stale downstream pending 检测。

### 8. TUI / API

TUI/API 应展示和操作 graph，而不是只显示聊天记录。

核心视图：

- node 列表与状态；
- dependency graph；
- gate 状态；
- evidence 摘要与引用；
- 当前 running executor；
- blocked / needs-context 原因；
- resume / cancel / retry / approve 操作。

## 关键执行 loop

```text
load GraphState
  ↓
find ready nodes
  ↓
select executor by node contract and capability
  ↓
execute node with scoped context and tools
  ↓
normalize tool results into evidence
  ↓
record node result
  ↓
reconcile graph, gates, pending nodes
  ↓
persist event log and snapshot
  ↓
render graph state to TUI/API
```

## NodeExecutor contract

NodeExecutor 是 agent 与 graph runtime 的边界。

建议字段：

| 字段 | 说明 |
| --- | --- |
| `node_id` | 被执行的 graph node |
| `contract` | node 目标、范围、验收标准、限制 |
| `context` | 经过裁剪的执行上下文 |
| `tools` | 允许使用的工具集合 |
| `permissions` | 权限策略与人工确认状态 |
| `evidence_policy` | 哪些工具结果必须记录为 evidence |
| `result_schema` | DONE、FAILED、BLOCKED、NEEDS_CONTEXT 等结果格式 |

NodeExecutor 不应直接修改全局 graph。它只返回 node result、evidence 和建议的 graph update，由 reconciler 统一落盘。

## Tool contract + evidence mapping

工具调用必须包含稳定契约：

- input schema：工具输入结构；
- output schema：工具输出结构；
- permission policy：是否需要确认、是否可写文件、是否可访问外部系统；
- evidence policy：哪些输出需要沉淀，如何摘要，如何引用；
- failure policy：失败是否可重试、是否阻塞 gate、是否需要人工介入。

Tool result 需要被规范化，而不是原样塞进 transcript。

建议 evidence 至少包含：

- evidence id；
- node id；
- tool name；
- tool input 摘要；
- output 摘要；
- 关键引用或文件路径；
- 时间戳；
- 权限决策；
- 是否截断；
- 是否可复现；
- 关联 gate 或 finding。

## Gate 与 Reconcile

### Gate

Gate 是 checkpoint，用于表达验收、风险、人工确认或阶段推进。

常见状态：

- pending；
- passed；
- failed；
- blocked；
- needs-context；
- waived。

### Reconcile

Reconciler 是一等生命周期，不应隐藏在 session 更新里。它负责把 node result 转换为新的 GraphState。

典型职责：

- 校验 node result 是否符合 node contract；
- 将 evidence 挂接到对应 node 或 gate；
- 移动 completed node；
- 更新 node 状态；
- 更新 gate 状态或 repair count；
- 追加 understanding；
- 生成或删除 pending node；
- 标记 stale downstream pending；
- 设置 awaiting_user_confirmation 或 awaiting_graph_reconcile；
- 持久化事件。

Reconcile 不应隐藏在下一轮 prompt 中，否则恢复、审计和人工接管都会失去确定边界。

## Multi-agent 模型

推荐把 multi-agent 视为 scheduling 问题，而不是聊天广播问题。

### 正确模型

```text
GraphState
  ├─ node A → executor: investigator
  ├─ node B → executor: implementer
  ├─ node C → executor: verifier
  └─ gate G → human / policy approval
```

### 关键要求

- agent 之间通过 graph 和 evidence 协作；
- executor 不共享未结构化私有上下文作为事实来源；
- scheduler 控制并发和依赖；
- gate 决定是否进入下一阶段；
- 每个 executor 的输出都要可审计。

## 关键设计原则

### 1. Session 不是 source of truth

Session 可以保存模型上下文、用户交互和运行轨迹，但不能替代 GraphState。任何影响任务进度的状态都必须进入 graph 或 graph event log。

### 2. Permission 与 gate 分离

- Permission：决定某个动作是否允许执行。
- Gate：决定某个阶段是否满足进入下一阶段的条件。

二者都可能需要人工确认，但语义不同，不能混用。

### 3. Evidence 必须可引用

Evidence 不应只是 transcript 中的一段文本。它需要有稳定 ID、来源、摘要、时间和关联对象，便于后续 gate、review、resume 和 audit 使用。

### 4. Reconcile 阻止状态漂移

当 node 执行失败或产生部分结果时，reconciler 必须重新推导 pending graph，避免 stale downstream work 在错误前提下继续执行。

### 5. 多 agent 是调度策略

多 agent 的关键问题是：哪个 executor 在什么能力边界下执行哪个 node。聊天分叉、thread-tree 或 delegation 只是实现细节，不能替代 scheduler。

## MVP 边界

MVP 应优先验证 GraphState、NodeExecutor、Tool contract/evidence mapping、Graph reconciler、Persistence/resume，以及基础 CLI/TUI 或 graph cockpit。详细阶段切法见 [MVP 切法与路线图](./mvp-roadmap.md)。

以下能力不应进入 MVP 的关键路径：

- 高级 plugin 系统；
- MCP marketplace；
- 大规模 multi-agent；
- remote server；
- ACP；
- 长期 memory；
- 知识图谱。

## 建议的早期交付物

1. `GraphState` 数据结构与序列化格式。
2. `NodeExecutor` 接口与最小实现。
3. 工具契约定义和 evidence record 格式。
4. Reconciler 状态迁移规则。
5. 持久化 event log + snapshot。
6. 最小 CLI/TUI：展示 graph、node、gate、evidence、resume 状态。

## 风险与约束

| 风险 | 影响 | 缓解建议 |
| --- | --- | --- |
| graph 设计过重 | MVP 推进困难 | 先覆盖最小 GraphState 和 node lifecycle |
| evidence 粒度过细 | 存储膨胀、阅读困难 | 保存摘要、引用和可复现信息，避免保存完整日志 |
| executor 绕过 reconciler | 状态分裂 | 禁止 executor 直接写 graph 主状态 |
| transcript 与 graph 双主状态 | 恢复和审计不一致 | graph 是 source of truth，transcript 仅作为附属记录 |
| multi-agent 失控并发 | 冲突写入和重复工作 | scheduler 统一控制依赖、权限和并发 budget |

补充约束：

- 如果 graph 与 session 双写，必须规定 graph 优先级，避免恢复后状态不一致。
- 如果 evidence 只存长文本，会降低可审计性；应存结构化引用和摘要。
- 如果 gate 只靠自然语言判断，会难以自动化；应允许绑定命令、测试、人工确认或 verifier 输出。
- 如果 scheduler 直接读取聊天上下文决策，会回到 message-driven 模式。

## 反模式

| 反模式 | 问题 |
| --- | --- |
| 把 harness 当作更长的 prompt | 不能解决恢复、审计、gate 和调度问题 |
| 用 todo list 代替 graph | 缺少依赖、evidence、reconcile、gate 生命周期 |
| 用 thread tree 代替 graph | thread 表达对话分支，不表达任务验收和证据 |
| tool_result 只回填下一轮上下文 | 工具事实不可独立审计，恢复后难以解释 |
| subagent chat fan-out | 产生多个对话分支，但缺少统一调度和收敛 |
| gate 只写在文档里 | runtime 无法自动阻塞、恢复或提示人工确认 |

## 结论

Harness-native code agent 的最小成功标准，是让 graph 成为执行真相源，并让 node、tool evidence、gate、reconcile、resume 都进入同一套生命周期。现有 conversation-native runtime 的 query loop、Tool contract、MCP、权限和 TUI 能力可以复用，但顶层执行模型需要切换为 graph scheduler。
