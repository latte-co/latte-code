# Lattecode

Lattecode is a code agent that understands repository context, makes scoped code changes, runs verification, and produces reviewable handoff.

The project is design-first and intentionally conservative about implementation claims. The current implementation focus is local repository workflows, but that is an execution boundary, not the product positioning. Runtime concepts are documented as long-term internal evolution; they do not imply that the full internal runtime is already implemented.

## Reference Frame

From the broader software-engineering-system perspective, Lattecode is a code-agent `Data Plane` component. It may read repositories, call tools, make scoped changes, run verification, and hand results to humans or existing engineering systems.

Lattecode does not replace repository permissions, CI, code review, compliance, release, or deployment gates. In this repository, `Control Plane Authority` means only Lattecode internal runtime authority inside the process and task boundary.

## Runtime Evolution Concepts

The long-term runtime evolution documents use the following concepts. These concepts should grow from working code-agent traces, evidence, permissions, effects, and recovery needs; they are not `v0.1` product promises.

- `ActionGraph`: execution ledger, scheduling surface, and user-facing audit surface.
- `ActionNode`: a concrete unit of planned or executed work inside the graph.
- `StateStore`: owner of `Observation`, `Evidence`, versioned `Fact`, and fact lifecycle.
- `Scheduler`: decides which `ActionNode` can run based on dependencies, blockers, budgets, and recovery state.
- `EffectLedger`: records declared effects, effect results, and compensation state for mutating actions.
- `TransactionManager`: owns overlays, checkpoints, commits, rollbacks, and transaction status.
- `Reconciler`: detects and repairs drift across graph, facts, effects, and transactions.
- `PolicyDecision`: constrained decision output used by policy and guard boundaries.
- `Observation`, `Evidence`, `Fact`: separate layers for raw observations, traceable evidence, and promoted versioned facts.
- `NodeExecutor`: executes nodes through deterministic, single-decision, or bounded exploratory profiles.

## Documentation

- [Documentation language index](docs/README.md)
- [English documentation index](docs/en-US/README.md)
- [Architecture overview](docs/en-US/design/architecture-overview.md)
- [Code Agent Evolution Roadmap v0.1-v0.5](docs/en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md)
- [Code Agent Evolution Task Breakdown v0.1-v0.5](docs/en-US/milestones/targets/runtime-kernel-task-breakdown.md)
- Near-term module technical designs:
  - [`Code Agent Loop`](docs/en-US/design/modules/code-agent-loop.md)
  - [`Context Management and Compression`](docs/en-US/design/modules/context-management-and-compression.md)
  - [`Provider Compatibility Layer`](docs/en-US/design/modules/provider-compatibility-layer.md)
- Long-term runtime evolution targets:
  - [`ActionGraph`](docs/en-US/design/runtime-evolution/modules/action-graph.md)
  - [`StateStore`](docs/en-US/design/runtime-evolution/modules/state-store.md)
  - [`Scheduler`](docs/en-US/design/runtime-evolution/modules/scheduler.md)
  - [`EffectLedger`](docs/en-US/design/runtime-evolution/modules/effect-ledger.md)
  - [`TransactionManager`](docs/en-US/design/runtime-evolution/modules/transaction-manager.md)
  - [`Reconciler`](docs/en-US/design/runtime-evolution/modules/reconciler.md)
  - [`Policy Core and Guard`](docs/en-US/design/runtime-evolution/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](docs/en-US/design/runtime-evolution/modules/capability-adapter.md)
  - [`ContextProjection`](docs/en-US/design/runtime-evolution/modules/context-projection.md)
  - [`NodeExecutor`](docs/en-US/design/runtime-evolution/modules/node-executor.md)

## Development

Declared runtime and tooling baseline:

- Node.js `>=20.0.0`
- TypeScript
- Vitest

Declared commands:

```bash
npm run build
npm test
npm run test:coverage
```

No root `dev` command is currently declared.

## Repository Layout

| Path | Purpose |
| --- | --- |
| `docs/` | Language-indexed documentation root. |
| `docs/en-US/` | English formal documentation and translation status. |
| `docs/en-US/design/` | Architecture overview, near-term module designs, and long-term runtime evolution targets. |
| `docs/zh-CN/` | Chinese formal documentation and maintained counterparts. |
| `src/` | TypeScript source area for code-agent implementation work. Do not infer full internal-runtime maturity from its presence. |
| `tests/` | Vitest unit and integration tests. |
| `package.json` | Package metadata, Node engine declaration, and declared scripts. |
| `tsconfig.json` | TypeScript configuration. |
| `vitest.config.ts` | Vitest configuration. |
| `lattecode.config.example.jsonc` | Example JSONC configuration without secrets. |
| `LICENSE` | Apache License 2.0 text. |

## License

Lattecode is licensed under the [Apache License 2.0](LICENSE).
