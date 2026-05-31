# Runtime 演进目标：EffectLedger

## 文档状态

本文定义 `EffectLedger` 的渐进式设计。早期可以先记录修改和命令摘要；当副作用需要审计、恢复或人工确认时，再演进为正式 effect ledger。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/effect-ledger.md`](../../../../en-US/design/runtime-evolution/modules/effect-ledger.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | change / command summary | 记录改了哪些文件、跑了哪些命令 |
| v0.2 | capability effect metadata | 区分 read-only 和 mutating capability |
| v0.4 | `EffectRecord` | 记录 mutating action 的声明、结果和恢复状态 |
| v0.5 | `EffectLedger` | 支持 effect audit、compensation 和 reconcile |

## 责任

- 记录所有 mutating action 的 effect declaration、执行结果和补偿状态。
- 区分 expected / observed / effective effect。
- 为 transaction、reconcile、audit 和 human handoff 提供 effect evidence。
- 阻止未声明的文件、shell、network、Git、外部 API 等副作用进入 runtime。

## 非目标

- v0.1 不要求完整副作用账本。
- 不直接执行能力。
- 不决定业务事实是否成立。
- 不替代 OS sandbox 或外部权限系统。
- 不允许工具日志代替 effect record。

## 最小数据契约

```ts
type EffectRecord = {
  id: string;
  stepId: string;
  kind: "file_write" | "command" | "network" | "external_api" | "git" | "approval";
  target: string;
  reversible: boolean;
  status: "planned" | "applied" | "failed" | "partial" | "compensated";
  transactionId?: string;
};
```

## 不变量

- v0.1 起，文件修改和命令执行必须出现在最终记录中。
- v0.4 起，mutating action 执行前必须有 `planned` effect。
- observed effect 必须能追溯到 step / action node 和 capability adapter。
- `reversible=false` effect 执行前必须经过相应 gate。
- partial / failed effect 必须进入 reconcile。

## 验收方向

- 文件写入、shell 命令、外部 API 都有 effect 记录。
- declared 与 observed 不一致时触发 effect reconcile。
- 不可补偿 effect 缺少 approval 时被阻断。
- rollback / compensation 后 effect status 可解释。

## 与其他模块关系

- `NodeExecutor` 只能通过 capability adapter 触发已声明 effect。
- `TransactionManager` 绑定 mutating effect 到 overlay / transaction。
- `Reconciler` 处理 failed / partial / compensated effect。
- `ActionGraph` 引用 effect id 做审计索引。
