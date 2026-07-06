# Runtime Evolution Target: Capability Adapter

## Status

This document defines the evolutionary design for `Capability Adapter`. Early focus is clear, controlled, and recorded local tool calls; mature stages make it the anti-corruption boundary for external protocols entering the runtime.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/capability-adapter.md`](../../../../zh-CN/design/runtime-evolution/modules/capability-adapter.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | local tool wrapper | File, search, edit, and shell verification are usable and recorded |
| v0.2 | `CapabilityDescriptor` | Declare inputs, outputs, permissions, risks, and failure modes |
| v0.4 | effect-aware adapter | Mutating capabilities produce effect records |
| v0.5 | anti-corruption layer | External protocols can only emit internal runtime objects |

## Responsibilities

- Wrap files, shell, LSP, Git, MCP, test runners, and model calls as internal runtime `Capability`.
- Declare input/output, pre/post conditions, permissions, sandbox, evidence requirements, and failure modes.
- Translate external results into `Observation`, `Evidence`, `EffectRecord`, or `ActionResult`.
- Isolate external protocol pollution such as prompt injection, inconsistent permission semantics, and opaque side effects.

## Non-goals

- `v0.1` does not optimize for tool count.
- External protocols cannot directly write internal stores.
- Tool output does not directly become `Fact`.
- Do not bypass `PolicyGuard`, `EffectLedger`, or transaction boundaries.

## Minimal Contract

```ts
type CapabilityDescriptor = {
  id: string;
  kind: "file" | "search" | "shell" | "lsp" | "git" | "mcp" | "test" | "model";
  mutating: boolean;
  requiredPermissions: string[];
  failureModes: string[];
};
```

## Invariants

- Every capability invocation must be traceable.
- External capability results must be translated into internal runtime objects.
- Mutating capabilities must first have an effect record or equivalent change summary.
- Adapter must record sandbox / trust boundary.

## Acceptance Direction

- `v0.1` basic tool calls are reviewable.
- Prompt-in-tool-output does not enter trusted context.
- Degraded / blocked capabilities block or degrade nodes.
- Mutating adapters cannot bypass effect declaration.

## Relationships

- `NodeExecutor` executes capabilities through adapters.
- `EffectLedger` records mutating capability declarations and results.
- `StateStore` receives observations / evidence from adapters.
- `PolicyGuard` and `Scheduler` use capability descriptors as constraints.
