export type ProviderType = "fake" | "openai-compatible";
export type PermissionMode = "allow" | "ask" | "deny";

export interface ModelProviderConfig {
  type: ProviderType;
  model: string;
  baseUrl?: string;
  apiKeyEnv?: string;
  temperature?: number;
  maxOutputTokens?: number;
}

export interface ModelConfig {
  default: string;
  providers: Record<string, ModelProviderConfig>;
}

export interface PermissionConfig {
  defaultMode: PermissionMode;
  allowReadOnlyTools: boolean;
  mutatingTools: PermissionMode;
  highRiskTools: PermissionMode;
  trustedDirectories: string[];
  denyGlobs: string[];
}

export interface ShellToolConfig {
  defaultTimeoutMs: number;
  requireApprovalFor: string[];
}

export interface ToolConfig {
  enabled: string[];
  disabled: string[];
  maxOutputBytes: number;
  shell: ShellToolConfig;
}

export interface SessionConfig {
  store: "filesystem" | "memory";
  directory: string;
  autosave: boolean;
  maxTranscriptBytes: number;
}

export interface EvidenceConfig {
  store: "filesystem" | "memory";
  directory: string;
  captureToolInputs: "summary" | "full" | "none";
  captureToolOutputs: "summary" | "full" | "none";
  maxEvidenceBytes: number;
}

export interface CoverageConfig {
  provider: "vitest";
  statements: number;
  branches: number;
  functions: number;
  lines: number;
  exclude: string[];
}

export interface FluxcodeConfig {
  schemaVersion: 1;
  models: ModelConfig;
  permissions: PermissionConfig;
  tools: ToolConfig;
  session: SessionConfig;
  evidence: EvidenceConfig;
  coverage: CoverageConfig;
}
