#!/usr/bin/env node
import { existsSync } from "node:fs";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

const apiKeyEnv = "FLUXCODE_MODEL_API_KEY";
const baseUrlEnv = "FLUXCODE_MODEL_BASE_URL";
const cliPath = join(process.cwd(), "dist", "src", "cli", "main.js");

if (!existsSync(cliPath)) {
  console.error("Provider smoke requires a built CLI. Run `npm run build` first.");
  process.exit(2);
}

if (process.env[apiKeyEnv] === undefined || process.env[apiKeyEnv]?.trim() === "") {
  console.error(`Provider smoke requires ${apiKeyEnv}.`);
  process.exit(2);
}

const smokeDir = await mkdtemp(join(tmpdir(), "fluxcode-provider-smoke-"));
const configPath = join(smokeDir, "fluxcode.provider-smoke.config.jsonc");
await writeFile(configPath, JSON.stringify({
  schemaVersion: 1,
  models: {
    default: "primary",
    providers: {
      primary: {
        type: "openai-compatible",
        model: process.env.FLUXCODE_MODEL_NAME ?? "gpt-5.5",
        baseUrl: process.env[baseUrlEnv] ?? "https://api.openai.com/v1",
        apiKeyEnv,
        temperature: 0,
        maxOutputTokens: 1024
      }
    }
  },
  permissions: { mutatingTools: "deny", highRiskTools: "deny" },
  session: { store: "memory" },
  evidence: { store: "memory" },
  tools: { disabled: ["write_file", "edit_file", "shell_exec"] }
}, null, 2), "utf8");

const child = spawn(process.execPath, [cliPath, "run", "Provider smoke: inspect the project manifest and return a blocked or completed AgentHandoff without editing files.", "--config", configPath, "--output", "json"], {
  cwd: process.cwd(),
  stdio: "inherit",
  env: process.env
});

child.on("exit", (code, signal) => {
  if (signal !== null) {
    console.error(`Provider smoke terminated by ${signal}.`);
    process.exit(1);
  }
  process.exit(code ?? 1);
});
