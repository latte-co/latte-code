import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { AgentEvent } from "../events/event-log.js";
import type { AgentStatus } from "../shared/types.js";
import { stableId } from "../shared/types.js";

export interface TranscriptEntry {
  role: "user" | "assistant" | "tool";
  content: string;
}

export interface PendingPermissionState {
  callId: string;
  toolName: string;
  reason: string;
}

export interface SessionState {
  id: string;
  status: AgentStatus | "running";
  transcript: TranscriptEntry[];
  evidenceIds: string[];
  lastEventSeq: number;
  pendingPermission?: PendingPermissionState;
  finalResponse?: string;
}

export interface SessionStore {
  create(id?: string): Promise<SessionState>;
  save(state: SessionState): Promise<void>;
  get(id: string): Promise<SessionState | undefined>;
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
    delete state.finalResponse;
    pushTranscript(state, { role: "user", content: event.payload.input });
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
    state.pendingPermission = { callId: event.payload.callId, toolName: event.payload.toolName, reason: event.payload.reason };
  }
  if (event.type === "permission.decided" && event.payload.action === "deny") {
    state.status = "denied";
    delete state.pendingPermission;
  }
  if (event.type === "loop.completed" && typeof event.payload.finalResponse === "string") {
    state.status = "completed";
    state.finalResponse = event.payload.finalResponse;
    delete state.pendingPermission;
  }
  if (event.type === "loop.failed") {
    state.status = "failed";
    delete state.pendingPermission;
  }
}

function pushTranscript(state: SessionState, entry: TranscriptEntry): void {
  const last = state.transcript.at(-1);
  if (last?.role === entry.role && last.content === entry.content) return;
  state.transcript.push(entry);
}

function copySession(state: SessionState): SessionState {
  return {
    id: state.id,
    status: state.status,
    transcript: state.transcript.map((entry) => ({ ...entry })),
    evidenceIds: [...state.evidenceIds],
    lastEventSeq: state.lastEventSeq,
    ...(state.pendingPermission === undefined ? {} : { pendingPermission: { ...state.pendingPermission } }),
    ...(state.finalResponse === undefined ? {} : { finalResponse: state.finalResponse })
  };
}
