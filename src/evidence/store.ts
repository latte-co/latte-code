import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { ToolCall, ToolDefinition, ToolResult } from "../tools/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import type { EvidenceDraft, EvidenceRecord } from "./types.js";
import { stableId, truncateText } from "../shared/types.js";

export interface EvidenceStore {
  record(sessionId: string, toolName: string, draft: EvidenceDraft, permission: PermissionDecision): Promise<EvidenceRecord>;
  get(id: string): Promise<EvidenceRecord | undefined>;
  list(sessionId: string): Promise<EvidenceRecord[]>;
}

export class InMemoryEvidenceStore implements EvidenceStore {
  private readonly records = new Map<string, EvidenceRecord>();

  async record(sessionId: string, toolName: string, draft: EvidenceDraft, permission: PermissionDecision): Promise<EvidenceRecord> {
    const timestamp = new Date().toISOString();
    const id = stableId("ev", [sessionId, toolName, draft.inputSummary, draft.outputSummary, timestamp]);
    const record: EvidenceRecord = { id, sessionId, toolName, permission, timestamp, ...draft };
    this.records.set(id, record);
    return record;
  }

  async get(id: string): Promise<EvidenceRecord | undefined> {
    return this.records.get(id);
  }

  async list(sessionId: string): Promise<EvidenceRecord[]> {
    return [...this.records.values()].filter((record) => record.sessionId === sessionId);
  }
}

export class FileEvidenceStore implements EvidenceStore {
  constructor(private readonly directory: string) {}

  async record(sessionId: string, toolName: string, draft: EvidenceDraft, permission: PermissionDecision): Promise<EvidenceRecord> {
    await mkdir(this.directory, { recursive: true });
    const timestamp = new Date().toISOString();
    const id = stableId("ev", [sessionId, toolName, draft.inputSummary, draft.outputSummary, timestamp]);
    const record: EvidenceRecord = { id, sessionId, toolName, permission, timestamp, ...draft };
    await writeFile(join(this.directory, `${id}.json`), JSON.stringify(record, null, 2), "utf8");
    const indexPath = join(this.directory, "index.json");
    const ids = JSON.parse(await readFile(indexPath, "utf8").catch(() => "[]")) as string[];
    await writeFile(indexPath, JSON.stringify([...ids, id], null, 2), "utf8");
    return record;
  }

  async get(id: string): Promise<EvidenceRecord | undefined> {
    try {
      return JSON.parse(await readFile(join(this.directory, `${id}.json`), "utf8")) as EvidenceRecord;
    } catch {
      return undefined;
    }
  }

  async list(sessionId: string): Promise<EvidenceRecord[]> {
    const index = await readFile(join(this.directory, "index.json"), "utf8").catch(() => "[]");
    const ids = JSON.parse(index) as string[];
    const records = await Promise.all(ids.map((id) => this.get(id)));
    return records.filter((record): record is EvidenceRecord => record !== undefined && record.sessionId === sessionId);
  }
}

export function mapToolEvidence(tool: ToolDefinition, call: ToolCall, result: ToolResult, permission: PermissionDecision, maxBytes: number): EvidenceDraft {
  if (tool.evidenceMapper !== undefined) return tool.evidenceMapper(call.input, result, permission);
  const input = truncateText(JSON.stringify(call.input), maxBytes);
  const output = truncateText(result.summary, maxBytes);
  return {
    inputSummary: input.text,
    outputSummary: output.text,
    references: result.references,
    truncated: result.truncated || input.truncated || output.truncated
  };
}
