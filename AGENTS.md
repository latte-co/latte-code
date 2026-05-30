# AGENTS.md

## 1. Project Overview

Fluxcode is currently a local-first TypeScript code agent / harness-native runtime design repository. The confirmed project baselines are as follows:

- Content format: Markdown technical documentation plus TypeScript project scaffolding / source area (see `src/`). Formal design documents must not imply that the runtime-kernel capabilities are already implemented.
- License: Apache License 2.0 (see `LICENSE`).
- Declared runtime / framework / package manager versions: Node.js >= 20, TypeScript, Vitest (see `package.json`, `tsconfig.json`, `vitest.config.ts`).
- Design theme: harness-native code agent as a data-plane code agent with internal runtime authority.
- Reference frame: from the external software-engineering-system perspective, Fluxcode is a code agent `Data Plane`; it does not replace repo permissions, CI, review, compliance, or deployment gates. `Control Plane Authority` means only Fluxcode internal runtime authority.
- Key terminology: `Data Plane`, `Control Plane Authority` scoped to internal runtime authority, `ActionGraph`, `ActionNode`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `PolicyDecision`, `Observation`, `Evidence`, `Fact`, `Reconciler`, `OverlayRevision`, `ContextProjection`, `NodeExecutor`.

Do NOT treat local tool or runtime files under `.opencode/`, `.oh-my-code/`, `.tmp/`, or `log/` as project source code, package manager evidence, or build entry points.

## 2. Project Structure Map

| Directory / File | Purpose | Local Documentation |
| --- | --- | --- |
| `docs/zh-CN/README.md` | Documentation index and maintenance conventions | – |
| `docs/zh-CN/research/` | Research facts, cross-product comparisons, verifiable observational conclusions | `docs/zh-CN/README.md` |
| `docs/zh-CN/research/code-agent-survey.md` | Cross-product research on `claude-code`, `codex`, `CodeWhale`, `opencode`, `oh-my-openagent` | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/` | Design proposals, architecture layering, interface models, phased roadmap | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/architecture-overview.md` | Current formal architecture overview: external `Data Plane` reference frame, internal runtime authority, module document map, promotion/gate/ReAct boundaries | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/modules/` | Current module-level technical design placeholders for implementation work | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/runtime-kernel-task-breakdown.md` | Current independent v0.1-v0.5 task breakdown | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md` | Current v0.1-v0.5 runtime kernel roadmap | `docs/zh-CN/README.md` |
| `docs/en-US/README.md` | English documentation index and translation status | `docs/README.md` |
| `docs/en-US/design/` | English counterparts for maintained formal design documents | `docs/en-US/README.md` |
| `src/` | TypeScript source area. Do not infer runtime-kernel implementation completeness from design documents; future runtime-kernel evolution should follow the current architecture overview, module designs, roadmap, and task breakdown. | `docs/zh-CN/design/architecture-overview.md`, `docs/zh-CN/design/modules/`, `docs/zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md`, `docs/zh-CN/design/runtime-kernel-task-breakdown.md` |
| `tests/` | Vitest unit + integration tests | `vitest.config.ts` |
| `package.json` / `tsconfig.json` / `vitest.config.ts` | Node/TypeScript/Vitest project configuration | – |
| `fluxcode.config.example.jsonc` | JSONC example configuration, no secrets | – |
| `LICENSE` | Apache License 2.0 text | – |
| `.gitignore` | Ignore Node/build/cache/coverage/local tool/runtime outputs | – |

## 3. Build & Development Commands

The following basic TypeScript/Vitest commands are currently declared:

```bash
npm run build
npm test
npm run test:coverage
```

No `dev`, release, remote service, or monorepo/workspace commands are declared; do not invent guessed commands.

## 4. Testing Instructions

The current authoritative testing framework is Vitest, with test files located at `tests/**/*.test.ts`.

```bash
npm test
npm run test:coverage
```

The coverage threshold is set to 98% in `vitest.config.ts`. No standalone lint command is currently declared.

## 5. Git Workflow

The repository currently appears to be in an early / newly initialized stage; do not assume established branching strategies or commit conventions.

Permitted operations:

- Modify Markdown documents directly related to the task.
- When adding documents to `docs/zh-CN/research/`, update the index in `docs/zh-CN/README.md` accordingly.
- When adding documents to `docs/zh-CN/design/`, update the index in `docs/zh-CN/README.md` accordingly.
- When adding or substantially updating maintained formal documentation in `docs/zh-CN/` or `docs/en-US/`, update the corresponding document in the other language or explicitly mark translation deferral in both language indexes.
- Verify that only expected file changes are included before committing.

Prohibited operations:

- (CRITICAL) Do not commit, amend, push, rebase, tag, or create PRs without explicit user request.
- (CRITICAL) Do not commit `.env*`, logs, caches, `.opencode/`, `.oh-my-code/`, `.tmp/`, `log/`, or `node_modules/`.
- Do not treat local tool state ignored by `.gitignore` as project deliverables.
- Do not introduce package manager files or scaffolding files without source code / build configuration.

## 6. Code Style Guidelines

This repository currently maintains primarily English technical Markdown. Writing should maintain clear boundaries between facts and design proposals; English technical identifiers should use backticks; filenames should use lowercase kebab-case.

### Documentation / Code Alignment

- Code changes that implement runtime concepts must keep the corresponding design documentation aligned.
- If implementation behavior diverges from `docs/` design documents, update the documents in the same change or explicitly document the divergence.
- Do not introduce new core runtime terminology in code without reflecting it in the relevant design documentation.

### Bilingual Documentation Alignment

- Formal documentation under `docs/zh-CN/` and `docs/en-US/` must stay structurally aligned.
- When adding or substantially updating a formal Chinese document, add or update the corresponding English document path, and vice versa.
- If a translation is intentionally deferred, mark it explicitly in both language indexes with the reason and expected follow-up.
- Do not let `docs/en-US/README.md` remain a generic placeholder once English counterparts exist.

✅ Recommended: Separate research facts from design proposals

```markdown
## Boundary between Research Facts and Design Proposals

- **Research Facts**: Record the existing architecture, capabilities, and boundaries of the system.
- **Design Proposals**: Explain the impact of these facts on the harness-native code agent.
```

❌ Avoid: Presenting unverified judgments as established facts

```markdown
This solution will definitely completely solve all code agent state management problems.
```

✅ Recommended: Technical prose + backtick-quoted identifiers

```markdown
Fluxcode is externally a code-agent `Data Plane`; `Control Plane Authority` refers only to internal runtime authority over facts, scheduling, effects, transactions, and reconcile semantics. `ActionGraph` remains the execution ledger and UX surface.
```

❌ Avoid: Unmarked terminology, sloganized expressions

```markdown
The graph is the soul of the system, and the scheduler closes the loop.
```

✅ Recommended: Use lowercase kebab-case for document filenames

```text
docs/design/architecture-overview.md
docs/research/code-agent-survey.md
```

❌ Avoid: Mixed spaces, uppercase, or ambiguous naming

```text
docs/design/Harness Native Final Draft.md
docs/research/agent调查.md
```

## 7. Boundaries & Guardrails

✅ **Always do**

- Always read `docs/zh-CN/README.md` before adding or moving documents.
- Always place research facts in `docs/zh-CN/research/`, and design proposals, interface models, and roadmaps in `docs/zh-CN/design/`.
- Always update the `docs/zh-CN/README.md` index when adding new documents.
- Always update `docs/en-US/README.md` when adding English counterparts or marking translation status.
- Always maintain consistency of terminology: `Data Plane`, internal-runtime-scoped `Control Plane Authority`, `ActionGraph`, `ActionNode`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `PolicyDecision`, `Observation`, `Evidence`, `Fact`, `Reconciler`, `OverlayRevision`, `ContextProjection`, `NodeExecutor`.
- Always state the fact that commands, tests, or toolchain are "non-existent / not declared" rather than filling in guesses.
- Before completing documentation changes, verify that `docs/README.md`, `docs/zh-CN/README.md`, and `docs/en-US/README.md` point only to existing maintained documents.

⚠️ **Ask first**

- Ask the user before introducing source directories, package managers, build systems, testing frameworks, or formatting tools.
- Confirm objectives before rewriting core design conclusions, MVP scope, or key terminology.
- Confirm before significantly restructuring the `docs/` directory or moving existing documents.
- Confirm before running commands that modify the environment, install dependencies, or generate caches.

🚫 **Never do**

- Never treat `.opencode/`, `.oh-my-code/`, `.tmp/`, or `log/` as project source code or configuration sources.
- Never treat `.tmp/` design drafts as formal source of truth; use them only as temporary inputs and re-establish conclusions in `docs/`.
- Never run guessed `test`, `build`, `lint`, `typecheck`, `dev`, install, or scaffolding commands.
- Never commit secrets, `.env*`, logs, caches, build artifacts, or dependency directories.
- Never overstate implementation maturity, release processes, or stable technology stack versions beyond the files and commands declared in this repository.
- Never write any license other than Apache License 2.0 into the project description, unless explicitly changed in the repository files.

## 8. Related Documentation

- `docs/zh-CN/README.md`: Documentation index and maintenance conventions.
- `docs/en-US/README.md`: English documentation index and translation status.
- `docs/zh-CN/research/code-agent-survey.md`: Cross-product research on code agent / agent workflow.
- `docs/zh-CN/design/architecture-overview.md`: Current formal architecture overview and top-level reference frame.
- `docs/zh-CN/design/modules/`: Current module-level technical design placeholders.
- `docs/zh-CN/design/runtime-kernel-task-breakdown.md`: Current independent v0.1-v0.5 task breakdown.
- `docs/zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md`: Current runtime kernel roadmap from v0.1 to v0.5.
- `docs/en-US/design/architecture-overview.md`: English counterpart for the architecture overview.
- `docs/en-US/design/modules/`: English counterparts for module-level technical design placeholders.
- `docs/en-US/design/runtime-kernel-task-breakdown.md`: English counterpart for the task breakdown.
- `docs/en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md`: English counterpart for the v0.1-v0.5 runtime kernel roadmap.
- `LICENSE`: Apache License 2.0.
- `.gitignore`: Ignore rules for local tools, runtime, caches, builds, and sensitive files.
