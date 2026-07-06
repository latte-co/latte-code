import type { PermissionDecision } from "../permissions/types.js";

export interface EvidenceDraft {
  inputSummary: string;
  outputSummary: string;
  references: string[];
  truncated: boolean;
}

export interface EvidenceRecord extends EvidenceDraft {
  id: string;
  sessionId: string;
  toolName: string;
  permission: PermissionDecision;
  timestamp: string;
}
