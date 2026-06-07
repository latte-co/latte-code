import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { AgentPhase, AgentHandoff, ChangePlan, ContextPack, ContextSnapshot, PatchSummary, PendingInput, ResumeMarker, StepTrace, TaskRunState, TaskRunStatus, TaskSpec, VerificationResult } from "./contracts.js";
import { isTaskRunState } from "./contracts.js";
import { stableId } from "../shared/types.js";
import type { PhaseArtifact } from "./phases.js";

export interface TaskRunStore {
  create(input: { sessionId: string; taskInput: string; runId?: string }): Promise<TaskRunState>;
  save(run: TaskRunState): Promise<void>;
  get(runId: string): Promise<TaskRunState | undefined>;
  list(sessionId?: string): Promise<TaskRunState[]>;
}

export class InMemoryTaskRunStore implements TaskRunStore {
  private readonly runs = new Map<string, TaskRunState>();

  async create(input: { sessionId: string; taskInput: string; runId?: string }): Promise<TaskRunState> {
    const run = createTaskRunState(input.sessionId, input.taskInput, input.runId);
    this.runs.set(run.id, copyTaskRunState(run));
    return copyTaskRunState(run);
  }

  async save(run: TaskRunState): Promise<void> {
    this.runs.set(run.id, copyTaskRunState(run));
  }

  async get(runId: string): Promise<TaskRunState | undefined> {
    const run = this.runs.get(runId);
    return run === undefined ? undefined : copyTaskRunState(run);
  }

  async list(sessionId?: string): Promise<TaskRunState[]> {
    return [...this.runs.values()].filter((run) => sessionId === undefined || run.sessionId === sessionId).map(copyTaskRunState);
  }
}

export class FileTaskRunStore implements TaskRunStore {
  constructor(private readonly directory: string) {}

  async create(input: { sessionId: string; taskInput: string; runId?: string }): Promise<TaskRunState> {
    const run = createTaskRunState(input.sessionId, input.taskInput, input.runId);
    await this.save(run);
    return copyTaskRunState(run);
  }

  async save(run: TaskRunState): Promise<void> {
    await mkdir(this.directory, { recursive: true });
    await writeFile(join(this.directory, `${run.id}.json`), JSON.stringify(run, null, 2), "utf8");
  }

  async get(runId: string): Promise<TaskRunState | undefined> {
    try {
      const value = JSON.parse(await readFile(join(this.directory, `${runId}.json`), "utf8")) as unknown;
      return isTaskRunState(value) ? value : undefined;
    } catch {
      return undefined;
    }
  }

  async list(sessionId?: string): Promise<TaskRunState[]> {
    const files = await readdir(this.directory).catch(() => []);
    const runs = await Promise.all(files.filter((file) => file.endsWith(".json")).map((file) => readFile(join(this.directory, file), "utf8").then((content) => JSON.parse(content) as unknown).catch(() => undefined)));
    return runs.filter((run): run is TaskRunState => isTaskRunState(run) && (sessionId === undefined || run.sessionId === sessionId)).map(copyTaskRunState);
  }
}

export function createTaskRunState(sessionId: string, taskInput: string, runId = stableId("run", [sessionId, taskInput, new Date().toISOString(), Math.random().toString()])): TaskRunState {
  return {
    id: runId,
    sessionId,
    status: "queued",
    currentPhase: "intake",
    verification: [],
    steps: [],
    contextSnapshot: createContextSnapshot(taskInput)
  };
}

export function createContextSnapshot(taskInput: string): ContextSnapshot {
  return { taskInput, messageRefs: [], decisionRefs: [], compactedSummary: "", pinnedConstraints: [] };
}

export function createStepTrace(input: { runId: string; phase: AgentPhase; index: number; maxSteps: number }): StepTrace {
  return {
    id: stableId("step", [input.runId, input.phase, String(input.index)]),
    phase: input.phase,
    status: "running",
    promptId: `phase:${input.phase}`,
    promptVersion: "v0.1",
    summary: `Running ${input.phase} phase`,
    toolCallIds: [],
    evidenceIds: [],
    reactBudget: { maxSteps: input.maxSteps, usedSteps: 0 }
  };
}

export function applyPhaseArtifact(run: TaskRunState, phase: AgentPhase, artifact: PhaseArtifact): void {
  if (phase === "intake") run.task = artifact as TaskSpec;
  if (phase === "understand") run.context = artifact as ContextPack;
  if (phase === "plan") run.plan = artifact as ChangePlan;
  if (phase === "edit") run.patch = artifact as PatchSummary;
  if (phase === "verify") run.verification = artifact as VerificationResult[];
  if (phase === "handoff") run.handoff = artifact as AgentHandoff;
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
    id: stableId("handoff", [run.id, "blocked", String(run.steps.length)]),
    status: "blocked",
    summary,
    changedFiles: run.patch?.changedFiles ?? [],
    verification: run.verification,
    risks: run.plan?.risks ?? [],
    blockers: [summary],
    requiredDecisions: requiredDecision,
    traceRefs: run.steps.map((step) => step.id),
    evidenceRefs: unique(run.steps.flatMap((step) => step.evidenceIds))
  };
}

export function buildFailedHandoff(run: TaskRunState, summary: string): AgentHandoff {
  return {
    id: stableId("handoff", [run.id, "failed", String(run.steps.length)]),
    status: "failed",
    summary,
    changedFiles: run.patch?.changedFiles ?? [],
    verification: run.verification,
    risks: [...(run.plan?.risks ?? []), summary],
    blockers: [],
    requiredDecisions: [],
    traceRefs: run.steps.map((step) => step.id),
    evidenceRefs: unique(run.steps.flatMap((step) => step.evidenceIds))
  };
}

export function finalizeAgentHandoff(run: TaskRunState, handoff: AgentHandoff): AgentHandoff {
  const pendingDecision = run.pendingInput === undefined ? [] : [{ kind: run.pendingInput.kind, id: run.pendingInput.kind === "permission" ? run.pendingInput.permissionId : run.pendingInput.questionId, reason: run.pendingInput.kind === "permission" ? run.pendingInput.reason : run.pendingInput.prompt }];
  return {
    ...handoff,
    changedFiles: unique([...(run.patch?.changedFiles ?? []), ...handoff.changedFiles]),
    verification: mergeVerification(run.verification, handoff.verification),
    risks: unique([...(run.plan?.risks ?? []), ...handoff.risks]),
    blockers: handoff.status === "blocked" && handoff.blockers.length === 0 ? [handoff.summary] : unique(handoff.blockers),
    requiredDecisions: uniqueDecisions([...handoff.requiredDecisions, ...pendingDecision]),
    traceRefs: unique([...run.steps.map((step) => step.id), ...handoff.traceRefs]),
    evidenceRefs: unique([...run.steps.flatMap((step) => step.evidenceIds), ...(run.patch?.evidenceRefs ?? []), ...run.verification.flatMap((entry) => entry.evidenceRefs), ...handoff.evidenceRefs])
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
