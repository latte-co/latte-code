# Runtime 演进目标：Policy Core and Guard

## 文档状态

本文定义 `Policy Core and Guard` 的渐进式设计。早期目标不是做复杂 policy engine，而是防止基础 code agent 越权、扩大范围或把不确定推断当成事实。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/policy-core-and-guard.md`](../../../../en-US/design/runtime-evolution/modules/policy-core-and-guard.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | basic task boundary | 尊重用户范围、非目标和允许命令 |
| v0.2 | `PolicyGuard` lite | 校验路径、命令、写入范围和危险操作 |
| v0.3 | evidence-aware policy | 区分 hypothesis、evidence 和 fact |
| v0.5 | `PolicyDecision` | 使用结构化决策和 gate 语义 |

## 责任

- `Policy Core` 将任务上下文转为受约束的 `PolicyDecision`。
- `PolicyGuard` 校验 schema、引用、权限、安全、信任边界和 evidence 要求。
- 区分可通过补充上下文重试的错误与必须 reject / ask / escalate 的错误。
- 防止 LLM 直接触发 syscall、写文件、run command、commit 或 rollback。

## 非目标

- v0.1 不要求完整 policy engine。
- LLM 不拥有全局调度权。
- LLM 不直接调用工具。
- LLM 推断不自动成为 `Fact`。
- Guard 不替代外部合规或代码评审。

## 最小数据契约

```ts
type PolicyDecision =
  | { kind: "Proceed"; reason: string }
  | { kind: "AskUser"; question: string; blockingStepId?: string }
  | { kind: "Reject"; reason: string }
  | { kind: "Abstain"; reason: string; missingContext?: string[] };
```

## 不变量

- 写入和命令执行必须受任务范围和用户授权约束。
- `GeneratePatch` 类决策只能产生候选 patch 或 edit step，不能绕过写入边界。
- `AskUser` 必须绑定阻塞原因或明确缺失上下文。
- `PermissionInvalid`、`PolicyUnsafe`、`TrustBoundaryBroken` 不通过 prompt retry 绕过。

## 验收方向

- schema invalid 可被识别并产生结构化错误。
- permission / trust failure 进入 reject / ask / escalate。
- every valid decision 可追溯到 task boundary、projection 或 evidence refs。
- v0.2 起危险命令和越权路径默认被阻止。

## 与其他模块关系

- 从 `ContextProjection` 获得输入。
- 向 `ActionGraph` 提交候选 node 或 policy reference。
- 由 `Scheduler` 决定何时执行相关 node。
- 与 `TrustGate` / `PermissionGate` 共享失败语义。
