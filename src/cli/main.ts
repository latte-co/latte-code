#!/usr/bin/env node
import { cwd } from "node:process";
import { join } from "node:path";
import { loadConfig } from "../config/config.js";
import type { FluxcodeConfig } from "../config/types.js";
import { createHeadlessRunEnvelopeFromAgentResult, createHeadlessRunEnvelopeFromTaskRunState, exitCodeForTaskRunStatus, isResumeInput, type HeadlessRunEnvelope, type HeadlessRunListEnvelope, type ResumeInput } from "../core/contracts.js";
import { createAgentLoop } from "../runtime/create-agent.js";
import { FileSessionStore, InMemorySessionStore, type SessionState, type SessionStore } from "../session/session.js";

type OutputMode = "json" | "text";

interface CliArgs {
  command: string;
  input: string;
  configPath?: string;
  sessionId?: string;
  output: OutputMode;
  resumeInput?: string;
}

export interface RunCliOptions {
  cwd?: string;
}

export function parseArgs(argv: string[]): CliArgs {
  const [command = "run", ...rest] = argv;
  let input = "";
  let configPath: string | undefined;
  let sessionId: string | undefined;
  let output: OutputMode = "json";
  let resumeInput: string | undefined;
  for (let index = 0; index < rest.length; index += 1) {
    const token = rest[index] ?? "";
    if (token === "--config") configPath = rest[(index += 1)];
    else if (token === "--session") sessionId = rest[(index += 1)];
    else if (token === "--output") output = parseOutputMode(rest[(index += 1)]);
    else if (token === "--input") resumeInput = rest[(index += 1)];
    else input = input.length === 0 ? token : `${input} ${token}`;
  }
  return { command, input, output, ...(configPath === undefined ? {} : { configPath }), ...(sessionId === undefined ? {} : { sessionId }), ...(resumeInput === undefined ? {} : { resumeInput }) };
}

export async function runCli(argv: string[], options: RunCliOptions = {}): Promise<number> {
  let args: CliArgs;
  try {
    args = parseArgs(argv);
  } catch (error) {
    console.error(errorMessage(error));
    printUsage();
    return 2;
  }

  try {
    return await runCliWithParsedArgs(args, options);
  } catch (error) {
    writeError(args.output, errorMessage(error));
    return 2;
  }
}

async function runCliWithParsedArgs(args: CliArgs, options: RunCliOptions): Promise<number> {
  const currentCwd = options.cwd ?? cwd();
  const { config } = await loadConfig({ cwd: currentCwd, ...(args.configPath === undefined ? {} : { configPath: args.configPath }) });
  if (!config.commands.enabled.includes(args.command)) {
    writeError(args.output, `Unsupported command '${args.command}'.`);
    printUsage();
    return 2;
  }

  if (args.command === "run") {
    const loop = createAgentLoop({ cwd: currentCwd, config });
    const result = await loop.run({ input: args.input, ...(args.sessionId === undefined ? {} : { sessionId: args.sessionId }) });
    const envelope = createHeadlessRunEnvelopeFromAgentResult(result);
    writeEnvelope(args.output, envelope);
    return exitCodeForTaskRunStatus(envelope.status);
  }

  if (args.command === "resume") {
    const resumeInput = parseResumeInput(args.resumeInput);
    const session = await resolveSessionForRunReference(createCliSessionStore(currentCwd, config), args.input);
    if (session === undefined) {
      writeError(args.output, `Run '${args.input}' was not found.`);
      return 21;
    }
    const loop = createAgentLoop({ cwd: currentCwd, config });
    const result = await loop.resume({ sessionId: session.id, input: resumeInput });
    const envelope = createHeadlessRunEnvelopeFromAgentResult(result);
    writeEnvelope(args.output, envelope);
    return exitCodeForTaskRunStatus(envelope.status);
  }

  if (args.command === "show") {
    const session = await resolveSessionForRunReference(createCliSessionStore(currentCwd, config), args.input);
    if (session?.runState === undefined) {
      writeError(args.output, `Run '${args.input}' was not found.`);
      return 21;
    }
    const envelope = createHeadlessRunEnvelopeFromTaskRunState(session.runState);
    writeEnvelope(args.output, envelope);
    return exitCodeForTaskRunStatus(envelope.status);
  }

  if (args.command === "list") {
    const sessions = await listSessions(createCliSessionStore(currentCwd, config));
    const runs = sessions.flatMap((session) => (session.runState === undefined ? [] : [createHeadlessRunEnvelopeFromTaskRunState(session.runState)]));
    writeRunList(args.output, { runs });
    return 0;
  }

  writeError(args.output, `Unsupported command '${args.command}'.`);
  printUsage();
  return 2;
}

function parseOutputMode(value: string | undefined): OutputMode {
  if (value === "json" || value === "text") return value;
  throw new Error("--output must be 'json' or 'text'.");
}

function parseResumeInput(value: string | undefined): ResumeInput {
  if (value === undefined || value.trim() === "") throw new Error("resume requires --input '<ResumeInput JSON>'.");
  let parsed: unknown;
  try {
    parsed = JSON.parse(value) as unknown;
  } catch (error) {
    throw new Error(`Invalid resume input JSON: ${errorMessage(error)}`);
  }
  if (!isResumeInput(parsed)) throw new Error("Invalid ResumeInput contract.");
  return parsed;
}

function createCliSessionStore(currentCwd: string, config: FluxcodeConfig): SessionStore {
  return config.session.store === "memory" ? new InMemorySessionStore() : new FileSessionStore(join(currentCwd, config.session.directory));
}

async function resolveSessionForRunReference(store: SessionStore, runReference: string): Promise<SessionState | undefined> {
  if (runReference.trim() === "") return undefined;
  const direct = await store.get(runReference);
  if (direct?.runState !== undefined) return direct;
  const sessions = await listSessions(store);
  return sessions.find((session) => session.runState?.id === runReference);
}

async function listSessions(store: SessionStore): Promise<SessionState[]> {
  return store.list === undefined ? [] : store.list();
}

function writeEnvelope(output: OutputMode, envelope: HeadlessRunEnvelope): void {
  if (output === "json") {
    console.log(JSON.stringify(envelope, null, 2));
    return;
  }
  console.log(renderTextEnvelope(envelope));
}

function writeRunList(output: OutputMode, envelope: HeadlessRunListEnvelope): void {
  if (output === "json") {
    console.log(JSON.stringify(envelope, null, 2));
    return;
  }
  if (envelope.runs.length === 0) {
    console.log("No runs found.");
    return;
  }
  console.log(envelope.runs.map((run) => `${run.runId}\t${run.sessionId}\t${run.status}`).join("\n"));
}

function renderTextEnvelope(envelope: HeadlessRunEnvelope): string {
  const lines = [`Run ${envelope.runId}`, `Session ${envelope.sessionId}`, `Status ${envelope.status}`];
  if (envelope.pendingInput !== undefined) {
    if (envelope.pendingInput.kind === "permission") {
      lines.push(`Pending permission ${envelope.pendingInput.permissionId}: ${envelope.pendingInput.reason}`);
      lines.push(`Resume with: fluxcode resume ${envelope.runId} --input '{"kind":"permission","permissionId":"${envelope.pendingInput.permissionId}","decision":"approve"}'`);
    } else {
      lines.push(`Pending question ${envelope.pendingInput.questionId}: ${envelope.pendingInput.prompt}`);
      lines.push(`Resume with: fluxcode resume ${envelope.runId} --input '{"kind":"question","questionId":"${envelope.pendingInput.questionId}","answerText":"..."}'`);
    }
  }
  if (envelope.handoff !== undefined) {
    lines.push(`Summary ${envelope.handoff.summary}`);
    if (envelope.handoff.changedFiles.length > 0) lines.push(`Changed files ${envelope.handoff.changedFiles.join(", ")}`);
    if (envelope.handoff.verification.length > 0) lines.push(`Verification ${envelope.handoff.verification.map((entry) => `${entry.command}:${entry.status}`).join(", ")}`);
    if (envelope.handoff.risks.length > 0) lines.push(`Risks ${envelope.handoff.risks.join("; ")}`);
    if (envelope.handoff.blockers.length > 0) lines.push(`Blockers ${envelope.handoff.blockers.join("; ")}`);
  }
  return lines.join("\n");
}

function writeError(output: OutputMode, message: string): void {
  if (output === "json") {
    console.error(JSON.stringify({ error: message }, null, 2));
    return;
  }
  console.error(message);
}

function printUsage(): void {
  console.error("Usage: fluxcode run <task> [--output json|text] [--config path] [--session id]");
  console.error("       fluxcode resume <runId> --input '<ResumeInput JSON>' [--output json|text] [--config path]");
  console.error("       fluxcode show <runId> [--output json|text] [--config path]");
  console.error("       fluxcode list [--output json|text] [--config path]");
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "Unknown CLI error";
}

if (import.meta.url === `file://${process.argv[1]}`) {
  runCli(process.argv.slice(2)).then((code) => process.exit(code));
}
