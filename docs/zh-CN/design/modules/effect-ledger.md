# 模块技术设计：EffectLedger

## 文档状态

当前设计占位，用于实现前明确 `EffectLedger` 的副作用账本边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/effect-ledger.md`](../../../en-US/design/modules/effect-ledger.md)。

## 责任

- 记录所有 mutating action 的 effect declaration、执行结果和补偿状态。
- 区分 expected / observed / effective effect。
- 为 transaction、reconcile、audit 和 human handoff 提供 effect evidence。
- 阻止未声明的文件、shell、network、Git、外部 API 等副作用进入 runtime。

## 非目标

- 不直接执行能力。
- 不决定业务事实是否成立。
- 不替代 OS sandbox 或外部权限系统。
- 不允许工具日志代替 effect record。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | effect declaration、capability result、sandbox metadata、compensation record |
| 输出 | `EffectRecord`、effect status、effect mismatch event、audit summary |

## 核心数据契约

```ts
type EffectRecord = {
  id: string;
  actionNodeId: string;
  kind: "file_write" | "command" | "network" | "external_api" | "git" | "approval";
  target: string;
  inputDigest: string;
  reversible: boolean;
  status: "planned" | "applied" | "failed" | "partial" | "compensated";
  transactionId?: string;
};
```

## 不变量

- mutating action 执行前必须有 `planned` effect。
- observed effect 必须能追溯到 action node 和 capability adapter。
- `reversible=false` effect 执行前必须经过相应 gate。
- partial / failed effect 必须进入 reconcile。

## 失败模式

- 工具产生未声明副作用。
- output summary 隐藏实际修改范围。
- compensation 标记成功但外部状态未恢复。
- effect 与 transaction 绑定缺失。

## 测试 / 验收方向

- 文件写入、shell 命令、外部 API 都必须产生 effect record。
- declared 与 observed 不一致时触发 effect reconcile。
- 不可补偿 effect 缺少 approval 时被阻断。
- rollback / compensation 后 effect status 可解释。

## 与其他模块关系

- `NodeExecutor` 只能通过 capability adapter 触发已声明 effect。
- `TransactionManager` 绑定 mutating effect 到 overlay / transaction。
- `Reconciler` 处理 failed / partial / compensated effect。
- `ActionGraph` 引用 effect id 做审计索引。
