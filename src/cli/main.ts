#!/usr/bin/env node
import { cwd } from "node:process";
import { loadConfig } from "../config/config.js";
import { createAgentLoop } from "../runtime/create-agent.js";

interface CliArgs {
  command: string;
  input: string;
  configPath?: string;
  sessionId?: string;
}

export function parseArgs(argv: string[]): CliArgs {
  const [command = "run", ...rest] = argv;
  let input = "";
  let configPath: string | undefined;
  let sessionId: string | undefined;
  for (let index = 0; index < rest.length; index += 1) {
    const token = rest[index] ?? "";
    if (token === "--config") configPath = rest[(index += 1)];
    else if (token === "--session") sessionId = rest[(index += 1)];
    else input = input.length === 0 ? token : `${input} ${token}`;
  }
  return { command, input, ...(configPath === undefined ? {} : { configPath }), ...(sessionId === undefined ? {} : { sessionId }) };
}

export async function runCli(argv: string[]): Promise<number> {
  const args = parseArgs(argv);
  const { config } = await loadConfig({ cwd: cwd(), ...(args.configPath === undefined ? {} : { configPath: args.configPath }) });
  if (args.command === "session") {
    console.log(JSON.stringify({ sessionDirectory: config.session.directory }, null, 2));
    return 0;
  }
  if (args.command === "evidence") {
    console.log(JSON.stringify({ evidenceDirectory: config.evidence.directory }, null, 2));
    return 0;
  }
  if (args.command !== "run" && args.command !== "resume") {
    console.error("Usage: fluxcode run|resume|session|evidence [input] [--config path] [--session id]");
    return 2;
  }
  const loop = createAgentLoop({ cwd: cwd(), config });
  const result = await loop.run({ input: args.input, ...(args.sessionId === undefined ? {} : { sessionId: args.sessionId }) });
  console.log(JSON.stringify({ status: result.status, sessionId: result.session.id, finalResponse: result.finalResponse, pendingPermission: result.pendingPermission, evidenceIds: result.session.evidenceIds }, null, 2));
  return result.status === "failed" ? 1 : 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  runCli(process.argv.slice(2)).then((code) => process.exit(code));
}
