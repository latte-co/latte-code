import type { AgentResult } from "./agent-loop.js";
import type { SessionState } from "../session/session.js";
import type { ToolCall } from "../tools/types.js";
import { isJsonValue, isRecord, stableId, type JsonValue } from "../shared/types.js";

export type AgentPhase = "intake" | "understand" | "plan" | "edit" | "verify" | "handoff";

export const TASK_RUN_STATUSES = ["queued", "running", "waiting_permission", "blocked", "failed", "completed"] as const;

export const V0_1_CONTRACT_AUTHORITY = {
  releaseCriticalPath: "headless-agent-handoff",
  graphReadyRole: "compatibility-wrapper",
  canonicalStatusOwner: "TaskRunState.status",
  legacyStatusBehavior: "compatibility-mapping-only",
  formalSources: [
    "docs/zh-CN/design/modules/code-agent-loop.md",
    "docs/zh-CN/milestones/targets/v0.1-engineering-baseline.md",
    "docs/zh-CN/milestones/targets/v0.1-implementation-plan-review.md"
  ],
  executionMirror: ".oh-my-code/plans/v0-1-docs-src-gap-implementation-plan.md"
} as const;

export type TaskRunStatus = (typeof TASK_RUN_STATUSES)[number];

export type PendingInput =
  | {
      kind: "permission";
      permissionId: string;
      toolCallId: string;
      phase: AgentPhase;
      action: "write_file" | "edit_file" | "shell_exec" | "mcp_call" | "external_path";
      reason: string;
      command?: string;
      path?: string;
      options: ["approve", "deny"];
    }
  | {
      kind: "question";
      questionId: string;
      phase: AgentPhase;
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
  | { type: "failed"; failedStepId: string }
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

export interface TaskSpec {
  objective: string;
  scope: string[];
  acceptance: string[];
  nonGoals: string[];
  constraints: string[];
  blockers: string[];
}

export interface ContextSnippet {
  path: string;
  summary: string;
  evidenceRefs: string[];
}

export interface ContextPack {
  summary: string;
  filesRead: string[];
  relevantSnippets: ContextSnippet[];
  commandSources: string[];
  openQuestions: string[];
}

export interface ChangePlan {
  summary: string;
  targetFiles: string[];
  steps: string[];
  verificationCommands: string[];
  risks: string[];
}

export interface PatchSummary {
  changedFiles: string[];
  diffRefs: string[];
  rationale: string;
  evidenceRefs: string[];
}

export type VerificationResult = HandoffVerificationRef;

export type StepTraceStatus = "pending" | "running" | "done" | "blocked" | "failed";

export interface StepTrace {
  id: string;
  phase: AgentPhase;
  status: StepTraceStatus;
  promptId: string;
  promptVersion: string;
  summary: string;
  toolCallIds: string[];
  evidenceIds: string[];
  reactBudget: { maxSteps: number; usedSteps: number };
  error?: string;
}

export interface TaskRunState {
  id: string;
  sessionId: string;
  status: TaskRunStatus;
  currentPhase: AgentPhase;
  task?: TaskSpec;
  context?: ContextPack;
  plan?: ChangePlan;
  patch?: PatchSummary;
  verification: VerificationResult[];
  steps: StepTrace[];
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

export function mapLegacyAgentStatus(status: AgentResult["status"] | SessionState["status"]): TaskRunStatus {
  if (status === "denied") return "blocked";
  if (isTaskRunStatus(status)) return status;
  return "failed";
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
    phase: inferAgentPhaseFromToolName(call.name),
    action: inferPermissionAction(call.name, call.input.path),
    reason,
    ...(typeof call.input.command === "string" ? { command: call.input.command } : {}),
    ...(typeof call.input.path === "string" ? { path: call.input.path } : {}),
    options: ["approve", "deny"]
  };
}

export function createQuestionPendingInput(input: { questionId: string; phase: AgentPhase; prompt: string; expectedAnswer: "text" | "json"; schemaName?: string }): Extract<PendingInput, { kind: "question" }> {
  return {
    kind: "question",
    questionId: input.questionId,
    phase: input.phase,
    prompt: input.prompt,
    expectedAnswer: input.expectedAnswer,
    ...(input.schemaName === undefined ? {} : { schemaName: input.schemaName })
  };
}

export function createHeadlessRunEnvelopeFromAgentResult(result: AgentResult): HeadlessRunEnvelope {
  const status = result.runState?.status ?? mapLegacyAgentStatus(result.status);
  const pendingInput = result.pendingInput ?? result.runState?.pendingInput ?? pendingInputFromLegacySession(result.session);
  const handoff = result.handoff ?? result.runState?.handoff ?? createCompatibilityHandoff(result, status, pendingInput);
  return {
    runId: result.runState?.id ?? result.session.id,
    sessionId: result.session.id,
    status,
    ...(pendingInput === undefined ? {} : { pendingInput }),
    ...(handoff === undefined ? {} : { handoff })
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
      isAgentPhase(value.phase) &&
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
    return typeof value.questionId === "string" && isAgentPhase(value.phase) && typeof value.prompt === "string" && (value.expectedAnswer === "text" || value.expectedAnswer === "json") && (value.schemaName === undefined || typeof value.schemaName === "string");
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

export function isTaskSpec(value: unknown): value is TaskSpec {
  return isRecord(value) && typeof value.objective === "string" && isStringArray(value.scope) && isStringArray(value.acceptance) && isStringArray(value.nonGoals) && isStringArray(value.constraints) && isStringArray(value.blockers);
}

export function isContextPack(value: unknown): value is ContextPack {
  return isRecord(value) && typeof value.summary === "string" && isStringArray(value.filesRead) && Array.isArray(value.relevantSnippets) && value.relevantSnippets.every(isContextSnippet) && isStringArray(value.commandSources) && isStringArray(value.openQuestions);
}

export function isChangePlan(value: unknown): value is ChangePlan {
  return isRecord(value) && typeof value.summary === "string" && isStringArray(value.targetFiles) && isStringArray(value.steps) && isStringArray(value.verificationCommands) && isStringArray(value.risks);
}

export function isPatchSummary(value: unknown): value is PatchSummary {
  return isRecord(value) && isStringArray(value.changedFiles) && isStringArray(value.diffRefs) && typeof value.rationale === "string" && isStringArray(value.evidenceRefs);
}

export function isVerificationResult(value: unknown): value is VerificationResult {
  return isHandoffVerificationRef(value);
}

export function isStepTrace(value: unknown): value is StepTrace {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    isAgentPhase(value.phase) &&
    isStepTraceStatus(value.status) &&
    typeof value.promptId === "string" &&
    typeof value.promptVersion === "string" &&
    typeof value.summary === "string" &&
    isStringArray(value.toolCallIds) &&
    isStringArray(value.evidenceIds) &&
    isRecord(value.reactBudget) &&
    typeof value.reactBudget.maxSteps === "number" &&
    typeof value.reactBudget.usedSteps === "number" &&
    (value.error === undefined || typeof value.error === "string")
  );
}

export function isTaskRunState(value: unknown): value is TaskRunState {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.sessionId === "string" &&
    isTaskRunStatus(value.status) &&
    isAgentPhase(value.currentPhase) &&
    (value.task === undefined || isTaskSpec(value.task)) &&
    (value.context === undefined || isContextPack(value.context)) &&
    (value.plan === undefined || isChangePlan(value.plan)) &&
    (value.patch === undefined || isPatchSummary(value.patch)) &&
    Array.isArray(value.verification) &&
    value.verification.every(isVerificationResult) &&
    Array.isArray(value.steps) &&
    value.steps.every(isStepTrace) &&
    (value.pendingInput === undefined || isPendingInput(value.pendingInput)) &&
    (value.handoff === undefined || isAgentHandoff(value.handoff)) &&
    isContextSnapshot(value.contextSnapshot)
  );
}

function createCompatibilityHandoff(result: AgentResult, status: TaskRunStatus, pendingInput: PendingInput | undefined): AgentHandoff | undefined {
  if (status === "waiting_permission") return undefined;
  if (status === "running" || status === "queued") return undefined;
  const evidenceRefs = result.session.evidenceIds;
  const blockedReason = pendingInput?.kind === "question" ? pendingInput.prompt : result.error;
  return {
    id: stableId("handoff", [result.session.id, status, String(result.session.lastEventSeq)]),
    status: status === "completed" ? "completed" : status === "failed" ? "failed" : "blocked",
    summary: result.finalResponse ?? result.error ?? pendingInputReason(pendingInput) ?? "Legacy agent result requires compatibility handoff.",
    changedFiles: [],
    verification: [],
    risks: status === "completed" ? [] : ["Compatibility handoff was derived from the legacy transcript loop."],
    blockers: blockedReason === undefined ? [] : [blockedReason],
    requiredDecisions: pendingInput === undefined ? [] : [pendingInputToDecisionRef(pendingInput)],
    traceRefs: [],
    evidenceRefs
  };
}

function pendingInputFromLegacySession(session: SessionState): PendingInput | undefined {
  if (session.pendingInput !== undefined) return session.pendingInput;
  if (session.pendingPermission === undefined) return undefined;
  return {
    kind: "permission",
    permissionId: session.pendingPermission.permissionId ?? stableId("permission", [session.id, session.pendingPermission.callId]),
    toolCallId: session.pendingPermission.callId,
    phase: session.pendingPermission.phase ?? inferAgentPhaseFromToolName(session.pendingPermission.toolName),
    action: session.pendingPermission.action ?? inferPermissionAction(session.pendingPermission.toolName),
    reason: session.pendingPermission.reason,
    ...(session.pendingPermission.command === undefined ? {} : { command: session.pendingPermission.command }),
    ...(session.pendingPermission.path === undefined ? {} : { path: session.pendingPermission.path }),
    options: ["approve", "deny"]
  };
}

function pendingInputToDecisionRef(input: PendingInput): RequiredDecisionRef {
  if (input.kind === "permission") {
    return { kind: "permission", id: input.permissionId, reason: input.reason };
  }
  return { kind: "question", id: input.questionId, reason: input.prompt };
}

function pendingInputReason(input: PendingInput | undefined): string | undefined {
  if (input === undefined) return undefined;
  return input.kind === "permission" ? input.reason : input.prompt;
}

function isAgentPhase(value: unknown): value is AgentPhase {
  return value === "intake" || value === "understand" || value === "plan" || value === "edit" || value === "verify" || value === "handoff";
}

function isPermissionPendingAction(value: unknown): value is Extract<PendingInput, { kind: "permission" }>["action"] {
  return value === "write_file" || value === "edit_file" || value === "shell_exec" || value === "mcp_call" || value === "external_path";
}

function inferAgentPhaseFromToolName(toolName: string): AgentPhase {
  if (toolName === "shell_exec") return "verify";
  if (toolName === "write_file" || toolName === "edit_file") return "edit";
  if (toolName === "read_file" || toolName === "list_directory" || toolName === "search" || toolName === "read_project_manifest") return "understand";
  return "plan";
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

function isContextSnippet(value: unknown): value is ContextSnippet {
  return isRecord(value) && typeof value.path === "string" && typeof value.summary === "string" && isStringArray(value.evidenceRefs);
}

function isStepTraceStatus(value: unknown): value is StepTraceStatus {
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
