import type { RiskLevel, ToolCall } from "../tools/types.js";

export type PermissionAction = "allow" | "ask" | "deny";

export interface PermissionRequirement {
  reason: string;
  paths?: string[];
  commandCategories?: string[];
}

export interface PermissionRequest {
  toolName: string;
  call: ToolCall;
  cwd: string;
  riskLevel: RiskLevel;
  mutating: boolean;
  requirement: PermissionRequirement;
}

export interface PermissionDecision {
  action: PermissionAction;
  reason: string;
  requirement: PermissionRequirement;
  metadata: {
    toolName: string;
    riskLevel: RiskLevel;
    mutating: boolean;
    sensitivePath: boolean;
    commandCategory?: string;
  };
}
