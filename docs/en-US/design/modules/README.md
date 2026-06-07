# Current / Near-term Module Technical Designs

This directory only stores module-level technical designs that the current `v0.1` / near-term implementation should directly align with. Long-term runtime module targets have moved to [`../runtime-evolution/modules/`](../runtime-evolution/modules/README.md).

Current / near-term module documents:

- [`Code Agent Loop`](./code-agent-loop.md)
- [`Provider Compatibility Layer`](./provider-compatibility-layer.md)

## Boundary Notes

- Documents in this directory may reference later runtime objects, but their primary role must be `v0.1` / near-term implementation constraints.
- Long-term targets such as `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `Reconciler`, `ContextProjection`, and `NodeExecutor` should be maintained in the runtime evolution directory.
- Runtime evolution documents must not be phrased as modules already fully implemented in `src/`.
