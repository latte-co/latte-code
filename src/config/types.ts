export const IMPLEMENTED_PROVIDER_TYPES = ["fake", "openai-compatible"] as const;
export const FUTURE_PROVIDER_TYPES = ["openai-responses", "anthropic", "gemini", "vertex", "bedrock", "ollama", "custom"] as const;

export type ImplementedProviderType = (typeof IMPLEMENTED_PROVIDER_TYPES)[number];
export type FutureProviderType = (typeof FUTURE_PROVIDER_TYPES)[number];
export type ProviderType = ImplementedProviderType | FutureProviderType;

export function providerTypeStatus(value: unknown): "implemented" | "future" | "unsupported" {
  if (typeof value === "string" && (IMPLEMENTED_PROVIDER_TYPES as readonly string[]).includes(value)) return "implemented";
  if (typeof value === "string" && (FUTURE_PROVIDER_TYPES as readonly string[]).includes(value)) return "future";
  return "unsupported";
}

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
  allowCommands: string[];
  requireApprovalFor: string[];
}

export interface ToolConfig {
  enabled: string[];
  disabled: string[];
  maxOutputBytes: number;
  shell: ShellToolConfig;
}

export interface RuntimeConfig {
  maxTurns: number;
}

export interface PromptConfig {
  profile: string;
  language: string;
}

export interface AgentsConfig {
  agentsFile: string;
  loadFrom: ("repoRoot" | "cwd")[];
  snapshot: boolean;
  hashAlgorithm: "sha256";
}

export interface ContextConfig {
  maxPromptBytes: number;
  maxToolResultBytes: number;
  recentTurnCount: number;
  preserve: string[];
}

export interface CommandsConfig {
  enabled: string[];
  localDirectory: string;
  allowLocalCommands: boolean;
}

export interface SkillsConfig {
  enabled: string[];
  localDirectories: string[];
  allowSideEffects: boolean;
}

export interface McpServerConfig {
  enabled?: boolean;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  tools?: Record<string, McpToolConfig>;
}

export interface McpToolConfig {
  description?: string;
  inputSchema?: Record<string, unknown>;
  mutating?: boolean;
  riskLevel?: "low" | "medium" | "high";
}

export interface McpConfig {
  enabled: boolean;
  servers: Record<string, McpServerConfig>;
  requireExplicitEnable: boolean;
  routeThroughPermission: boolean;
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
  maxEvidenceBytes: number;
}

export interface LattecodeConfig {
  schemaVersion: 1;
  models: ModelConfig;
  runtime: RuntimeConfig;
  prompts: PromptConfig;
  agents: AgentsConfig;
  context: ContextConfig;
  permissions: PermissionConfig;
  tools: ToolConfig;
  commands: CommandsConfig;
  skills: SkillsConfig;
  mcp: McpConfig;
  session: SessionConfig;
  evidence: EvidenceConfig;
}
