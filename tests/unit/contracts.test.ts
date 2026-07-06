import { describe, expect, it } from "vitest";
import {
  createHeadlessRunEnvelopeFromAgentResult,
  createHeadlessRunEnvelopeFromTaskRunState,
  createPermissionPendingInput,
  createQuestionPendingInput,
  exitCodeForTaskRunStatus,
  isAgentTaskContext,
  isAgentHandoff,
  isHeadlessRunEnvelope,
  isPendingInput,
  isResumeInput,
  isTaskRunState,
  isTaskRunStatus,
  isTurnTrace,
  isVerificationResult
} from "../../src/core/contracts.js";
import type { AgentResult } from "../../src/core/agent-loop.js";
import { buildBlockedHandoff, createTaskRunState, createTurnTrace, setRunPendingInput, setRunStatus } from "../../src/core/run-state.js";

describe("v0.1 contract foundations", () => {
  it("accepts only frozen TaskRunState statuses", () => {
    expect(isTaskRunStatus("queued")).toBe(true);
    expect(isTaskRunStatus("running")).toBe(true);
    expect(isTaskRunStatus("waiting_permission")).toBe(true);
    expect(isTaskRunStatus("blocked")).toBe(true);
    expect(isTaskRunStatus("failed")).toBe(true);
    expect(isTaskRunStatus("completed")).toBe(true);
    expect(isTaskRunStatus("denied")).toBe(false);
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
    expect(isPendingInput(createQuestionPendingInput({ questionId: "question_1", prompt: "Pick a framework", expectedAnswer: "text" }))).toBe(true);
  });

  it("validates persisted task, turn, change, verification and run-state schemas", () => {
    const agentContext = { objective: "implement", scope: ["src"], acceptance: ["tests pass"], nonGoals: [], constraints: ["no extra direct-loop turn"], blockers: [] };
    const verification = { command: "npm test", status: "passed" as const, exitCode: 0, summary: "passed", evidenceRefs: ["ev3"], outputRefs: ["ev3"] };
    const run = createTaskRunState("s1", "implement", "run_1");
    const turn = createTurnTrace({ runId: run.id, index: 0, maxTurns: 8 });
    turn.evidenceIds.push("ev2");
    run.agentContext = agentContext;
    run.changedFiles = ["a.ts"];
    run.changeEvidenceRefs = ["ev2"];
    run.verification = [verification];
    const handoff = { id: "handoff_1", status: "completed" as const, summary: "done", changedFiles: ["a.ts"], verification: [verification], risks: [], blockers: [], requiredDecisions: [], traceRefs: [turn.id], evidenceRefs: ["ev2", "ev3"] };
    run.handoff = handoff;
    run.turns.push(turn);
    expect(isAgentTaskContext(agentContext)).toBe(true);
    expect(isVerificationResult(verification)).toBe(true);
    expect(isAgentHandoff(handoff)).toBe(true);
    expect(isTurnTrace(run.turns[0])).toBe(true);
    expect(isTaskRunState(run)).toBe(true);
    expect(isTaskRunState({ ...run, turns: ["turn_1"] })).toBe(false);
  });

  it("builds machine-readable headless run envelopes from current run state", () => {
    const completedRun = createTaskRunState("s1", "implement", "run_1");
    const completedHandoff = { id: "handoff_1", status: "completed" as const, summary: "done", changedFiles: ["a.ts"], verification: [], risks: [], blockers: [], requiredDecisions: [], traceRefs: [], evidenceRefs: ["ev1"] };
    completedRun.handoff = completedHandoff;
    setRunStatus(completedRun, "completed", { type: "completed", handoffId: completedHandoff.id });
    const completed: AgentResult = {
      status: "completed",
      session: { id: "s1", status: "completed", transcript: [], evidenceIds: ["ev1"], lastEventSeq: 4, finalResponse: "done", runState: completedRun },
      finalResponse: "done",
      runState: completedRun,
      handoff: completedHandoff,
      evidence: []
    };
    const completedEnvelope = createHeadlessRunEnvelopeFromAgentResult(completed);
    expect(completedEnvelope).toMatchObject({ runId: "run_1", sessionId: "s1", status: "completed", handoff: { status: "completed", summary: "done", evidenceRefs: ["ev1"] } });
    expect(isHeadlessRunEnvelope(completedEnvelope)).toBe(true);

    const pendingInput = createPermissionPendingInput("s2", { id: "c1", name: "shell_exec", input: { command: "npm test" } }, "Shell command requires approval");
    const waitingRun = createTaskRunState("s2", "verify", "run_2");
    setRunPendingInput(waitingRun, pendingInput);
    const pendingPermission = {
      callId: "c1",
      toolName: "shell_exec",
      reason: pendingInput.reason,
      permissionId: pendingInput.permissionId,
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
        runState: waitingRun,
        pendingInput,
        pendingPermission
      },
      evidence: [],
      runState: waitingRun,
      pendingInput,
      pendingPermission
    };
    const waitingEnvelope = createHeadlessRunEnvelopeFromAgentResult(waiting);
    expect(waitingEnvelope.status).toBe("waiting_permission");
    expect(waitingEnvelope.pendingInput).toEqual(pendingInput);
    expect(waitingEnvelope.handoff).toBeUndefined();
    expect(isHeadlessRunEnvelope(waitingEnvelope)).toBe(true);

    const blockedRun = createTaskRunState("s3", "blocked", "run_3");
    const blockedInput = createQuestionPendingInput({ questionId: "q3", prompt: "need context", expectedAnswer: "text" });
    setRunPendingInput(blockedRun, blockedInput);
    blockedRun.handoff = buildBlockedHandoff(blockedRun, blockedInput.prompt, blockedInput);
    const blocked: AgentResult = {
      status: "blocked",
      session: { id: "s3", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 3, runState: blockedRun, pendingInput: blockedInput },
      runState: blockedRun,
      handoff: blockedRun.handoff,
      evidence: [],
      pendingInput: blockedInput,
      error: "need context"
    };
    expect(createHeadlessRunEnvelopeFromTaskRunState(blockedRun)).toMatchObject({ status: "blocked", pendingInput: blockedInput, handoff: { status: "blocked", blockers: ["need context"] } });
    expect(createHeadlessRunEnvelopeFromAgentResult(blocked)).toMatchObject({ status: "blocked", pendingInput: blockedInput, handoff: { status: "blocked", blockers: ["need context"] } });
  });
});
