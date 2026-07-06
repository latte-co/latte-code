# Runtime 演进目标：Capability Adapter

## 文档状态

本文定义 `Capability Adapter` 的渐进式设计。早期重点是把本地工具调用变得清晰、可控、可记录；成熟阶段再作为外部协议进入 runtime 的 anti-corruption boundary。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/capability-adapter.md`](../../../../en-US/design/runtime-evolution/modules/capability-adapter.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | local tool wrapper | 文件、搜索、编辑、shell 验证可用且有记录 |
| v0.2 | `CapabilityDescriptor` | 声明输入、输出、权限、风险和 failure modes |
| v0.4 | effect-aware adapter | mutating capability 产生 effect record |
| v0.5 | anti-corruption layer | 外部协议只能输出内部 runtime 对象 |

## 责任

- 把文件、shell、LSP、Git、MCP、测试运行器、模型调用等外部能力包装为内部 runtime `Capability`。
- 声明 input/output、pre/post condition、permission、sandbox、evidence requirement 和 failure modes。
- 将外部结果翻译成 `Observation`、`Evidence`、`EffectRecord` 或 `ActionResult`。
- 隔离外部协议污染，如 prompt injection、权限语义不一致和不透明副作用。

## 非目标

- v0.1 不追求工具数量。
- 不允许外部协议直接写入内部 store。
- 不把工具输出直接变成 `Fact`。
- 不绕过 `PolicyGuard`、`EffectLedger` 或 transaction boundary。

## 最小数据契约

```ts
type CapabilityDescriptor = {
  id: string;
  kind: "file" | "search" | "shell" | "lsp" | "git" | "mcp" | "test" | "model";
  mutating: boolean;
  requiredPermissions: string[];
  failureModes: string[];
};
```

## 不变量

- 每次能力调用必须能被 trace 记录。
- 外部能力结果必须被翻译成内部 runtime 对象。
- mutating capability 必须先有 effect 记录或等价修改摘要。
- adapter 必须记录 sandbox / trust boundary。

## 验收方向

- v0.1 基础工具调用可复盘。
- prompt-in-tool-output 不进入可信上下文。
- degraded / blocked capability 会阻塞或降级 node。
- mutating adapter 无法绕过 effect declaration。

## 与其他模块关系

- `NodeExecutor` 通过 adapter 执行 capability。
- `EffectLedger` 记录 mutating capability 的声明和结果。
- `StateStore` 接收 adapter 产出的 observation / evidence。
- `PolicyGuard` 和 `Scheduler` 使用 capability descriptor 做约束判断。
