import type { EventLog } from "../events/event-log.js";
import type { EvidenceStore } from "../evidence/store.js";
import { mapToolEvidence } from "../evidence/store.js";
import type { EvidenceRecord } from "../evidence/types.js";
import type { ModelClient, ModelMessage } from "../model/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import { PermissionPolicy } from "../permissions/policy.js";
import type { SessionState, SessionStore } from "../session/session.js";
import { recoverSessionFromSnapshotAndEvents } from "../session/session.js";
import type { ToolCall, ToolResult } from "../tools/types.js";
import { ToolRegistry } from "../tools/registry.js";
import { SchemaValidationError } from "../tools/schema.js";
import type { FluxcodeConfig } from "../config/types.js";

export interface AgentLoopOptions {
  cwd: string;
  config: FluxcodeConfig;
  model: ModelClient;
  registry: ToolRegistry;
  permissions: PermissionPolicy;
  sessions: SessionStore;
  events: EventLog;
  evidence: EvidenceStore;
  maxTurns?: number;
}

export interface RunAgentInput {
  input: string;
  sessionId?: string;
  allowedTools?: string[];
}

export interface AgentResult {
  status: "completed" | "waiting_permission" | "denied" | "failed";
  session: SessionState;
  finalResponse?: string;
  evidence: EvidenceRecord[];
  pendingPermission?: SessionState["pendingPermission"];
  error?: string;
}

export class AgentLoop {
  constructor(private readonly options: AgentLoopOptions) {}

  async run(input: RunAgentInput): Promise<AgentResult> {
    const session = await this.openSession(input.sessionId);
    const allowedTools = input.allowedTools === undefined ? undefined : new Set(input.allowedTools);
    const visibleTools = allowedTools === undefined ? this.options.registry.list() : this.options.registry.list().filter((tool) => allowedTools.has(tool.name));
    const evidenceRecords: EvidenceRecord[] = [];
    const toolResults: ToolResult[] = [];
    await this.options.events.append("user.input", session.id, { input: input.input });
    session.transcript.push({ role: "user", content: input.input });

    try {
      for (let turn = 0; turn < (this.options.maxTurns ?? 8); turn += 1) {
        await this.options.events.append("model.requested", session.id, { turn });
        const modelTurn = await this.options.model.generate({ messages: toMessages(session), tools: visibleTools, toolResults });
        await this.options.events.append("model.responded", session.id, modelTurn.type === "message" ? { type: modelTurn.type, content: modelTurn.content } : { type: modelTurn.type, toolCallCount: modelTurn.toolCalls.length });

        if (modelTurn.type === "message") {
          session.status = "completed";
          session.finalResponse = modelTurn.content;
          session.transcript.push({ role: "assistant", content: modelTurn.content });
          const completed = await this.options.events.append("loop.completed", session.id, { finalResponse: modelTurn.content });
          session.lastEventSeq = completed.seq;
          await this.options.sessions.save(session);
          return { status: "completed", session, finalResponse: modelTurn.content, evidence: evidenceRecords };
        }

        for (const call of modelTurn.toolCalls) {
          const handled = await this.handleToolCall(session, call, allowedTools, evidenceRecords, toolResults);
          if (handled !== undefined) return handled;
        }
      }
      throw new Error("Agent loop exceeded maxTurns");
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown agent loop error";
      session.status = "failed";
      const failed = await this.options.events.append("loop.failed", session.id, { error: message });
      session.lastEventSeq = failed.seq;
      await this.options.sessions.save(session);
      return { status: "failed", session, evidence: evidenceRecords, error: message };
    }
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
        return recovered;
      }
      if (events.length > 0) {
        const recovered = recoverSessionFromSnapshotAndEvents({ id, status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 }, events);
        await this.options.sessions.save(recovered);
        return recovered;
      }
    }
    const created = await this.options.sessions.create(id);
    const event = await this.options.events.append("session.created", created.id, { sessionId: created.id });
    created.lastEventSeq = event.seq;
    return created;
  }

  private async handleToolCall(session: SessionState, call: ToolCall, allowedTools: Set<string> | undefined, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<AgentResult | undefined> {
    await this.options.events.append("tool.requested", session.id, { callId: call.id, toolName: call.name, input: call.input });
    const validation = this.validateCall(call);
    if (validation.kind === "invalid") {
      const permission = syntheticPermission(call, "deny", "Tool input failed schema validation");
      const toolResult = errorResult(call, validation.error);
      await this.recordToolOutcome(session, call, validation.toolName, toolResult, permission, evidenceRecords, toolResults);
      return undefined;
    }

    if (allowedTools !== undefined && !allowedTools.has(validation.tool.name)) {
      const permission = syntheticPermission(call, "deny", `Tool '${validation.tool.name}' is not allowed by node contract`);
      const toolResult = errorResult(call, permission.reason);
      await this.recordToolOutcome(session, call, validation.tool.name, toolResult, permission, evidenceRecords, toolResults);
      session.status = "denied";
      await this.options.sessions.save(session);
      return { status: "denied", session, evidence: evidenceRecords, error: permission.reason };
    }

    const decision = this.options.permissions.decide({
      toolName: validation.tool.name,
      call,
      cwd: this.options.cwd,
      riskLevel: validation.tool.riskLevel,
      mutating: validation.tool.mutating,
      requirement: validation.tool.permission
    });
    const permissionEvent = await this.options.events.append("permission.decided", session.id, { callId: call.id, toolName: call.name, action: decision.action, reason: decision.reason });

    if (decision.action === "ask") {
      session.status = "waiting_permission";
      session.pendingPermission = { callId: call.id, toolName: call.name, reason: decision.reason };
      session.lastEventSeq = permissionEvent.seq;
      await this.options.sessions.save(session);
      return { status: "waiting_permission", session, evidence: evidenceRecords, pendingPermission: session.pendingPermission };
    }
    if (decision.action === "deny") {
      const toolResult = errorResult(call, decision.reason);
      await this.recordToolOutcome(session, call, validation.tool.name, toolResult, decision, evidenceRecords, toolResults);
      session.status = "denied";
      await this.options.sessions.save(session);
      return { status: "denied", session, evidence: evidenceRecords, error: decision.reason };
    }

    const result = await this.options.registry.execute(call, { cwd: this.options.cwd, sessionId: session.id, maxOutputBytes: this.options.config.tools.maxOutputBytes, shellDefaultTimeoutMs: this.options.config.tools.shell.defaultTimeoutMs });
    const withCallId: ToolResult = { ...result, callId: call.id };
    await this.recordToolOutcome(session, call, validation.tool.name, withCallId, decision, evidenceRecords, toolResults);
    return undefined;
  }

  private validateCall(call: ToolCall): { kind: "valid"; tool: ReturnType<ToolRegistry["get"]> } | { kind: "invalid"; error: string; toolName: string } {
    try {
      return { kind: "valid", tool: this.options.registry.validate(call) };
    } catch (error) {
      const message = error instanceof SchemaValidationError ? error.message : error instanceof Error ? error.message : "Unknown validation error";
      return { kind: "invalid", error: message, toolName: call.name };
    }
  }

  private async recordToolOutcome(session: SessionState, call: ToolCall, toolName: string, result: ToolResult, permission: PermissionDecision, evidenceRecords: EvidenceRecord[], toolResults: ToolResult[]): Promise<void> {
    await this.options.events.append("tool.completed", session.id, { callId: call.id, toolName, ok: result.ok, summary: result.summary });
    const tool = this.options.registry.get(toolName);
    const draft = mapToolEvidence(tool, call, result, permission, this.options.config.evidence.maxEvidenceBytes);
    const evidence = await this.options.evidence.record(session.id, toolName, draft, permission);
    const evidenceEvent = await this.options.events.append("evidence.recorded", session.id, { evidenceId: evidence.id, toolName });
    session.lastEventSeq = evidenceEvent.seq;
    session.evidenceIds.push(evidence.id);
    session.transcript.push({ role: "tool", content: result.summary });
    evidenceRecords.push(evidence);
    toolResults.push(result);
    await this.options.sessions.save(session);
  }
}

function toMessages(session: SessionState): ModelMessage[] {
  return [
    { role: "system", content: "You are Fluxcode, a local-first code agent. Use tools only through declared contracts." },
    ...session.transcript.map((entry): ModelMessage => ({ role: entry.role, content: entry.content }))
  ];
}

function errorResult(call: ToolCall, error: string): ToolResult {
  return { callId: call.id, toolName: call.name, ok: false, error, summary: `Tool ${call.name} failed: ${error}`, references: [], truncated: false };
}

function syntheticPermission(call: ToolCall, action: PermissionDecision["action"], reason: string): PermissionDecision {
  return { action, reason, requirement: { reason }, metadata: { toolName: call.name, riskLevel: "high", mutating: false, sensitivePath: false } };
}
