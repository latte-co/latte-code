# 模块技术设计：StateStore

## 文档状态

当前设计占位，用于实现前明确 `StateStore` 的事实与证据边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/state-store.md`](../../../en-US/design/modules/state-store.md)。

## 责任

- 管理 `Observation`、`Evidence`、versioned `Fact`。
- 维护 `Fact` lifecycle：`candidate`、`active`、`stale`、`superseded`、`invalidated`、`retracted`。
- 为 `ContextProjection` 提供可信、带版本和覆盖范围的上下文材料。
- 记录 promotion rule / `TrustGate` 如何把证据晋升为事实。

## 非目标

- 不执行工具或能力。
- 不调度 `ActionNode`。
- 不把 transcript、prompt 或外部文档直接当成事实。
- 不替代外部 repo、CI 或评审系统的权威。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | `Observation`、`Evidence`、promotion record、revision changes、reconcile requests |
| 输出 | active / candidate / stale facts、fact history、projection material、fact invalidation events |

## 核心数据契约

```ts
type Fact = {
  id: string;
  namespace: string;
  claim: string;
  repoRevision: string;
  overlayRevision?: string;
  lifecycle: "candidate" | "active" | "stale" | "superseded" | "invalidated" | "retracted";
  confidence: number;
  coverage: { scope: "local" | "module" | "repo" | "external"; paths?: string[]; symbols?: string[] };
  evidenceIds: string[];
};
```

## 不变量

- active `Fact` 必须至少引用一个 `Evidence`。
- `Observation` 不得自动成为 `Fact`。
- stale / invalidated / retracted fact 不得作为强事实进入 `ContextProjection`。
- `Fact` 必须绑定 repo / overlay revision 或明确外部来源。

## 失败模式

- evidence 缺少来源、时间或边界。
- promotion rule 缺失导致模型推断被误当作事实。
- repo / overlay 改变后 facts 未标 stale。
- 冲突事实静默覆盖。

## 测试 / 验收方向

- promotion 必须生成可审计记录。
- revision 改变后相关 facts 生命周期更新。
- projection 查询不会返回 stale fact 作为强事实。
- conflicting facts 可并存、降级、撤回或请求验证。

## 与其他模块关系

- `ContextProjection` 从 `StateStore` 读取事实和证据。
- `Reconciler` 更新 stale / invalidated 状态。
- `TrustGate` 决定哪些证据可晋升。
- `ActionGraph` 只引用 fact / evidence id。
