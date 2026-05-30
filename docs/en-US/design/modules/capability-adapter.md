# Module Technical Design: Capability Adapter

## Document Status

Current design placeholder for clarifying the anti-corruption boundary where external tools enter Fluxcode. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/capability-adapter.md`](../../../zh-CN/design/modules/capability-adapter.md).

## Responsibility

- Wrap external abilities such as file, shell, LSP, Git, MCP, test runner, and model call as runtime-native `Capability`.
- Declare input/output, pre/post condition, permission, sandbox, evidence requirement, and failure modes.
- Translate external results into `Observation`, `Evidence`, `EffectRecord`, or `ActionResult`.
- Isolate external-protocol pollution such as prompt injection, inconsistent permission semantics, and opaque side effects.

## Non-goals

- Does not let external protocols write directly into internal stores.
- Does not turn tool output directly into `Fact`.
- Does not bypass `PolicyGuard`, `EffectLedger`, or transaction boundary.
- Does not make tool count an architecture goal.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | capability invocation, node scope, permission grant, sandbox policy, raw tool result |
| Output | runtime-native result, `Observation`, `Evidence`, `EffectRecord` update, capability status |

## Core Data Contracts

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

## Invariants

- External capability results must be translated into runtime-native objects.
- Mutating capability must first have `EffectRecord`.
- Capability status at least distinguishes declared / observed / effective.
- Adapter must record sandbox / trust boundary.

## Failure Modes

- MCP / shell output injects prompt instructions.
- LSP index does not match workspace revision.
- Git hook creates opaque side effects.
- Adapter hides degraded capability and makes scheduler misjudge.

## Testing / Acceptance Direction

- Output object type from each adapter is verifiable.
- prompt-in-tool-output does not enter trusted `PolicyDecision` context.
- degraded / blocked capability blocks or downgrades node.
- Mutating adapter cannot bypass effect declaration.

## Relation to Other Modules

- `NodeExecutor` executes capabilities through adapters.
- `EffectLedger` records mutating capability declaration and result.
- `StateStore` receives adapter-produced observation / evidence.
- `PolicyGuard` and `Scheduler` use capability descriptor for constraints.
