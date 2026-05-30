import type { FluxcodeConfig } from "./types.js";

export const DEFAULT_CONFIG: FluxcodeConfig = {
  schemaVersion: 1,
  models: {
    default: "fake",
    providers: {
      fake: { type: "fake", model: "fake-scripted" }
    }
  },
  permissions: {
    defaultMode: "ask",
    allowReadOnlyTools: true,
    mutatingTools: "ask",
    highRiskTools: "deny",
    trustedDirectories: ["."],
    denyGlobs: ["**/.env*", "**/node_modules/**", "**/.git/**"]
  },
  tools: {
    enabled: ["read_file", "list_directory", "search", "write_file", "shell_exec"],
    disabled: [],
    maxOutputBytes: 32768,
    shell: {
      defaultTimeoutMs: 120000,
      requireApprovalFor: ["network", "install", "delete", "git-write"]
    }
  },
  session: {
    store: "filesystem",
    directory: ".fluxcode/sessions",
    autosave: true,
    maxTranscriptBytes: 1048576
  },
  evidence: {
    store: "filesystem",
    directory: ".fluxcode/evidence",
    captureToolInputs: "summary",
    captureToolOutputs: "summary",
    maxEvidenceBytes: 262144
  },
  coverage: {
    provider: "vitest",
    statements: 98,
    branches: 98,
    functions: 98,
    lines: 98,
    exclude: ["tests/**", "src/testing/**", "src/cli/main.ts", "src/index.ts"]
  }
};
