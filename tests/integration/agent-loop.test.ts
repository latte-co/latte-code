import { mkdtemp, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { AgentLoop } from "../../src/core/agent-loop.js";
import { InMemoryEventLog } from "../../src/events/event-log.js";
import { InMemoryEvidenceStore } from "../../src/evidence/store.js";
import { AgentNodeExecutor, concernFor, summarizeResult } from "../../src/graph-ready/node-executor.js";
import { FakeModelClient } from "../../src/model/fake.js";
import { PermissionPolicy } from "../../src/permissions/policy.js";
import { InMemorySessionStore, recoverSessionFromEvents } from "../../src/session/session.js";
import { createDefaultRegistry } from "../../src/runtime/create-agent.js";
import { ToolRegistry } from "../../src/tools/registry.js";

function createLoop(dir: string, model: FakeModelClient, overrides: Record<string, unknown> = {}): { loop: AgentLoop; events: InMemoryEventLog; sessions: InMemorySessionStore } {
  const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" }, ...overrides });
  const events = new InMemoryEventLog();
  const sessions = new InMemorySessionStore();
  const loop = new AgentLoop({
    cwd: dir,
    config,
    model,
    registry: createDefaultRegistry(config),
    permissions: new PermissionPolicy(config.permissions, config.tools.shell),
    sessions,
    events,
    evidence: new InMemoryEvidenceStore()
  });
  return { loop, events, sessions };
}

describe("AgentLoop integration", () => {
  it("runs fake-model tool call through permission, execution, evidence and final response", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-"));
    await writeFile(join(dir, "note.txt"), "hello agent", "utf8");
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "c1", name: "read_file", input: { path: "note.txt" } }] },
      { type: "message", content: "File says hello agent." }
    ]);
    const { loop, events } = createLoop(dir, model);
    const result = await loop.run({ input: "read note" });
    expect(result.status).toBe("completed");
    expect(result.finalResponse).toContain("hello agent");
    expect(result.evidence).toHaveLength(1);
    expect((await events.read(result.session.id)).map((event) => event.type)).toContain("permission.decided");
    expect(recoverSessionFromEvents(result.session.id, await events.read(result.session.id)).status).toBe("completed");
  });

  it("resumes an existing session snapshot by id", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-resume-"));
    const model = new FakeModelClient([{ type: "message", content: "first" }, { type: "message", content: "second" }]);
    const { loop } = createLoop(dir, model);
    const first = await loop.run({ input: "one", sessionId: "fixed" });
    const second = await loop.run({ input: "two", sessionId: first.session.id });
    expect(second.session.transcript.some((entry) => entry.content === "one")).toBe(true);
    expect(second.finalResponse).toBe("second");
  });

  it("replays event log after snapshot cursor when opening an existing session", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-replay-"));
    const model = new FakeModelClient([{ type: "message", content: "after replay" }]);
    const { loop, events, sessions } = createLoop(dir, model);
    await events.append("session.created", "stale", { sessionId: "stale" });
    await events.append("user.input", "stale", { input: "old" });
    await events.append("tool.completed", "stale", { summary: "Read file" });
    await events.append("evidence.recorded", "stale", { evidenceId: "ev1" });
    await sessions.save({ id: "stale", status: "running", transcript: [{ role: "user", content: "old" }, { role: "tool", content: "Read file" }], evidenceIds: ["ev1"], lastEventSeq: 3 });
    const result = await loop.run({ input: "new", sessionId: "stale" });
    expect(result.session.evidenceIds).toEqual(["ev1"]);
    expect(result.session.lastEventSeq).toBeGreaterThan(4);
  });

  it("recovers an existing session from event log when the snapshot is missing", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-log-recover-"));
    const model = new FakeModelClient([{ type: "message", content: "after log recovery" }]);
    const { loop, events } = createLoop(dir, model);
    await events.append("session.created", "log-only", { sessionId: "log-only" });
    await events.append("user.input", "log-only", { input: "old" });
    const result = await loop.run({ input: "new", sessionId: "log-only" });
    expect(result.session.transcript.some((entry) => entry.content === "old")).toBe(true);
    expect(result.finalResponse).toBe("after log recovery");
  });

  it("pauses mutating write for confirmation by default", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-ask-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c2", name: "write_file", input: { path: "out.txt", content: "unsafe" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "write" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingPermission?.toolName).toBe("write_file");
  });

  it("denies dangerous shell commands and does not execute them", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-deny-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c3", name: "shell_exec", input: { command: "rm -rf important" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "delete" });
    expect(result.status).toBe("denied");
    expect(result.error).toContain("delete");
    expect(result.evidence[0]?.permission.action).toBe("deny");
  });

  it("asks for shell commands matching requireApprovalFor even when generic mutating shell is allowed", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-shell-ask-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c3b", name: "shell_exec", input: { command: "curl https://example.com" } }] }]);
    const { loop } = createLoop(dir, model, { permissions: { mutatingTools: "allow" }, tools: { shell: { requireApprovalFor: ["network"] } } });
    const result = await loop.run({ input: "network" });
    expect(result.status).toBe("waiting_permission");
    expect(result.pendingPermission?.reason).toContain("network");
  });

  it("records invalid tool parameters without executing the tool", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-invalid-"));
    const model = new FakeModelClient([
      { type: "tool_calls", toolCalls: [{ id: "c4", name: "read_file", input: { missing: "path" } }] },
      { type: "message", content: "Recovered from invalid params." }
    ]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "bad" });
    expect(result.status).toBe("completed");
    expect(result.evidence[0]?.outputSummary).toContain("Schema validation failed");
  });

  it("wraps agent loop as a graph-ready NodeExecutor", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-node-"));
    const model = new FakeModelClient([{ type: "message", content: "node done" }]);
    const { loop } = createLoop(dir, model);
    const executor = new AgentNodeExecutor(loop);
    const result = await executor.execute({ input: "go", contract: { nodeId: "N2.implement-agent", goal: "implement", allowedTools: ["read_file"], acceptance: ["done"] } });
    expect(result.nodeId).toBe("N2.implement-agent");
    expect(result.status).toBe("completed");
    expect(result.graphUpdate.eventCursor).toBeGreaterThan(0);
  });

  it("NodeExecutor reports pending permission as concern", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-node-wait-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c5", name: "write_file", input: { path: "x", content: "y" } }] }]);
    const { loop } = createLoop(dir, model);
    const result = await new AgentNodeExecutor(loop).execute({ input: "write", contract: { nodeId: "N", goal: "g", allowedTools: ["write_file"], acceptance: [] } });
    expect(result.status).toBe("waiting_permission");
    expect(result.concerns[0]).toContain("Mutating");
  });

  it("NodeExecutor enforces node contract allowedTools", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-node-tools-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [{ id: "c6", name: "write_file", input: { path: "x", content: "y" } }] }]);
    const { loop, events } = createLoop(dir, model);
    const result = await new AgentNodeExecutor(loop).execute({ input: "write", contract: { nodeId: "N", goal: "g", allowedTools: ["read_file"], acceptance: [] } });
    expect(result.status).toBe("denied");
    expect(result.summary).toContain("not allowed by node contract");
    const toolList = model.requests[0]?.tools.map((tool) => tool.name);
    expect(toolList).toEqual(["read_file"]);
    expect((await events.read()).some((event) => event.type === "evidence.recorded")).toBe(true);
  });

  it("NodeExecutor reports failed loop errors as concerns", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-node-fail-"));
    const { loop } = createLoop(dir, new FakeModelClient([new Error("provider down")]));
    const result = await new AgentNodeExecutor(loop).execute({ input: "fail", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    expect(result.status).toBe("failed");
    expect(result.summary).toBe("provider down");
    expect(result.concerns).toEqual(["provider down"]);
    const fallbackResult = { status: "denied" as const, session: { id: "s", status: "denied" as const, transcript: [], evidenceIds: [], lastEventSeq: 0 }, evidence: [] };
    expect(summarizeResult(fallbackResult)).toBe("Agent loop ended without final response.");
    expect(concernFor(fallbackResult)).toBe("Non-completed agent result");
  });

  it("NodeExecutor forwards an optional session id", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-node-session-"));
    const model = new FakeModelClient([{ type: "message", content: "first" }, { type: "message", content: "second" }]);
    const { loop } = createLoop(dir, model);
    const executor = new AgentNodeExecutor(loop);
    await executor.execute({ input: "one", sessionId: "node-session", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    const result = await executor.execute({ input: "two", sessionId: "node-session", contract: { nodeId: "N", goal: "g", allowedTools: [], acceptance: [] } });
    expect(result.summary).toBe("second");
  });

  it("fails safely when the model provider throws", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-fail-"));
    const model = new FakeModelClient([new Error("provider down")]);
    const { loop } = createLoop(dir, model);
    const result = await loop.run({ input: "fail" });
    expect(result.status).toBe("failed");
    expect(result.error).toBe("provider down");
    expect(model.calls).toBe(1);
  });

  it("normalizes non-Error provider failures", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-string-fail-"));
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const model = { async generate() { throw "string failure"; } };
    const loop = new AgentLoop({ cwd: dir, config, model, registry: createDefaultRegistry(config), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore() });
    expect((await loop.run({ input: "fail" })).error).toBe("Unknown agent loop error");
  });

  it("uses default fake model response when script is exhausted", async () => {
    const model = new FakeModelClient([]);
    await expect(model.generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "message", content: "No scripted response." });
    expect(model.calls).toBe(1);
  });

  it("fails safely when maxTurns is exceeded", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-loop-max-"));
    const model = new FakeModelClient([{ type: "tool_calls", toolCalls: [] }, { type: "tool_calls", toolCalls: [] }]);
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const loop = new AgentLoop({ cwd: dir, config, model, registry: new ToolRegistry(), permissions: new PermissionPolicy(config.permissions, config.tools.shell), sessions: new InMemorySessionStore(), events: new InMemoryEventLog(), evidence: new InMemoryEvidenceStore(), maxTurns: 1 });
    expect((await loop.run({ input: "loop" })).error).toBe("Agent loop exceeded maxTurns");
  });
});
