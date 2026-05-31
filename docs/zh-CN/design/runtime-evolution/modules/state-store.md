# Runtime 演进目标：StateStore

## 文档状态

本文定义 `StateStore` 的渐进式设计。早期 Fluxcode 不需要完整事实数据库，但必须避免把 transcript、工具输出或模型推断直接当成长期事实。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/state-store.md`](../../../../en-US/design/runtime-evolution/modules/state-store.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | verification summary | 保存任务摘要、验证结果和关键输出引用 |
| v0.3 | evidence / fact lite | 区分 `Observation`、`Evidence`、`Fact` |
| v0.5 | `StateStore` | 管理 versioned fact、lifecycle、coverage 和 confidence |

## 责任

- 管理 `Observation`、`Evidence`、versioned `Fact`。
- 维护 `Fact` lifecycle：`candidate`、`active`、`stale`、`superseded`、`invalidated`、`retracted`。
- 为 `ContextProjection` 提供可信、带版本和覆盖范围的上下文材料。
- 记录 promotion rule / `TrustGate` 如何把证据晋升为事实。

## 非目标

- v0.1 不要求构建完整事实系统。
- 不执行工具或能力。
- 不调度 `ActionNode`。
- 不把 transcript、prompt 或外部文档直接当成事实。
- 不替代外部 repo、CI 或评审系统的权威。

## 最小数据契约

```ts
type Observation = {
  id: string;
  source: "tool" | "user" | "environment" | "external";
  summary: string;
  rawRef?: string;
};

type Evidence = {
  id: string;
  observationIds: string[];
  scope: string[];
  producedByStepId: string;
};

type Fact = {
  id: string;
  claim: string;
  lifecycle: "candidate" | "active" | "stale" | "superseded" | "invalidated" | "retracted";
  evidenceIds: string[];
};
```

## 不变量

- active `Fact` 必须至少引用一个 `Evidence`。
- `Observation` 不得自动成为 `Fact`。
- stale / invalidated / retracted fact 不得作为强事实进入 `ContextProjection`。
- `Fact` 必须绑定 repo / overlay revision 或明确外部来源。

## 验收方向

- v0.3 起，LLM hypothesis 与 verified fact 能被区分。
- promotion 必须生成可审计记录。
- revision 改变后相关 facts 生命周期更新。
- projection 查询不会返回 stale fact 作为强事实。

## 与其他模块关系

- `ContextProjection` 从 `StateStore` 读取事实和证据。
- `Reconciler` 更新 stale / invalidated 状态。
- `TrustGate` 决定哪些证据可晋升。
- `ActionGraph` 只引用 fact / evidence id。
