# Lattecode

Lattecode is a local-first, harness-native code agent runtime design project. It combines English technical documentation with an early TypeScript source area for runtime-kernel implementation work.

The project is design-first and intentionally conservative about implementation claims. The current documentation defines the reference frame, architecture boundaries, module contracts, roadmap, and task breakdown; it does not imply that the full runtime kernel is already implemented.

## Reference Frame

From the broader software-engineering-system perspective, Lattecode is a code-agent `Data Plane` component. It may read repositories, call tools, propose changes, run verification, and hand results to humans or existing engineering systems.

Lattecode does not replace repository permissions, CI, code review, compliance, release, or deployment gates. In this repository, `Control Plane Authority` means only Lattecode internal runtime authority inside the process and task boundary.

## Architecture Concepts

The current runtime-kernel design uses the following core concepts:

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
- [Runtime kernel roadmap v0.1-v0.5](docs/en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md)
- [Runtime kernel task breakdown v0.1-v0.5](docs/en-US/design/runtime-kernel-task-breakdown.md)
- Module technical designs:
  - [`ActionGraph`](docs/en-US/design/modules/action-graph.md)
  - [`StateStore`](docs/en-US/design/modules/state-store.md)
  - [`Scheduler`](docs/en-US/design/modules/scheduler.md)
  - [`EffectLedger`](docs/en-US/design/modules/effect-ledger.md)
  - [`TransactionManager`](docs/en-US/design/modules/transaction-manager.md)
  - [`Reconciler`](docs/en-US/design/modules/reconciler.md)
  - [`Policy Core and Guard`](docs/en-US/design/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](docs/en-US/design/modules/capability-adapter.md)
  - [`ContextProjection`](docs/en-US/design/modules/context-projection.md)
  - [`NodeExecutor`](docs/en-US/design/modules/node-executor.md)

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
| `docs/en-US/design/` | Architecture overview, roadmap, task breakdown, and module technical designs. |
| `docs/zh-CN/` | Chinese formal documentation and maintained counterparts. |
| `src/` | TypeScript source area for implementation work. Do not infer full runtime-kernel maturity from its presence. |
| `tests/` | Vitest unit and integration tests. |
| `package.json` | Package metadata, Node engine declaration, and declared scripts. |
| `tsconfig.json` | TypeScript configuration. |
| `vitest.config.ts` | Vitest configuration. |
| `lattecode.config.example.jsonc` | Example JSONC configuration without secrets. |
| `LICENSE` | Apache License 2.0 text. |

## License

Lattecode is licensed under the [Apache License 2.0](LICENSE).
