# Extension and Delegation Capabilities

Status: **design proposal; not implemented.**

Chinese counterpart: [扩展与委派能力](../../../zh-CN/design/agent-harness/extensions-and-delegation.md).

## 1. Decision

Extension points are declarative capability contracts, not callbacks, shell scripts, or arbitrary native plugins loaded into agent runtime. Every capability has stable ID, version, provenance, visibility, input/output schema, resource bound, and required Effect class. Composition root creates an immutable registry at startup.

Phase one permits built-in typed tools, built-in slash actions, trusted text-only prompt commands, and explicit Provider adapters. Dynamic local code, shell interpolation, workspace overrides of built-ins, unverified MCP tools, and executable plugins are out of scope.

## 2. Invocation path

```text
catalog descriptor
-> typed request
-> headless orchestration
-> engine policy / approval / effect
-> redacted result projection
```

The catalog is metadata only, never an arbitrary executable callback. TUI popup and dispatch use one registry and recheck availability at dispatch. A prompt command expands only to bounded text through normal user submission; parsing reads no file/environment/shell and grants no extra privilege. Non-built-in capability shows provenance in TUI, logs, and transcript projection. Name conflicts are fixed reject-or-explicit-disambiguation, never silent replacement of a security-sensitive built-in.

## 3. Delegation

A delegated agent is a restricted child Run of the primary Session, not a second owner writing that Session concurrently. It has input budget, deadline, cancellation token, tool allowlist, and provenance; it asks engine for every Effect and returns a bounded redacted result summary to parent runner. Parent runner serially chooses start/await/merge/cancel. A child Effect binds its own Run revision, lease, and approval and cannot inherit a consumed parent approval. Primary conversation appends only user-visible delegation summary, never private scratchpad, partial stream, or credential.

## 4. Acceptance

- UT covers registry schema/version/name conflict/provenance and availability at both build and dispatch.
- UT proves an extension cannot obtain private descriptor, file handle, credential, or general Effect capability; child cancellation, resource limits, approval isolation, and result boundary are deterministic.
- E2E proves slash/prompt commands/children use one Engine approval and public projection path; disabled or unknown capability cannot affect a Session.
