import type { AgentHandoff, ContextSnapshot, PendingInput, ResumeMarker, TaskRunState, TaskRunStatus, TurnTrace, VerificationResult } from "./contracts.js";
import { stableId } from "../shared/types.js";

export function createTaskRunState(sessionId: string, taskInput: string, runId = stableId("run", [sessionId, taskInput, new Date().toISOString(), Math.random().toString()])): TaskRunState {
  return {
    id: runId,
    sessionId,
    status: "queued",
    changedFiles: [],
    changeEvidenceRefs: [],
    verification: [],
    turns: [],
    contextSnapshot: createContextSnapshot(taskInput)
  };
}

export function createContextSnapshot(taskInput: string): ContextSnapshot {
  return { taskInput, messageRefs: [], decisionRefs: [], compactedSummary: "", pinnedConstraints: [] };
}

export function createTurnTrace(input: { runId: string; index: number; maxTurns: number }): TurnTrace {
  return {
    id: stableId("turn", [input.runId, String(input.index)]),
    status: "running",
    promptId: "code-agent",
    promptVersion: "v0.1",
    summary: "Running direct ReAct code-agent turn",
    toolCallIds: [],
    evidenceIds: [],
    turnBudget: { maxTurns: input.maxTurns, usedTurns: 0 }
  };
}

export function setRunStatus(run: TaskRunState, status: TaskRunStatus, resume?: ResumeMarker): void {
  run.status = status;
  if (resume === undefined) delete run.resume;
  else run.resume = resume;
  if (status !== "waiting_permission" && status !== "blocked") delete run.pendingInput;
}

export function setRunPendingInput(run: TaskRunState, pendingInput: PendingInput): void {
  run.pendingInput = pendingInput;
  run.status = pendingInput.kind === "permission" ? "waiting_permission" : "blocked";
  run.resume = pendingInput.kind === "permission" ? { type: "permission", permissionId: pendingInput.permissionId } : { type: "blocked", questionId: pendingInput.questionId };
}

export function buildBlockedHandoff(run: TaskRunState, summary: string, pendingInput?: PendingInput): AgentHandoff {
  const requiredDecision = pendingInput === undefined ? [] : [{ kind: pendingInput.kind, id: pendingInput.kind === "permission" ? pendingInput.permissionId : pendingInput.questionId, reason: pendingInput.kind === "permission" ? pendingInput.reason : pendingInput.prompt }];
  return {
    id: stableId("handoff", [run.id, "blocked", String(run.turns.length)]),
    status: "blocked",
    summary,
    changedFiles: run.changedFiles,
    verification: run.verification,
    risks: [],
    blockers: [summary],
    requiredDecisions: requiredDecision,
    traceRefs: run.turns.map((turn) => turn.id),
    evidenceRefs: unique(run.turns.flatMap((turn) => turn.evidenceIds))
  };
}

export function buildFailedHandoff(run: TaskRunState, summary: string): AgentHandoff {
  return {
    id: stableId("handoff", [run.id, "failed", String(run.turns.length)]),
    status: "failed",
    summary,
    changedFiles: run.changedFiles,
    verification: run.verification,
    risks: [summary],
    blockers: [],
    requiredDecisions: [],
    traceRefs: run.turns.map((turn) => turn.id),
    evidenceRefs: unique(run.turns.flatMap((turn) => turn.evidenceIds))
  };
}

export function finalizeAgentHandoff(run: TaskRunState, handoff: AgentHandoff): AgentHandoff {
  const pendingDecision = run.pendingInput === undefined ? [] : [{ kind: run.pendingInput.kind, id: run.pendingInput.kind === "permission" ? run.pendingInput.permissionId : run.pendingInput.questionId, reason: run.pendingInput.kind === "permission" ? run.pendingInput.reason : run.pendingInput.prompt }];
  return {
    ...handoff,
    changedFiles: unique([...run.changedFiles, ...handoff.changedFiles]),
    verification: mergeVerification(run.verification, handoff.verification),
    risks: unique(handoff.risks),
    blockers: handoff.status === "blocked" && handoff.blockers.length === 0 ? [handoff.summary] : unique(handoff.blockers),
    requiredDecisions: uniqueDecisions([...handoff.requiredDecisions, ...pendingDecision]),
    traceRefs: unique([...run.turns.map((turn) => turn.id), ...handoff.traceRefs]),
    evidenceRefs: unique([...run.turns.flatMap((turn) => turn.evidenceIds), ...run.changeEvidenceRefs, ...run.verification.flatMap((entry) => entry.evidenceRefs), ...handoff.evidenceRefs])
  };
}

export function copyTaskRunState(run: TaskRunState): TaskRunState {
  return JSON.parse(JSON.stringify(run)) as TaskRunState;
}

function unique(values: string[]): string[] {
  return [...new Set(values)];
}

function mergeVerification(runVerification: VerificationResult[], handoffVerification: VerificationResult[]): VerificationResult[] {
  const byCommand = new Map<string, VerificationResult>();
  for (const entry of handoffVerification) byCommand.set(entry.command, entry);
  for (const entry of runVerification) byCommand.set(entry.command, entry);
  return [...byCommand.values()];
}

function uniqueDecisions(values: AgentHandoff["requiredDecisions"]): AgentHandoff["requiredDecisions"] {
  const seen = new Set<string>();
  return values.filter((entry) => {
    const key = `${entry.kind}:${entry.id}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}
