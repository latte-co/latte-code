import type { ContextConfig } from "../config/types.js";
import type { TaskRunState } from "../core/contracts.js";
import type { ModelMessage } from "../model/types.js";
import type { TranscriptEntry } from "../session/session.js";
import { truncateText } from "../shared/types.js";
import type { ToolResult } from "../tools/types.js";

export interface BuildContextMessagesInput {
  run: TaskRunState;
  transcript: TranscriptEntry[];
  config: ContextConfig;
  baseMessages: ModelMessage[];
  toolResults: ToolResult[];
}

export interface BuiltContextMessages {
  messages: ModelMessage[];
  compacted: boolean;
  blockedReason?: string;
}

export function buildContextProjection(run: TaskRunState, toolResults: readonly ToolResult[]): string {
  const projection = {
    agentContext: run.agentContext,
    acceptance: run.agentContext?.acceptance ?? [],
    constraints: run.agentContext?.constraints ?? run.contextSnapshot.pinnedConstraints,
    nonGoals: run.agentContext?.nonGoals ?? [],
    changes: {
      changedFiles: run.changedFiles,
      evidenceRefs: run.changeEvidenceRefs
    },
    verification: run.verification,
    contextSnapshot: run.contextSnapshot,
    recentToolResults: toolResults.map((result) => ({ toolName: result.toolName, ok: result.ok, summary: result.summary, references: result.references, truncated: result.truncated }))
  };
  return `ContextProjection: ${JSON.stringify(projection)}`;
}

export function buildContextMessages(input: BuildContextMessagesInput): BuiltContextMessages {
  const transcript = input.transcript.map((entry): ModelMessage => ({ role: entry.role, content: entry.content }));
  let messages = [...input.baseMessages, ...transcript];
  if (bytes(messages) <= input.config.maxPromptBytes) return { messages, compacted: false };

  const compactedTranscript = compactTranscript(input.transcript, input.config.recentTurnCount, input.config.maxToolResultBytes).map((entry): ModelMessage => ({ role: entry.role, content: entry.content }));
  const compactedSummary = summarizeCompaction(input.transcript, compactedTranscript.length);
  input.run.contextSnapshot.compactedSummary = compactedSummary;
  messages = [...input.baseMessages, { role: "system", content: `Compacted prior context: ${compactedSummary}` }, ...compactedTranscript];
  if (bytes(messages) <= input.config.maxPromptBytes) return { messages, compacted: true };

  const essentials = input.baseMessages;
  if (bytes(essentials) > input.config.maxPromptBytes) {
    return { messages: essentials, compacted: true, blockedReason: "context_budget_gate: preserved request, acceptance, constraints, changedFiles, and verification lanes exceed maxPromptBytes" };
  }
  input.run.contextSnapshot.compactedSummary = `${compactedSummary}\nDropped older transcript entries after context budget compaction.`;
  return { messages: essentials, compacted: true };
}

function compactTranscript(transcript: readonly TranscriptEntry[], recentCount: number, maxToolBytes: number): TranscriptEntry[] {
  return transcript.slice(-recentCount).map((entry) => {
    if (entry.role !== "tool") return entry;
    return { role: entry.role, content: truncateText(entry.content, maxToolBytes).text };
  });
}

function summarizeCompaction(transcript: readonly TranscriptEntry[], kept: number): string {
  const dropped = Math.max(0, transcript.length - kept);
  const byRole = transcript.reduce<Record<TranscriptEntry["role"], number>>((accumulator, entry) => {
    accumulator[entry.role] += 1;
    return accumulator;
  }, { user: 0, assistant: 0, tool: 0 });
  return `Compacted ${dropped} older transcript entries. Totals before compaction: user=${byRole.user}, assistant=${byRole.assistant}, tool=${byRole.tool}.`;
}

function bytes(messages: readonly ModelMessage[]): number {
  return Buffer.byteLength(messages.map((message) => `${message.role}:${message.content}`).join("\n"), "utf8");
}
