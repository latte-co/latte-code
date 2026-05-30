# AGENTS.md

## 1. Project Overview

Fluxcode is currently a local-first TypeScript code agent / harness-native runtime research and implementation repository built upon document design. The confirmed project baselines are as follows:

- Content format: Markdown technical documentation + Phase 1 TypeScript basic code agent source code (see `src/`).
- License: Apache License 2.0 (see `LICENSE`).
- Declared runtime / framework / package manager versions: Node.js >= 20, TypeScript, Vitest (see `package.json`, `tsconfig.json`, `vitest.config.ts`).
- Design theme: harness-native code agent, evolving from message-driven tool runner to graph-driven execution runtime.
- Key terminology: `GraphState`, `Scheduler`, `Runtime Kernel`, `NodeExecutor`, `Tool evidence`, `Reconciler`, `Persistence / Recovery`.

Do NOT treat local tool or runtime files under `.opencode/`, `.oh-my-code/`, `.tmp/`, or `log/` as project source code, package manager evidence, or build entry points.

## 2. Project Structure Map

| Directory / File | Purpose | Local Documentation |
| --- | --- | --- |
| `docs/zh-CN/README.md` | Documentation index and maintenance conventions | – |
| `docs/zh-CN/research/` | Research facts, cross-product comparisons, verifiable observational conclusions | `docs/zh-CN/README.md` |
| `docs/zh-CN/research/code-agent-survey.md` | Cross-product research on `claude-code`, `codex`, `CodeWhale`, `opencode`, `oh-my-openagent` | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/` | Design proposals, architecture layering, interface models, phased roadmap | `docs/zh-CN/README.md` |
| `docs/zh-CN/design/harness-native-code-agent.md` | Target architecture and MVP proposal for harness-native code agent | `docs/zh-CN/README.md` |
| `src/` | TypeScript basic code agent implementation: CLI, agent loop, model, tools, permissions, events, session, evidence, graph-ready boundaries | `docs/zh-CN/design/basic-code-agent-implementation-plan.md` |
| `tests/` | Vitest unit + integration tests | `vitest.config.ts` |
| `package.json` / `tsconfig.json` / `vitest.config.ts` | Node/TypeScript/Vitest project configuration | – |
| `fluxcode.config.example.jsonc` | JSONC example configuration, no secrets | `docs/zh-CN/design/basic-code-agent-implementation-plan.md` |
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
- Verify that only expected file changes are included before committing.

Prohibited operations:

- (CRITICAL) Do not commit, amend, push, rebase, tag, or create PRs without explicit user request.
- (CRITICAL) Do not commit `.env*`, logs, caches, `.opencode/`, `.oh-my-code/`, `.tmp/`, `log/`, or `node_modules/`.
- Do not treat local tool state ignored by `.gitignore` as project deliverables.
- Do not introduce package manager files or scaffolding files without source code / build configuration.

## 6. Code Style Guidelines

This repository currently maintains primarily English technical Markdown. Writing should maintain clear boundaries between facts and design proposals; English technical identifiers should use backticks; filenames should use lowercase kebab-case.

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
`GraphState` is the central state of the runtime, containing at minimum nodes, dependencies, gates, evidence, and reconcile history.
```

❌ Avoid: Unmarked terminology, sloganized expressions

```markdown
Graph state is the soul of the entire system, and the scheduler is responsible for closing the loop.
```

✅ Recommended: Use lowercase kebab-case for document filenames

```text
docs/design/harness-native-code-agent.md
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
- Always maintain consistency of terminology: `GraphState`, `Scheduler`, `Runtime Kernel`, `NodeExecutor`, `Tool evidence`, `Reconciler`, `Persistence / Recovery`.
- Always state the fact that commands, tests, or toolchain are "non-existent / not declared" rather than filling in guesses.

⚠️ **Ask first**

- Ask the user before introducing source directories, package managers, build systems, testing frameworks, or formatting tools.
- Confirm objectives before rewriting core design conclusions, MVP scope, or key terminology.
- Confirm before significantly restructuring the `docs/` directory or moving existing documents.
- Confirm before running commands that modify the environment, install dependencies, or generate caches.

🚫 **Never do**

- Never treat `.opencode/`, `.oh-my-code/`, `.tmp/`, or `log/` as project source code or configuration sources.
- Never run guessed `test`, `build`, `lint`, `typecheck`, `dev`, install, or scaffolding commands.
- Never commit secrets, `.env*`, logs, caches, build artifacts, or dependency directories.
- Never claim that the current repository has application source code, a testing system, release processes, or stable technology stack versions.
- Never write any license other than Apache License 2.0 into the project description, unless explicitly changed in the repository files.

## 8. Related Documentation

- `docs/zh-CN/README.md`: Documentation index and maintenance conventions.
- `docs/zh-CN/research/code-agent-survey.md`: Cross-product research on code agent / agent workflow.
- `docs/zh-CN/design/harness-native-code-agent.md`: Target architecture, core model, and MVP scope for harness-native code agent.
- `LICENSE`: Apache License 2.0.
- `.gitignore`: Ignore rules for local tools, runtime, caches, builds, and sensitive files.
