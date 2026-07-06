import { mkdtemp } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { InMemoryEventLog, FileEventLog } from "../../src/events/event-log.js";
import { FileEvidenceStore, InMemoryEvidenceStore, mapToolEvidence } from "../../src/evidence/store.js";
import { FileSessionStore, InMemorySessionStore, recoverSessionFromEvents, recoverSessionFromSnapshotAndEvents } from "../../src/session/session.js";
import { stableId, toJsonObject, isJsonValue, truncateText } from "../../src/shared/types.js";
import type { PermissionDecision } from "../../src/permissions/types.js";
import type { ToolDefinition } from "../../src/tools/types.js";
import { createTaskRunState } from "../../src/core/run-state.js";

const permission: PermissionDecision = { action: "allow", reason: "test", requirement: { reason: "test" }, metadata: { toolName: "read_file", riskLevel: "low", mutating: false, sensitivePath: false } };

describe("event log, evidence store and recovery", () => {
  it("records in-memory evidence and recovers session state from event log", async () => {
    const events = new InMemoryEventLog();
    const store = new InMemoryEvidenceStore();
    await events.append("session.created", "s1", { sessionId: "s1" });
    await events.append("user.input", "s1", { input: "read" });
    await events.append("tool.completed", "s1", { summary: "Read file" });
    const evidence = await store.record("s1", "read_file", { inputSummary: "in", outputSummary: "out", references: ["a"], truncated: false }, permission);
    await store.record("other", "read_file", { inputSummary: "in", outputSummary: "out", references: ["b"], truncated: false }, permission);
    await events.append("evidence.recorded", "s1", { evidenceId: evidence.id });
    await events.append("loop.completed", "s1", { finalResponse: "done" });
    const recovered = recoverSessionFromEvents("s1", await events.read("s1"));
    expect(recovered.status).toBe("completed");
    expect(recovered.evidenceIds).toEqual([evidence.id]);
    expect(recovered.lastEventSeq).toBe(5);
    await expect(store.get(evidence.id)).resolves.toEqual(evidence);
    await expect(store.list("s1")).resolves.toHaveLength(1);
  });

  it("replays event log after a stale snapshot cursor without duplicating evidence", async () => {
    const events = new InMemoryEventLog();
    await events.append("session.created", "s1", { sessionId: "s1" });
    await events.append("user.input", "s1", { input: "read" });
    await events.append("tool.completed", "s1", { summary: "Read file" });
    await events.append("evidence.recorded", "s1", { evidenceId: "ev1" });
    const snapshot = { id: "s1", status: "running" as const, transcript: [{ role: "user" as const, content: "read" }, { role: "tool" as const, content: "Read file" }], evidenceIds: ["ev1"], lastEventSeq: 3 };
    const recovered = recoverSessionFromSnapshotAndEvents(snapshot, await events.read("s1"));
    expect(recovered.evidenceIds).toEqual(["ev1"]);
    expect(recovered.lastEventSeq).toBe(4);
    const duplicateTranscript = recoverSessionFromSnapshotAndEvents({ id: "s1", status: "running", transcript: [{ role: "user", content: "read" }], evidenceIds: [], lastEventSeq: 1 }, await events.read("s1"));
    expect(duplicateTranscript.transcript.filter((entry) => entry.content === "read")).toHaveLength(1);
  });

  it("persists file event log, file evidence and session snapshots", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-state-"));
    const events = new FileEventLog(join(dir, "events.jsonl"));
    await events.append("session.created", "s2", { sessionId: "s2" });
    await events.append("permission.decided", "s2", { action: "ask", callId: "c1", toolName: "write_file", reason: "mutating" });
    await events.append("permission.decided", "s4", { action: "ask", callId: "c4", toolName: "shell_exec", reason: "verify", permissionId: "p4", pendingAction: "shell_exec", command: "npm test", path: "package.json", toolCall: { id: "c4", name: "shell_exec", input: { command: "npm test" } }, pendingInput: { kind: "permission", permissionId: "p4", toolCallId: "c4", action: "shell_exec", reason: "verify", command: "npm test", path: "package.json", options: ["approve", "deny"] } });
    expect((await events.read("s2")).length).toBe(2);
    const evidenceStore = new FileEvidenceStore(join(dir, "evidence"));
    const record = await evidenceStore.record("s2", "read_file", { inputSummary: "in", outputSummary: "out", references: ["ref"], truncated: false }, permission);
    expect((await evidenceStore.list("s2"))[0]?.id).toBe(record.id);
    await expect(evidenceStore.get("missing")).resolves.toBeUndefined();
    const sessions = new InMemorySessionStore();
    const session = await sessions.create("s2");
    session.evidenceIds.push(record.id);
    await sessions.save(session);
    expect((await sessions.get("s2"))?.evidenceIds).toEqual([record.id]);
    const fileSessions = new FileSessionStore(join(dir, "sessions"));
    const saved = await fileSessions.create("s3");
    saved.finalResponse = "ok";
    saved.pendingInput = { kind: "question", questionId: "q", prompt: "json", expectedAnswer: "json", schemaName: "AgentTaskContext" };
    await fileSessions.save(saved);
    expect((await fileSessions.get("s3"))?.finalResponse).toBe("ok");
    expect((await fileSessions.get("s3"))?.pendingInput).toMatchObject({ schemaName: "AgentTaskContext" });
    await expect(fileSessions.get("missing")).resolves.toBeUndefined();
    const recoveredAsk = recoverSessionFromEvents("s2", await events.read("s2"));
    expect(recoveredAsk.status).toBe("waiting_permission");
    const recoveredDetailedAsk = recoverSessionFromEvents("s4", await events.read("s4"));
    expect(recoveredDetailedAsk.pendingPermission).toMatchObject({ command: "npm test", path: "package.json" });
    await events.append("loop.failed", "s2", { error: "boom" });
    expect(recoverSessionFromEvents("s2", await events.read("s2")).status).toBe("failed");
    await events.append("permission.decided", "s-deny", { action: "deny", callId: "c2", toolName: "write_file", reason: "blocked" });
    expect(recoverSessionFromEvents("s-deny", await events.read("s-deny")).status).toBe("blocked");
    await expect(new InMemoryEvidenceStore().get("missing")).resolves.toBeUndefined();
  });

  it("recovers run updates from append-only events", async () => {
    const events = new InMemoryEventLog();
    const run = createTaskRunState("s-recover", "recover", "run_recover");
    run.status = "blocked";
    await events.append("run.updated", "s-recover", { runId: run.id, runState: JSON.parse(JSON.stringify(run)) });
    const recovered = recoverSessionFromEvents("s-recover", await events.read("s-recover"));
    expect(recovered.runState?.id).toBe("run_recover");
    expect(recovered.status).toBe("blocked");
  });

  it("maps tool evidence with truncation", () => {
    const tool: ToolDefinition = {
      name: "plain",
      description: "plain",
      inputSchema: { type: "object" },
      outputSchema: { type: "object" },
      riskLevel: "low",
      mutating: false,
      permission: { reason: "plain" },
      async execute() {
        return { callId: "c", toolName: "plain", ok: true, summary: "ok", references: [], truncated: false };
      }
    };
    const draft = mapToolEvidence(tool, { id: "c", name: "plain", input: { path: "a" } }, { callId: "c", toolName: "plain", ok: true, summary: "x".repeat(50), references: ["a"], truncated: false }, permission, 10);
    expect(draft.truncated).toBe(true);
    expect(draft.references).toEqual(["a"]);
    const alreadyTruncated = mapToolEvidence(tool, { id: "c", name: "plain", input: { path: "a" } }, { callId: "c", toolName: "plain", ok: true, summary: "ok", references: [], truncated: true }, permission, 1000);
    expect(alreadyTruncated.truncated).toBe(true);
    const inputTruncated = mapToolEvidence(tool, { id: "c", name: "plain", input: { path: "x".repeat(50) } }, { callId: "c", toolName: "plain", ok: true, summary: "ok", references: [], truncated: false }, permission, 10);
    expect(inputTruncated.truncated).toBe(true);
  });

  it("covers shared JSON helpers and stable ids", () => {
    expect(stableId("x", ["a"])).toBe(stableId("x", ["a"]));
    expect(isJsonValue({ a: [1, "b", true, null] })).toBe(true);
    expect(isJsonValue(() => undefined)).toBe(false);
    expect(toJsonObject({ a: 1, nested: { ok: true } })).toEqual({ a: 1, nested: { ok: true } });
    expect(() => toJsonObject("bad")).toThrow("Expected JSON object");
    expect(truncateText("abcdef", 3).truncated).toBe(true);
    expect(truncateText("abc", 3).truncated).toBe(false);
  });
});
