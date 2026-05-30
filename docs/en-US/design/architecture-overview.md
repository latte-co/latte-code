# Fluxcode Architecture Overview

## Document Status

This document is the current formal Fluxcode architecture overview draft. It aligns the roadmap, module technical design documents, and task breakdown under `docs/en-US/design/`. It does not claim that the current `src/` implementation already provides these capabilities.

Chinese counterpart: [`docs/zh-CN/design/architecture-overview.md`](../../zh-CN/design/architecture-overview.md).

## 1. Top-level Reference Frame

From the perspective of the whole software-engineering system, a Code Agent is part of the `Data Plane`: it reads repositories, calls tools, proposes changes, runs verification, and hands results to humans and existing engineering systems. Fluxcode does not replace repo permissions, CI, code review, compliance, release, or deployment gates.

Fluxcode still needs a local runtime authority, also called an internal runtime control plane. This internal control plane has authority only inside the Fluxcode process and task boundary. It turns model output, tool observations, file effects, and human confirmations into traceable runtime objects.

Therefore, every use of `Control Plane Authority` in this design means **Fluxcode internal runtime authority**, not an external engineering governance `Control Plane`.

## 2. Relationship to Lark Documents

Lark documents may contain requirements, review comments, external design drafts, and human collaboration records. They are external collaboration and governance material, not Fluxcode runtime sources of truth.

| Lark document layer | Role in Fluxcode | Runtime ingestion path |
| --- | --- | --- |
| Product / requirement notes | User intent, constraints, acceptance context | Parsed into `TaskSpec`, acceptance criteria, and user-provided `Observation` records |
| Architecture / design review docs | External design basis or human decision record | Enter as sourced `Evidence` or candidate `Fact`; require explicit promotion |
| Task breakdown / project-management docs | External execution plan and collaboration state | Mapped to candidate `ActionGraph` / `ActionNode` objects; validated by `Scheduler` |
| Review comments / approval results | Human signal and gate input | Feed `human_approval_gate`, `trust_gate`, or `validation_gate` |

In short, Lark documents can be inputs, evidence, and collaboration surfaces for Fluxcode, but they cannot bypass `StateStore`, `EffectLedger`, `TransactionManager`, or `Reconciler` to directly change internal facts or commit state.

## 3. Top-level Planes / Layers

| Layer | Responsibility | Does Fluxcode own external authority? |
| --- | --- | --- |
| External software-engineering control systems | Repo permissions, CI, review, compliance, deployment gates, organizational process | No. Fluxcode can only adapt to, read, or request results from them |
| Fluxcode data-plane code agent boundary | Execute code tasks, propose edits, run verification, produce evidence and handoff information | No. It is an execution-oriented data-plane component inside the engineering system |
| Fluxcode internal runtime authority services | `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `Reconciler` | Only inside the Fluxcode runtime |
| Executor / capability layer | File, shell, LSP, Git, MCP, test runner, model-call adapters | No independent authority; adapters must emit runtime-native objects |
| UX / human handoff layer | Show plans, blockers, risks, evidence, approval requests, and recovery options | Does not replace human judgment; provides an auditable handoff surface |

## 4. Core Object Boundaries

### 4.1 `ActionGraph`

`ActionGraph` is an execution ledger, scheduling surface, and UX surface. It is not an omniscient state container.

It is responsible for:

- Recording task decomposition into `ActionNode` objects.
- Expressing dependency, blocking, verification, and reconcile relations.
- Exposing ready / blocked / failed / completed nodes to `Scheduler`.
- Acting as an audit index that links `PolicyDecision`, `Evidence`, `EffectRecord`, `Transaction`, and `Fact` back to concrete actions.
- Showing users what the system plans to do, has done, and needs help with.

It does not directly own `Fact` lifecycle, effect authority, commit / rollback state, or reconcile semantics.

### 4.2 Internal runtime authority services

| Service | Internal authority | Must not be delegated to |
| --- | --- | --- |
| `StateStore` | `Observation`, `Evidence`, versioned `Fact`, fact lifecycle, `ContextProjection` | transcript, prompt, one graph blob |
| `Scheduler` | `ActionNode` executability, dependencies, budget, blockers, recovery points | LLM natural-language reasoning |
| `EffectLedger` | Declaration, result, and compensation state for file, shell, network, Git, external API, and approval effects | tool logs or chat history |
| `TransactionManager` | `OverlayRevision`, checkpoint, commit, rollback, compensation, transaction status | patch text or model memory |
| `Reconciler` | Drift detection and recovery semantics across graph / fact / effect / transaction | prompt retry after failure |

## 5. `Observation → Evidence → Fact` Promotion

Fluxcode does not treat tool output, model inference, or Lark document content as facts by default.

| Layer | Meaning | Default state |
| --- | --- | --- |
| `Observation` | Raw observation from a tool, user, environment, or external system | Unreviewed; may be partial, noisy, or stale |
| `Evidence` | Traceable evidence carrier with source, time, boundary, summary, and artifact reference | Traceable, but still not a fact |
| `Fact` | Versioned fact promoted into `StateStore` by a promotion rule / `TrustGate` | Requires lifecycle, coverage, confidence, and `evidenceIds` |

An LLM natural-language inference defaults to `Hypothesis`. Each bounded mini-loop step produces `Event`, `Observation`, `PolicyDecision`, or `EvidenceRef` by default; it becomes a `Fact` only through `TrustGate` or an explicit promotion rule.

## 6. Gate Taxonomy

| Gate kind | Owner | Typical input | Semantics after failure |
| --- | --- | --- | --- |
| `validation_gate` | verifier capability / `StateStore` | test, typecheck, LSP, tree-sitter evidence | Add verification, downgrade fact, or block node |
| `trust_gate` | trust policy / `StateStore` | trust zone, source, external effect scope | Escalate or abort; do not rely on prompt retry |
| `permission_gate` | capability resolver / permission store | capability grant, node scope, user authorization | Reject, ask user, or block |
| `human_approval_gate` | Human | risk summary, diff, non-compensable effect | Wait for user choice or abort |
| `transaction_gate` | `TransactionManager` | overlay status, rollback handle, verification freshness | Block commit / rollback / rebase |
| `reconcile_gate` | `Reconciler` | stale facts, partial effects, invalidated overlay, affected nodes | Repair state before further scheduling |

## 7. Node-level Bounded ReAct

Fluxcode rejects agent-level / global ReAct as the runtime main controller. It accepts node-level bounded ReAct as a local execution strategy used by `NodeExecutor` for exploratory `ActionNode` execution.

ReAct is an execution strategy, not the runtime architecture. Global scheduling, fact promotion, effect declaration, and commit / rollback remain owned by internal runtime authority services.

`NodeExecutor` supports three execution profiles:

| Profile | Use case | Mini-loop |
| --- | --- | --- |
| `deterministic` | Deterministic node with known inputs, capability, and output contract | None |
| `single_decision` | Node requiring one LLM `PolicyDecision` | None |
| `exploratory` | Node requiring local exploration, recall, or tentative verification | Bounded ReAct mini-loop |

The bounded mini-loop must have a step budget, capability allowlist, read/write/effect boundary, evidence policy, and exit condition. It cannot directly promote `Fact`, bypass `EffectLedger`, directly commit / rollback, or modify global scheduling.

## 8. Document Structure Relationship

| Document | Role |
| --- | --- |
| This document | Current formal architecture overview; defines reference frame, layers, core boundaries, and document relationships |
| [`modules/`](./modules/action-graph.md) | Detailed module design placeholders for implementation work; each module owns inputs/outputs, contracts, invariants, and acceptance direction |
| [`runtime-kernel-roadmap-v0.1-v0.5.md`](./runtime-kernel-roadmap-v0.1-v0.5.md) | Version goals and cross-version invariants from `v0.1` through `v0.5` |
| [`runtime-kernel-task-breakdown.md`](./runtime-kernel-task-breakdown.md) | Independent task breakdown with tasks, dependencies, acceptance criteria, and non-goals per version |

## 9. Current Invariants

- Fluxcode is externally a code-agent `Data Plane`, not an external engineering governance `Control Plane`.
- Every `Control Plane Authority` reference must be scoped to Fluxcode internal runtime authority.
- `ActionGraph` is a ledger, scheduling surface, and UX surface, not an omniscient state container.
- `Observation`, `Evidence`, and `Fact` remain separate; `Fact` can only be promoted by promotion rules / gates.
- `NodeExecutor` may use node-level bounded ReAct, but the global runtime architecture must not collapse into an agent-level ReAct loop.
- Design documents are drafts and must not imply that these capabilities are fully implemented.
