import type { PermissionDecision, PermissionRequirement } from "../permissions/types.js";
import type { EvidenceDraft } from "../evidence/types.js";
import type { LightweightSchema } from "./schema.js";
import type { JsonObject } from "../shared/types.js";

export type RiskLevel = "low" | "medium" | "high";

export interface ToolCall {
  id: string;
  name: string;
  input: JsonObject;
}

export interface ToolResult {
  callId: string;
  toolName: string;
  ok: boolean;
  output?: JsonObject;
  error?: string;
  summary: string;
  references: string[];
  truncated: boolean;
}

export interface ToolExecutionContext {
  cwd: string;
  sessionId: string;
  maxOutputBytes: number;
  shellDefaultTimeoutMs?: number;
}

export type EvidenceMapper = (input: JsonObject, result: ToolResult, permission: PermissionDecision) => EvidenceDraft;

export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: LightweightSchema;
  outputSchema: LightweightSchema;
  riskLevel: RiskLevel;
  mutating: boolean;
  permission: PermissionRequirement;
  execute(input: JsonObject, context: ToolExecutionContext): Promise<ToolResult>;
  evidenceMapper?: EvidenceMapper;
}
