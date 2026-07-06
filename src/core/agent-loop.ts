import type { EventLog } from "../events/event-log.js";
import type { EvidenceStore } from "../evidence/store.js";
import { mapToolEvidence } from "../evidence/store.js";
import type { EvidenceRecord } from "../evidence/types.js";
import type { ModelClient } from "../model/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import { PermissionPolicy } from "../permissions/policy.js";
import { routeCommandInput } from "../commands/registry.js";
import { buildContextMessages, buildContextProjection } from "../context/compactor.js";
import { createDefaultPromptRegistry, type PromptRegistry } from "../prompts/registry.js";
import type { RuntimeContextSources } from "../runtime/context-sources.js";
import type { SessionState, SessionStore } from "../session/session.js";
import { recoverSessionFromSnapshotAndEvents } from "../session/session.js";
import type { ToolCall, ToolResult } from "../tools/types.js";
import { ToolRegistry } from "../tools/registry.js";
import { SchemaValidationError } from "../tools/schema.js";
import type { LattecodeConfig } from "../config/types.js";
import { stableId } from "../shared/types.js";
import type { AgentHandoff, AgentPhase, PendingInput, ResumeInput, StepTrace, TaskRunState, TaskRunStatus } from "./contracts.js";
import { createPermissionPendingInput, createQuestionPendingInput, isResumeInput } from "./contracts.js";
import { createDefaultPhaseContracts, type PhaseArtifact, type PhaseContract } from "./phases.js";
import { applyPhaseArtifact, buildBlockedHandoff, buildFailedHandoff, createStepTrace, createTaskRunState, finalizeAgentHandoff, setRunPendingInput, setRunStatus } from "./run-state.js";

export interface AgentLoopOptions {
  cwd: string;
  config: LattecodeConfig;
  model: ModelClient;
  registry: ToolRegistry;
  permissions: PermissionPolicy;
  sessions: SessionStore;
  events: EventLog;
  evidence: EvidenceStore;
  promptRegistry?: PromptRegistry;
  loadContextSources?: () => Promise<RuntimeContextSources>;
  maxTurns?: number;
}

export interface RunAgentInput {
  input: string;
  sessionId?: string;
  allowedTools?: string[];
}

export interface ResumeAgentInput {
  sessionId: string;
  input: ResumeInput;
  allowedTools?: string[];
}

export interface AgentResult {
  status: TaskRunStatus | "denied";
  session: SessionState;
  finalResponse?: string;
  runState?: TaskRunState;
  handoff?: AgentHandoff;
  evidence: EvidenceRecord[];
  pendingInput?: PendingInput;
  pendingPermission?: SessionState["pendingPermission"];
  error?: string;
}

export class AgentLoop {
  constructor(private readonly options: AgentLoopOptions) {}

  async run(input: RunAgentInput): Promise<AgentResult> {
    const session = await this.openSession(input.sessionId);
    const evidenceRecords: EvidenceRecord[] = [];
    await this.options.events.append("user.input", session.id, { input: input.input });
    session.transcript.push({ role: "user", content: input.input });
    const run = createTaskRunState(session.id, input.input);
    session.runState = run;
    session.status = run.status;
    const sourceResult = await this.applyRuntimeContextSources(session, run, input.input);
    if (sourceResult !== undefined) return sourceResult;
    await this.updateRun(session, run, "run.created");

    return this.runFromState(session, run, input.allowedTools, evidenceRecords, []);
  }

  async resume(input: ResumeAgentInput): Promise<AgentResult> {
    const session = await this.openSession(input.sessionId);
    const run = session.runState;
    if (run?.status === "failed" && run.handoff !== undefined) {
      return { status: "failed", session, runState: run, handoff: run.handoff, evidence: [], error: run.handoff.summary };
    }
    if (run?.status === "completed" && run.handoff !== undefined) {
      return { status: "completed", session, runState: run, handoff: run.handoff, finalResponse: run.handoff.summary, evidence: [] };
    }
    if (run === undefined || run.pendingInput === undefined) {
      return { status: "blocked", session, evidence: [], error: "No resumable pending input found." };
    }
    if (!isResumeInput(input.input)) {
      /* v8 ignore next -- TypeScript callers pass ResumeInput; this protects untyped JavaScript callers. */
      return { status: "blocked", session, runState: run, evidence: [], error: "Invalid resume input." };
    }
    await this.options.events.append("resume.received", session.id, { runId: run.id, input: input.input });

    if (input.input.kind !== run.pendingInput.kind) {
      return await this.blockRun(session, run, `Resume input kind '${input.input.kind}' does not match pending '${run.pendingInput.kind}'.`, undefined, []);
    }

    if (input.input.kind === "permission") {
      if (run.pendingInput.kind !== "permission" || input.input.permissionId !== run.pendingInput.permissionId) {
        return await this.blockRun(session, run, "Permission resume id does not match pending permission.", undefined, []);
      }
      const pendingPermissionInput = run.pendingInput;
      const call = session.pendingToolCall;
      delete session.pendingInput;
      delete session.pendingPermission;
      delete session.pendingToolCall;
      delete run.pendingInput;
      delete run.resume;
      run.status = "running";
      await this.updateRun(session, run, "run.updated");
      if (input.input.decision === "deny") {
        const handoff = buildBlockedHandoff(run, input.input.reason ?? "Permission denied by resume input.", pendingPermissionInput);
        run.handoff = handoff;
        setRunStatus(run, "blocked");
        await this.updateRun(session, run, "run.updated");
        return { status: "blocked", session, runState: run, handoff, evidence: [], error: handoff.summary };
      }
      if (call !== undefined) {
        const step = activeStep(run);
        const evidenceRecords: EvidenceRecord[] = [];
        const toolResults: ToolResult[] = [];
        await this.executeApprovedPendingCall(session, run, step, call, input.input.reason ?? "Permission approved by resume input", evidenceRecords, toolResults);
        return this.runFromState(session, run, input.allowedTools, evidenceRecords, toolResults);
      }
      return this.runFromState(session, run, input.allowedTools, [], []);
    }

    if (run.pendingInput.kind !== "question" || input.input.questionId !== run.pendingInput.questionId) {
      return await this.blockRun(session, run, "Question resume id does not match pending question.", undefined, []);
    }
    session.transcript.push({ role: "user", content: input.input.answerText ?? JSON.stringify(input.input.answerJson) });
    delete session.pendingInput;
    delete run.pendingInput;
    delete run.resume;
    run.status = "running";
    await this.updateRun(session, run, "run.updated");
    return this.runFromState(session, run, input.allowedTools, [], []);
  }

  private async runFromState(session: SessionState, run: TaskRunState, allowedToolNames: string[] | undefined, evidenceRecords: EvidenceRecord[], carryToolResults: ToolResult[]): Promise<AgentResult> {
    const allowedByNode = allowedToolNames === undefined ? undefined : new Set(allowedToolNames);
    const contracts = createDefaultPhaseContracts(this.options.config.runtime.maxPhaseSteps);

    try {
      while (run.status !== "completed" && run.status !== "failed" && run.status !== "blocked" && run.status !== "waiting_permission") {
        const contract = contracts[run.currentPhase];
        const result = await this.runPhase(session, run, contract, allowedByNode, evidenceRecords, carryToolResults);
        if (result !== undefined) return result;
      }
      if (run.status === "completed" && run.handoff !== undefined) return { status: "completed", session, runState: run, finalResponse: run.handoff.summary, handoff: run.handoff, evidence: evidenceRecords };
      if (run.status === "failed") return { status: "failed", session, runState: run, ...(run.handoff === undefined ? {} : { handoff: run.handoff }), evidence: evidenceRecords, ...(run.handoff?.summary === undefined ? {} : { error: run.handoff.summary }) };
      if (run.status === "blocked") return { status: "blocked", session, runState: run, ...(run.handoff === undefined ? {} : { handoff: run.handoff }), evidence: evidenceRecords, ...(run.pendingInput === undefined ? {} : { pendingInput: run.pendingInput }), ...(run.handoff?.summary === undefined ? {} : { error: run.handoff.summary }) };
      return { status: "waiting_permission", session, runState: run, evidence: evidenceRecords, ...(run.pendingInput === undefined ? {} : { pendingInput: run.pendingInput }), ...(session.pendingPermission === undefined ? {} : { pendingPermission: session.pendingPermission }) };
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown agent loop error";
      run.handoff = buildFailedHandoff(run, message);
      setRunStatus(run, "failed", { type: "failed", failedStepId: activeStep(run)?.id ?? "unknown" });
      const failed = await this.options.events.append("loop.failed", session.id, { error: message, runId: run.id });
      session.lastEventSeq = failed.seq;
      session.status = "failed";
      session.runState = run;
      await this.options.sessions.save(session);
      return { status: "failed", session, runState: run, handoff: run.handoff, evidence: evidenceRecords, error: message };
    }
  }

  private async runPhase(session: SessionState, run: TaskRunState, contract: PhaseContract<PhaseArtifact>, allowedByNode: Set<string> | undefined, evidenceRecords: EvidenceRecord[], carryToolResults: ToolResult[]): Promise<AgentResult | undefined> {
    run.status = "running";
    run.currentPhase = contract.phase;
    const step = createStepTrace({ runId: run.id, phase: contract.phase, index: run.steps.length, maxSteps: contract.maxReactSteps });
    run.steps.push(step);
    await this.updateRun(session, run, "phase.started", { phase: contract.phase, stepId: step.id });
    await this.options.events.append("step.started", session.id, { runId: run.id, step: JSON.parse(JSON.stringify(step)) });
    const toolResults = [...carryToolResults];
    carryToolResults.length = 0;
    let invalidArtifactCount = 0;
    for (let turn = 0; turn < contract.maxReactSteps; turn += 1) {
      step.reactBudget.usedSteps = turn + 1;
      const visibleTools = this.visibleTools(contract, allowedByNode);
      const promptRegistry = this.options.promptRegistry ?? createDefaultPromptRegistry(this.options.config.prompts.profile, this.options.config.prompts.language);
      const promptTemplate = promptRegistry.get(contract.phase);
      step.promptId = promptTemplate.id;
      step.promptVersion = promptTemplate.version;
      const baseMessages = promptTemplate.render({
        run,
        phase: contract.phase,
        allowedTools: visibleTools.map((tool) => tool.name),
        contextProjection: buildContextProjection(run, toolResults),
        toolResults
      });
      const builtMessages = buildContextMessages({ run, transcript: session.transcript, config: this.options.config.context, baseMessages, toolResults });
      if (builtMessages.blockedReason !== undefined) {
        const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, step.id, contract.phase, "context_budget_gate"]), phase: contract.phase, prompt: builtMessages.blockedReason, expectedAnswer: "text" });
        step.status = "blocked";
        step.summary = builtMessages.blockedReason;
        step.error = builtMessages.blockedReason;
        return this.blockRun(session, run, builtMessages.blockedReason, pendingInput, evidenceRecords);
      }
      await this.options.events.append("model.requested", session.id, { runId: run.id, phase: contract.phase, stepId: step.id, turn });
      const modelTurn = await this.options.model.generate({ messages: builtMessages.messages, tools: visibleTools, toolResults });
      await this.options.events.append("model.responded", session.id, modelTurn.type === "message" ? { runId: run.id, phase: contract.phase, stepId: step.id, type: modelTurn.type, content: modelTurn.content } : { runId: run.id, phase: contract.phase, stepId: step.id, type: modelTurn.type, toolCallCount: modelTurn.toolCalls.length });

      if (modelTurn.type === "message") {
        session.transcript.push({ role: "assistant", content: modelTurn.content });
        try {
          const artifactValue = parseArtifact(modelTurn.content);
          const artifact = contract.validateOutput(artifactValue);
          const acceptedArtifact = contract.phase === "handoff" ? finalizeAgentHandoff(run, artifact as AgentHandoff) : artifact;
          this.validateExecutionGates(contract.phase, acceptedArtifact, run);
          applyPhaseArtifact(run, contract.phase, acceptedArtifact);
          step.status = "done";
          step.summary = `${contract.outputSchemaName} accepted for ${contract.phase}`;
          const next = contract.next(acceptedArtifact, run);
          await this.advanceAfterArtifact(session, run, step, contract.phase, next, acceptedArtifact);
          return undefined;
        } catch (error) {
          invalidArtifactCount += 1;
          /* v8 ignore next -- JSON parsing and phase validators throw Error instances in this runtime. */
          const message = error instanceof Error ? error.message : "Invalid phase artifact";
          await this.options.events.append("phase.blocked", session.id, { runId: run.id, phase: contract.phase, stepId: step.id, reason: message, repairAttempt: invalidArtifactCount });
          if (invalidArtifactCount > this.options.config.runtime.maxRepairTurns) {
            const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, step.id, contract.phase, message]), phase: contract.phase, prompt: `${contract.outputSchemaName} artifact failed validation: ${message}`, expectedAnswer: "json", schemaName: contract.outputSchemaName });
            setRunPendingInput(run, pendingInput);
            step.status = "blocked";
            step.error = message;
            step.summary = pendingInput.prompt;
            run.handoff = buildBlockedHandoff(run, pendingInput.prompt, pendingInput);
            await this.updateRun(session, run, "run.updated");
            await this.options.events.append("step.completed", session.id, { runId: run.id, step: JSON.parse(JSON.stringify(step)) });
            await this.options.sessions.save(session);
            return { status: "blocked", session, runState: run, handoff: run.handoff, evidence: evidenceRecords, pendingInput, error: pendingInput.prompt };
          }
          session.transcript.push({ role: "tool", content: `Artifact validation failed for ${contract.outputSchemaName}: ${message}. Return corrected JSON only.` });
        }
      } else {
        for (const call of modelTurn.toolCalls) {
          const handled = await this.handleToolCall(session, run, step, call, phaseAllowedSet(contract, allowedByNode), evidenceRecords, toolResults);
          if (handled !== undefined) return handled;
        }
      }
    }
    throw new Error(`Phase '${contract.phase}' exceeded maxReactSteps`);
  }

  private async openSession(id: string | undefined): Promise<SessionState> {
    if (id !== undefined) {
      const existing = await this.options.sessions.get(id);
      const events = await this.options.events.read(id);
      if (existing !== undefined) {
        const recovered = recoverSessionFromSnapshotAndEvents(existing, events);
        if (recovered.lastEventSeq !== existing.lastEventSeq || recovered.evidenceIds.length !== existing.evidenceIds.length || recovered.transcript.length !== existing.transcript.length || recovered.status !== existing.status) {
          await this.options.sessions.save(recovered);
        }
        await this.enforceRecoveryGate(recovered);
        return recovered;
      }
      if (events.length > 0) {
        const recovered = recoverSessionFromSnapshotAndEvents({ id, status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 }, events);
        await this.enforceRecoveryGate(recovered);
        await this.options.sessions.save(recovered);
        return recovered;
      }
    }
    const created = await this.options.sessions.create(id);
    const event = await this.options.events.append("session.created", created.id, { sessionId: created.id });
    created.lastEventSeq = event.seq;
    return created;
  }

  private async handleToolCall(session: SessionState, run: TaskRunState, step: StepTrace, call: ToolCall, allowedTools: Set<string> | undefined, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<AgentResult | undefined> {
    await this.options.events.append("tool.requested", session.id, { runId: run.id, stepId: step.id, phase: step.phase, callId: call.id, toolName: call.name, input: call.input });
    const validation = this.validateCall(call);
    if (validation.kind === "invalid") {
      const permission = syntheticPermission(call, "deny", "Tool input failed schema validation");
      const toolResult = errorResult(call, validation.error);
      await this.recordToolOutcome(session, run, step, call, validation.toolName, toolResult, permission, evidenceRecords, toolResults);
      return undefined;
    }

      if (allowedTools !== undefined && !allowedTools.has(validation.tool.name) && !isMcpToolAllowedInPhase(step.phase, validation.tool.name)) {
      const permission = syntheticPermission(call, "deny", `Tool '${validation.tool.name}' is not allowed by node contract`);
      const toolResult = errorResult(call, permission.reason);
      await this.recordToolOutcome(session, run, step, call, validation.tool.name, toolResult, permission, evidenceRecords, toolResults);
      return this.blockRun(session, run, permission.reason, undefined, evidenceRecords);
    }

    const decision = this.options.permissions.decide({
      toolName: validation.tool.name,
      call,
      cwd: this.options.cwd,
      riskLevel: validation.tool.riskLevel,
      mutating: validation.tool.mutating,
      requirement: validation.tool.permission
    });
    const pendingInputForAsk = decision.action === "ask" ? createPermissionPendingInput(session.id, call, decision.reason) : undefined;
    const permissionEvent = await this.options.events.append("permission.decided", session.id, {
      runId: run.id,
      stepId: step.id,
      phase: step.phase,
      callId: call.id,
      toolName: call.name,
      action: decision.action,
      reason: decision.reason,
      toolCall: JSON.parse(JSON.stringify(call)),
      ...(pendingInputForAsk === undefined ? {} : { pendingInput: pendingInputForAsk }),
      ...(pendingInputForAsk === undefined ? {} : { permissionId: pendingInputForAsk.permissionId, phase: pendingInputForAsk.phase, pendingAction: pendingInputForAsk.action }),
      ...(pendingInputForAsk?.command === undefined ? {} : { command: pendingInputForAsk.command }),
      ...(pendingInputForAsk?.path === undefined ? {} : { path: pendingInputForAsk.path })
    });

    if (decision.action === "ask") {
      /* v8 ignore next -- pendingInputForAsk is created from the same ask decision immediately above. */
      if (pendingInputForAsk === undefined) throw new Error("Missing pending input for permission ask");
      const pendingInput = pendingInputForAsk;
      session.status = "waiting_permission";
      session.pendingInput = pendingInput;
      session.pendingToolCall = call;
      session.pendingPermission = {
        callId: call.id,
        toolName: call.name,
        reason: decision.reason,
        permissionId: pendingInput.permissionId,
        phase: pendingInput.phase,
        action: pendingInput.action,
        ...(pendingInput.command === undefined ? {} : { command: pendingInput.command }),
        ...(pendingInput.path === undefined ? {} : { path: pendingInput.path })
      };
      session.lastEventSeq = permissionEvent.seq;
      setRunPendingInput(run, pendingInput);
      step.status = "blocked";
      step.summary = decision.reason;
      await this.updateRun(session, run, "run.updated");
      await this.options.sessions.save(session);
      return { status: "waiting_permission", session, runState: run, evidence: evidenceRecords, pendingInput, pendingPermission: session.pendingPermission };
    }
    if (decision.action === "deny") {
      const toolResult = errorResult(call, decision.reason);
      await this.recordToolOutcome(session, run, step, call, validation.tool.name, toolResult, decision, evidenceRecords, toolResults);
      const permissionId = stableId("permission", [session.id, call.id]);
      const handoff = buildBlockedHandoff(run, decision.reason, { kind: "permission", permissionId, toolCallId: call.id, phase: step.phase, action: createPermissionPendingInput(session.id, call, decision.reason).action, reason: decision.reason, ...(typeof call.input.command === "string" ? { command: call.input.command } : {}), ...(typeof call.input.path === "string" ? { path: call.input.path } : {}), options: ["approve", "deny"] });
      run.handoff = handoff;
      setRunStatus(run, "blocked");
      step.status = "blocked";
      step.summary = decision.reason;
      await this.updateRun(session, run, "run.updated");
      await this.options.sessions.save(session);
      return { status: "blocked", session, runState: run, handoff, evidence: evidenceRecords, error: decision.reason };
    }

    return this.executeToolCall(session, run, step, call, validation.tool.name, decision, evidenceRecords, toolResults);
  }

  private async executeToolCall(session: SessionState, run: TaskRunState, step: StepTrace, call: ToolCall, toolName: string, decision: PermissionDecision, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<AgentResult | undefined> {
    try {
      const toolResult = await this.options.registry.execute(call, executionContext(this.options.cwd, session, this.options.config));
      await this.recordToolOutcome(session, run, step, call, toolName, { ...toolResult, callId: call.id }, decision, evidenceRecords, toolResults);
      return undefined;
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown tool execution error";
      if (!isBlockingToolGate(message)) throw error;
      const toolResult = errorResult(call, message);
      await this.recordToolOutcome(session, run, step, call, toolName, toolResult, decision, evidenceRecords, toolResults);
      const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, step.id, call.id, message]), phase: step.phase, prompt: message, expectedAnswer: "text" });
      step.status = "blocked";
      step.summary = message;
      step.error = message;
      return this.blockRun(session, run, message, pendingInput, evidenceRecords);
    }
  }

  private validateCall(call: ToolCall): { kind: "valid"; tool: ReturnType<ToolRegistry["get"]> } | { kind: "invalid"; error: string; toolName: string } {
    try {
      return { kind: "valid", tool: this.options.registry.validate(call) };
    } catch (error) {
      /* v8 ignore next -- ToolRegistry validation throws Error-compatible exceptions. */
      const message = error instanceof SchemaValidationError ? error.message : error instanceof Error ? error.message : "Unknown validation error";
      return { kind: "invalid", error: message, toolName: call.name };
    }
  }

  private async recordToolOutcome(session: SessionState, run: TaskRunState, step: StepTrace, call: ToolCall, toolName: string, result: ToolResult, permission: PermissionDecision, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<void> {
    await this.options.events.append("tool.completed", session.id, { runId: run.id, stepId: step.id, phase: step.phase, callId: call.id, toolName, ok: result.ok, summary: result.summary });
    const draft = this.mapEvidenceDraft(call, toolName, result, permission);
    const evidence = await this.options.evidence.record(session.id, toolName, draft, permission);
    const evidenceEvent = await this.options.events.append("evidence.recorded", session.id, { runId: run.id, stepId: step.id, phase: step.phase, evidenceId: evidence.id, toolName, callId: call.id });
    session.lastEventSeq = evidenceEvent.seq;
    session.evidenceIds.push(evidence.id);
    session.transcript.push({ role: "tool", content: result.summary });
    step.toolCallIds.push(call.id);
    step.evidenceIds.push(evidence.id);
    run.contextSnapshot.messageRefs.push(evidence.id);
    if (toolName === "git_diff") run.contextSnapshot.decisionRefs.push(`git_diff:${result.ok ? "ok" : "failed"}:${evidence.id}`);
    evidenceRecords.push(evidence);
    toolResults.push(result);
    session.runState = run;
    await this.options.sessions.save(session);
  }

  private mapEvidenceDraft(call: ToolCall, toolName: string, result: ToolResult, permission: PermissionDecision): ReturnType<typeof mapToolEvidence> {
    try {
      return mapToolEvidence(this.options.registry.get(toolName), call, result, permission, this.options.config.evidence.maxEvidenceBytes);
    } catch {
      return {
        inputSummary: JSON.stringify(call.input),
        outputSummary: result.summary,
        references: result.references,
        truncated: result.truncated
      };
    }
  }

  private async executeApprovedPendingCall(session: SessionState, run: TaskRunState, step: StepTrace | undefined, call: ToolCall, reason: string, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<void> {
    const active = step ?? createStepTrace({ runId: run.id, phase: run.currentPhase, index: run.steps.length, maxSteps: this.options.config.runtime.maxPhaseSteps });
    if (step === undefined) run.steps.push(active);
    const result = await this.options.registry.execute(call, executionContext(this.options.cwd, session, this.options.config));
    await this.recordToolOutcome(session, run, active, call, call.name, { ...result, callId: call.id }, syntheticPermission(call, "allow", reason), evidenceRecords, toolResults);
  }

  private visibleTools(contract: PhaseContract<PhaseArtifact>, allowedByNode: Set<string> | undefined): ReturnType<ToolRegistry["list"]> {
    const allowed = phaseAllowedSet(contract, allowedByNode);
    return this.options.registry.list().filter((tool) => allowed.has(tool.name) || isMcpToolAllowedInPhase(contract.phase, tool.name));
  }

  private async applyRuntimeContextSources(session: SessionState, run: TaskRunState, input: string): Promise<AgentResult | undefined> {
    if (this.options.loadContextSources === undefined) return undefined;
    try {
      const sources = await this.options.loadContextSources();
      if (sources.agentsMd !== undefined) {
        run.contextSnapshot.agentsMd = { path: sources.agentsMd.path, hash: sources.agentsMd.hash, summary: sources.agentsMd.summary };
        run.contextSnapshot.pinnedConstraints.push(`AGENTS.md ${sources.agentsMd.hash}: ${sources.agentsMd.summary}`);
        await this.options.events.append("agents.snapshot", session.id, { runId: run.id, snapshot: JSON.parse(JSON.stringify(sources.agentsMd)) });
      }
      if (sources.skills.length > 0) {
        run.contextSnapshot.skills = sources.skills.map((skill) => ({ name: skill.name, path: skill.path, hash: skill.hash, summary: skill.instructions }));
        await this.options.events.append("skills.loaded", session.id, { runId: run.id, skills: run.contextSnapshot.skills });
      }
      if (sources.commands.length > 0) {
        run.contextSnapshot.commands = sources.commands.map((command) => ({ name: command.name, path: command.path, hash: command.hash, description: command.description }));
      }
      if (sources.mcpTools.length > 0) {
        run.contextSnapshot.mcpTools = sources.mcpTools.map((tool) => ({ server: tool.server, tool: tool.tool, toolName: tool.toolName }));
      }
      const routed = routeCommandInput(input, sources.commands);
      if (routed !== undefined) {
        run.task = routed.task;
        run.currentPhase = "understand";
        run.contextSnapshot.decisionRefs.push(`command:${routed.command.name}:${routed.command.hash}`);
        await this.options.events.append("command.routed", session.id, { runId: run.id, command: routed.command.name, args: routed.args, task: JSON.parse(JSON.stringify(routed.task)) });
      }
      return undefined;
    } catch (error) {
      const message = error instanceof Error ? error.message : "runtime context source loading failed";
      const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, "context_sources", message]), phase: "understand", prompt: message, expectedAnswer: "text" });
      setRunPendingInput(run, pendingInput);
      run.handoff = buildBlockedHandoff(run, message, pendingInput);
      await this.updateRun(session, run, "run.created");
      return { status: "blocked", session, runState: run, handoff: run.handoff, evidence: [], pendingInput, error: message };
    }
  }

  private async advanceAfterArtifact(session: SessionState, run: TaskRunState, step: StepTrace, phase: AgentPhase, next: AgentPhase | "completed" | "blocked" | "failed", artifact: PhaseArtifact): Promise<void> {
    await this.options.events.append("phase.completed", session.id, { runId: run.id, phase, stepId: step.id, outputSchemaName: phase, artifact: JSON.parse(JSON.stringify(artifact)) });
    await this.options.events.append("step.completed", session.id, { runId: run.id, step: JSON.parse(JSON.stringify(step)) });
    if (next === "completed") {
      /* v8 ignore next -- only accepted handoff artifacts can complete the phase graph, so handoff is present. */
      setRunStatus(run, "completed", run.handoff === undefined ? undefined : { type: "completed", handoffId: run.handoff.id });
      if (run.handoff?.summary !== undefined) session.finalResponse = run.handoff.summary;
    } else if (next === "failed") {
      if (phase !== "handoff" || run.handoff === undefined) run.handoff = buildFailedHandoff(run, "Verification failed.");
      setRunStatus(run, "failed", { type: "failed", failedStepId: step.id });
    } else if (next === "blocked") {
      const blockedDecision = phase === "handoff" ? run.handoff?.requiredDecisions.find((decision) => decision.kind === "question") : undefined;
      const pendingInput = createQuestionPendingInput({ questionId: blockedDecision?.id ?? stableId("question", [run.id, step.id, phase, "blocked"]), phase, prompt: blockedDecision?.reason ?? `${phase} phase produced blockers or open questions.`, expectedAnswer: "text" });
      setRunPendingInput(run, pendingInput);
      run.handoff = phase === "handoff" && run.handoff !== undefined ? finalizeAgentHandoff(run, run.handoff) : buildBlockedHandoff(run, pendingInput.prompt, pendingInput);
      session.pendingInput = pendingInput;
    } else {
      run.currentPhase = next;
      run.status = "running";
    }
    await this.updateRun(session, run, "run.updated");
    await this.options.sessions.save(session);
  }

  private async blockRun(session: SessionState, run: TaskRunState, summary: string, pendingInput: PendingInput | undefined, evidenceRecords: EvidenceRecord[]): Promise<AgentResult> {
    if (pendingInput !== undefined) {
      setRunPendingInput(run, pendingInput);
      session.pendingInput = pendingInput;
    } else {
      setRunStatus(run, "blocked");
    }
    run.handoff = buildBlockedHandoff(run, summary, pendingInput);
    session.status = "blocked";
    session.runState = run;
    await this.updateRun(session, run, "run.updated");
    await this.options.sessions.save(session);
    return { status: "blocked", session, runState: run, handoff: run.handoff, evidence: evidenceRecords, ...(pendingInput === undefined ? {} : { pendingInput }), error: summary };
  }

  private validateExecutionGates(phase: AgentPhase, artifact: PhaseArtifact, run: TaskRunState): void {
    if (phase === "verify") {
      const verification = artifact as import("./contracts.js").VerificationResult[];
      const declaredCommands = run.plan?.verificationCommands ?? [];
      const missing = declaredCommands.filter((command) => !verification.some((entry) => entry.command === command));
      if (missing.length > 0) throw new Error(`verification_gate: missing declared verification results for ${missing.join(", ")}`);
      const skippedWithoutReason = verification.filter((entry) => entry.status === "skipped" && entry.summary.trim().length === 0).map((entry) => entry.command);
      if (skippedWithoutReason.length > 0) throw new Error(`verification_gate: skipped verification requires a reason for ${skippedWithoutReason.join(", ")}`);
    }
    if (phase === "handoff") {
      const handoff = artifact as AgentHandoff;
      if (handoff.status === "completed" && run.verification.some((entry) => entry.status === "failed")) throw new Error("handoff_gate: completed handoff cannot include failed verification");
      if (handoff.status === "completed" && (handoff.blockers.length > 0 || handoff.requiredDecisions.length > 0)) throw new Error("handoff_gate: completed handoff cannot include blockers or required decisions");
      const changedFiles = run.patch?.changedFiles ?? [];
      const missingChangedFiles = changedFiles.filter((file) => !handoff.changedFiles.includes(file));
      /* v8 ignore next -- finalizeAgentHandoff merges run.patch.changedFiles before this post-finalize gate. */
      if (missingChangedFiles.length > 0) throw new Error(`handoff_gate: handoff missing changed files ${missingChangedFiles.join(", ")}`);
      const missingVerification = run.verification.filter((entry) => !handoff.verification.some((handoffEntry) => handoffEntry.command === entry.command));
      /* v8 ignore next -- finalizeAgentHandoff merges run.verification before this post-finalize gate. */
      if (missingVerification.length > 0) throw new Error(`handoff_gate: handoff missing verification ${missingVerification.map((entry) => entry.command).join(", ")}`);
      const missingTraceRefs = run.steps.filter((step) => !handoff.traceRefs.includes(step.id));
      /* v8 ignore next -- finalizeAgentHandoff merges current step refs before this post-finalize gate. */
      if (missingTraceRefs.length > 0) throw new Error(`handoff_gate: handoff missing trace refs ${missingTraceRefs.map((step) => step.id).join(", ")}`);
      if (changedFiles.length > 0 && !run.contextSnapshot.decisionRefs.some((ref) => ref.startsWith("git_diff:ok:"))) throw new Error("diff_review_gate: successful git_diff summary is required before handoff for changed files");
    }
  }

  private async enforceRecoveryGate(session: SessionState): Promise<void> {
    const run = session.runState;
    if (run === undefined) return;
    if (run.status === "failed" && run.handoff === undefined) {
      await this.failRecovery(session, run, "recovery_gate: failed run is missing failed handoff");
      return;
    }
    if (run.status === "completed" && run.handoff === undefined) {
      await this.failRecovery(session, run, "recovery_gate: completed run is missing handoff");
      return;
    }
    if (run.status === "waiting_permission") {
      if (session.pendingInput?.kind === "permission" && session.pendingToolCall !== undefined && session.pendingPermission !== undefined && run.pendingInput?.kind === "permission" && run.pendingInput.permissionId === session.pendingInput.permissionId) return;
      await this.failRecovery(session, run, "recovery_gate: waiting_permission session is missing pending permission or tool call state");
      return;
    }
    if (run.status === "blocked" && run.pendingInput !== undefined) {
      if (session.pendingInput?.kind === run.pendingInput.kind) return;
      if (session.pendingInput === undefined) {
        session.pendingInput = run.pendingInput;
        await this.options.sessions.save(session);
        return;
      }
      await this.failRecovery(session, run, "recovery_gate: blocked run is missing matching pending input state");
      return;
    }
    if (session.pendingToolCall !== undefined || session.pendingPermission !== undefined) await this.failRecovery(session, run, "recovery_gate: non-permission run has dangling pending tool state");
  }

  private async failRecovery(session: SessionState, run: TaskRunState, summary: string): Promise<void> {
    run.handoff = buildFailedHandoff(run, summary);
    setRunStatus(run, "failed", { type: "failed", failedStepId: run.steps.at(-1)?.id ?? "recovery_gate" });
    session.status = "failed";
    session.runState = run;
    delete session.pendingInput;
    delete session.pendingPermission;
    delete session.pendingToolCall;
    await this.options.events.append("recovery.failed", session.id, { runId: run.id, reason: summary, handoff: JSON.parse(JSON.stringify(run.handoff)) });
    await this.options.sessions.save(session);
  }

  private async updateRun(session: SessionState, run: TaskRunState, type: "run.created" | "run.updated" | "phase.started", extra: Record<string, unknown> = {}): Promise<void> {
    session.runState = run;
    session.status = run.status;
    const event = await this.options.events.append(type, session.id, { ...jsonPayload(extra), runId: run.id, runState: JSON.parse(JSON.stringify(run)) });
    session.lastEventSeq = event.seq;
    await this.options.sessions.save(session);
  }
}

function parseArtifact(content: string): unknown {
  const trimmed = content.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/u);
  const candidate = fenced?.[1] ?? trimmed;
  return JSON.parse(candidate) as unknown;
}

function isMcpToolAllowedInPhase(phase: AgentPhase, toolName: string): boolean {
  return toolName.startsWith("mcp_") && phase !== "intake" && phase !== "handoff";
}

function phaseAllowedSet(contract: PhaseContract<PhaseArtifact>, allowedByNode: Set<string> | undefined): Set<string> {
  const phaseAllowed = new Set(contract.allowedTools);
  if (allowedByNode === undefined) return phaseAllowed;
  return new Set([...phaseAllowed].filter((tool) => allowedByNode.has(tool)));
}

function activeStep(run: TaskRunState): StepTrace | undefined {
  return [...run.steps].reverse().find((step) => step.status === "running" || step.status === "blocked");
}

function jsonPayload(value: Record<string, unknown>): Record<string, import("../shared/types.js").JsonValue> {
  return JSON.parse(JSON.stringify(value)) as Record<string, import("../shared/types.js").JsonValue>;
}

function errorResult(call: ToolCall, error: string): ToolResult {
  return { callId: call.id, toolName: call.name, ok: false, error, summary: `Tool ${call.name} failed: ${error}`, references: [], truncated: false };
}

function syntheticPermission(call: ToolCall, action: PermissionDecision["action"], reason: string): PermissionDecision {
  return { action, reason, requirement: { reason }, metadata: { toolName: call.name, riskLevel: "high", mutating: false, sensitivePath: false } };
}

function executionContext(cwd: string, session: SessionState, config: LattecodeConfig) {
  session.fileSnapshots ??= {};
  return { cwd, sessionId: session.id, maxOutputBytes: config.tools.maxOutputBytes, shellDefaultTimeoutMs: config.tools.shell.defaultTimeoutMs, fileSnapshots: session.fileSnapshots };
}

function isBlockingToolGate(message: string): boolean {
  return message.startsWith("read_before_write_gate:") || message.startsWith("stale_write_gate:") || message.startsWith("edit_match_gate:");
}
