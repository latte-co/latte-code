# Runtime 演进目标：ContextProjection

## 文档状态

本文定义 `ContextProjection` 的渐进式设计。早期可以是任务上下文摘要；当事实、证据和不确定性增多后，再升级为正式上下文投影。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/context-projection.md`](../../../../en-US/design/runtime-evolution/modules/context-projection.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | task context summary | 为修改和验证提供最小相关上下文 |
| v0.3 | evidence-aware projection | 区分 fact、evidence、hypothesis 和 stale material |
| v0.5 | `ContextProjection` | 生成带来源、预算、遗漏项和信任边界的 LLM 输入 |

## 责任

- 从 `StateStore`、`ActionGraph`、task acceptance 和 policy constraints 中生成 LLM 可用上下文。
- 明确纳入哪些 fact、evidence、hypothesis，排除哪些 stale / redacted / over-budget 信息。
- 防止 transcript 裁剪成为事实来源。
- 为 `PolicyDecision` 提供可审计输入。

## 非目标

- v0.1 不要求完整 projection 引擎。
- 不自动读取整个仓库或全部历史。
- 不晋升 fact。
- 不执行工具。
- 不把外部文档内容无条件注入 prompt。

## 最小数据契约

```ts
type ContextProjection = {
  id: string;
  stepId: string;
  factIds: string[];
  evidenceIds: string[];
  hypotheses: string[];
  omittedDueToBudget: string[];
  trustScope: string[];
};
```

## 不变量

- stale / invalidated fact 不能作为强事实进入 projection。
- projection 必须记录 omitted / redacted 信息。
- prompt 中的关键事实必须可追溯到 fact 或 evidence id。
- 不可信外部内容必须标注 trust scope。

## 验收方向

- v0.1 上下文摘要覆盖任务相关文件和验收条件。
- v0.3 stale fact 被排除或显式标记。
- projection audit 可解释模型看到了什么、没看到什么。
- `PolicyDecision` 可追溯到 projection id。

## 与其他模块关系

- 从 `StateStore` 读取 fact / evidence。
- 从 `ActionGraph` 获取当前 node 目标与依赖。
- 向 `Policy Core` 提供输入。
- 接收 `Reconciler` 更新后的 stale / invalidation 结果。
