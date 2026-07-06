import type { EventLog } from "../events/event-log.js";
import { isAbsolute, relative, resolve } from "node:path";
import type { EvidenceStore } from "../evidence/store.js";
import { mapToolEvidence } from "../evidence/store.js";
import type { EvidenceRecord } from "../evidence/types.js";
import type { ModelClient } from "../model/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import { PermissionPolicy } from "../permissions/policy.js";
import { routeCommandInput } from "../commands/registry.js";
import { buildContextMessages, buildContextProjection } from "../context/compactor.js";
import { createDefaultCodeAgentPrompt, type CodeAgentPromptTemplate } from "../prompts/registry.js";
import type { RuntimeContextSources } from "../runtime/context-sources.js";
import type { SessionState, SessionStore } from "../session/session.js";
import { recoverSessionFromSnapshotAndEvents } from "../session/session.js";
import type { ToolCall, ToolResult } from "../tools/types.js";
import { ToolRegistry } from "../tools/registry.js";
import { SchemaValidationError } from "../tools/schema.js";
import type { LattecodeConfig } from "../config/types.js";
import { stableId } from "../shared/types.js";
import type { AgentHandoff, PendingInput, ResumeInput, TaskRunState, TaskRunStatus, TurnTrace } from "./contracts.js";
import { createPermissionPendingInput, createQuestionPendingInput, isResumeInput } from "./contracts.js";
import { buildBlockedHandoff, buildFailedHandoff, createTaskRunState, createTurnTrace, finalizeAgentHandoff, setRunPendingInput, setRunStatus } from "./run-state.js";

export interface AgentLoopOptions {
  cwd: string;
  config: LattecodeConfig;
  model: ModelClient;
  registry: ToolRegistry;
  permissions: PermissionPolicy;
  sessions: SessionStore;
  events: EventLog;
  evidence: EvidenceStore;
  codeAgentPrompt?: CodeAgentPromptTemplate;
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
  status: TaskRunStatus;
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
        const turnTrace = activeTurn(run);
        const evidenceRecords: EvidenceRecord[] = [];
        const toolResults: ToolResult[] = [];
        try {
          await this.executeApprovedPendingCall(session, run, turnTrace, call, input.input.reason ?? "Permission approved by resume input", evidenceRecords, toolResults);
        } catch (error) {
          return this.failDirectLoopRun(session, run, error, evidenceRecords);
        }
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
    return this.runDirectReactLoop(session, run, allowedToolNames, evidenceRecords, carryToolResults);
  }

  private async runDirectReactLoop(session: SessionState, run: TaskRunState, allowedToolNames: string[] | undefined, evidenceRecords: EvidenceRecord[], carryToolResults: ToolResult[]): Promise<AgentResult> {
    const allowedByToolAllowlist = allowedToolNames === undefined ? undefined : new Set(allowedToolNames);
    const maxTurns = this.options.maxTurns ?? this.options.config.runtime.maxTurns;
    try {
      run.status = "running";
      const turnTrace = createTurnTrace({ runId: run.id, index: run.turns.length, maxTurns });
      turnTrace.summary = "Running direct ReAct code-agent loop";
      run.turns.push(turnTrace);
      await this.updateRun(session, run, "run.updated");
      await this.options.events.append("turn.started", session.id, { runId: run.id, turn: JSON.parse(JSON.stringify(turnTrace)), mode: "direct" });
      const toolResults = [...carryToolResults];
      carryToolResults.length = 0;
      for (let turn = 0; turn < maxTurns; turn += 1) {
        turnTrace.turnBudget.usedTurns = turn + 1;
        const visibleTools = this.visibleCodeAgentTools(allowedByToolAllowlist);
        const promptTemplate = this.options.codeAgentPrompt ?? createDefaultCodeAgentPrompt(this.options.config.prompts.profile, this.options.config.prompts.language);
        turnTrace.promptId = promptTemplate.id;
        turnTrace.promptVersion = promptTemplate.version;
        const baseMessages = promptTemplate.render({
          run,
          allowedTools: visibleTools.map((tool) => tool.name),
          contextProjection: buildContextProjection(run, toolResults),
          toolResults
        });
        const builtMessages = buildContextMessages({ run, transcript: session.transcript, config: this.options.config.context, baseMessages, toolResults });
        if (builtMessages.blockedReason !== undefined) {
          const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, turnTrace.id, "direct", "context_budget_gate"]), prompt: builtMessages.blockedReason, expectedAnswer: "text" });
          turnTrace.status = "blocked";
          turnTrace.summary = builtMessages.blockedReason;
          turnTrace.error = builtMessages.blockedReason;
          return this.blockRun(session, run, builtMessages.blockedReason, pendingInput, evidenceRecords);
        }
        await this.options.events.append("model.requested", session.id, { runId: run.id, turnId: turnTrace.id, turn, mode: "direct" });
        const modelTurn = await this.options.model.generate({ messages: builtMessages.messages, tools: visibleTools, toolResults });
        await this.options.events.append("model.responded", session.id, modelTurn.type === "message" ? { runId: run.id, turnId: turnTrace.id, type: modelTurn.type, content: modelTurn.content, mode: "direct" } : { runId: run.id, turnId: turnTrace.id, type: modelTurn.type, toolCallCount: modelTurn.toolCalls.length, mode: "direct" });

        if (modelTurn.type === "message") {
          session.transcript.push({ role: "assistant", content: modelTurn.content });
          const handoff = this.buildDirectHandoff(session, run, modelTurn.content, toolResults, evidenceRecords);
          run.handoff = handoff;
          const finalStatus = handoff.status === "completed" ? "completed" : handoff.status === "blocked" ? "blocked" : "failed";
          turnTrace.status = finalStatus === "completed" ? "done" : finalStatus;
          turnTrace.summary = modelTurn.content;
          if (finalStatus === "completed") {
            setRunStatus(run, "completed", { type: "completed", handoffId: handoff.id });
            session.finalResponse = modelTurn.content;
          } else if (finalStatus === "blocked") {
            setRunStatus(run, "blocked");
          } else {
            turnTrace.error = handoff.summary;
            setRunStatus(run, "failed", { type: "failed", failedTurnId: turnTrace.id });
          }
          await this.options.events.append("turn.completed", session.id, { runId: run.id, turn: JSON.parse(JSON.stringify(turnTrace)), mode: "direct" });
          await this.updateRun(session, run, "run.updated");
          const finalEvent = finalStatus === "completed" ? await this.options.events.append("loop.completed", session.id, { runId: run.id, finalResponse: modelTurn.content, handoff: JSON.parse(JSON.stringify(handoff)) }) : await this.options.events.append("loop.failed", session.id, { runId: run.id, error: handoff.summary, handoff: JSON.parse(JSON.stringify(handoff)) });
          session.lastEventSeq = finalEvent.seq;
          session.status = finalStatus;
          session.runState = run;
          await this.options.sessions.save(session);
          return finalStatus === "completed" ? { status: "completed", session, runState: run, finalResponse: modelTurn.content, handoff, evidence: evidenceRecords } : { status: finalStatus, session, runState: run, handoff, evidence: evidenceRecords, error: handoff.summary };
        }

        for (const call of modelTurn.toolCalls) {
          const handled = await this.handleToolCall(session, run, turnTrace, call, allowedByToolAllowlist, evidenceRecords, toolResults);
          if (handled !== undefined) return handled;
        }
      }
      throw new Error(`Direct ReAct loop exceeded maxTurns (${maxTurns})`);
    } catch (error) {
      return this.failDirectLoopRun(session, run, error, evidenceRecords);
    }
  }

  private async failDirectLoopRun(session: SessionState, run: TaskRunState, error: unknown, evidenceRecords: EvidenceRecord[]): Promise<AgentResult> {
    const message = error instanceof Error ? error.message : "Unknown agent loop error";
    const turnTrace = activeTurn(run);
    if (turnTrace !== undefined) {
      turnTrace.status = "failed";
      turnTrace.summary = message;
      turnTrace.error = message;
    }
    run.handoff = buildFailedHandoff(run, message);
    setRunStatus(run, "failed", { type: "failed", failedTurnId: turnTrace?.id ?? "unknown" });
    if (turnTrace !== undefined) await this.options.events.append("turn.completed", session.id, { runId: run.id, turn: JSON.parse(JSON.stringify(turnTrace)), mode: "direct" });
    await this.updateRun(session, run, "run.updated");
    const failed = await this.options.events.append("loop.failed", session.id, { error: message, runId: run.id, handoff: JSON.parse(JSON.stringify(run.handoff)) });
    session.lastEventSeq = failed.seq;
    session.status = "failed";
    session.runState = run;
    await this.options.sessions.save(session);
    return { status: "failed", session, runState: run, handoff: run.handoff, evidence: evidenceRecords, error: message };
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

  private async handleToolCall(
    session: SessionState,
    run: TaskRunState,
    turnTrace: TurnTrace,
    call: ToolCall,
    allowedTools: Set<string> | undefined,
    evidenceRecords: EvidenceRecord[],
    toolResults: ToolResult[]
  ): Promise<AgentResult | undefined> {
    await this.options.events.append("tool.requested", session.id, { runId: run.id, turnId: turnTrace.id, callId: call.id, toolName: call.name, input: call.input });
    const validation = this.validateCall(call);
    if (validation.kind === "invalid") {
      const permission = syntheticPermission(call, "deny", "Tool input failed schema validation");
      const toolResult = errorResult(call, validation.error);
      await this.recordToolOutcome(session, run, turnTrace, call, validation.toolName, toolResult, permission, evidenceRecords, toolResults);
      return undefined;
    }
    if (allowedTools !== undefined && !allowedTools.has(validation.tool.name)) {
      const permission = syntheticPermission(call, "deny", `Tool '${validation.tool.name}' is not allowed by tool allowlist`);
      const toolResult = errorResult(call, permission.reason);
      await this.recordToolOutcome(session, run, turnTrace, call, validation.tool.name, toolResult, permission, evidenceRecords, toolResults);
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
      turnId: turnTrace.id,
      callId: call.id,
      toolName: call.name,
      action: decision.action,
      reason: decision.reason,
      toolCall: JSON.parse(JSON.stringify(call)),
      ...(pendingInputForAsk === undefined ? {} : { pendingInput: pendingInputForAsk }),
      ...(pendingInputForAsk === undefined ? {} : { permissionId: pendingInputForAsk.permissionId, pendingAction: pendingInputForAsk.action }),
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
        action: pendingInput.action,
        ...(pendingInput.command === undefined ? {} : { command: pendingInput.command }),
        ...(pendingInput.path === undefined ? {} : { path: pendingInput.path })
      };
      session.lastEventSeq = permissionEvent.seq;
      setRunPendingInput(run, pendingInput);
      turnTrace.status = "blocked";
      turnTrace.summary = decision.reason;
      await this.updateRun(session, run, "run.updated");
      await this.options.sessions.save(session);
      return { status: "waiting_permission", session, runState: run, evidence: evidenceRecords, pendingInput, pendingPermission: session.pendingPermission };
    }
    if (decision.action === "deny") {
      const toolResult = errorResult(call, decision.reason);
      await this.recordToolOutcome(session, run, turnTrace, call, validation.tool.name, toolResult, decision, evidenceRecords, toolResults);
      const permissionId = stableId("permission", [session.id, call.id]);
      const handoff = buildBlockedHandoff(run, decision.reason, { kind: "permission", permissionId, toolCallId: call.id, action: createPermissionPendingInput(session.id, call, decision.reason).action, reason: decision.reason, ...(typeof call.input.command === "string" ? { command: call.input.command } : {}), ...(typeof call.input.path === "string" ? { path: call.input.path } : {}), options: ["approve", "deny"] });
      run.handoff = handoff;
      setRunStatus(run, "blocked");
      turnTrace.status = "blocked";
      turnTrace.summary = decision.reason;
      await this.updateRun(session, run, "run.updated");
      await this.options.sessions.save(session);
      return { status: "blocked", session, runState: run, handoff, evidence: evidenceRecords, error: decision.reason };
    }

    return this.executeToolCall(session, run, turnTrace, call, validation.tool.name, decision, evidenceRecords, toolResults);
  }

  private async executeToolCall(session: SessionState, run: TaskRunState, turnTrace: TurnTrace, call: ToolCall, toolName: string, decision: PermissionDecision, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<AgentResult | undefined> {
    try {
      const toolResult = await this.options.registry.execute(call, executionContext(this.options.cwd, session, this.options.config));
      await this.recordToolOutcome(session, run, turnTrace, call, toolName, { ...toolResult, callId: call.id }, decision, evidenceRecords, toolResults);
      return undefined;
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown tool execution error";
      if (!isBlockingToolGate(message)) throw error;
      const toolResult = errorResult(call, message);
      await this.recordToolOutcome(session, run, turnTrace, call, toolName, toolResult, decision, evidenceRecords, toolResults);
      const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, turnTrace.id, call.id, message]), prompt: message, expectedAnswer: "text" });
      turnTrace.status = "blocked";
      turnTrace.summary = message;
      turnTrace.error = message;
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

  private async recordToolOutcome(session: SessionState, run: TaskRunState, turnTrace: TurnTrace, call: ToolCall, toolName: string, result: ToolResult, permission: PermissionDecision, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<void> {
    await this.options.events.append("tool.completed", session.id, { runId: run.id, turnId: turnTrace.id, callId: call.id, toolName, ok: result.ok, summary: result.summary });
    const draft = this.mapEvidenceDraft(call, toolName, result, permission);
    const evidence = await this.options.evidence.record(session.id, toolName, draft, permission);
    const evidenceEvent = await this.options.events.append("evidence.recorded", session.id, { runId: run.id, turnId: turnTrace.id, evidenceId: evidence.id, toolName, callId: call.id });
    session.lastEventSeq = evidenceEvent.seq;
    session.evidenceIds.push(evidence.id);
    session.transcript.push({ role: "tool", content: result.summary });
    turnTrace.toolCallIds.push(call.id);
    turnTrace.evidenceIds.push(evidence.id);
    run.contextSnapshot.messageRefs.push(evidence.id);
    if (toolName === "git_diff") run.contextSnapshot.decisionRefs.push(`git_diff:${result.ok ? "ok" : "failed"}:${evidence.id}`);
    if (toolName === "shell_exec" && permission.action === "allow") updateShellVerification(run, result, evidence);
    updateObservedChangedFiles(run, this.options.cwd, result, evidence.id);
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

  private async executeApprovedPendingCall(session: SessionState, run: TaskRunState, turnTrace: TurnTrace | undefined, call: ToolCall, reason: string, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<void> {
    const active = turnTrace ?? createTurnTrace({ runId: run.id, index: run.turns.length, maxTurns: this.options.config.runtime.maxTurns });
    if (turnTrace === undefined) run.turns.push(active);
    const result = await this.options.registry.execute(call, executionContext(this.options.cwd, session, this.options.config));
    await this.recordToolOutcome(session, run, active, call, call.name, { ...result, callId: call.id }, syntheticPermission(call, "allow", reason), evidenceRecords, toolResults);
  }

  private visibleCodeAgentTools(allowedByToolAllowlist: Set<string> | undefined): ReturnType<ToolRegistry["list"]> {
    const tools = this.options.registry.list();
    if (allowedByToolAllowlist === undefined) return tools;
    return tools.filter((tool) => allowedByToolAllowlist.has(tool.name));
  }

  private buildDirectHandoff(session: SessionState, run: TaskRunState, finalResponse: string, toolResults: ToolResult[], evidenceRecords: EvidenceRecord[]): AgentHandoff {
    const diffChangedFiles = uniqueStrings(toolResults.flatMap((result) => (isDiffChangedFilesTool(result.toolName) && Array.isArray(result.output?.changedFiles) ? result.output.changedFiles.flatMap((entry) => (typeof entry === "string" ? canonicalChangedFilePath(this.options.cwd, entry) : [])) : [])));
    const mutatingChangedFiles = uniqueStrings(
      toolResults.flatMap((result) => {
        if (!result.ok || (result.toolName !== "write_file" && result.toolName !== "edit_file")) return [];
        const path = typeof result.output?.path === "string" ? result.output.path : undefined;
        return (path === undefined ? result.references : [path]).flatMap((entry) => canonicalChangedFilePath(this.options.cwd, entry));
      })
    );
    const changedFiles = diffChangedFiles.length > 0 ? diffChangedFiles : mutatingChangedFiles;
    const verification = run.verification;
    const hasFailedVerification = verification.some((entry) => entry.status === "failed");
    return finalizeAgentHandoff(run, {
      id: stableId("handoff", [run.id, "direct", String(run.turns.length), finalResponse]),
      status: hasFailedVerification ? "failed" : "completed",
      summary: finalResponse,
      changedFiles,
      verification,
      risks: hasFailedVerification ? verification.filter((entry) => entry.status === "failed").map((entry) => entry.summary) : [],
      blockers: [],
      requiredDecisions: [],
      traceRefs: run.turns.map((turn) => turn.id),
      evidenceRefs: uniqueStrings([...session.evidenceIds, ...evidenceRecords.map((record) => record.id)])
    });
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
        run.agentContext = routed.context;
        run.contextSnapshot.decisionRefs.push(`command:${routed.command.name}:${routed.command.hash}`);
        await this.options.events.append("command.routed", session.id, { runId: run.id, command: routed.command.name, args: routed.args, context: JSON.parse(JSON.stringify(routed.context)) });
      }
      return undefined;
    } catch (error) {
      const message = error instanceof Error ? error.message : "runtime context source loading failed";
      const pendingInput = createQuestionPendingInput({ questionId: stableId("question", [run.id, "context_sources", message]), prompt: message, expectedAnswer: "text" });
      setRunPendingInput(run, pendingInput);
      run.handoff = buildBlockedHandoff(run, message, pendingInput);
      await this.updateRun(session, run, "run.created");
      return { status: "blocked", session, runState: run, handoff: run.handoff, evidence: [], pendingInput, error: message };
    }
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
    setRunStatus(run, "failed", { type: "failed", failedTurnId: run.turns.at(-1)?.id ?? "recovery_gate" });
    session.status = "failed";
    session.runState = run;
    delete session.pendingInput;
    delete session.pendingPermission;
    delete session.pendingToolCall;
    await this.options.events.append("recovery.failed", session.id, { runId: run.id, reason: summary, handoff: JSON.parse(JSON.stringify(run.handoff)) });
    await this.options.sessions.save(session);
  }

  private async updateRun(session: SessionState, run: TaskRunState, type: "run.created" | "run.updated", extra: Record<string, unknown> = {}): Promise<void> {
    session.runState = run;
    session.status = run.status;
    const event = await this.options.events.append(type, session.id, { ...jsonPayload(extra), runId: run.id, runState: JSON.parse(JSON.stringify(run)) });
    session.lastEventSeq = event.seq;
    await this.options.sessions.save(session);
  }
}

function uniqueStrings(values: string[]): string[] {
  return [...new Set(values)];
}

function canonicalChangedFilePath(cwd: string, path: string): string[] {
  const root = resolve(cwd);
  const absolute = isAbsolute(path) ? resolve(path) : resolve(root, path);
  const changedFile = relative(root, absolute).replaceAll("\\", "/");
  if (changedFile === "" || changedFile.startsWith("../") || changedFile === ".." || isAbsolute(changedFile)) return [];
  return [changedFile];
}

function isDiffChangedFilesTool(toolName: string): boolean {
  return toolName === "diff" || toolName.endsWith("_diff");
}

function updateObservedChangedFiles(run: TaskRunState, cwd: string, result: ToolResult, evidenceId: string): void {
  const changedFiles = observedChangedFiles(cwd, result);
  if (changedFiles.length === 0) return;
  run.changedFiles = uniqueStrings([...run.changedFiles, ...changedFiles]);
  run.changeEvidenceRefs = uniqueStrings([...run.changeEvidenceRefs, evidenceId]);
}

function observedChangedFiles(cwd: string, result: ToolResult): string[] {
  if (isDiffChangedFilesTool(result.toolName) && Array.isArray(result.output?.changedFiles)) {
    return uniqueStrings(result.output.changedFiles.flatMap((entry) => (typeof entry === "string" ? canonicalChangedFilePath(cwd, entry) : [])));
  }
  if (!result.ok || (result.toolName !== "write_file" && result.toolName !== "edit_file")) return [];
  const path = typeof result.output?.path === "string" ? result.output.path : undefined;
  return uniqueStrings((path === undefined ? result.references : [path]).flatMap((entry) => canonicalChangedFilePath(cwd, entry)));
}

function updateShellVerification(run: TaskRunState, result: ToolResult, evidence: EvidenceRecord): void {
  const command = result.references[0] ?? "shell_exec";
  const exitCode = typeof result.output?.exitCode === "number" ? result.output.exitCode : result.ok ? 0 : 1;
  const verification = {
    command,
    status: result.ok ? "passed" : "failed",
    exitCode,
    summary: result.summary,
    evidenceRefs: [evidence.id]
  } satisfies TaskRunState["verification"][number];
  run.verification = [...run.verification.filter((entry) => entry.command !== command), verification];
}

function activeTurn(run: TaskRunState): TurnTrace | undefined {
  return [...run.turns].reverse().find((turn) => turn.status === "running" || turn.status === "blocked");
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
