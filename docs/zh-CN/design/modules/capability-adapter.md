# 模块技术设计：Capability Adapter

## 文档状态

当前设计占位，用于实现前明确外部工具进入 Fluxcode 的 anti-corruption 边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/capability-adapter.md`](../../../en-US/design/modules/capability-adapter.md)。

## 责任

- 把文件、shell、LSP、Git、MCP、测试运行器、模型调用等外部能力包装为 runtime-native `Capability`。
- 声明 input/output、pre/post condition、permission、sandbox、evidence requirement 和 failure modes。
- 将外部结果翻译成 `Observation`、`Evidence`、`EffectRecord` 或 `ActionResult`。
- 隔离外部协议污染，如 prompt injection、权限语义不一致和不透明副作用。

## 非目标

- 不允许外部协议直接写入内部 store。
- 不把工具输出直接变成 `Fact`。
- 不绕过 `PolicyGuard`、`EffectLedger` 或 transaction boundary。
- 不以工具数量作为架构目标。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | capability invocation、node scope、permission grant、sandbox policy、raw tool result |
| 输出 | runtime-native result、`Observation`、`Evidence`、`EffectRecord` update、capability status |

## 核心数据契约

```ts
type CapabilityDescriptor = {
  id: string;
  kind: "file" | "shell" | "lsp" | "git" | "mcp" | "test" | "model";
  inputs: string[];
  outputs: string[];
  requiredPermissions: string[];
  evidencePolicy: string[];
  failureModes: string[];
};
```

## 不变量

- 外部能力结果必须被翻译成 runtime-native 对象。
- mutating capability 必须先有 `EffectRecord`。
- capability status 至少区分 declared / observed / effective。
- adapter 必须记录 sandbox / trust boundary。

## 失败模式

- MCP / shell 输出注入 prompt 指令。
- LSP index 与 workspace revision 不一致。
- Git hook 产生不透明副作用。
- adapter 隐藏 degraded capability，导致 scheduler 误判。

## 测试 / 验收方向

- 每种 adapter 输出的对象类型可验证。
- prompt-in-tool-output 不进入 `PolicyDecision` 可信上下文。
- degraded / blocked capability 会阻塞或降级 node。
- mutating adapter 无法绕过 effect declaration。

## 与其他模块关系

- `NodeExecutor` 通过 adapter 执行 capability。
- `EffectLedger` 记录 mutating capability 的声明和结果。
- `StateStore` 接收 adapter 产出的 observation / evidence。
- `PolicyGuard` 和 `Scheduler` 使用 capability descriptor 做约束判断。
