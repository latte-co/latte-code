import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { AgentEvent } from "../events/event-log.js";
import type { AgentStatus } from "../shared/types.js";
import { stableId } from "../shared/types.js";
import type { FileReadSnapshot } from "../tools/types.js";
import type { ToolCall } from "../tools/types.js";
import { isPendingInput, isTaskRunState, type AgentPhase, type PendingInput, type TaskRunState } from "../core/contracts.js";

export interface TranscriptEntry {
  role: "user" | "assistant" | "tool";
  content: string;
}

export interface PendingPermissionState {
  callId: string;
  toolName: string;
  reason: string;
  permissionId?: string;
  phase?: AgentPhase;
  action?: Extract<PendingInput, { kind: "permission" }>["action"];
  command?: string;
  path?: string;
}

export interface SessionState {
  id: string;
  status: AgentStatus | "running";
  transcript: TranscriptEntry[];
  evidenceIds: string[];
  lastEventSeq: number;
  pendingPermission?: PendingPermissionState;
  pendingInput?: PendingInput;
  pendingToolCall?: ToolCall;
  fileSnapshots?: Record<string, FileReadSnapshot>;
  runState?: TaskRunState;
  finalResponse?: string;
}

export interface SessionStore {
  create(id?: string): Promise<SessionState>;
  save(state: SessionState): Promise<void>;
  get(id: string): Promise<SessionState | undefined>;
  list?(): Promise<SessionState[]>;
}

export class InMemorySessionStore implements SessionStore {
  private readonly sessions = new Map<string, SessionState>();

  async create(id = stableId("session", [new Date().toISOString(), Math.random().toString()])): Promise<SessionState> {
    const state: SessionState = { id, status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 };
    this.sessions.set(id, state);
    return { ...state, transcript: [...state.transcript], evidenceIds: [...state.evidenceIds] };
  }

  async save(state: SessionState): Promise<void> {
    this.sessions.set(state.id, copySession(state));
  }

  async get(id: string): Promise<SessionState | undefined> {
    const state = this.sessions.get(id);
    return state === undefined ? undefined : copySession(state);
  }

  async list(): Promise<SessionState[]> {
    return [...this.sessions.values()].map(copySession);
  }

}

export class FileSessionStore implements SessionStore {
  constructor(private readonly directory: string) {}

  async create(id = stableId("session", [new Date().toISOString(), Math.random().toString()])): Promise<SessionState> {
    const state: SessionState = { id, status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 };
    await this.save(state);
    return copySession(state);
  }

  async save(state: SessionState): Promise<void> {
    await mkdir(this.directory, { recursive: true });
    await writeFile(join(this.directory, `${state.id}.json`), JSON.stringify(state, null, 2), "utf8");
  }

  async get(id: string): Promise<SessionState | undefined> {
    try {
      return JSON.parse(await readFile(join(this.directory, `${id}.json`), "utf8")) as SessionState;
    } catch {
      return undefined;
    }
  }

  async list(): Promise<SessionState[]> {
    const files = await readdir(this.directory).catch(() => []);
    const sessions = await Promise.all(files.filter((file) => file.endsWith(".json")).map((file) => readFile(join(this.directory, file), "utf8").then((content) => JSON.parse(content) as SessionState).catch(() => undefined)));
    return sessions.filter((session): session is SessionState => session !== undefined).map(copySession);
  }
}

export function recoverSessionFromEvents(sessionId: string, events: AgentEvent[]): SessionState {
  return recoverSessionFromSnapshotAndEvents({ id: sessionId, status: "running", transcript: [], evidenceIds: [], lastEventSeq: 0 }, events);
}

export function recoverSessionFromSnapshotAndEvents(snapshot: SessionState, events: AgentEvent[]): SessionState {
  const state = copySession(snapshot);
  for (const event of events.filter((entry) => entry.sessionId === state.id && entry.seq > snapshot.lastEventSeq).sort((left, right) => left.seq - right.seq)) {
    applyEvent(state, event);
  }
  return state;
}

function applyEvent(state: SessionState, event: AgentEvent): void {
  state.lastEventSeq = event.seq;
  if (event.type === "user.input" && typeof event.payload.input === "string") {
    state.status = "running";
    delete state.pendingPermission;
    delete state.pendingInput;
    delete state.finalResponse;
    pushTranscript(state, { role: "user", content: event.payload.input });
  }
  if (event.type === "run.created" && isTaskRunState(event.payload.runState)) {
    state.runState = event.payload.runState;
    state.status = event.payload.runState.status;
  }
  if (event.type === "run.updated" && isTaskRunState(event.payload.runState)) {
    state.runState = event.payload.runState;
    state.status = event.payload.runState.status;
    if (event.payload.runState.pendingInput !== undefined) state.pendingInput = event.payload.runState.pendingInput;
    else delete state.pendingInput;
  }
  if (event.type === "model.responded" && typeof event.payload.content === "string") {
    pushTranscript(state, { role: "assistant", content: event.payload.content });
  }
  if (event.type === "tool.completed" && typeof event.payload.summary === "string") {
    pushTranscript(state, { role: "tool", content: event.payload.summary });
  }
  if (event.type === "evidence.recorded" && typeof event.payload.evidenceId === "string" && !state.evidenceIds.includes(event.payload.evidenceId)) {
    state.evidenceIds.push(event.payload.evidenceId);
  }
  if (event.type === "permission.decided" && event.payload.action === "ask" && typeof event.payload.callId === "string" && typeof event.payload.toolName === "string" && typeof event.payload.reason === "string") {
    state.status = "waiting_permission";
    const pendingPermission: PendingPermissionState = {
      callId: event.payload.callId,
      toolName: event.payload.toolName,
      reason: event.payload.reason,
      ...(typeof event.payload.permissionId === "string" ? { permissionId: event.payload.permissionId } : {}),
      ...(isAgentPhasePayload(event.payload.phase) ? { phase: event.payload.phase } : {}),
      ...(isPermissionActionPayload(event.payload.pendingAction) ? { action: event.payload.pendingAction } : {}),
      ...(typeof event.payload.command === "string" ? { command: event.payload.command } : {}),
      ...(typeof event.payload.path === "string" ? { path: event.payload.path } : {})
    };
    state.pendingPermission = pendingPermission;
    if (isToolCallPayload(event.payload.toolCall)) {
      state.pendingToolCall = event.payload.toolCall;
    }
    if (isPendingInput(event.payload.pendingInput)) {
      state.pendingInput = event.payload.pendingInput;
    }
  }
  if (event.type === "permission.decided" && event.payload.action === "deny") {
    state.status = "denied";
    delete state.pendingPermission;
    delete state.pendingInput;
    delete state.pendingToolCall;
  }
  if (event.type === "loop.completed" && typeof event.payload.finalResponse === "string") {
    state.status = "completed";
    state.finalResponse = event.payload.finalResponse;
    delete state.pendingPermission;
    delete state.pendingInput;
    delete state.pendingToolCall;
  }
  if (event.type === "loop.failed") {
    state.status = "failed";
    delete state.pendingPermission;
    delete state.pendingInput;
    delete state.pendingToolCall;
  }
}

function pushTranscript(state: SessionState, entry: TranscriptEntry): void {
  const last = state.transcript.at(-1);
  if (last?.role === entry.role && last.content === entry.content) return;
  state.transcript.push(entry);
}

function isAgentPhasePayload(value: unknown): value is AgentPhase {
  return value === "intake" || value === "understand" || value === "plan" || value === "edit" || value === "verify" || value === "handoff";
}

function isPermissionActionPayload(value: unknown): value is PendingPermissionState["action"] {
  return value === "write_file" || value === "edit_file" || value === "shell_exec" || value === "mcp_call" || value === "external_path";
}

function copySession(state: SessionState): SessionState {
  return {
    id: state.id,
    status: state.status,
    transcript: state.transcript.map((entry) => ({ ...entry })),
    evidenceIds: [...state.evidenceIds],
    lastEventSeq: state.lastEventSeq,
    ...(state.pendingPermission === undefined ? {} : { pendingPermission: { ...state.pendingPermission } }),
    ...(state.pendingInput === undefined ? {} : { pendingInput: copyPendingInput(state.pendingInput) }),
    ...(state.pendingToolCall === undefined ? {} : { pendingToolCall: { id: state.pendingToolCall.id, name: state.pendingToolCall.name, input: { ...state.pendingToolCall.input } } }),
    ...(state.fileSnapshots === undefined ? {} : { fileSnapshots: Object.fromEntries(Object.entries(state.fileSnapshots).map(([path, snapshot]) => [path, { ...snapshot }])) }),
    ...(state.runState === undefined ? {} : { runState: JSON.parse(JSON.stringify(state.runState)) as TaskRunState }),
    ...(state.finalResponse === undefined ? {} : { finalResponse: state.finalResponse })
  };
}

function copyPendingInput(input: PendingInput): PendingInput {
  if (input.kind === "permission") {
    return {
      kind: "permission",
      permissionId: input.permissionId,
      toolCallId: input.toolCallId,
      phase: input.phase,
      action: input.action,
      reason: input.reason,
      ...(input.command === undefined ? {} : { command: input.command }),
      ...(input.path === undefined ? {} : { path: input.path }),
      options: ["approve", "deny"]
    };
  }
  return {
    kind: "question",
    questionId: input.questionId,
    phase: input.phase,
    prompt: input.prompt,
    expectedAnswer: input.expectedAnswer,
    ...(input.schemaName === undefined ? {} : { schemaName: input.schemaName })
  };
}

function isToolCallPayload(value: unknown): value is ToolCall {
  return typeof value === "object" && value !== null && !Array.isArray(value) && typeof (value as { id?: unknown }).id === "string" && typeof (value as { name?: unknown }).name === "string" && typeof (value as { input?: unknown }).input === "object" && (value as { input?: unknown }).input !== null && !Array.isArray((value as { input?: unknown }).input);
}
