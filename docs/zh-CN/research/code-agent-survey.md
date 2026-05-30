# Code Agent 横向调研

## 目的

本文从 harness-native code agent 的视角，对五个 code agent / harness workflow 相关系统做横向比较。重点不是评判哪个系统“更强”，而是识别哪些 runtime 能力可复用，哪些状态模型会限制 graph-native execution runtime。

总体结论：**harness-native code agent 的核心特征，是将 code agent 的状态管理从 message-driven tool runner 演进为 graph-driven execution runtime。**

在 harness-native 模型中：

- agent 执行 harness graph 中的 node；
- tool result 被规范化为 evidence；
- gate 决定阶段性检查是否通过；
- reconciler 更新 graph；
- scheduler 基于 GraphState 决定下一步。

## 调研事实与设计建议的边界

- **调研事实**：本文件记录五个系统已有的架构、能力、状态模型和边界。
- **设计建议**：仅在“对 harness-native 的启发”“需要重构的能力”等小节中说明这些事实对 harness-native code agent 的影响；完整目标架构见 [架构设计总览](../design/architecture-overview.md)。

## 调研对象

| 名称 | 类型定位 | 说明 |
| --- | --- | --- |
| `claude-code` | conversation-native code agent runtime | 强 Tool contract、query loop、MCP、subagent/coordinator、JSONL recovery |
| `codex` | Rust core code agent runtime | 强 thread/session、ToolRouter、MultiAgentV2 thread-tree、ThreadStore |
| `CodeWhale`（原路径：`.tmp/codeagent/DeepSeek-TUI`） | Rust TUI runtime | turn loop、ToolRegistry、mode/safety、subagent、runtime API |
| `opencode` | TS/Bun 平台化 code agent runtime | SessionPrompt、tool registry、permission、MCP/plugin/skill、SQLite session、snapshot |
| `oh-my-openagent`（原路径：`.tmp/codeagent/oh-my-opencode`） | OpenCode plugin / harness workflow 外挂层 | hooks、Boulder state、task delegation、Ralph/Atlas continuation |

## 横向结论

现有系统普遍已经具备工具执行、权限控制、会话恢复、MCP 接入、TUI 或插件生态等能力，但它们的主状态大多仍围绕 message、turn、session、thread、todo 或 snapshot 展开。

对 harness-native runtime 来说，关键缺口是：

- graph 不是 source of truth；
- tool result 通常只作为下一轮上下文，而不是可审计 evidence；
- gate / checkpoint 不是一等状态；
- reconcile 多数隐含在下一轮 prompt、插件 hook 或人工流程中；
- multi-agent 多数表现为 conversation fork、thread tree 或 chat fan-out，而不是 graph scheduler 对 node executor 的调度。

## 能力对比

| 维度 | `claude-code` | `codex` | `CodeWhale` | `opencode` | `oh-my-openagent` |
| --- | --- | --- | --- | --- | --- |
| 主状态模型 | transcript / session / message chain | thread / session / ThreadStore | turn / runtime state | session / todo / snapshot | plugin state / Boulder workflow |
| 工具模型 | 强 Tool contract，含 schema、permission、UI、MCP metadata | ToolRouter | ToolRegistry | tool registry + permission | 借用 OpenCode tool/hook 能力 |
| 恢复能力 | append-only JSONL recovery | ThreadStore/session | runtime 侧恢复思路 | SQLite session + snapshot | continuation workflow |
| 多 agent | AgentTool、local agent、coordinator、team、teammate、mailbox | MultiAgentV2 thread-tree | subagent | skill/plugin/task delegation | Ralph/Atlas continuation |
| TUI/runtime | Ink/React TUI、REPL、headless、SDK | Rust core 更偏 runtime | Rust TUI runtime 强 | 平台化 TS/Bun runtime | 插件化外挂展示 |
| Graph-native 程度 | 非 graph-native | 非 graph-native | 非 graph-native | 非完整 graph-native | workflow 外挂，非原生 graph |

## 1. `claude-code`

### 调研事实

`claude-code` 的核心设计围绕强 Tool contract、query loop、MCP 集成以及 subagent / coordinator 展开。它将模型交互、工具调用和多 agent 协作纳入统一 loop，并通过 JSONL recovery 保留可恢复的执行记录。

- 具备强 Tool contract：工具 schema、权限、read-only 标识、并发能力、UI 渲染、MCP metadata 都进入统一契约。
- query loop 是核心执行模型：`tool_use` 执行后以 `tool_result` 回填进入下一轮。
- 支持 MCP、subagent/coordinator、多入口调用、session JSONL recovery、context compaction、IDE/LSP/telemetry。
- 状态仍偏 transcript / message chain，graph 关系主要由 parentUuid、logicalParentUuid、conversation fork 等机制间接表达。

### 能力边界

- 能表达工具输入输出契约，并围绕工具调用形成稳定执行流程。
- 能通过 MCP 扩展外部能力。
- 能通过 subagent/coordinator 组织更复杂的任务执行。
- 状态表达仍偏 transcript / message chain，主要记录“发生了什么”，而不是以 graph 形式声明“下一步应该做什么”。

### 核心工具与机制

- Tool contract：约束工具输入、输出和调用边界。
- Query loop：驱动模型生成、工具执行和结果回填。
- MCP：扩展外部工具能力。
- Subagent / coordinator：支持任务分派和协调。
- JSONL recovery：提供基于日志的恢复基础。

### Agent loop

典型 loop 是：模型接收上下文 → 选择工具 → 执行工具 → 工具结果回填到上下文 → 继续下一轮 query。该 loop 擅长 message-driven execution，但 graph 层面的 node 状态、gate 结果和 evidence 归档不是一等概念。

### 对 harness-native 的启发

- Harness-native agent 需要同等强度的工具契约，避免工具结果不可验证或不可沉淀。
- Tool contract、权限前置、MCP adapter、session recovery、context compaction、多 agent execution、TUI/API/telemetry 都值得借鉴。
- JSONL recovery 表明“可恢复执行日志”是 agent runtime 的基础能力。
- Query loop 可保留为 NodeExecutor 的内部执行循环，但不应继续作为顶层 scheduler。
- Subagent/coordinator 的经验可转化为 scheduler 层能力，但调度对象应是 graph node，而非会话分支。

### 缺口

- 缺少 GraphState 作为 source of truth。
- 缺少将 tool result 显式映射为 evidence 的结构。
- gate 与 reconcile 不是执行生命周期的一等阶段。

## 2. `codex`

### 调研事实

`codex` 以 Rust core 为核心，围绕 `run_turn`、`ToolRouter`、MultiAgentV2 thread-tree 和 ThreadStore 构建稳定的会话执行系统。它的 thread / session 抽象较强，适合承载多轮上下文、分支和恢复。

- Rust core 提供较强的 runtime 可控性和工程边界。
- 核心能力包括 `run_turn`、ToolRouter、MultiAgentV2 thread-tree、ThreadStore。
- ThreadStore 能管理 session / thread 历史。
- MultiAgentV2 thread-tree 能表达多 agent 分支协作。
- 其主抽象仍是 thread/session，而不是 node/gate/evidence graph。

### 核心工具与机制

- `run_turn`：单轮 agent 执行入口。
- `ToolRouter`：工具路由与调用分发。
- MultiAgentV2 thread-tree：多 agent 分支结构。
- ThreadStore：持久化 thread/session 状态。

### Agent loop

典型 loop 是：进入 `run_turn` → 基于 session/thread 构造上下文 → 模型决策 → `ToolRouter` 执行工具 → 更新 thread/session。该模型强化了会话树，但没有把执行目标拆成可调度、可验收、可 reconcile 的 graph node。

### 对 harness-native 的启发

- 可借鉴 Rust core runtime、thread persistence、工具路由和多 agent thread 组织方式。
- `run_turn` 可作为 NodeExecutor 的工程参考：每次只执行一个明确执行单元。
- `ToolRouter` 可演化为 Tool / MCP Capability Layer。
- 需要避免把 thread tree 误当作 harness graph；thread 记录对话关系，graph 记录任务依赖、证据、gate 与 reconcile 状态。

### 缺口

- thread-tree 不等价于 harness graph：缺少 gate、evidence、node dependency 和 reconcile 语义。
- session 持久化无法直接回答“哪些节点已满足验收、哪些 gate 阻塞下一步”。

## 3. `CodeWhale`

### 调研事实

`CodeWhale`（原路径：`.tmp/codeagent/DeepSeek-TUI`）是 Rust TUI runtime，重点包括 turn loop、ToolRegistry、mode/safety、subagent 和 runtime API。它把交互界面、runtime loop、安全模式与工具注册纳入一个可控执行环境。

- Rust runtime 适合处理 TUI、异步交互和受控执行。
- ToolRegistry 能集中管理工具能力。
- mode/safety 提供运行时安全边界。
- subagent 和 runtime API 支持扩展执行形态。
- 其重点是安全与 TUI/runtime 体验，不是 harness graph 状态机。

### 核心工具与机制

- turn loop：驱动交互轮次。
- ToolRegistry：注册和查找工具能力。
- Mode / safety：约束 agent 可执行行为。
- Subagent：支持局部委派。
- Runtime API：对外暴露 runtime 能力。

### Agent loop

典型 loop 是：TUI / runtime 接收输入 → turn loop 驱动模型与工具 → ToolRegistry 分发工具 → mode/safety 检查执行边界 → 结果返回界面或 runtime。该设计关注运行时安全和交互控制。

### 对 harness-native 的启发

- 适合作为安全执行、TUI 状态呈现、runtime API 的参考。
- Harness-native runtime 需要明确 mode/safety，特别是文件写入、命令执行和权限边界。
- TUI 不应只是交互窗口，而应成为 graph cockpit：展示 node、gate、evidence、reconcile 状态。
- 若引入 harness graph，需要把 turn loop 从主控循环下沉为 NodeExecutor 或 interaction loop。

### 缺口

- 缺少原生 GraphState。
- 缺少 gate checkpoint 与 evidence record。
- reconcile 没有作为独立生命周期阶段呈现。

## 4. `opencode`

### 调研事实

`opencode` 是 TS/Bun runtime，围绕 SessionPrompt、tool registry、permission、MCP/plugin/skill、SQLite session 和 snapshot 构建平台化 code agent。它在能力扩展、权限控制和会话持久化方面较完整。

- TS/Bun runtime 便于快速迭代平台能力。
- tool registry、MCP、plugin、skill 组成较完整的 capability layer。
- permission 提供工具调用前的权限控制。
- SQLite session 与 snapshot 支持恢复和状态记录。
- todo、snapshot、session 提供了任务与状态管理能力，但不是完整 harness graph。

### 核心工具与机制

- SessionPrompt：将 session 状态和模型上下文组织为 prompt。
- Tool registry：集中管理工具能力。
- Permission：工具调用权限和用户确认。
- MCP / plugin / skill：扩展能力来源。
- SQLite session：持久化会话。
- Snapshot：记录工作区或会话状态。

### Agent loop

典型 loop 是：加载 session → 构造 SessionPrompt → 模型选择工具 → permission 判断 → tool registry 执行工具 → 写回 SQLite session / snapshot / todo。该 loop 平台化能力强，但 todo 与 snapshot 更像辅助状态，不是调度与验收的中心。

### 对 harness-native 的启发

- Capability Layer 应将 tool、MCP、plugin、skill 纳入统一注册和权限体系。
- plugin/skill/MCP/permission/session 生态适合作为 capability layer 的参考。
- SQLite session/snapshot 可作为 Persistence / Recovery 的基础实现参考。
- Permission 应与 gate 区分：permission 决定“能不能执行”，gate 决定“是否达到检查点”。
- Harness-native 不应只把 todo 扩展为任务列表，而应把 node result、evidence、gate、reconcile 都纳入持久化 graph。

### 缺口

- 缺少 harness graph 作为 source of truth。
- todo/snapshot/session 无法替代 node dependency、gate status 和 evidence mapping。
- Reconciler 不应只是修改 session，而应负责 graph 状态迁移。

## 5. `oh-my-openagent`

### 调研事实

`oh-my-openagent`（原路径：`.tmp/codeagent/oh-my-opencode`）以 OpenCode plugin、hooks、Boulder state、task delegation、Ralph/Atlas continuation 为核心，展示了在现有 code agent 外挂 harness workflow 的可行性。

- 能通过 plugin/hooks 介入 OpenCode 执行流程。
- 能用 Boulder state 承载额外 workflow 状态。
- 能通过 task delegation 和 continuation 组织更长任务链。
- 它证明可以在现有 code agent 上叠加 harness-like 协作流程。
- 上限在于缺少原生 graph reconcile/gate record，workflow 与 runtime 主状态仍有割裂。

### 核心工具与机制

- OpenCode plugin：扩展宿主 agent。
- Hooks：插入执行生命周期。
- Boulder state：外挂 workflow 状态。
- Task delegation：任务委派与执行分层。
- Ralph / Atlas continuation：长任务延续和上下文继承。

### Agent loop

典型 loop 是：OpenCode session 执行 → plugin hooks 捕获关键事件 → Boulder state 更新 workflow → task delegation 分派后续执行 → continuation 延续上下文。该模型证明了 harness workflow 可以外挂实现，但也暴露出非原生集成的上限。

### 对 harness-native 的启发

- 可借鉴任务分派、continuation、workflow hook 的产品形态。
- Hooks 是连接 agent runtime 与 harness lifecycle 的有效机制。
- Continuation 能降低长任务中断后的恢复成本。
- Task delegation 应由 scheduler 统一建模，而非散落在会话或插件逻辑中。
- 不应停留在外挂层；graph reconcile、gate record、evidence mapping 需要成为 runtime 内建生命周期。

### 缺口

- Graph reconcile 不是宿主 runtime 的一等生命周期。
- Gate record 与 evidence record 依赖外挂协议，难以形成强一致状态。
- 外挂 workflow 难以彻底避免 session 与 graph 双写不一致。

## 横向发现

### 共同能力

- 都有某种 agent loop：query loop、`run_turn`、turn loop 或 session-driven loop。
- 都有工具注册或路由机制。
- 多数系统具备恢复、session 或 thread 级状态记录。
- 多数系统开始支持 subagent、plugin、skill、MCP 或 delegation。

### 共同边界

- 主状态仍多以 message、session、thread、todo 或 snapshot 表达。
- 工具结果通常回到上下文，而不是稳定沉淀为可引用 evidence。
- 多 agent 多数表现为 chat fan-out、thread-tree 或 delegation，而不是 graph scheduler。
- gate / checkpoint 往往缺失，或者只表现为人工确认、权限确认、测试结果片段。

## 对 harness-native 的综合启发

### 可直接吸收的能力

- 统一 Tool contract；
- 权限前置和安全模式；
- MCP adapter；
- session recovery；
- context compaction；
- 多 agent execution；
- TUI/API/telemetry；
- plugin/skill registry。

### 需要重构的能力

| 现有做法 | harness-native 改造方向 |
| --- | --- |
| transcript/session/message chain 作为主状态 | GraphState 作为 source of truth |
| query loop / run_turn 作为顶层循环 | Scheduler 选择 ready node，executor 执行 node |
| tool result 只进入下一轮上下文 | tool result 映射为 evidence 并持久化 |
| conversation fork 表示任务分支 | graph node/dependency 表示任务结构 |
| 人工或插件 hook 处理 checkpoint | gate 作为一等状态，reconciler 显式更新 |
| multi-agent chat fan-out | scheduler 调度不同 agent executor |

### 对 harness-native code agent 的直接结论

Harness-native code agent 不应只是在现有 chat loop 上增加 todo 或插件；它需要将 harness graph 提升为 runtime source of truth：

1. graph 定义执行目标、依赖和验收；
2. agent 执行 graph node；
3. tool result 映射为 evidence；
4. gate 记录检查点结论；
5. reconciler 根据执行结果更新 graph；
6. scheduler 根据 graph 状态选择下一节点。

五个系统提供了大量可复用的 runtime 经验，但没有一个可以直接等同于 harness-native code agent。harness-native 的分水岭是：graph 是否成为主状态，tool result 是否成为 evidence，gate/reconcile 是否成为一等生命周期，multi-agent 是否由 scheduler 而不是 chat fan-out 驱动。
