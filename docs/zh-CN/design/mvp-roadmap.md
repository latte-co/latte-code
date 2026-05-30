# MVP 切法与路线图

## 目标

MVP 的目标不是一次性实现完整 agent 平台，而是验证 harness-native code agent 的核心 graph lifecycle：

```text
GraphState → scheduler → NodeExecutor → tool evidence → node result → reconciler → persistence/resume → CLI/TUI 展示
```

只要这个 lifecycle 成立，后续的 plugin、MCP marketplace、remote server、多 agent 扩展才有稳定基础。

## 设计建议与调研事实的边界

- 调研事实见 [Code Agent 横向调研](../research/code-agent-survey.md)。
- 本文件是基于调研事实和 [Harness-native Code Agent 设计建议](./harness-native-code-agent.md) 推导出的阶段性路线图，属于设计建议。
- 本文件只定义 MVP 切法、非目标、阶段验收和后置能力，不表示当前仓库已经具备对应实现。

## MVP 必须覆盖

| 能力 | MVP 要求 | 验收方式 |
| --- | --- | --- |
| GraphState | 能表达 node、dependency、status、gate、evidence、reconcile history | 可从文件或存储中恢复同一 graph 状态 |
| NodeExecutor | 能执行单个 ready node，并返回结构化结果 | 至少支持 DONE、FAILED、BLOCKED、NEEDS_CONTEXT |
| Tool contract + evidence mapping | 工具结果能被规范化为 evidence | evidence 可追溯 node、tool、输入摘要、输出摘要和引用 |
| Graph reconciler | 能根据 node result 更新 graph/gate/pending nodes | 失败节点不会让下游 pending 继续使用陈旧假设 |
| Persistence / resume | 支持中断后恢复 | 恢复后 scheduler 能从正确 ready node 继续 |
| Basic CLI/TUI 或 graph cockpit 文档 | 能查看 graph、node、gate、evidence、blocked 原因 | 使用者能判断当前运行状态和下一步操作 |

## MVP 非目标

以下能力不应进入第一阶段关键路径：

- 高级 plugin 系统；
- MCP marketplace；
- 大规模 multi-agent；
- remote server；
- ACP；
- 长期 memory；
- 知识图谱；
- 复杂权限策略 UI；
- 完整 IDE 集成；
- 自动 PR / 发布流水线。

这些能力可以预留接口，但不应阻塞核心 graph lifecycle。

## 建议阶段

### Phase 0：模型定稿

目标：明确最小数据模型和状态机。

交付物：

- GraphState schema；
- Node schema；
- Evidence schema；
- Gate schema；
- NodeResult schema；
- Reconcile event schema。

验收标准：

- 能用静态样例表达 pending、running、completed、failed、blocked；
- 能表达 gate 阻塞；
- 能表达 evidence 与 node 的关联；
- 能表达 reconcile 后的 graph 变化。

### Phase 1：单 agent node lifecycle

目标：打通单 node 执行 lifecycle。

交付物：

- scheduler 选择 ready node；
- NodeExecutor 执行 node；
- 工具结果转 evidence；
- node result 写回；
- reconciler 更新状态；
- append-only event log。

验收标准：

- 一个 graph 可从 pending 推进到 completed；
- 工具 evidence 可查看；
- failed / blocked / needs-context 能阻塞下游；
- 不依赖 transcript 才能理解状态。

### Phase 2：恢复与人工接管

目标：让 runtime 可中断、可恢复、可人工接管。

交付物：

- GraphState snapshot；
- resume cursor；
- gate approval / rejection；
- retry / cancel；
- stale downstream pending 检测。

验收标准：

- 执行中断后能恢复；
- gate 未放行时 scheduler 不继续执行；
- failed node 后下游 pending 不会继续使用旧计划；
- 人工确认能被持久化。

### Phase 3：基础 graph cockpit

目标：让使用者能理解和控制 graph。

交付物：

- node 状态视图；
- dependency 视图；
- evidence 摘要；
- gate 状态；
- blocked / needs-context 原因；
- basic CLI/TUI 操作。

验收标准：

- 使用者无需读原始日志即可判断当前状态；
- 可以定位失败 node、相关 evidence 和下一步操作；
- 可以执行 resume、retry、cancel、approve 等基础操作。

### Phase 4：受控 multi-agent

目标：在 graph scheduler 下引入多个 executor。

交付物：

- agent registry；
- executor capability matching；
- 并发 budget；
- node-level isolation；
- multi-agent evidence merge。

验收标准：

- scheduler 根据 node contract 选择 executor；
- 多 executor 并发不产生状态竞争；
- 所有结果通过 reconciler 合并；
- agent 之间通过 graph/evidence 协作，而不是共享未结构化聊天上下文。

## 后置能力路线图

| 能力 | 前置条件 | 说明 |
| --- | --- | --- |
| 高级 plugin | Tool contract 和 capability layer 稳定 | plugin 不应绕过 evidence 和 permission |
| MCP marketplace | MCP adapter 和权限模型稳定 | marketplace 是分发问题，不是核心 runtime 问题 |
| 大规模 multi-agent | scheduler、isolation、evidence merge 稳定 | 先支持受控 multi-agent，再扩展规模 |
| remote server | persistence、auth、API 稳定 | remote 化前要先保证本地生命周期正确 |
| ACP | agent/executor contract 稳定 | 协议化应基于已有 node executor 边界 |
| 长期 memory | evidence 和 graph history 稳定 | memory 应从结构化历史提取，不应替代 graph |
| 知识图谱 | evidence schema 稳定 | 知识图谱是 evidence 的派生层 |

## MVP 风险

| 风险 | 判断 | 建议 |
| --- | --- | --- |
| 过早平台化 | plugin、marketplace、remote server 会稀释核心 lifecycle | 先证明 graph lifecycle |
| 过早多 agent | 并发复杂度会掩盖状态模型问题 | Phase 4 前只做单 executor 或串行 executor |
| evidence 无规范 | 后续审计和恢复困难 | MVP 就定义 evidence schema |
| 只保存 transcript | 无法恢复 graph truth | event log 与 GraphState snapshot 必须存在 |
| gate 不入 runtime | 无法自动阻塞和人工接管 | gate 从 MVP 开始进入状态机 |

## 最小验收场景

建议用一个小型 graph 验证 MVP：

1. node A：调研事实收集；
2. node B：基于 A 产出设计草案；
3. gate G：人工确认设计方向；
4. node C：确认后执行实现或文档生成；
5. node D：验证输出；
6. reconciler：根据 D 的结果关闭 graph 或生成修复 node。

验收重点：

- A 的工具输出能成为 evidence；
- B 依赖 A；
- G 未通过时 C 不执行；
- D 失败时 reconciler 生成修复路径或阻塞下游；
- 中断后能恢复到正确节点。

## 结论

MVP 应围绕 graph lifecycle 收敛，而不是围绕工具数量、agent 数量或 UI 完整度扩张。只要 GraphState、NodeExecutor、evidence、gate、reconcile、persistence/resume 打通，后续平台化能力才有可维护基础。
