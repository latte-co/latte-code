import { mkdtemp, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { listConfiguredMcpTools, createMcpToolDefinitions } from "../../src/mcp/bridge.js";
import { createHeadlessRunEnvelopeFromAgentResult, createHeadlessRunEnvelopeFromTaskRunState, createPermissionPendingInput, createQuestionPendingInput, exitCodeForTaskRunStatus, isAgentHandoff, isHeadlessRunEnvelope, isPendingInput, isResumeInput, isTaskRunState } from "../../src/core/contracts.js";
import { buildBlockedHandoff, buildFailedHandoff, copyTaskRunState, createTaskRunState, createTurnTrace, finalizeAgentHandoff, setRunPendingInput, setRunStatus } from "../../src/core/run-state.js";
import { InMemorySessionStore, FileSessionStore, recoverSessionFromEvents } from "../../src/session/session.js";
import { AgentLoop } from "../../src/core/agent-loop.js";
import { FakeModelClient } from "../../src/model/fake.js";
import { createModelClient } from "../../src/model/provider.js";
import { ToolRegistry } from "../../src/tools/registry.js";
import { InMemoryEventLog } from "../../src/events/event-log.js";
import { InMemoryEvidenceStore } from "../../src/evidence/store.js";
import { PermissionPolicy } from "../../src/permissions/policy.js";
import { createAgentLoop } from "../../src/runtime/create-agent.js";

describe("coverage edge contracts for release path", () => {
  it("covers MCP compatibility defaults and explicit tool metadata", async () => {
    expect(listConfiguredMcpTools(DEFAULT_CONFIG)).toEqual([]);
    expect(createMcpToolDefinitions(DEFAULT_CONFIG)).toEqual([]);
    const config = mergeConfig(DEFAULT_CONFIG, {
      mcp: {
        enabled: true,
        requireExplicitEnable: true,
        routeThroughPermission: true,
        servers: {
          off: { enabled: false, tools: { skip: {} } },
          "on-server": { enabled: true, tools: { "read-value": { description: "Read value", inputSchema: { type: "object", properties: { q: { type: "string" } } }, mutating: false, riskLevel: "low" } } }
        }
      }
    });
    expect(listConfiguredMcpTools(config)).toEqual([{ server: "on-server", tool: "read-value", toolName: "mcp_on_server_read_value", enabled: true }]);
    const [tool] = createMcpToolDefinitions(config, { async callTool() { return { summary: "ok", output: { ok: true }, references: ["ref"], truncated: true }; } });
    if (tool === undefined) throw new Error("expected mcp tool");
    expect(tool).toMatchObject({ name: "mcp_on_server_read_value", mutating: false, riskLevel: "low" });
    await expect(tool.execute({ q: "x" }, { cwd: process.cwd(), sessionId: "s", maxOutputBytes: 10 })).resolves.toMatchObject({ output: { ok: true }, references: ["ref"], truncated: true });
    const [missingClient] = createMcpToolDefinitions(config);
    await expect(missingClient?.execute({}, { cwd: process.cwd(), sessionId: "s", maxOutputBytes: 10 })).rejects.toThrow("mcp_gate");
    const permissive = mergeConfig(config, { mcp: { requireExplicitEnable: false, servers: { off: { enabled: false, tools: { skip: { inputSchema: { bad: Symbol("x") } } } } } } });
    expect(createMcpToolDefinitions(permissive).map((entry) => entry.name)).toContain("mcp_off_skip");
  });

  it("covers contract guards and handoff finalization fallbacks", () => {
    const state = copyTaskRunState({ id: "run", sessionId: "s", status: "running", changedFiles: ["a.ts"], changeEvidenceRefs: ["ev-change"], verification: [{ command: "npm test", status: "passed", summary: "ok", evidenceRefs: ["ev-v"] }], turns: [{ id: "turn", status: "done", promptId: "p", promptVersion: "1", summary: "done", toolCallIds: [], evidenceIds: ["ev-turn"], turnBudget: { maxTurns: 1, usedTurns: 1 } }], contextSnapshot: { taskInput: "task", messageRefs: [], decisionRefs: [], compactedSummary: "", pinnedConstraints: [] } });
    const finalized = finalizeAgentHandoff(state, { id: "h", status: "completed", summary: "done", changedFiles: [], verification: [], risks: [], blockers: [], requiredDecisions: [], traceRefs: [], evidenceRefs: [] });
    expect(finalized).toMatchObject({ changedFiles: ["a.ts"], risks: [], traceRefs: ["turn"], evidenceRefs: ["ev-turn", "ev-change", "ev-v"] });
    const pending = createPermissionPendingInput("s", { id: "c", name: "mcp_server_tool", input: {} }, "approve mcp");
    setRunPendingInput(state, pending);
    expect(finalizeAgentHandoff(state, buildBlockedHandoff(state, "blocked", pending)).requiredDecisions).toEqual([{ kind: "permission", id: pending.permissionId, reason: "approve mcp" }]);
    expect(buildFailedHandoff(state, "failed").status).toBe("failed");
    expect(isAgentHandoff({ ...finalized, verification: [{}] })).toBe(false);
    expect(isHeadlessRunEnvelope({ runId: "r", sessionId: "s", status: "completed", handoff: finalized })).toBe(true);
    expect(isPendingInput({ kind: "question", questionId: "q", prompt: "p", expectedAnswer: "text" })).toBe(true);
    expect(isResumeInput({ kind: "question", questionId: "q", answerJson: () => undefined })).toBe(false);
    expect(isTaskRunState({ ...state, contextSnapshot: { taskInput: "x" } })).toBe(false);
  });

  it("covers file session stores plus event recovery branches", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-store-edges-"));
    const memory = new InMemorySessionStore();
    await memory.create("mem");
    expect(await memory.list()).toHaveLength(1);
    const files = new FileSessionStore(join(dir, "sessions"));
    await files.create("file");
    await writeFile(join(dir, "sessions", "bad.json"), "not json", "utf8");
    expect(await files.list()).toHaveLength(1);
    await expect(files.get("missing")).resolves.toBeUndefined();

    const recovered = recoverSessionFromEvents("s", [
      { seq: 1, type: "session.created", sessionId: "s", timestamp: "t", payload: { sessionId: "s" } },
      { seq: 2, type: "permission.decided", sessionId: "s", timestamp: "t", payload: { action: "ask", callId: "c", toolName: "shell_exec", reason: "ask", permissionId: "p", pendingAction: "shell_exec", command: "npm test", toolCall: { id: "c", name: "shell_exec", input: { command: "npm test" } }, pendingInput: { kind: "permission", permissionId: "p", toolCallId: "c", action: "shell_exec", reason: "ask", command: "npm test", options: ["approve", "deny"] } } },
      { seq: 3, type: "loop.completed", sessionId: "s", timestamp: "t", payload: { finalResponse: "done" } }
    ]);
    expect(recovered).toMatchObject({ status: "completed", finalResponse: "done" });
  });

  it("covers MCP defaults, model factory branches, and prompt lookup failures", async () => {
    const mcpConfig = mergeConfig(DEFAULT_CONFIG, { mcp: { enabled: true, requireExplicitEnable: false, servers: { empty: { enabled: true }, plain: { enabled: true, tools: { lookup: {} } } } } });
    expect(listConfiguredMcpTools(mcpConfig).map((tool) => tool.toolName)).toEqual(["mcp_plain_lookup"]);
    const [tool] = createMcpToolDefinitions(mcpConfig, { async callTool() { return { summary: "no output" }; } });
    if (tool === undefined) throw new Error("expected mcp tool");
    await expect(tool.execute({}, { cwd: process.cwd(), sessionId: "s", maxOutputBytes: 10 })).resolves.toMatchObject({ summary: "no output", references: ["mcp://plain/lookup"], truncated: false });

    expect(() => createModelClient({ config: DEFAULT_CONFIG })).toThrow("no fakeScript was supplied");
    await expect(createModelClient({ config: DEFAULT_CONFIG, fakeScript: [{ type: "message", content: "scripted" }] }).generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "message", content: "scripted" });
    const missingProviderConfig = mergeConfig(DEFAULT_CONFIG, undefined);
    missingProviderConfig.models.default = "missing";
    expect(() => createModelClient({ config: missingProviderConfig })).toThrow("not defined");
    const unsupportedProviderConfig = mergeConfig(DEFAULT_CONFIG, undefined);
    Object.assign(unsupportedProviderConfig.models.providers.fake ?? {}, { type: "unsupported" });
    expect(() => createModelClient({ config: unsupportedProviderConfig })).toThrow("unsupported provider type");
    const futureProviderConfig = mergeConfig(DEFAULT_CONFIG, undefined);
    Object.assign(futureProviderConfig.models.providers.fake ?? {}, { type: "anthropic" });
    expect(() => createModelClient({ config: futureProviderConfig })).toThrow("reserved for a future provider adapter");
    const apiModeProviderConfig = mergeConfig(DEFAULT_CONFIG, undefined);
    Object.assign(apiModeProviderConfig.models.providers.fake ?? {}, { apiMode: "openai-compatible-chat" });
    expect(() => createModelClient({ config: apiModeProviderConfig })).toThrow(/apiMode.*type/);
    const openAiConfig = mergeConfig(DEFAULT_CONFIG, { models: { default: "primary", providers: { primary: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" } } } });
    const openAi = createModelClient({ config: openAiConfig, env: { MODEL_KEY: "secret" }, fetch: async () => new Response(JSON.stringify({ choices: [{ message: { content: "ok" } }] }), { status: 200 }) });
    await expect(openAi.generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "message", content: "ok" });
    const previous = process.env.MODEL_KEY;
    process.env.MODEL_KEY = "secret";
    try {
      expect(() => createModelClient({ config: openAiConfig })).not.toThrow();
    } finally {
      if (previous === undefined) delete process.env.MODEL_KEY;
      else process.env.MODEL_KEY = previous;
    }
    expect(createAgentLoop({ cwd: process.cwd(), config: openAiConfig, env: { MODEL_KEY: "secret" }, fetch: async () => new Response(JSON.stringify({ choices: [{ message: { content: "ok" } }] }), { status: 200 }) })).toBeInstanceOf(AgentLoop);
  });

  it("covers contract guard false branches and current envelope creation", () => {
    expect(exitCodeForTaskRunStatus("completed")).toBe(0);
    expect(exitCodeForTaskRunStatus("waiting_permission")).toBe(20);
    expect(exitCodeForTaskRunStatus("blocked")).toBe(21);
    expect(exitCodeForTaskRunStatus("failed")).toBe(22);
    expect(exitCodeForTaskRunStatus("queued")).toBe(22);

    const permission = createPermissionPendingInput("s", { id: "w", name: "write_file", input: { path: "a.ts" } }, "write");
    expect(permission.action).toBe("write_file");
    expect(createPermissionPendingInput("s", { id: "e", name: "edit_file", input: { path: "a.ts" } }, "edit").action).toBe("edit_file");
    expect(createPermissionPendingInput("s", { id: "sh", name: "shell_exec", input: { command: "npm test" } }, "shell").action).toBe("shell_exec");
    expect(createPermissionPendingInput("s", { id: "m", name: "mcp_server_tool", input: {} }, "mcp").action).toBe("mcp_call");
    expect(createPermissionPendingInput("s", { id: "x", name: "external", input: { path: "outside" } }, "external").action).toBe("external_path");
    expect(createPermissionPendingInput("s", { id: "x2", name: "external", input: {} }, "external").action).toBe("mcp_call");

    const question = createQuestionPendingInput({ questionId: "q", prompt: "why", expectedAnswer: "json", schemaName: "Schema" });
    expect(isPendingInput(question)).toBe(true);
    expect(isPendingInput({ ...permission, options: ["approve"] })).toBe(false);
    expect(isPendingInput({ ...permission, command: 1 })).toBe(false);
    expect(isPendingInput({ ...permission, path: 1 })).toBe(false);
    expect(isPendingInput({ ...question, schemaName: 1 })).toBe(false);
    expect(isPendingInput({ ...question, expectedAnswer: "yaml" })).toBe(false);
    expect(isResumeInput({ kind: "permission", permissionId: "p", decision: "approve", reason: "ok" })).toBe(true);
    expect(isResumeInput({ kind: "permission", permissionId: "p", decision: "approve", reason: 1 })).toBe(false);
    expect(isResumeInput({ kind: "question", questionId: "q", answerText: "text" })).toBe(true);
    expect(isResumeInput({ kind: "question", questionId: "q", answerJson: { ok: true } })).toBe(true);
    expect(isResumeInput({ kind: "question", questionId: "q", answerJson: () => undefined })).toBe(false);

    const run = createTaskRunState("s", "task", "r-contract");
    run.pendingInput = question;
    run.handoff = buildBlockedHandoff(run, "blocked", question);
    expect(createHeadlessRunEnvelopeFromTaskRunState(createTaskRunState("s", "fresh", "r-fresh"))).toEqual({ runId: "r-fresh", sessionId: "s", status: "queued" });
    expect(createHeadlessRunEnvelopeFromTaskRunState(run)).toMatchObject({ pendingInput: question, handoff: run.handoff });
    expect(createHeadlessRunEnvelopeFromAgentResult({ status: "waiting_permission", session: { id: "s", status: "waiting_permission", transcript: [], evidenceIds: [], lastEventSeq: 0 }, evidence: [] })).toEqual({ runId: "s", sessionId: "s", status: "waiting_permission" });
    expect(createHeadlessRunEnvelopeFromAgentResult({ status: "running", session: { id: "s", status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 }, evidence: [] })).toEqual({ runId: "s", sessionId: "s", status: "running" });
    expect(createHeadlessRunEnvelopeFromAgentResult({ status: "blocked", session: { id: "s", status: "blocked", transcript: [], evidenceIds: [], lastEventSeq: 4, runState: run }, runState: run, handoff: run.handoff, pendingInput: question, evidence: [] })).toMatchObject({ runId: "r-contract", pendingInput: question, handoff: run.handoff });
    expect(isHeadlessRunEnvelope({ runId: "r", sessionId: "s", status: "running" })).toBe(true);
    expect(isHeadlessRunEnvelope({ runId: "r", sessionId: "s", status: "running", pendingInput: { kind: "bad" } })).toBe(false);
    expect(isAgentHandoff({ ...run.handoff, status: "failed" })).toBe(true);
    expect(isAgentHandoff({ ...run.handoff, status: "blocked" })).toBe(true);
    expect(isAgentHandoff({ ...run.handoff, requiredDecisions: [{ kind: "other", id: "x", reason: "r" }] })).toBe(false);
    expect(isTaskRunState({ ...run, turns: [{ ...createTurnTrace({ runId: "r", index: 0, maxTurns: 1 }), status: "failed", error: "x" }] })).toBe(true);
    expect(isTaskRunState({ ...run, agentContext: { objective: "x" } })).toBe(false);
    expect(isTaskRunState({ ...run, contextSnapshot: { ...run.contextSnapshot, skills: [{ name: "s", path: "p", hash: "h", summary: "i" }], commands: [{ name: "c", path: "p", hash: "h", description: "d" }], mcpTools: [{ server: "s", tool: "t", toolName: "mcp_s_t" }] } })).toBe(true);
    expect(isTaskRunState({ ...run, contextSnapshot: { ...run.contextSnapshot, skills: [{ name: "s" }] } })).toBe(false);
    expect(isTaskRunState({ ...run, contextSnapshot: { ...run.contextSnapshot, agentsMd: { path: "p", hash: "h" } } })).toBe(false);
  });

  it("covers run-state merge and dedupe branches", async () => {
    const run = createTaskRunState("s", "task", "r-state");
    const turn = createTurnTrace({ runId: run.id, index: 0, maxTurns: 1 });
    turn.evidenceIds.push("ev-turn");
    run.turns.push(turn);
    run.agentContext = { objective: "o", scope: [], acceptance: [], nonGoals: [], constraints: [], blockers: [] };
    run.changedFiles = ["a.ts"];
    run.changeEvidenceRefs = ["ev-change"];
    run.verification = [{ command: "npm test", status: "passed", summary: "ok", evidenceRefs: ["ev-v"] }];
    const pending = createQuestionPendingInput({ questionId: "q-state", prompt: "answer", expectedAnswer: "text" });
    setRunPendingInput(run, pending);
    const finalized = finalizeAgentHandoff(run, { id: "h", status: "blocked", summary: "blocked", changedFiles: ["a.ts", "b.ts"], verification: [{ command: "npm test", status: "skipped", summary: "old", evidenceRefs: [] }], risks: ["risk"], blockers: [], requiredDecisions: [{ kind: "question", id: "q-state", reason: "answer" }], traceRefs: [turn.id], evidenceRefs: ["ev-v", "ev-handoff"] });
    expect(finalized.blockers).toEqual(["blocked"]);
    expect(finalized.changedFiles).toEqual(["a.ts", "b.ts"]);
    expect(finalized.evidenceRefs).toEqual(expect.arrayContaining(["ev-change", "ev-v", "ev-handoff"]));
    expect(finalized.requiredDecisions).toHaveLength(1);
    expect(finalized.verification[0]?.summary).toBe("ok");
    setRunStatus(run, "completed", { type: "completed", handoffId: "h" });
    expect(run.resume).toEqual({ type: "completed", handoffId: "h" });
    setRunStatus(run, "running");
    expect(run.resume).toBeUndefined();
  });
});
