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
import { AgentNodeExecutor, concernFor, summarizeResult } from "../../src/graph-ready/node-executor.js";
import { FakeModelClient } from "../../src/model/fake.js";
import { PermissionPolicy } from "../../src/permissions/policy.js";
import { InMemorySessionStore, recoverSessionFromEvents } from "../../src/session/session.js";
import { createDefaultRegistry } from "../../src/runtime/create-agent.js";
import { buildFailedHandoff, createTaskRunState, setRunStatus } from "../../src/core/run-state.js";
import { loadRuntimeContextSources } from "../../src/runtime/context-sources.js";
import { ToolRegistry } from "../../src/tools/registry.js";
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

function artifact(value: unknown): ModelTurn {
  return { type: "message", content: JSON.stringify(value) };
}

function task(objective = "test task") {
  return artifact({ objective, scope: ["workspace"], acceptance: ["complete"], nonGoals: [], constraints: [], blockers: [] });
}

function context(overrides: Record<string, unknown> = {}) {
  return artifact({ summary: "context ready", filesRead: [], relevantSnippets: [], commandSources: [], openQuestions: [], ...overrides });
}

function plan(overrides: Record<string, unknown> = {}) {
  return artifact({ summary: "plan ready", targetFiles: [], steps: ["change"], verificationCommands: [], risks: [], ...overrides });
}

function patch(overrides: Record<string, unknown> = {}) {
  return artifact({ changedFiles: [], diffRefs: [], rationale: "no changes", evidenceRefs: [], ...overrides });
}

function verification() {
  return artifact([{ command: "not run", status: "skipped", summary: "not required", evidenceRefs: [] }]);
}

function handoff(summary = "done", overrides: Record<string, unknown> = {}) {
  return artifact({ id: "handoff_test", status: "completed", summary, changedFiles: [], verification: [{ command: "not run", status: "skipped", summary: "not required", evidenceRefs: [] }], risks: [], blockers: [], requiredDecisions: [], traceRefs: [], evidenceRefs: [], ...overrides });
}

function happyScript(summary = "done"): ModelTurn[] {
  return [task(), context(), plan(), patch(), verification(), handoff(summary)];
}

describe("AgentLoop integration", () => {
  it("runs fake-model tool call through permission, execution, evidence and final response", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-"));
    await writeFile(join(dir, "note.txt"), "hello agent", "utf8");
    const model = new FakeModelClient([
      task("read note"),
      { type: "tool_calls", toolCalls: [{ id: "c1", name: "read_file", input: { path: "note.txt" } }] },
      context({ filesRead: ["note.txt"], relevantSnippets: [{ path: "note.txt", summary: "hello agent", evidenceRefs: [] }] }),
      plan(),
      patch(),
      verification(),
      handoff("File says hello agent.")
    ]);
    const { loop, events } = createLoop(dir, model);
    const result = await loop.run({ input: "read note" });
    expect(result.status).toBe("completed");
    expect(result.finalResponse).toContain("hello agent");
    expect(result.evidence).toHaveLength(1);
    expect((await events.read(result.session.id)).map((event) => event.type)).toContain("permission.decided");
    expect(recoverSessionFromEvents(result.session.id, await events.read(result.session.id)).status).toBe("completed");
  });

  it("accepts fenced JSON phase artifacts from model responses", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-fenced-"));
    const fencedTask: ModelTurn = { type: "message", content: `\`\`\`json\n${JSON.stringify({ objective: "fenced", scope: [], acceptance: [], nonGoals: [], constraints: [], blockers: [] })}\n\`\`\`` };
    const { loop } = createLoop(dir, new FakeModelClient([fencedTask, context(), plan(), patch(), verification(), handoff("fenced done")]));
    await expect(loop.run({ input: "fenced" })).resolves.toMatchObject({ status: "completed", finalResponse: "fenced done" });
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
    const model = new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "c2", name: "write_file", input: { path: "out.txt", content: "unsafe" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "write" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingInput).toMatchObject({ kind: "permission", action: "write_file", phase: "edit", toolCallId: "c2" });
    expect(result.pendingPermission?.toolName).toBe("write_file");
  });

  it("denies dangerous shell commands and does not execute them", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-deny-"));
    const model = new FakeModelClient([task("delete"), context(), plan(), patch(), { type: "tool_calls", toolCalls: [{ id: "c3", name: "shell_exec", input: { command: "rm -rf important" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "delete" });
    expect(result.status).toBe("blocked");
    expect(result.error).toContain("delete");
    expect(result.evidence[0]?.permission.action).toBe("deny");
  });

  it("asks for shell commands matching requireApprovalFor even when generic mutating shell is allowed", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-shell-ask-"));
    const model = new FakeModelClient([task("network"), context(), plan(), patch(), { type: "tool_calls", toolCalls: [{ id: "c3b", name: "shell_exec", input: { command: "curl https://example.com" } }] }]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" }, tools: { shell: { requireApprovalFor: ["network"] } } });
    const result = await loop.run({ input: "network" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingPermission?.reason).toContain("network");
  });

  it("records invalid tool parameters without executing the tool", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-invalid-"));
    const model = new FakeModelClient([
      task("bad"),
      { type: "tool_calls", toolCalls: [{ id: "c4", name: "read_file", input: { missing: "path" } }] },
      context(),
      plan(),
      patch(),
      verification(),
      handoff("Recovered from invalid params.")
    ]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "bad" });
    expect(result.status).toBe("completed");
    expect(result.evidence[0]?.outputSummary).toContain("Schema validation failed");
  });

  it("executes Step 4 edit_file and requires git_diff before changed-file handoff", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-step4-edit-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      task("edit file"),
      { type: "tool_calls", toolCalls: [{ id: "read-target", name: "read_file", input: { path: "target.txt" } }] },
      context({ filesRead: ["target.txt"] }),
      plan(),
      { type: "tool_calls", toolCalls: [{ id: "edit-target", name: "edit_file", input: { path: "target.txt", mode: "replace", oldText: "before", newText: "after" } }] },
      patch({ changedFiles: ["target.txt"] }),
      verification(),
      { type: "tool_calls", toolCalls: [{ id: "diff-target", name: "git_diff", input: {} }] },
      handoff("edit complete", { changedFiles: ["target.txt"] })
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "edit" });
    expect(result.status).toBe("completed");
    expect(await readFile(join(dir, "target.txt"), "utf8")).toContain("after");
    expect(result.evidence.map((entry) => entry.toolName)).toEqual(expect.arrayContaining(["read_file", "edit_file", "git_diff"]));
  });

  it("finalizes release handoff with canonical verification, trace, evidence, and changed files", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-release-handoff-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      task("release path edit"),
      { type: "tool_calls", toolCalls: [{ id: "read-release", name: "read_file", input: { path: "target.txt" } }] },
      context({ filesRead: ["target.txt"] }),
      plan({ targetFiles: ["target.txt"], verificationCommands: ["npm test"], risks: ["local fixture risk"] }),
      { type: "tool_calls", toolCalls: [{ id: "edit-release", name: "edit_file", input: { path: "target.txt", mode: "replace", oldText: "before", newText: "after" } }] },
      patch({ changedFiles: ["target.txt"] }),
      artifact([{ command: "npm test", status: "passed", exitCode: 0, summary: "fixture tests passed", evidenceRefs: ["ev-verification"], outputRefs: ["ev-verification"] }]),
      { type: "tool_calls", toolCalls: [{ id: "diff-release", name: "git_diff", input: {} }] },
      handoff("release handoff", { changedFiles: [], verification: [], risks: [], traceRefs: [], evidenceRefs: [] })
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "release" });
    expect(result.status).toBe("completed");
    expect(result.handoff).toMatchObject({ status: "completed", changedFiles: ["target.txt"], risks: ["local fixture risk"] });
    expect(result.handoff?.verification).toEqual(expect.arrayContaining([expect.objectContaining({ command: "npm test", status: "passed" })]));
    expect(result.handoff?.traceRefs).toEqual(result.runState?.steps.map((step) => step.id));
    expect(result.handoff?.evidenceRefs.length).toBeGreaterThan(0);
    expect(result.handoff?.evidenceRefs).toContain("ev-verification");
  });

  it("blocks write_file through read_before_write_gate when overwrite intent is unsafe", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-read-before-write-"));
    await writeFile(join(dir, "target.txt"), "before\n", "utf8");
    const model = new FakeModelClient([
      task("unsafe overwrite"),
      context(),
      plan(),
      { type: "tool_calls", toolCalls: [{ id: "overwrite-target", name: "write_file", input: { path: "target.txt", content: "after" } }] }
    ]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" } });
    const result = await loop.run({ input: "overwrite" });
    expect(result.status).toBe("blocked");
    expect(result.pendingInput).toMatchObject({ kind: "question", phase: "edit" });
    expect(result.error).toContain("read_before_write_gate");
    expect(await readFile(join(dir, "target.txt"), "utf8")).toBe("before\n");
  });

  it("repairs an invalid phase artifact before advancing", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-repair-"));
    const model = new FakeModelClient([
      { type: "message", content: "not json" },
      task("repaired"),
      context(),
      plan(),
      patch(),
      verification(),
      handoff("repair complete")
    ]);
    const { loop, events } = createLoop(dir, model);
    const result = await loop.run({ input: "repair" });
    expect(result.status).toBe("completed");
    expect(result.runState?.steps[0]?.reactBudget.usedSteps).toBe(2);
    expect((await events.read(result.session.id)).some((event) => event.type === "phase.blocked")).toBe(true);
  });

  it("resumes a waiting permission with the same permission id and binds tool evidence to the run", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-permission-"));
    await execFileAsync("git", ["init"], { cwd: dir });
    const model = new FakeModelClient([
      task("write and resume"),
      context(),
      plan(),
      { type: "tool_calls", toolCalls: [{ id: "write-1", name: "write_file", input: { path: "approved.txt", content: "ok", createIntent: true } }] },
      patch({ changedFiles: ["approved.txt"], evidenceRefs: ["placeholder"] }),
      verification(),
      { type: "tool_calls", toolCalls: [{ id: "diff-approved", name: "git_diff", input: {} }] },
      handoff("permission resumed", { changedFiles: ["approved.txt"] })
    ]);
    const { loop, events } = createLoop(dir, model);
    const first = await loop.run({ input: "write" });
    expect(first.status).toBe("waiting_permission");
    const permissionId = first.pendingInput?.kind === "permission" ? first.pendingInput.permissionId : "";
    const resumed = await loop.resume({ sessionId: first.session.id, input: { kind: "permission", permissionId, decision: "approve", reason: "approved in test" } });
    expect(resumed.status).toBe("completed");
    expect(resumed.runState?.patch?.changedFiles).toEqual(["approved.txt"]);
    expect(resumed.runState?.steps.flatMap((step) => step.evidenceIds).length).toBeGreaterThan(0);
    expect((await events.read(first.session.id)).some((event) => event.type === "resume.received")).toBe(true);
  });

  it("resumes a blocked question and continues the current phase", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-question-"));
    const model = new FakeModelClient([
      task("needs context"),
      context({ openQuestions: ["Which file?"] }),
      context({ filesRead: ["answer.ts"] }),
      plan(),
      patch(),
      verification(),
      handoff("question resumed")
    ]);
    const { loop } = createLoop(dir, model);
    const blocked = await loop.run({ input: "needs context" });
    expect(blocked.status).toBe("blocked");
    expect(blocked.pendingInput).toMatchObject({ kind: "question", phase: "understand" });
    const questionId = blocked.pendingInput?.kind === "question" ? blocked.pendingInput.questionId : "";
    const resumed = await loop.resume({ sessionId: blocked.session.id, input: { kind: "question", questionId, answerText: "Use answer.ts" } });
    expect(resumed.status).toBe("completed");
    expect(resumed.runState?.context?.filesRead).toEqual(["answer.ts"]);
  });

  it("maps mismatched resume inputs and denied permission resume to blocked handoff contracts", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-negative-"));
    const questionLoop = createLoop(dir, new FakeModelClient([task("needs context"), context({ openQuestions: ["Which file?"] })])).loop;
    const blocked = await questionLoop.run({ input: "needs context" });
    const mismatch = await questionLoop.resume({ sessionId: blocked.session.id, input: { kind: "permission", permissionId: "wrong", decision: "approve" } });
    expect(mismatch.status).toBe("blocked");
    expect(mismatch.handoff?.blockers[0]).toContain("does not match pending");

    const denyLoop = createLoop(dir, new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "write-deny", name: "write_file", input: { path: "denied.txt", content: "no", createIntent: true } }] }])).loop;
    const waiting = await denyLoop.run({ input: "write" });
    const wrongId = await denyLoop.resume({ sessionId: waiting.session.id, input: { kind: "permission", permissionId: "wrong", decision: "approve" } });
    expect(wrongId.status).toBe("blocked");
    const denyOnlyLoop = createLoop(dir, new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "write-deny-2", name: "write_file", input: { path: "denied.txt", content: "no", createIntent: true } }] }])).loop;
    const waitingToDeny = await denyOnlyLoop.run({ input: "write" });
    const permissionId = waitingToDeny.pendingInput?.kind === "permission" ? waitingToDeny.pendingInput.permissionId : "";
    const denied = await denyOnlyLoop.resume({ sessionId: waitingToDeny.session.id, input: { kind: "permission", permissionId, decision: "deny", reason: "not approved" } });
    expect(denied.status).toBe("blocked");
    expect(denied.handoff?.summary).toBe("not approved");
    expect(denied.handoff?.requiredDecisions).toEqual([{ kind: "permission", id: permissionId, reason: "Mutating tool follows mutatingTools policy" }]);

    const denyDefaultLoop = createLoop(dir, new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "write-deny-default", name: "write_file", input: { path: "denied-default.txt", content: "no", createIntent: true } }] }])).loop;
    const waitingDefault = await denyDefaultLoop.run({ input: "write" });
    const defaultPermissionId = waitingDefault.pendingInput?.kind === "permission" ? waitingDefault.pendingInput.permissionId : "";
    expect((await denyDefaultLoop.resume({ sessionId: waitingDefault.session.id, input: { kind: "permission", permissionId: defaultPermissionId, decision: "deny" } })).error).toBe("Permission denied by resume input.");

    const questionWrongIdLoop = createLoop(dir, new FakeModelClient([task("needs context"), context({ openQuestions: ["Which file?"] })])).loop;
    const questionBlocked = await questionWrongIdLoop.run({ input: "needs context" });
    expect((await questionWrongIdLoop.resume({ sessionId: questionBlocked.session.id, input: { kind: "question", questionId: "wrong", answerText: "answer" } })).error).toContain("Question resume id");
  });

  it("covers resume edge states and recovery repairs without changing run semantics", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-resume-edges-"));
    const { loop, sessions } = createLoop(dir, new FakeModelClient([...happyScript("after repaired question"), patch(), verification(), handoff("after pending call"), patch(), verification(), handoff("after missing pending call")]), { permissions: { mutatingTools: "allow" } });
    const noPending = await loop.resume({ sessionId: "new-session", input: { kind: "question", questionId: "q", answerText: "answer" } });
    expect(noPending.status).toBe("blocked");
    expect(noPending.error).toContain("No resumable");

    const question = createQuestionPendingInput({ questionId: "q-repair", phase: "plan", prompt: "Need answer", expectedAnswer: "json" });
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
    waitingRun.currentPhase = "edit";
    waitingRun.pendingInput = permission;
    await sessions.save({ id: "pending-call-session", status: "waiting_permission", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: waitingRun, pendingInput: permission, pendingPermission: { callId: call.id, toolName: call.name, reason: permission.reason, permissionId: permission.permissionId, phase: permission.phase, action: permission.action, path: "approved-edge.txt" }, pendingToolCall: call });
    const approved = await loop.resume({ sessionId: "pending-call-session", input: { kind: "permission", permissionId: permission.permissionId, decision: "approve" } });
    expect(approved.status).toBe("completed");
    expect(await readFile(join(dir, "approved-edge.txt"), "utf8")).toBe("ok");

    const noCallRun = createTaskRunState("pending-no-call-session", "pending", "run_pending_no_call");
    const noCallPermission = createPermissionPendingInput("pending-no-call-session", { id: "missing-call", name: "write_file", input: { path: "no-call.txt" } }, "approve without call");
    noCallRun.status = "blocked";
    noCallRun.currentPhase = "edit";
    noCallRun.pendingInput = noCallPermission;
    await sessions.save({ id: "pending-no-call-session", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 0, runState: noCallRun, pendingInput: noCallPermission });
    expect((await loop.resume({ sessionId: "pending-no-call-session", input: { kind: "permission", permissionId: noCallPermission.permissionId, decision: "approve" } })).status).toBe("completed");
  });

  it("covers release gate validation failures as blocked repair prompts", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-gate-edges-"));
    const missingVerification = createLoop(dir, new FakeModelClient([task("verify missing"), context(), plan({ verificationCommands: ["npm test"] }), patch(), artifact([])]), { runtime: { maxRepairTurns: 0 } }).loop;
    const missing = await missingVerification.run({ input: "missing verification" });
    expect(missing.status).toBe("blocked");
    expect(missing.error).toContain("verification_gate");

    const skippedWithoutReason = createLoop(dir, new FakeModelClient([task("skip reason"), context(), plan(), patch(), artifact([{ command: "npm test", status: "skipped", summary: " ", evidenceRefs: [] }])]), { runtime: { maxRepairTurns: 0 } }).loop;
    expect((await skippedWithoutReason.run({ input: "skip" })).error).toContain("skipped verification");

    const completedWithFailedVerification = createLoop(dir, new FakeModelClient([task("failed handoff"), context(), plan(), patch(), artifact([{ command: "npm test", status: "failed", summary: "failed", evidenceRefs: [] }]), handoff("bad complete", { verification: [{ command: "npm test", status: "failed", summary: "failed", evidenceRefs: [] }] })]), { runtime: { maxRepairTurns: 0 } }).loop;
    expect((await completedWithFailedVerification.run({ input: "failed handoff" })).error).toContain("Verification failed");

    const completedWithBlocker = createLoop(dir, new FakeModelClient([task("blocker handoff"), context(), plan(), patch(), verification(), handoff("blocked complete", { blockers: ["still blocked"] })]), { runtime: { maxRepairTurns: 0 } }).loop;
    expect((await completedWithBlocker.run({ input: "blocker handoff" })).error).toContain("blockers");

    const failedHandoff = createLoop(dir, new FakeModelClient([task("failed handoff artifact"), context(), plan(), patch(), verification(), handoff("handoff failed", { status: "failed", summary: "handoff failed", risks: ["risk"] })])).loop;
    expect((await failedHandoff.run({ input: "failed handoff" })).status).toBe("failed");

    const blockedHandoff = createLoop(dir, new FakeModelClient([task("blocked handoff artifact"), context(), plan(), patch(), verification(), handoff("handoff blocked", { status: "blocked", summary: "handoff blocked", requiredDecisions: [{ kind: "question", id: "handoff-q", reason: "Need answer" }] })])).loop;
    const blocked = await blockedHandoff.run({ input: "blocked handoff" });
    expect(blocked.status).toBe("blocked");
    expect(blocked.pendingInput).toMatchObject({ kind: "question", questionId: "handoff-q" });

    const diffGate = createLoop(dir, new FakeModelClient([task("diff gate"), context(), plan(), patch({ changedFiles: ["a.ts"] }), verification(), handoff("missing diff")]), { runtime: { maxRepairTurns: 0 } }).loop;
    expect((await diffGate.run({ input: "diff gate" })).error).toContain("diff_review_gate");
  });

  it("covers tool execution non-error and blocking gate normalization", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-tool-errors-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, permissions: { mutatingTools: "allow" }, runtime: { maxRepairTurns: 0 } });
    const throwingRegistry = new ToolRegistry();
    throwingRegistry.register({ name: "read_file", description: "throws", inputSchema: { type: "object" }, outputSchema: { type: "object" }, riskLevel: "low", mutating: false, permission: { reason: "read" }, async execute() { throw "non-error tool failure"; } });
    const throwingLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([task("throw"), { type: "tool_calls", toolCalls: [{ id: "throw-read", name: "read_file", input: {} }] }]), registry: throwingRegistry, permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    expect((await throwingLoop.run({ input: "throw" })).error).toBe("Unknown agent loop error");

    const blockingRegistry = new ToolRegistry();
    blockingRegistry.register({ name: "edit_file", description: "blocks", inputSchema: { type: "object" }, outputSchema: { type: "object" }, riskLevel: "medium", mutating: true, permission: { reason: "edit" }, async execute() { throw new Error("stale_write_gate: file changed"); } });
    const blockingLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([task("block"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "block-edit", name: "edit_file", input: {} }] }]), registry: blockingRegistry, permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const blocked = await blockingLoop.run({ input: "block" });
    expect(blocked.status).toBe("blocked");
    expect(blocked.pendingInput).toMatchObject({ kind: "question", phase: "edit" });

    const unknownToolLoop = new AgentLoop({ cwd: dir, config, model: new FakeModelClient([task("unknown"), { type: "tool_calls", toolCalls: [{ id: "unknown-tool", name: "missing_tool", input: {} }] }, context(), plan(), patch(), verification(), handoff("unknown tool recovered")]), registry: new ToolRegistry(), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    const recovered = await unknownToolLoop.run({ input: "unknown" });
    expect(recovered.status).toBe("completed");
    expect(recovered.evidence[0]?.inputSummary).toBe("{}");

    const gitDiffLoop = createLoop(dir, new FakeModelClient([task("git diff fail"), context(), plan(), patch(), verification(), { type: "tool_calls", toolCalls: [{ id: "git-diff-fail", name: "git_diff", input: {} }] }, handoff("git diff failure recorded")])).loop;
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
    expect(result.pendingInput).toMatchObject({ kind: "question", phase: "understand" });
  });

  it("fails recovery with an explicit failed handoff when persisted run state is incomplete", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-recovery-fallback-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const sessions = new InMemorySessionStore();
    const run = createTaskRunState("corrupt", "recover", "run_corrupt");
    setRunStatus(run, "failed", { type: "failed", failedStepId: "missing" });
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

  it("routes local commands to TaskSpec and continues through the phase runner", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-command-"));
    await mkdir(join(dir, ".lattecode", "commands"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "commands", "fix.json"), JSON.stringify({
      name: "fix",
      description: "Fix a target",
      task: { objective: "Fix {{args}}", scope: ["workspace"], acceptance: ["done"], nonGoals: ["no direct tools"], constraints: ["route through loop"], blockers: [] }
    }), "utf8");
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, commands: { enabled: ["run", "resume", "show", "list", "fix"] } });
    const model = new FakeModelClient([context(), plan(), patch(), verification(), handoff("command routed")]);
    const events = new InMemoryEventLog();
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events, evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "/fix bug" });
    expect(result.status).toBe("completed");
    expect(result.runState?.task?.objective).toBe("Fix bug");
    expect(model.requests[0]?.messages[0]?.content).toContain("Current phase: understand");
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
      task("mcp lookup"),
      { type: "tool_calls", toolCalls: [{ id: "mcp-1", name: "mcp_local_lookup", input: { query: "x" } }] },
      context(),
      plan(),
      patch(),
      verification(),
      handoff("mcp routed")
    ]);
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config, { async callTool() { return { summary: "mcp result", output: { value: "x" } }; } }), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), loadContextSources: () => loadRuntimeContextSources(dir, config) });
    const result = await loop.run({ input: "use mcp" });
    expect(result.status).toBe("completed");
    expect(result.evidence.map((entry) => entry.toolName)).toContain("mcp_local_lookup");
    expect(result.runState?.steps.flatMap((step) => step.toolCallIds)).toContain("mcp-1");
  });

  it("wraps agent loop as a graph-ready NodeExecutor", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-node-"));
    const model = new FakeModelClient(happyScript("node done"));
    const { loop } = createLoop(dir, model);
    const executor = new AgentNodeExecutor(loop);
    const result = await executor.execute({ input: "go", contract: { nodeId: "N2.implement-agent", goal: "implement", allowedTools: ["read_file"], acceptance: ["done"] } });
    expect(result.nodeId).toBe("N2.implement-agent");
    expect(result.status).toBe("completed");
    expect(result.graphUpdate.eventCursor).toBeGreaterThan(0);
  });

  it("NodeExecutor reports pending permission as concern", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-node-wait-"));
    const model = new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "c5", name: "write_file", input: { path: "x", content: "y" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await new AgentNodeExecutor(loop).execute({ input: "write", contract: { nodeId: "N", goal: "g", allowedTools: ["write_file"], acceptance: [] } });
    expect(result.status).toBe("waiting_permission");
    expect(result.concerns[0]).toContain("Mutating");
  });

  it("NodeExecutor enforces node contract allowedTools", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-node-tools-"));
    const model = new FakeModelClient([task("write"), context(), plan(), { type: "tool_calls", toolCalls: [{ id: "c6", name: "write_file", input: { path: "x", content: "y" } }] }]);
    const { loop, events } = createLoop(dir, model);
    const result = await new AgentNodeExecutor(loop).execute({ input: "write", contract: { nodeId: "N", goal: "g", allowedTools: ["read_file"], acceptance: [] } });
    expect(result.status).toBe("blocked");
    expect(result.summary).toContain("not allowed by node contract");
    const editPhaseToolList = model.requests.find((request) => request.messages[0]?.content.includes("Current phase: edit"))?.tools.map((tool) => tool.name);
    expect(editPhaseToolList).toEqual(["read_file"]);
    expect((await events.read()).some((event) => event.type === "evidence.recorded")).toBe(true);
  });

  it("NodeExecutor reports failed loop errors as concerns", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-node-fail-"));
    const { loop } = createLoop(dir, new FakeModelClient([new Error("provider down")]));
    const result = await new AgentNodeExecutor(loop).execute({ input: "fail", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    expect(result.status).toBe("failed");
    expect(result.summary).toBe("provider down");
    expect(result.concerns).toEqual(["provider down"]);
    const fallbackResult = { status: "denied" as const, session: { id: "s", status: "denied" as const, transcript: [], evidenceIds: [], lastEventSeq: 0 }, evidence: [] };
    expect(summarizeResult(fallbackResult)).toBe("Agent loop ended without final response.");
    expect(concernFor(fallbackResult)).toBe("Non-completed agent result");
  });

  it("NodeExecutor forwards an optional session id", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-node-session-"));
    const model = new FakeModelClient([...happyScript("first"), ...happyScript("second")]);
    const { loop } = createLoop(dir, model);
    const executor = new AgentNodeExecutor(loop);
    await executor.execute({ input: "one", sessionId: "node-session", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    const result = await executor.execute({ input: "two", sessionId: "node-session", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    expect(result.summary).toBe("second");
  });

  it("fails safely when the model provider throws", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-fail-"));
    const model = new FakeModelClient([new Error("provider down")]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "fail" });
    expect(result.status).toBe("failed");
    expect(result.error).toBe("provider down");
    expect(model.calls).toBe(1);
  });

  it("normalizes non-Error provider failures", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-loop-string-fail-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const model = { async generate() { throw "string failure"; } };
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    expect((await loop.run({ input: "fail" })).error).toBe("Unknown agent loop error");
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
    const loop = new AgentLoop({ cwd: dir, config, model, registry: new ToolRegistry(), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), maxTurns: 1 });
    expect((await loop.run({ input: "loop" })).error).toContain("TaskSpec artifact failed validation");
  });
});
