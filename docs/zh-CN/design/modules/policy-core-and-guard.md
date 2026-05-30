# 模块技术设计：Policy Core and Guard

## 文档状态

当前设计占位，用于实现前明确 LLM policy 输出与 guard 边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/policy-core-and-guard.md`](../../../en-US/design/modules/policy-core-and-guard.md)。

## 责任

- `Policy Core` 将任务上下文转为受约束的 `PolicyDecision`。
- `PolicyGuard` 校验 schema、引用、权限、安全、信任边界和 evidence 要求。
- 区分可通过补充上下文重试的错误与必须 reject / ask / escalate 的错误。
- 防止 LLM 直接触发 syscall、写文件、run command、commit 或 rollback。

## 非目标

- LLM 不拥有全局调度权。
- LLM 不直接调用工具。
- LLM 推断不自动成为 `Fact`。
- Guard 不替代外部合规或代码评审。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | `ContextProjection`、candidate graph、capability metadata、policy constraints |
| 输出 | valid / rejected `PolicyDecision`、guard failure、missing context request |

## 核心数据契约

```ts
type PolicyDecision =
  | { kind: "GeneratePatch"; targetNodes: string[]; assumptions: string[]; requiredCapabilities: string[] }
  | { kind: "ExplainFailure"; failedNodeId: string; evidenceIds: string[] }
  | { kind: "AskUser"; question: string; options?: string[]; blockingNodeIds: string[] }
  | { kind: "Abstain"; reason: string; missingContext?: string[] };
```

## 不变量

- `PolicyDecision` 必须是 closed sum type。
- `GeneratePatch` 只能产生候选 patch 或 edit node，不能写文件。
- `AskUser` 必须绑定 blocking node 或明确缺失上下文。
- `PermissionInvalid`、`PolicyUnsafe`、`TrustBoundaryBroken` 不通过 prompt retry 绕过。

## 失败模式

- 模型输出引用不存在的 node / evidence / capability。
- 模型把 hypothesis 写成事实。
- guard failure 被包装成普通重试。
- policy 输出试图越过 `EffectLedger` 或 transaction boundary。

## 测试 / 验收方向

- schema invalid 可被识别并产生结构化错误。
- permission / trust failure 进入 reject / ask / escalate。
- `GeneratePatch` 不产生直接写入 effect。
- every valid decision 可追溯到 projection 与 evidence refs。

## 与其他模块关系

- 从 `ContextProjection` 获得输入。
- 向 `ActionGraph` 提交候选 node 或 policy reference。
- 由 `Scheduler` 决定何时执行相关 node。
- 与 `TrustGate` / `PermissionGate` 共享失败语义。
