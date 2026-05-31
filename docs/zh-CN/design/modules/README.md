# 当前 / 近期模块技术设计

本目录只存放当前 `v0.1` / 近期实现需要直接对齐的模块级技术设计。长期 runtime 模块目标已经移动到 [`../runtime-evolution/modules/`](../runtime-evolution/modules/README.md)。

当前 / 近期模块文档：

- [`Code Agent Loop`](./code-agent-loop.md)

## 边界说明

- 本目录文档可以引用后续 runtime 对象，但必须以 `v0.1` / 近期实现约束为主。
- `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler`、`ContextProjection`、`NodeExecutor` 等长期目标请维护在 runtime evolution 目录中。
- 不得把 runtime evolution 文档表述为当前 `src/` 已经完整实现的模块。
