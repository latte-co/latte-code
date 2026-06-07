import { describe, expect, it } from "vitest";
import {
  createHeadlessRunEnvelopeFromAgentResult,
  createPermissionPendingInput,
  createQuestionPendingInput,
  exitCodeForTaskRunStatus,
  isAgentHandoff,
  isChangePlan,
  isContextPack,
  isHeadlessRunEnvelope,
  isPendingInput,
  isPatchSummary,
  isResumeInput,
  isStepTrace,
  isTaskRunState,
  isTaskRunStatus,
  isTaskSpec,
  isVerificationResult,
  mapLegacyAgentStatus,
  V0_1_CONTRACT_AUTHORITY
} from "../../src/core/contracts.js";
import type { AgentResult } from "../../src/core/agent-loop.js";
import { createStepTrace, createTaskRunState } from "../../src/core/run-state.js";

describe("v0.1 contract foundations", () => {
  it("records formal docs as contract authority and graph-ready as compatibility-only", () => {
    expect(V0_1_CONTRACT_AUTHORITY.releaseCriticalPath).toBe("headless-agent-handoff");
    expect(V0_1_CONTRACT_AUTHORITY.graphReadyRole).toBe("compatibility-wrapper");
    expect(V0_1_CONTRACT_AUTHORITY.legacyStatusBehavior).toBe("compatibility-mapping-only");
    expect(V0_1_CONTRACT_AUTHORITY.formalSources).toContain("docs/zh-CN/milestones/targets/v0.1-implementation-plan-review.md");
    expect(V0_1_CONTRACT_AUTHORITY.executionMirror).toContain(".oh-my-code/plans/");
  });

  it("accepts only frozen TaskRunState statuses", () => {
    expect(isTaskRunStatus("queued")).toBe(true);
    expect(isTaskRunStatus("running")).toBe(true);
    expect(isTaskRunStatus("waiting_permission")).toBe(true);
    expect(isTaskRunStatus("blocked")).toBe(true);
    expect(isTaskRunStatus("failed")).toBe(true);
    expect(isTaskRunStatus("completed")).toBe(true);
    expect(isTaskRunStatus("denied")).toBe(false);
    expect(mapLegacyAgentStatus("denied")).toBe("blocked");
    expect(mapLegacyAgentStatus("completed")).toBe("completed");
    expect(exitCodeForTaskRunStatus("completed")).toBe(0);
    expect(exitCodeForTaskRunStatus("waiting_permission")).toBe(20);
    expect(exitCodeForTaskRunStatus("blocked")).toBe(21);
    expect(exitCodeForTaskRunStatus("failed")).toBe(22);
    expect(exitCodeForTaskRunStatus("queued")).toBe(22);
    expect(exitCodeForTaskRunStatus("running")).toBe(22);
  });

  it("serializes canonical permission pending input with stable ids", () => {
    const pending = createPermissionPendingInput("session-1", { id: "tool-1", name: "write_file", input: { path: "src/a.ts" } }, "Mutating tool requires approval");
    expect(pending).toEqual({
      kind: "permission",
      permissionId: createPermissionPendingInput("session-1", { id: "tool-1", name: "write_file", input: { path: "src/a.ts" } }, "again").permissionId,
      toolCallId: "tool-1",
      phase: "edit",
      action: "write_file",
      reason: "Mutating tool requires approval",
      path: "src/a.ts",
      options: ["approve", "deny"]
    });
    expect(isPendingInput(pending)).toBe(true);
    expect(isPendingInput({ ...pending, options: ["deny", "approve"] })).toBe(false);
  });

  it("validates frozen ResumeInput variants", () => {
    expect(isResumeInput({ kind: "permission", permissionId: "permission_1", decision: "approve" })).toBe(true);
    expect(isResumeInput({ kind: "permission", permissionId: "permission_1", decision: "maybe" })).toBe(false);
    expect(isResumeInput({ kind: "question", questionId: "question_1", answerText: "continue" })).toBe(true);
    expect(isResumeInput({ kind: "question", questionId: "question_1", answerJson: { ok: true } })).toBe(true);
    expect(isResumeInput({ kind: "question", questionId: "question_1" })).toBe(false);
    expect(isPendingInput(createQuestionPendingInput({ questionId: "question_1", phase: "plan", prompt: "Pick a framework", expectedAnswer: "text" }))).toBe(true);
  });

  it("validates Step 3 phase artifact and run-state schemas", () => {
    const task = { objective: "implement", scope: ["src"], acceptance: ["tests pass"], nonGoals: [], constraints: ["no Step 4"], blockers: [] };
    const context = { summary: "read docs", filesRead: ["a.ts"], relevantSnippets: [{ path: "a.ts", summary: "snippet", evidenceRefs: ["ev1"] }], commandSources: ["package.json"], openQuestions: [] };
    const plan = { summary: "edit", targetFiles: ["a.ts"], steps: ["patch"], verificationCommands: ["npm test"], risks: [] };
    const patch = { changedFiles: ["a.ts"], diffRefs: ["diff:1"], rationale: "fix", evidenceRefs: ["ev2"] };
    const verification = { command: "npm test", status: "passed" as const, exitCode: 0, summary: "passed", evidenceRefs: ["ev3"], outputRefs: ["ev3"] };
    const handoff = { id: "handoff_1", status: "completed" as const, summary: "done", changedFiles: ["a.ts"], verification: [verification], risks: [], blockers: [], requiredDecisions: [], traceRefs: ["step_1"], evidenceRefs: ["ev1", "ev2", "ev3"] };
    const run = createTaskRunState("s1", "implement", "run_1");
    run.task = task;
    run.context = context;
    run.plan = plan;
    run.patch = patch;
    run.verification = [verification];
    run.handoff = handoff;
    run.steps.push(createStepTrace({ runId: run.id, phase: "intake", index: 0, maxSteps: 8 }));
    expect(isTaskSpec(task)).toBe(true);
    expect(isContextPack(context)).toBe(true);
    expect(isChangePlan(plan)).toBe(true);
    expect(isPatchSummary(patch)).toBe(true);
    expect(isVerificationResult(verification)).toBe(true);
    expect(isAgentHandoff(handoff)).toBe(true);
    expect(isStepTrace(run.steps[0])).toBe(true);
    expect(isTaskRunState(run)).toBe(true);
    expect(isTaskRunState({ ...run, steps: ["step_1"] })).toBe(false);
  });

  it("builds machine-readable headless run envelopes from legacy loop results", () => {
    const completed: AgentResult = {
      status: "completed",
      session: { id: "s1", status: "completed", transcript: [], evidenceIds: ["ev1"], lastEventSeq: 4, finalResponse: "done" },
      finalResponse: "done",
      evidence: []
    };
    const completedEnvelope = createHeadlessRunEnvelopeFromAgentResult(completed);
    expect(completedEnvelope).toMatchObject({ runId: "s1", sessionId: "s1", status: "completed", handoff: { status: "completed", summary: "done", evidenceRefs: ["ev1"] } });
    expect(isHeadlessRunEnvelope(completedEnvelope)).toBe(true);

    const pendingInput = createPermissionPendingInput("s2", { id: "c1", name: "shell_exec", input: { command: "npm test" } }, "Shell command requires approval");
    const pendingPermission = {
      callId: "c1",
      toolName: "shell_exec",
      reason: pendingInput.reason,
      permissionId: pendingInput.permissionId,
      phase: pendingInput.phase,
      action: pendingInput.action,
      ...(pendingInput.command === undefined ? {} : { command: pendingInput.command })
    };
    const waiting: AgentResult = {
      status: "waiting_permission",
      session: {
        id: "s2",
        status: "waiting_permission",
        transcript: [],
        evidenceIds: [],
        lastEventSeq: 2,
        pendingInput,
        pendingPermission
      },
      evidence: [],
      pendingInput,
      pendingPermission
    };
    const waitingEnvelope = createHeadlessRunEnvelopeFromAgentResult(waiting);
    expect(waitingEnvelope.status).toBe("waiting_permission");
    expect(waitingEnvelope.pendingInput).toEqual(pendingInput);
    expect(waitingEnvelope.handoff).toBeUndefined();
    expect(isHeadlessRunEnvelope(waitingEnvelope)).toBe(true);

    const denied: AgentResult = {
      status: "denied",
      session: { id: "s3", status: "denied", transcript: [], evidenceIds: [], lastEventSeq: 3 },
      evidence: [],
      error: "permission denied"
    };
    expect(createHeadlessRunEnvelopeFromAgentResult(denied)).toMatchObject({ status: "blocked", handoff: { status: "blocked", blockers: ["permission denied"] } });
  });
});
