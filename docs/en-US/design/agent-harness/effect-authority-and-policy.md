# Effect Authority, Policy, and Isolation

Status: **design proposal; not implemented.**

Chinese counterpart: [Effect Authority、策略与隔离](../../../zh-CN/design/agent-harness/effect-authority-and-policy.md).

## 1. Decision

`latte-engine` is the only authority that may observe or change the external world. Providers, the headless agent loop, TUI, slash commands, extensions, and delegated agents submit typed requests and read redacted projections only. They never receive a SQLite writer, workspace-directory capability, process spawn, or general shell capability.

## 2. Effect protocol

Every Effect binds `thread_id`, `run_id`, exact Run revision, lease fencing token, input digest, and one-time approval. Its state machine is fixed:

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

An engine-private table holds executable descriptor, precondition, and observation detail. Public transcript/events contain only redacted operation, target, scope, result summary, and stable Effect ID. `Prepared` is durable before execution; approval consumption and `Prepared -> Started` share one transaction.

## 3. Policy, approval, and cancellation

Policy is fail-closed before execution: allow, require one-time approval, or deny. An approval card binds an exact descriptor digest and cannot be reused by Enter, stale UI, another revision, lease, or Effect. A permission answer is a runner control input; final validation and consumption remain in engine.

Cancellation requests stopping cancellable Provider/process work; it cannot infer that an Effect did not occur. Ambiguous process, filesystem, or network observation becomes `Unknown`. Reconciliation is explicit and auditable, never bypassed by automatic retry or later prompt.

## 4. Isolation and acceptance

Filesystem uses held workspace-relative capabilities and rejects path escape, link replacement, and unavailable safety primitives. Processes are argv-first; explicit shell is a separate high-risk action. Output, timeout, cancellation, and process-group supervision are bounded; platforms without full supervision fail before `Started`. Provider, repository, tool, and reminder text are untrusted and cannot alter classification, approval scope, workspace root, or policy.

- UT makes every transition, duplicate command, expired lease, wrong revision, and repeated approval fail closed; descriptors never leak via snapshot/event/log/transcript.
- UT gives cancellation, timeout, and observer failure distinct provable state, never inferred success.
- E2E requires precise approval in the final binary; denial, approval replay, path/link attack, and interruption cannot execute unauthorized Effect.
