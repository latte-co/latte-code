# 模块技术设计：ContextProjection

## 文档状态

当前设计占位，用于实现前明确 LLM 输入上下文的投影边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/context-projection.md`](../../../en-US/design/modules/context-projection.md)。

## 责任

- 从 `StateStore`、`ActionGraph`、task acceptance 和 policy constraints 中生成 LLM 可用上下文。
- 明确纳入哪些 fact、evidence、hypothesis，排除哪些 stale / redacted / over-budget 信息。
- 防止 transcript 裁剪成为事实来源。
- 为 `PolicyDecision` 提供可审计输入。

## 非目标

- 不自动读取整个仓库或全部历史。
- 不晋升 fact。
- 不执行工具。
- 不把外部文档内容无条件注入 prompt。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | fact ids、evidence ids、action node context、acceptance criteria、token budget、trust policy |
| 输出 | `ContextProjection`、omitted list、redaction list、trust scope、projection audit record |

## 核心数据契约

```ts
type ContextProjection = {
  id: string;
  actionNodeId: string;
  factIds: string[];
  evidenceIds: string[];
  hypotheses: string[];
  omittedDueToBudget: string[];
  redactions: string[];
  trustScope: string[];
  tokenBudget: number;
};
```

## 不变量

- stale / invalidated fact 不能作为强事实进入 projection。
- projection 必须记录 omitted / redacted 信息。
- prompt 中的关键事实必须可追溯到 fact 或 evidence id。
- 不可信外部内容必须标注 trust scope。

## 失败模式

- transcript trimming 带入过期事实。
- 预算裁剪删除关键约束但未记录。
- 外部文档被当成 runtime fact。
- hypothesis 与 fact 在 prompt 中不可区分。

## 测试 / 验收方向

- stale fact 被排除或显式标记。
- projection audit 可解释模型看到了什么、没看到什么。
- redaction 不破坏必要验收条件。
- `PolicyDecision` 可追溯到 projection id。

## 与其他模块关系

- 从 `StateStore` 读取 fact / evidence。
- 从 `ActionGraph` 获取当前 node 目标与依赖。
- 向 `Policy Core` 提供输入。
- 接收 `Reconciler` 更新后的 stale / invalidation 结果。
