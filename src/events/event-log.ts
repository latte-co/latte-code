import { appendFile, mkdir, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import type { JsonObject } from "../shared/types.js";

export type EventType =
  | "session.created"
  | "run.created"
  | "run.updated"
  | "phase.started"
  | "phase.completed"
  | "phase.blocked"
  | "agents.snapshot"
  | "skills.loaded"
  | "command.routed"
  | "step.started"
  | "step.completed"
  | "recovery.failed"
  | "resume.received"
  | "user.input"
  | "model.requested"
  | "model.responded"
  | "tool.requested"
  | "permission.decided"
  | "tool.completed"
  | "evidence.recorded"
  | "session.snapshotted"
  | "loop.completed"
  | "loop.failed";

export interface AgentEvent {
  seq: number;
  type: EventType;
  sessionId: string;
  timestamp: string;
  payload: JsonObject;
}

export interface EventLog {
  append(type: EventType, sessionId: string, payload: JsonObject): Promise<AgentEvent>;
  read(sessionId?: string): Promise<AgentEvent[]>;
}

export class InMemoryEventLog implements EventLog {
  private readonly events: AgentEvent[] = [];

  async append(type: EventType, sessionId: string, payload: JsonObject): Promise<AgentEvent> {
    const event: AgentEvent = { seq: this.events.length + 1, type, sessionId, timestamp: new Date().toISOString(), payload };
    this.events.push(event);
    return event;
  }

  async read(sessionId?: string): Promise<AgentEvent[]> {
    return this.events.filter((event) => sessionId === undefined || event.sessionId === sessionId);
  }
}

export class FileEventLog implements EventLog {
  constructor(private readonly path: string) {}

  async append(type: EventType, sessionId: string, payload: JsonObject): Promise<AgentEvent> {
    const events = await this.read();
    const event: AgentEvent = { seq: events.length + 1, type, sessionId, timestamp: new Date().toISOString(), payload };
    await mkdir(dirname(this.path), { recursive: true });
    await appendFile(this.path, `${JSON.stringify(event)}\n`, "utf8");
    return event;
  }

  async read(sessionId?: string): Promise<AgentEvent[]> {
    const raw = await readFile(this.path, "utf8").catch(() => "");
    return raw
      .split("\n")
      .filter((line) => line.trim().length > 0)
      .map((line) => JSON.parse(line) as AgentEvent)
      .filter((event) => sessionId === undefined || event.sessionId === sessionId);
  }
}
