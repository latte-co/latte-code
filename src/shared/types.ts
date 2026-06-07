export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonObject | JsonValue[];

export interface JsonObject {
  [key: string]: JsonValue;
}

export type AgentStatus = "queued" | "running" | "completed" | "waiting_permission" | "blocked" | "denied" | "failed";

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function jsonClone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

export function toJsonObject(value: unknown): JsonObject {
  if (!isRecord(value)) {
    throw new Error("Expected JSON object");
  }
  const result: JsonObject = {};
  for (const [key, entry] of Object.entries(value)) {
    if (isJsonValue(entry)) {
      result[key] = entry;
    }
  }
  return result;
}

export function isJsonValue(value: unknown): value is JsonValue {
  if (value === null) return true;
  if (["string", "number", "boolean"].includes(typeof value)) return true;
  if (Array.isArray(value)) return value.every(isJsonValue);
  if (isRecord(value)) return Object.values(value).every(isJsonValue);
  return false;
}

export function stableId(prefix: string, parts: readonly string[]): string {
  const raw = parts.join("|");
  let hash = 0;
  for (let index = 0; index < raw.length; index += 1) {
    hash = (hash * 31 + raw.charCodeAt(index)) >>> 0;
  }
  return `${prefix}_${hash.toString(16).padStart(8, "0")}`;
}

export function truncateText(text: string, maxBytes: number): { text: string; truncated: boolean } {
  const bytes = Buffer.byteLength(text, "utf8");
  if (bytes <= maxBytes) {
    return { text, truncated: false };
  }
  let result = text;
  while (Buffer.byteLength(result, "utf8") > maxBytes && result.length > 0) {
    result = result.slice(0, -1);
  }
  return { text: `${result}\n[truncated ${bytes - Buffer.byteLength(result, "utf8")} bytes]`, truncated: true };
}
