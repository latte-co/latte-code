import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { AgentLoop } from "../../src/core/agent-loop.js";
import { InMemoryEventLog } from "../../src/events/event-log.js";
import { InMemoryEvidenceStore } from "../../src/evidence/store.js";
import { FakeModelClient } from "../../src/model/fake.js";
import { PermissionPolicy } from "../../src/permissions/policy.js";
import { InMemorySessionStore, recoverSessionFromEvents } from "../../src/session/session.js";
import { createDefaultRegistry } from "../../src/runtime/create-agent.js";
import { buildFailedHandoff, createTaskRunState, setRunStatus } from "../../src/core/run-state.js";
import { loadRuntimeContextSources } from "../../src/runtime/context-sources.js";
import { ToolRegistry } from "../../src/tools/registry.js";
import type { ToolDefinition } from "../../src/tools/types.js";
import type { ModelTurn } from "../../src/model/types.js";
import { createPermissionPendingInput, createQuestionPendingInput } from "../../src/core/contracts.js";

const execFileAsync = promisify(execFile);

function createLoop(dir: string, model: FakeModelClient, overrides: Record<string, unknown> = {}): { loop: AgentLoop; events: InMemoryEventLog; sessions: InMemorySessionStore } {
  const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, ...overrides });
  const events = new InMemoryEventLog();
  const sessions = new InMemorySessionStore();
  const loop = new AgentLoop({
    cwd: dir,
    config,
    model,
    registry: createDefaultRegistry(config),
    permissions: new PermissionPolicy(config.permissions, config.tools.shell),
    sessions,
    events,
    evidence: new InMemoryEvidenceStore()
  });
  return { loop, events, sessions };
}

function final(content = "done"): ModelTurn {
  return { type: "message", content };
}

function happyScript(summary = "done"): ModelTurn[] {
  return [final(summary)];
}

describe("AgentLoop integration", () => {
  it("runs fake-model tool call through permission, execution, evidence and final response", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-"));
    await writeFile(join(dir, "note.txt"), "hello agent", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "c1", name: "read_file", input: { path: "note.txt" } }] },
      final("File says hello agent.")
    ]);
    const { loop, events } = createLoop(dir, model);
    const result = await loop.run({ input: "read note" });
    expect(result.status).toBe("completed");
    expect(result.finalResponse).toContain("hello agent");
    expect(result.evidence).toHaveLength(1);
    expect((await events.read(result.session.id)).map((event) => event.type)).toContain("permission.decided");
    expect(recoverSessionFromEvents(result.session.id, await events.read(result.session.id)).status).toBe("completed");
  });

  it("resumes an existing session snapshot by id", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-"));
    const model = new FakeModelClient([...happyScript("first"), ...happyScript("second")]);
    const { loop } = createLoop(dir, model);
    const first = await loop.run({ input: "one", sessionId: "fixed" });
    const second = await loop.run({ input: "two", sessionId: first.session.id });
    expect(second.session.transcript.some((entry) => entry.content === "one")).toBe(true);
    expect(second.finalResponse).toBe("second");
  });

  it("replays event log after snapshot cursor when opening an existing session", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-replay-"));
    const model = new FakeModelClient(happyScript("after replay"));
    const { loop, events, sessions } = createLoop(dir, model);
    await events.append("session.created", "stale", { sessionId: "stale" });
    await events.append("user.input", "stale", { input: "old" });
    await events.append("tool.completed", "stale", { summary: "Read file" });
    await events.append("evidence.recorded", "stale", { evidenceId: "ev1" });
    await sessions.save({ id: "stale", status: "running", transcript: [{ role: "user", content: "old" }, { role: "tool", content: "Read file" }], evidenceIds: ["ev1"], lastEventSeq: 3 });
    const result = await loop.run({ input: "new", sessionId: "stale" });
    expect(result.session.evidenceIds).toEqual(["ev1"]);
    expect(result.session.lastEventSeq).toBeGreaterThan(4);
  });

  it("recovers an existing session from event log when the snapshot is missing", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-log-recover-"));
    const model = new FakeModelClient(happyScript("after log recovery"));
    const { loop, events } = createLoop(dir, model);
    await events.append("session.created", "log-only", { sessionId: "log-only" });
    await events.append("user.input", "log-only", { input: "old" });
    const result = await loop.run({ input: "new", sessionId: "log-only" });
    expect(result.session.transcript.some((entry) => entry.content === "old")).toBe(true);
    expect(result.finalResponse).toBe("after log recovery");
  });

  it("pauses mutating write for confirmation by default", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-ask-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c2", name: "write_file", input: { path: "out.txt", content: "unsafe" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "write" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingInput).toMatchObject({ kind: "permission", action: "write_file", toolCallId: "c2" });
    expect(result.pendingPermission?.toolName).toBe("write_file");
  });

  it("denies dangerous shell commands and does not execute them", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-deny-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c3", name: "shell_exec", input: { command: "rm -rf important" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "delete" });
    expect(result.status).toBe("blocked");
    expect(result.error).toContain("delete");
    expect(result.evidence[0]?.permission.action).toBe("deny");
  });

  it("asks for shell commands matching requireApprovalFor even when generic mutating shell is allowed", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-shell-ask-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c3b", name: "shell_exec", input: { command: "curl https://example.com" } }] }]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" }, tools: { shell: { requireApprovalFor: ["network"] } } });
    const result = await loop.run({ input: "network" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingPermission?.reason).toContain("network");
  });

  it("records invalid tool parameters without executing the tool", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-invalid-"));
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "c4", name: "read_file", input: { missing: "path" } }] },
      final("Recovered from invalid params.")
    ]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "bad" });
    expect(result.status).toBe("completed");
    expect(result.evidence[0]?.outputSummary).toContain("Schema validation failed");
  });

  it("executes direct-loop edit_file and records git_diff before changed-file handoff", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-edit-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "read-target", name: "read_file", input: { path: "target.txt" } }] },
      { type: "tool_calls", toolCalls: [{ id: "edit-target", name: "edit_file", input: { path: "target.txt", mode: "replace", oldText: "before", newText: "after" } }] },
      { type: "tool_calls", toolCalls: [{ id: "diff-target", name: "git_diff", input: {} }] },
      final("edit complete")
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "edit" });
    expect(result.status).toBe("completed");
    expect(await readFile(join(dir, "target.txt"), "utf8")).toContain("after");
    expect(result.evidence.map((entry) => entry.toolName)).toEqual(expect.arrayContaining(["read_file", "edit_file", "git_diff"]));
  });

  it("fails direct handoff when shell verification fails before final response", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-shell-fail-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "verify-fail", name: "shell_exec", input: { command: "npm test" } }] }, final("verification failed but model finalized")]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "verify" });
    expect(result.status).toBe("failed");
    expect(result.finalResponse).toBeUndefined();
    expect(result.runState?.status).toBe("failed");
    expect(result.handoff).toMatchObject({ status: "failed", summary: "verification failed but model finalized" });
    expect(result.handoff?.verification).toEqual([
      expect.objectContaining({ command: "npm test", status: "failed", exitCode: 1 })
    ]);
  });

  it("uses latest shell verification result by command for direct handoff", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-shell-latest-"));
    await writeFile(join(dir, "package.json"), JSON.stringify({ scripts: { test: "node -e \"require('node:fs').existsSync('pass.flag') || process.exit(1)\"" } }), "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "verify-fail", name: "shell_exec", input: { command: "npm test" } }] },
      { type: "tool_calls", toolCalls: [{ id: "write-flag", name: "write_file", input: { path: "pass.flag", content: "ok", createIntent: true } }] },
      { type: "tool_calls", toolCalls: [{ id: "verify-pass", name: "shell_exec", input: { command: "npm test" } }] },
      final("verification recovered")
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "verify" });
    expect(result.status).toBe("completed");
    expect(result.finalResponse).toBe("verification recovered");
    expect(result.handoff?.verification).toEqual([
      expect.objectContaining({ command: "npm test", status: "passed", exitCode: 0 })
    ]);
    expect(result.handoff?.risks).toEqual([]);
  });

  it("keeps failed shell verification across permission resume when model finalizes without rerun", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-shell-resume-fail-"));
    await writeFile(join(dir, "package.json"), JSON.stringify({ scripts: { test: "node -e \"require('node:fs').existsSync('pass.flag') || process.exit(1)\"" } }), "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "verify-before-pause", name: "shell_exec", input: { command: "npm test" } }] },
      { type: "tool_calls", toolCalls: [{ id: "write-after-fail", name: "write_file", input: { path: "pass.flag", content: "ok", createIntent: true } }] },
      final("model finalized without rerunning tests")
    ]);
    const { loop } = createLoop(dir, model);
    const waiting = await loop.run({ input: "verify then repair" });
    expect(waiting.status).toBe("waiting_permission");

    const permissionId = waiting.pendingInput?.kind === "permission" ? waiting.pendingInput.permissionId : "";
    const resumed = await loop.resume({ sessionId: waiting.session.id, input: { kind: "permission", permissionId, decision: "approve", reason: "approve test repair" } });

    expect(await readFile(join(dir, "pass.flag"), "utf8")).toBe("ok");
    expect(resumed.status).toBe("failed");
    expect(resumed.finalResponse).toBeUndefined();
    expect(resumed.handoff?.verification).toEqual([
      expect.objectContaining({ command: "npm test", status: "failed", exitCode: 1 })
    ]);
    expect(resumed.handoff?.risks).toEqual(["Command failed: npm test"]);
  });

  it("allows direct handoff after permission resume when same shell command later passes", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-shell-resume-pass-"));
    await writeFile(join(dir, "package.json"), JSON.stringify({ scripts: { test: "node -e \"require('node:fs').existsSync('pass.flag') || process.exit(1)\"" } }), "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "verify-before-pause", name: "shell_exec", input: { command: "npm test" } }] },
      { type: "tool_calls", toolCalls: [{ id: "write-after-fail", name: "write_file", input: { path: "pass.flag", content: "ok", createIntent: true } }] },
      { type: "tool_calls", toolCalls: [{ id: "verify-after-resume", name: "shell_exec", input: { command: "npm test" } }] },
      final("model reran tests after repair")
    ]);
    const { loop } = createLoop(dir, model);
    const waiting = await loop.run({ input: "verify then repair" });
    expect(waiting.status).toBe("waiting_permission");

    const permissionId = waiting.pendingInput?.kind === "permission" ? waiting.pendingInput.permissionId : "";
    const resumed = await loop.resume({ sessionId: waiting.session.id, input: { kind: "permission", permissionId, decision: "approve", reason: "approve test repair" } });

    expect(resumed.status).toBe("completed");
    expect(resumed.finalResponse).toBe("model reran tests after repair");
    expect(resumed.handoff?.verification).toEqual([
      expect.objectContaining({ command: "npm test", status: "passed", exitCode: 0 })
    ]);
    expect(resumed.handoff?.risks).toEqual([]);
  });

  it("canonicalizes and filters direct diff changedFiles paths", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-diff-paths-"));
    const registry = new ToolRegistry();
    const fakeGitDiff: ToolDefinition = {
      name: "git_diff",
      description: "fake diff",
      inputSchema: { type: "object" },
      outputSchema: { type: "object", required: ["changedFiles"], properties: { changedFiles: { type: "array", items: { type: "string" } } } },
      riskLevel: "low",
      mutating: false,
      permission: { reason: "fake diff" },
      async execute() {
        return {
          callId: "",
          toolName: "git_diff",
          ok: true,
          summary: "fake diff",
          references: [dir],
          output: { changedFiles: [join(dir, "src", "inside.ts"), "relative.ts", "../outside.ts", join(dir, "..", "absolute-outside.ts"), dir] },
          truncated: false
        };
      }
    };
    registry.register(fakeGitDiff);
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const loop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "fake-diff", name: "git_diff", input: {} }] }, final("diff paths")]), registry, permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const result = await loop.run({ input: "diff" });
    expect(result.status).toBe("completed");
    expect(result.handoff?.changedFiles).toEqual(["src/inside.ts", "relative.ts"]);
  });

  it("normalizes direct changed-file fallback paths from write/edit outputs", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-relative-path-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "write-relative", name: "write_file", input: { path: "nested/out.txt", content: "ok", createDirs: true, createIntent: true } }] }, final("write complete")]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "write" });
    expect(result.status).toBe("completed");
    expect(result.handoff?.changedFiles).toEqual(["nested/out.txt"]);
    expect(result.handoff?.changedFiles[0]).not.toContain(dir);
    expect(await readFile(join(dir, "nested", "out.txt"), "utf8")).toBe("ok");
  });

  it("finalizes release handoff with canonical verification, trace, evidence, and changed files", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-release-handoff-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "read-release", name: "read_file", input: { path: "target.txt" } }] },
      { type: "tool_calls", toolCalls: [{ id: "edit-release", name: "edit_file", input: { path: "target.txt", mode: "replace", oldText: "before", newText: "after" } }] },
      { type: "tool_calls", toolCalls: [{ id: "diff-release", name: "git_diff", input: {} }] },
      final("release handoff")
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "release" });
    expect(result.status).toBe("completed");
    expect(result.handoff).toMatchObject({ status: "completed", changedFiles: ["target.txt"], risks: [] });
    expect(result.handoff?.verification).toEqual([]);
    expect(result.handoff?.traceRefs).toEqual(result.runState?.turns.map((turn) => turn.id));
    expect(result.handoff?.evidenceRefs.length).toBeGreaterThan(0);
  });

  it("blocks write_file through read_before_write_gate when overwrite intent is unsafe", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-read-before-write-"));
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "overwrite-target", name: "write_file", input: { path: "target.txt", content: "after" } }] }
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "overwrite" });
    expect(result.status).toBe("blocked");
    expect(result.pendingInput).toMatchObject({ kind: "question" });
    expect(result.error).toContain("read_before_write_gate");
    expect(await readFile(join(dir, "target.txt"), "utf8")).toBe("before\n");
  });

  it("resumes a waiting permission with the same permission id and binds tool evidence to the run", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-permission-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "write-1", name: "write_file", input: { path: "approved.txt", content: "ok", createIntent: true } }] },
      { type: "tool_calls", toolCalls: [{ id: "diff-approved", name: "git_diff", input: {} }] },
      final("permission resumed")
    ]);
    const { loop, events } = createLoop(dir, model);
    const first = await loop.run({ input: "write" });
    expect(first.status).toBe("waiting_permission");
    const permissionId = first.pendingInput?.kind === "permission" ? first.pendingInput.permissionId : "";
    const resumed = await loop.resume({ sessionId: first.session.id, input: { kind: "permission", permissionId, decision: "approve", reason: "approved in test" } });
    expect(resumed.status).toBe("completed");
    expect(resumed.handoff?.changedFiles).toEqual(["approved.txt"]);
    expect(resumed.runState?.turns.flatMap((turn) => turn.evidenceIds).length).toBeGreaterThan(0);
    expect((await events.read(first.session.id)).some((event) => event.type === "resume.received")).toBe(true);
  });

  it("persists approved pending tool execution failures during resume", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-pending-fail-"));
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "read-target-before-edit", name: "read_file", input: { path: "target.txt" } }] },
      { type: "tool_calls", toolCalls: [{ id: "edit-target-after-approval", name: "edit_file", input: { path: "target.txt", mode: "replace", oldText: "before", newText: "after" } }] }
    ]);
    const { loop, sessions, events } = createLoop(dir, model);
    const waiting = await loop.run({ input: "read then edit" });
    expect(waiting.status).toBe("waiting_permission");

    await writeFile(join(dir, "target.txt"), "outside change\n", "utf8");
    const permissionId = waiting.pendingInput?.kind === "permission" ? waiting.pendingInput.permissionId : "";
    const resumed = await loop.resume({ sessionId: waiting.session.id, input: { kind: "permission", permissionId, decision: "approve", reason: "approved stale edit" } });

    expect(resumed.status).toBe("failed");
    expect(resumed.error).toContain("stale_write_gate");
    expect(resumed.handoff).toMatchObject({ status: "failed" });
    expect(resumed.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: expect.stringContaining("stale_write_gate"), summary: expect.stringContaining("stale_write_gate") });
    expect((await sessions.get(waiting.session.id))?.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: expect.stringContaining("stale_write_gate") });
    const recovered = recoverSessionFromEvents(waiting.session.id, await events.read(waiting.session.id));
    expect(recovered.status).toBe("failed");
    expect(recovered.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: expect.stringContaining("stale_write_gate") });
    expect((await events.read(waiting.session.id)).map((event) => event.type)).toEqual(expect.arrayContaining(["resume.received", "turn.completed", "loop.failed"]));
  });

  it("keeps direct changedFiles across a later permission resume", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-direct-changedfiles-resume-"));
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "write-before-pause", name: "write_file", input: { path: "early.txt", content: "ok", createIntent: true } }] },
      { type: "tool_calls", toolCalls: [{ id: "shell-after-write", name: "shell_exec", input: { command: "printf resumed" } }] },
      final("permission resumed after earlier write")
    ]);
    const { loop } = createLoop(dir, model);
    const waitingForWrite = await loop.run({ input: "write then ask shell" });
    expect(waitingForWrite.status).toBe("waiting_permission");
    const writePermissionId = waitingForWrite.pendingInput?.kind === "permission" ? waitingForWrite.pendingInput.permissionId : "";

    const waitingForShell = await loop.resume({ sessionId: waitingForWrite.session.id, input: { kind: "permission", permissionId: writePermissionId, decision: "approve", reason: "approve early write" } });
    expect(waitingForShell.status).toBe("waiting_permission");
    expect(await readFile(join(dir, "early.txt"), "utf8")).toBe("ok");
    expect(waitingForShell.runState?.changedFiles).toEqual(["early.txt"]);
    const shellPermissionId = waitingForShell.pendingInput?.kind === "permission" ? waitingForShell.pendingInput.permissionId : "";

    const resumed = await loop.resume({ sessionId: waitingForShell.session.id, input: { kind: "permission", permissionId: shellPermissionId, decision: "approve", reason: "approve later shell" } });
    expect(resumed.status).toBe("completed");
    expect(resumed.handoff?.changedFiles).toEqual(["early.txt"]);
    expect(resumed.runState?.changedFiles).toEqual(["early.txt"]);
  });

  it("resumes a persisted blocked question and continues the direct loop", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-question-"));
    const { loop, sessions } = createLoop(dir, new FakeModelClient([final("question resumed")]));
    const question = createQuestionPendingInput({ questionId: "q-direct", prompt: "Which file?", expectedAnswer: "text" });
    const run = createTaskRunState("blocked-direct", "needs context", "run_blocked_direct");
    run.pendingInput = question;
    run.status = "blocked";
    await sessions.save({ id: "blocked-direct", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: run, pendingInput: question });
    const resumed = await loop.resume({ sessionId: "blocked-direct", input: { kind: "question", questionId: question.questionId, answerText: "Use answer.ts" } });
    expect(resumed.status).toBe("completed");
    expect(resumed.finalResponse).toBe("question resumed");
    expect(resumed.session.transcript.some((entry) => entry.content === "Use answer.ts")).toBe(true);
  });

  it("maps mismatched resume inputs and denied permission resume to blocked handoff contracts", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-negative-"));
    const { loop: questionLoop, sessions: questionSessions } = createLoop(dir, new FakeModelClient([]));
    const question = createQuestionPendingInput({ questionId: "q-mismatch", prompt: "Which file?", expectedAnswer: "text" });
    const blockedRun = createTaskRunState("question-mismatch", "needs context", "run_question_mismatch");
    blockedRun.pendingInput = question;
    blockedRun.status = "blocked";
    await questionSessions.save({ id: "question-mismatch", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: blockedRun, pendingInput: question });
    const mismatch = await questionLoop.resume({ sessionId: "question-mismatch", input: { kind: "permission", permissionId: "wrong", decision: "approve" } });
    expect(mismatch.status).toBe("blocked");
    expect(mismatch.handoff?.blockers[0]).toContain("does not match pending");

    const denyLoop = createLoop(dir, new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "write-deny", name: "write_file", input: { path: "denied.txt", content: "no", createIntent: true } }] }])).loop;
    const waiting = await denyLoop.run({ input: "write" });
    const wrongId = await denyLoop.resume({ sessionId: waiting.session.id, input: { kind: "permission", permissionId: "wrong", decision: "approve" } });
    expect(wrongId.status).toBe("blocked");
    const denyOnlyLoop = createLoop(dir, new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "write-deny-2", name: "write_file", input: { path: "denied.txt", content: "no", createIntent: true } }] }])).loop;
    const waitingToDeny = await denyOnlyLoop.run({ input: "write" });
    const permissionId = waitingToDeny.pendingInput?.kind === "permission" ? waitingToDeny.pendingInput.permissionId : "";
    const denied = await denyOnlyLoop.resume({ sessionId: waitingToDeny.session.id, input: { kind: "permission", permissionId, decision: "deny", reason: "not approved" } });
    expect(denied.status).toBe("blocked");
    expect(denied.handoff?.summary).toBe("not approved");
    expect(denied.handoff?.requiredDecisions).toEqual([{ kind: "permission", id: permissionId, reason: "Mutating tool follows mutatingTools policy" }]);

    const denyDefaultLoop = createLoop(dir, new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "write-deny-default", name: "write_file", input: { path: "denied-default.txt", content: "no", createIntent: true } }] }])).loop;
    const waitingDefault = await denyDefaultLoop.run({ input: "write" });
    const defaultPermissionId = waitingDefault.pendingInput?.kind === "permission" ? waitingDefault.pendingInput.permissionId : "";
    expect((await denyDefaultLoop.resume({ sessionId: waitingDefault.session.id, input: { kind: "permission", permissionId: defaultPermissionId, decision: "deny" } })).error).toBe("Permission denied by resume input.");

    const { loop: questionWrongIdLoop, sessions: questionWrongIdSessions } = createLoop(dir, new FakeModelClient([]));
    const wrongIdQuestion = createQuestionPendingInput({ questionId: "q-right", prompt: "Which file?", expectedAnswer: "text" });
    const wrongIdRun = createTaskRunState("question-wrong-id", "needs context", "run_question_wrong_id");
    wrongIdRun.pendingInput = wrongIdQuestion;
    wrongIdRun.status = "blocked";
    await questionWrongIdSessions.save({ id: "question-wrong-id", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: wrongIdRun, pendingInput: wrongIdQuestion });
    expect((await questionWrongIdLoop.resume({ sessionId: "question-wrong-id", input: { kind: "question", questionId: "wrong", answerText: "answer" } })).error).toContain("Question resume id");
  });

  it("covers resume edge states and recovery repairs without changing run semantics", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-edges-"));
    const { loop, sessions } = createLoop(dir, new FakeModelClient([final("after repaired question"), final("after pending call"), final("after missing pending call")]), { permissions: { mutatingTools: "allow" } });
    const noPending = await loop.resume({ sessionId: "new-session", input: { kind: "question", questionId: "q", answerText: "answer" } });
    expect(noPending.status).toBe("blocked");
    expect(noPending.error).toContain("No resumable");

    const question = createQuestionPendingInput({ questionId: "q-repair", prompt: "Need answer", expectedAnswer: "json" });
    const blockedRun = createTaskRunState("blocked-session", "blocked", "run_blocked_repair");
    blockedRun.pendingInput = question;
    blockedRun.status = "blocked";
    await sessions.save({ id: "blocked-session", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: blockedRun });
    const repaired = await loop.resume({ sessionId: "blocked-session", input: { kind: "question", questionId: "q-repair", answerJson: { file: "answer.ts" } } });
    expect(repaired.status).toBe("completed");
    expect(repaired.session.pendingInput).toBeUndefined();

    const call = { id: "pending-write", name: "write_file", input: { path: "approved-edge.txt", content: "ok", createIntent: true } };
    const permission = createPermissionPendingInput("pending-call-session", call, "approve write");
    const waitingRun = createTaskRunState("pending-call-session", "pending", "run_pending_call");
    waitingRun.status = "waiting_permission";
    waitingRun.pendingInput = permission;
    await sessions.save({ id: "pending-call-session", status: "waiting_permission", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: waitingRun, pendingInput: permission, pendingPermission: { callId: call.id, toolName: call.name, reason: permission.reason, permissionId: permission.permissionId, action: permission.action, path: "approved-edge.txt" }, pendingToolCall: call });
    const approved = await loop.resume({ sessionId: "pending-call-session", input: { kind: "permission", permissionId: permission.permissionId, decision: "approve" } });
    expect(approved.status).toBe("completed");
    expect(await readFile(join(dir, "approved-edge.txt"), "utf8")).toBe("ok");

    const noCallRun = createTaskRunState("pending-no-call-session", "pending", "run_pending_no_call");
    const noCallPermission = createPermissionPendingInput("pending-no-call-session", { id: "missing-call", name: "write_file", input: { path: "no-call.txt" } }, "approve without call");
    noCallRun.status = "blocked";
    noCallRun.pendingInput = noCallPermission;
    await sessions.save({ id: "pending-no-call-session", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: noCallRun, pendingInput: noCallPermission });
    expect((await loop.resume({ sessionId: "pending-no-call-session", input: { kind: "permission", permissionId: noCallPermission.permissionId, decision: "approve" } })).status).toBe("completed");
  });

  it("covers tool execution non-error and blocking gate normalization", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-tool-errors-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, permissions: { mutatingTools: "allow" } });
    const throwingRegistry = new ToolRegistry();
    throwingRegistry.register({ name: "read_file", description: "throws", inputSchema: { type: "object" }, outputSchema: { type: "object" }, riskLevel: "low", mutating: false, permission: { reason: "read" }, async execute() { throw "non-error tool failure"; } });
    const throwingLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "throw-read", name: "read_file", input: {} }] }]), registry: throwingRegistry, permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    expect((await throwingLoop.run({ input: "throw" })).error).toBe("Unknown agent loop error");

    const blockingRegistry = new ToolRegistry();
    blockingRegistry.register({ name: "edit_file", description: "blocks", inputSchema: { type: "object" }, outputSchema: { type: "object" }, riskLevel: "medium", mutating: true, permission: { reason: "edit" }, async execute() { throw new Error("stale_write_gate: file changed"); } });
    const blockingLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "block-edit", name: "edit_file", input: {} }] }]), registry: blockingRegistry, permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const blocked = await blockingLoop.run({ input: "block" });
    expect(blocked.status).toBe("blocked");
    expect(blocked.pendingInput).toMatchObject({ kind: "question" });

    const unknownToolLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "unknown-tool", name: "missing_tool", input: {} }] }, final("unknown tool recovered")]), registry: new ToolRegistry(), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const recovered = await unknownToolLoop.run({ input: "unknown" });
    expect(recovered.status).toBe("completed");
    expect(recovered.evidence[0]?.inputSummary).toBe("{}");

    const gitDiffLoop = createLoop(dir, new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "git-diff-fail", name: "git_diff", input: {} }] }, final("git diff failure recorded")])).loop;
    const gitDiffResult = await gitDiffLoop.run({ input: "git diff fail" });
    expect(gitDiffResult.status).toBe("completed");
    expect(gitDiffResult.runState?.contextSnapshot.decisionRefs.some((ref) => ref.startsWith("git_diff:failed:"))).toBe(true);
  });

  it("blocks before model execution when preserved context exceeds budget", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-context-budget-"));
    const { loop } = createLoop(dir, new FakeModelClient(happyScript("unused")), { context: { maxPromptBytes: 10, maxToolResultBytes: 5 } });
    const result = await loop.run({ input: "budget" });
    expect(result.status).toBe("blocked");
    expect(result.error).toContain("context_budget_gate");
  });

  it("blocks when runtime context sources fail before prompt execution", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-context-source-fail-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, agents: { agentsFile: join(dir, "AGENTS.md") } });
    const loop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient(happyScript("unused")), registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "load context" });
    expect(result.status).toBe("blocked");
    expect(result.pendingInput).toMatchObject({ kind: "question" });
  });

  it("fails recovery with an explicit failed handoff when persisted run state is incomplete", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-recovery-fallback-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const sessions = new InMemorySessionStore();
    const run = createTaskRunState("corrupt", "recover", "run_corrupt");
    setRunStatus(run, "failed", { type: "failed", failedTurnId: "missing" });
    await sessions.save({ id: "corrupt", status: "failed", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: run });
    const events = new InMemoryEventLog();
    const loop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([]), registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions, events, evidence: new InMemoryEvidenceStore() });
    const result = await loop.resume({ sessionId: "corrupt", input: { kind: "question", questionId: "q", answerText: "retry" } });
    expect(result.status).toBe("failed");
    expect(result.handoff).toMatchObject({ status: "failed", summary: "recovery_gate: failed run is missing failed handoff" });
    expect((await events.read("corrupt")).map((event) => event.type)).toContain("recovery.failed");

    const complete = createTaskRunState("done", "complete", "run_done");
    complete.handoff = buildFailedHandoff(complete, "already complete fallback");
    complete.handoff.status = "completed";
    complete.handoff.summary = "done";
    setRunStatus(complete, "completed", { type: "completed", handoffId: complete.handoff.id });
    await sessions.save({ id: "done", status: "completed", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: complete });
    const completed = await loop.resume({ sessionId: "done", input: { kind: "question", questionId: "q", answerText: "retry" } });
    expect(completed.status).toBe("completed");
    expect(completed.finalResponse).toBe("done");

    const missingHandoff = createTaskRunState("done-missing", "complete", "run_done_missing");
    setRunStatus(missingHandoff, "completed", { type: "completed", handoffId: "missing" });
    await sessions.save({ id: "done-missing", status: "completed", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: missingHandoff });
    const failedCompleted = await loop.resume({ sessionId: "done-missing", input: { kind: "question", questionId: "q", answerText: "retry" } });
    expect(failedCompleted.status).toBe("failed");

    const waiting = createTaskRunState("waiting-corrupt", "permission", "run_waiting_corrupt");
    setRunStatus(waiting, "waiting_permission", { type: "permission", permissionId: "p" });
    await sessions.save({ id: "waiting-corrupt", status: "waiting_permission", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: waiting });
    expect((await loop.resume({ sessionId: "waiting-corrupt", input: { kind: "permission", permissionId: "p", decision: "approve" } })).status).toBe("failed");
  });

  it("injects AGENTS.md snapshots into context without untracked prompt text", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-agents-"));
    await mkdir(join(dir, ".git"));
    await mkdir(join(dir, ".lattecode", "skills", "safe"), { recursive: true });
    await writeFile(join(dir, "AGENTS.md"), "# Agent Rules\n\n- Preserve permission gates.\n", "utf8");
    await writeFile(join(dir, ".lattecode", "skills", "safe", "skill.json"), JSON.stringify({ name: "safe", instructions: "Use safe skill instructions." }), "utf8");
    const model = new FakeModelClient(happyScript("agents loaded"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, skills: { enabled: ["safe"] } });
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "respect agents" });
    expect(result.status).toBe("completed");
    expect(result.runState?.contextSnapshot.agentsMd?.summary).toContain("Preserve permission gates");
    expect(result.runState?.contextSnapshot.skills?.[0]?.summary).toContain("safe skill");
    expect(model.requests[0]?.messages.map((message) => message.content).join("\n")).toContain("agentsMd");
    expect(model.requests[0]?.messages.map((message) => message.content).join("\n")).toContain("hash");
  });

  it("routes local commands into direct code-agent context", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-command-"));
    await mkdir(join(dir, ".lattecode", "commands"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "commands", "fix.json"), JSON.stringify({
      name: "fix",
      description: "Fix a target",
      context: { objective: "Fix {{args}}", scope: ["workspace"], acceptance: ["done"], nonGoals: ["no direct tools"], constraints: ["route through loop"], blockers: [] }
    }), "utf8");
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, commands: { enabled: ["run", "resume", "show", "list", "fix"] } });
    const model = new FakeModelClient([final("command routed")]);
    const events = new InMemoryEventLog();
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events, evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "/fix bug" });
    expect(result.status).toBe("completed");
    expect(result.runState?.agentContext?.objective).toBe("Fix bug");
    expect(model.requests[0]?.messages[0]?.content).toContain("direct ReAct code agent");
    expect(model.requests[0]?.messages.map((message) => message.content).join("\n")).toContain("Fix bug");
    expect((await events.read(result.session.id)).some((event) => event.type === "command.routed")).toBe(true);
  });

  it("routes enabled MCP tools through permission, evidence, trace, and session", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-mcp-"));
    const config = mergeConfig(DEFAULT_CONFIG, {
      session: { store: "memory" },
      evidence: { store: "memory" },
      permissions: { mutatingTools: "allow" },
      mcp: { enabled: true, servers: { local: { enabled: true, tools: { lookup: { description: "Lookup", mutating: true, riskLevel: "medium" } } } } }
    });
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "mcp-1", name: "mcp_local_lookup", input: { query: "x" } }] },
      final("mcp routed")
    ]);
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config, { async callTool() { return { summary: "mcp result", output: { value: "x" } }; } }), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "use mcp" });
    expect(result.status).toBe("completed");
    expect(result.evidence.map((entry) => entry.toolName)).toContain("mcp_local_lookup");
    expect(result.runState?.turns.flatMap((turn) => turn.toolCallIds)).toContain("mcp-1");
  });

  it("fails safely when the model provider throws", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-fail-"));
    const model = new FakeModelClient([new Error("provider down")]);
    const { loop, sessions, events } = createLoop(dir, model);
    const result = await loop.run({ input: "fail" });
    expect(result.status).toBe("failed");
    expect(result.error).toBe("provider down");
    expect(result.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "provider down", summary: "provider down" });
    expect((await sessions.get(result.session.id))?.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "provider down" });
    expect(recoverSessionFromEvents(result.session.id, await events.read(result.session.id)).runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "provider down" });
    expect(model.calls).toBe(1);
  });

  it("normalizes non-Error provider failures", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-string-fail-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const model = { async generate() { throw "string failure"; } };
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const result = await loop.run({ input: "fail" });
    expect(result.error).toBe("Unknown agent loop error");
    expect(result.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "Unknown agent loop error" });
  });

  it("uses default fake model response when script is exhausted", async () => {
    const model = new FakeModelClient([]);
    await expect(model.generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "message", content: "No scripted response." });
    expect(model.calls).toBe(1);
  });

  it("fails safely when maxTurns is exceeded", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-max-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [] }, { type: "tool_calls", toolCalls: [] }]);
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const sessions = new InMemorySessionStore();
    const events = new InMemoryEventLog();
    const loop = new AgentLoop({ cwd: dir, config, model, registry: new ToolRegistry(), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions, events, evidence: new InMemoryEvidenceStore(), maxTurns: 1 });
    const result = await loop.run({ input: "loop" });
    expect(result.error).toContain("Direct ReAct loop exceeded maxTurns");
    expect(result.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "Direct ReAct loop exceeded maxTurns (1)", summary: "Direct ReAct loop exceeded maxTurns (1)" });
    expect((await sessions.get(result.session.id))?.runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "Direct ReAct loop exceeded maxTurns (1)" });
    expect(recoverSessionFromEvents(result.session.id, await events.read(result.session.id)).runState?.turns.at(-1)).toMatchObject({ status: "failed", error: "Direct ReAct loop exceeded maxTurns (1)" });
  });
});
