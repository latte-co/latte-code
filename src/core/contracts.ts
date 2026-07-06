import type { AgentResult } from "./agent-loop.js";
import type { ToolCall } from "../tools/types.js";
import { isJsonValue, isRecord, stableId, type JsonValue } from "../shared/types.js";

export const TASK_RUN_STATUSES = ["queued", "running", "waiting_permission", "blocked", "failed", "completed"] as const;

export type TaskRunStatus = (typeof TASK_RUN_STATUSES)[number];

export type PendingInput =
  | {
      kind: "permission";
      permissionId: string;
      toolCallId: string;
      action: "write_file" | "edit_file" | "shell_exec" | "mcp_call" | "external_path";
      reason: string;
      command?: string;
      path?: string;
      options: ["approve", "deny"];
    }
  | {
      kind: "question";
      questionId: string;
      prompt: string;
      expectedAnswer: "text" | "json";
      schemaName?: string;
    };

export type ResumeInput =
  | {
      kind: "permission";
      permissionId: string;
      decision: "approve" | "deny";
      reason?: string;
    }
  | {
      kind: "question";
      questionId: string;
      answerText?: string;
      answerJson?: JsonValue;
    };

export type ResumeMarker =
  | { type: "permission"; permissionId: string }
  | { type: "blocked"; questionId: string }
  | { type: "failed"; failedTurnId: string }
  | { type: "completed"; handoffId: string };

export interface ContextSnapshot {
  taskInput: string;
  messageRefs: string[];
  decisionRefs: string[];
  compactedSummary: string;
  pinnedConstraints: string[];
  agentsMd?: { path: string; hash: string; summary: string };
  skills?: { name: string; path: string; hash: string; summary: string }[];
  commands?: { name: string; path: string; hash: string; description: string }[];
  mcpTools?: { server: string; tool: string; toolName: string }[];
}

export interface HandoffVerificationRef {
  command: string;
  status: "passed" | "failed" | "skipped";
  exitCode?: number;
  summary: string;
  evidenceRefs: string[];
  outputRefs?: string[];
}

export interface RequiredDecisionRef {
  kind: "permission" | "question";
  id: string;
  reason: string;
}

export interface AgentHandoff {
  id: string;
  status: "completed" | "failed" | "blocked";
  summary: string;
  changedFiles: string[];
  verification: HandoffVerificationRef[];
  risks: string[];
  blockers: string[];
  requiredDecisions: RequiredDecisionRef[];
  traceRefs: string[];
  evidenceRefs: string[];
}

export interface AgentTaskContext {
  objective: string;
  scope: string[];
  acceptance: string[];
  nonGoals: string[];
  constraints: string[];
  blockers: string[];
}

export type VerificationResult = HandoffVerificationRef;

export type TurnTraceStatus = "pending" | "running" | "done" | "blocked" | "failed";

export interface TurnTrace {
  id: string;
  status: TurnTraceStatus;
  promptId: string;
  promptVersion: string;
  summary: string;
  toolCallIds: string[];
  evidenceIds: string[];
  turnBudget: { maxTurns: number; usedTurns: number };
  error?: string;
}

export interface TaskRunState {
  id: string;
  sessionId: string;
  status: TaskRunStatus;
  agentContext?: AgentTaskContext;
  changedFiles: string[];
  changeEvidenceRefs: string[];
  verification: VerificationResult[];
  turns: TurnTrace[];
  resume?: ResumeMarker;
  pendingInput?: PendingInput;
  handoff?: AgentHandoff;
  contextSnapshot: ContextSnapshot;
}

export interface HeadlessRunEnvelope {
  runId: string;
  sessionId: string;
  status: TaskRunStatus;
  pendingInput?: PendingInput;
  handoff?: AgentHandoff;
}

export interface HeadlessRunListEnvelope {
  runs: HeadlessRunEnvelope[];
}

export function isTaskRunStatus(value: unknown): value is TaskRunStatus {
  return value === "queued" || value === "running" || value === "waiting_permission" || value === "blocked" || value === "failed" || value === "completed";
}

export function exitCodeForTaskRunStatus(status: TaskRunStatus): number {
  if (status === "completed") return 0;
  if (status === "waiting_permission") return 20;
  if (status === "blocked") return 21;
  if (status === "failed") return 22;
  return 22;
}

export function createPermissionPendingInput(sessionId: string, call: ToolCall, reason: string): Extract<PendingInput, { kind: "permission" }> {
  return {
    kind: "permission",
    permissionId: stableId("permission", [sessionId, call.id]),
    toolCallId: call.id,
    action: inferPermissionAction(call.name, call.input.path),
    reason,
    ...(typeof call.input.command === "string" ? { command: call.input.command } : {}),
    ...(typeof call.input.path === "string" ? { path: call.input.path } : {}),
    options: ["approve", "deny"]
  };
}

export function createQuestionPendingInput(input: { questionId: string; prompt: string; expectedAnswer: "text" | "json"; schemaName?: string }): Extract<PendingInput, { kind: "question" }> {
  return {
    kind: "question",
    questionId: input.questionId,
    prompt: input.prompt,
    expectedAnswer: input.expectedAnswer,
    ...(input.schemaName === undefined ? {} : { schemaName: input.schemaName })
  };
}

export function createHeadlessRunEnvelopeFromAgentResult(result: AgentResult): HeadlessRunEnvelope {
  const run = result.runState ?? result.session.runState;
  if (run !== undefined) return createHeadlessRunEnvelopeFromTaskRunState(run);
  return {
    runId: result.session.id,
    sessionId: result.session.id,
    status: result.status,
    ...(result.pendingInput === undefined ? {} : { pendingInput: result.pendingInput }),
    ...(result.handoff === undefined ? {} : { handoff: result.handoff })
  };
}

export function createHeadlessRunEnvelopeFromTaskRunState(run: TaskRunState): HeadlessRunEnvelope {
  return {
    runId: run.id,
    sessionId: run.sessionId,
    status: run.status,
    ...(run.pendingInput === undefined ? {} : { pendingInput: run.pendingInput }),
    ...(run.handoff === undefined ? {} : { handoff: run.handoff })
  };
}

export function isPendingInput(value: unknown): value is PendingInput {
  if (!isRecord(value) || typeof value.kind !== "string") return false;
  if (value.kind === "permission") {
    return (
      typeof value.permissionId === "string" &&
      typeof value.toolCallId === "string" &&
      isPermissionPendingAction(value.action) &&
      typeof value.reason === "string" &&
      (value.command === undefined || typeof value.command === "string") &&
      (value.path === undefined || typeof value.path === "string") &&
      Array.isArray(value.options) &&
      value.options.length === 2 &&
      value.options[0] === "approve" &&
      value.options[1] === "deny"
    );
  }
  if (value.kind === "question") {
    return typeof value.questionId === "string" && typeof value.prompt === "string" && (value.expectedAnswer === "text" || value.expectedAnswer === "json") && (value.schemaName === undefined || typeof value.schemaName === "string");
  }
  return false;
}

export function isResumeInput(value: unknown): value is ResumeInput {
  if (!isRecord(value) || typeof value.kind !== "string") return false;
  if (value.kind === "permission") {
    return typeof value.permissionId === "string" && (value.decision === "approve" || value.decision === "deny") && (value.reason === undefined || typeof value.reason === "string");
  }
  if (value.kind === "question") {
    const hasText = typeof value.answerText === "string";
    const hasJson = value.answerJson !== undefined && isJsonValue(value.answerJson);
    return typeof value.questionId === "string" && (hasText || hasJson) && (value.answerText === undefined || typeof value.answerText === "string") && (value.answerJson === undefined || isJsonValue(value.answerJson));
  }
  return false;
}

export function isHeadlessRunEnvelope(value: unknown): value is HeadlessRunEnvelope {
  return isRecord(value) && typeof value.runId === "string" && typeof value.sessionId === "string" && isTaskRunStatus(value.status) && (value.pendingInput === undefined || isPendingInput(value.pendingInput)) && (value.handoff === undefined || isAgentHandoff(value.handoff));
}

export function isAgentHandoff(value: unknown): value is AgentHandoff {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    (value.status === "completed" || value.status === "failed" || value.status === "blocked") &&
    typeof value.summary === "string" &&
    isStringArray(value.changedFiles) &&
    Array.isArray(value.verification) &&
    value.verification.every(isHandoffVerificationRef) &&
    isStringArray(value.risks) &&
    isStringArray(value.blockers) &&
    Array.isArray(value.requiredDecisions) &&
    value.requiredDecisions.every(isRequiredDecisionRef) &&
    isStringArray(value.traceRefs) &&
    isStringArray(value.evidenceRefs)
  );
}

export function isAgentTaskContext(value: unknown): value is AgentTaskContext {
  return isRecord(value) && typeof value.objective === "string" && isStringArray(value.scope) && isStringArray(value.acceptance) && isStringArray(value.nonGoals) && isStringArray(value.constraints) && isStringArray(value.blockers);
}

export function isVerificationResult(value: unknown): value is VerificationResult {
  return isHandoffVerificationRef(value);
}

export function isTurnTrace(value: unknown): value is TurnTrace {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    isTurnTraceStatus(value.status) &&
    typeof value.promptId === "string" &&
    typeof value.promptVersion === "string" &&
    typeof value.summary === "string" &&
    isStringArray(value.toolCallIds) &&
    isStringArray(value.evidenceIds) &&
    isRecord(value.turnBudget) &&
    typeof value.turnBudget.maxTurns === "number" &&
    typeof value.turnBudget.usedTurns === "number" &&
    (value.error === undefined || typeof value.error === "string")
  );
}

export function isTaskRunState(value: unknown): value is TaskRunState {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.sessionId === "string" &&
    isTaskRunStatus(value.status) &&
    (value.agentContext === undefined || isAgentTaskContext(value.agentContext)) &&
    isStringArray(value.changedFiles) &&
    isStringArray(value.changeEvidenceRefs) &&
    Array.isArray(value.verification) &&
    value.verification.every(isVerificationResult) &&
    Array.isArray(value.turns) &&
    value.turns.every(isTurnTrace) &&
    (value.pendingInput === undefined || isPendingInput(value.pendingInput)) &&
    (value.handoff === undefined || isAgentHandoff(value.handoff)) &&
    isContextSnapshot(value.contextSnapshot)
  );
}

function isPermissionPendingAction(value: unknown): value is Extract<PendingInput, { kind: "permission" }>["action"] {
  return value === "write_file" || value === "edit_file" || value === "shell_exec" || value === "mcp_call" || value === "external_path";
}

function inferPermissionAction(toolName: string, path?: unknown): Extract<PendingInput, { kind: "permission" }>["action"] {
  if (toolName === "write_file") return "write_file";
  if (toolName === "edit_file") return "edit_file";
  if (toolName === "shell_exec") return "shell_exec";
  if (toolName.startsWith("mcp_")) return "mcp_call";
  return typeof path === "string" ? "external_path" : "mcp_call";
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((entry) => typeof entry === "string");
}

function isHandoffVerificationRef(value: unknown): value is HandoffVerificationRef {
  return isRecord(value) && typeof value.command === "string" && (value.status === "passed" || value.status === "failed" || value.status === "skipped") && (value.exitCode === undefined || typeof value.exitCode === "number") && typeof value.summary === "string" && isStringArray(value.evidenceRefs) && (value.outputRefs === undefined || isStringArray(value.outputRefs));
}

function isRequiredDecisionRef(value: unknown): value is RequiredDecisionRef {
  return isRecord(value) && (value.kind === "permission" || value.kind === "question") && typeof value.id === "string" && typeof value.reason === "string";
}

function isTurnTraceStatus(value: unknown): value is TurnTraceStatus {
  return value === "pending" || value === "running" || value === "done" || value === "blocked" || value === "failed";
}

function isContextSnapshot(value: unknown): value is ContextSnapshot {
  return (
    isRecord(value) &&
    typeof value.taskInput === "string" &&
    isStringArray(value.messageRefs) &&
    isStringArray(value.decisionRefs) &&
    typeof value.compactedSummary === "string" &&
    isStringArray(value.pinnedConstraints) &&
    (value.agentsMd === undefined || (isRecord(value.agentsMd) && typeof value.agentsMd.path === "string" && typeof value.agentsMd.hash === "string" && typeof value.agentsMd.summary === "string")) &&
    (value.skills === undefined || (Array.isArray(value.skills) && value.skills.every(isSkillSnapshot))) &&
    (value.commands === undefined || (Array.isArray(value.commands) && value.commands.every(isCommandSnapshot))) &&
    (value.mcpTools === undefined || (Array.isArray(value.mcpTools) && value.mcpTools.every(isMcpToolSnapshot)))
  );
}

function isSkillSnapshot(value: unknown): boolean {
  return isRecord(value) && typeof value.name === "string" && typeof value.path === "string" && typeof value.hash === "string" && typeof value.summary === "string";
}

function isCommandSnapshot(value: unknown): boolean {
  return isRecord(value) && typeof value.name === "string" && typeof value.path === "string" && typeof value.hash === "string" && typeof value.description === "string";
}

function isMcpToolSnapshot(value: unknown): boolean {
  return isRecord(value) && typeof value.server === "string" && typeof value.tool === "string" && typeof value.toolName === "string";
}
