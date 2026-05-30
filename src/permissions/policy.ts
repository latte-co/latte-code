import { isAbsolute, relative, resolve } from "node:path";
import type { PermissionConfig, ShellToolConfig } from "../config/types.js";
import type { PermissionDecision, PermissionRequest } from "./types.js";

const HIGH_RISK_COMMAND_CATEGORIES = new Set(["delete", "install", "git-write", "privilege"]);

export class PermissionPolicy {
  constructor(private readonly config: PermissionConfig, private readonly shellConfig?: ShellToolConfig) {}

  decide(request: PermissionRequest): PermissionDecision {
    const commandCategory = classifyCommand(request.call.input.command);
    const pathDecision = this.pathDecision(request);
    if (pathDecision.denied) return this.result("deny", request, pathDecision.reason, commandCategory, true);
    if (commandCategory !== undefined && this.shellRequiresApproval(request, commandCategory)) {
      if (HIGH_RISK_COMMAND_CATEGORIES.has(commandCategory)) {
        return this.result(this.config.highRiskTools, request, `Command category '${commandCategory}' requires high-risk policy`, commandCategory, false);
      }
      return this.result("ask", request, `Command category '${commandCategory}' requires shell approval`, commandCategory, false);
    }
    if (commandCategory !== undefined && HIGH_RISK_COMMAND_CATEGORIES.has(commandCategory)) {
      return this.result(this.config.highRiskTools, request, `Command category '${commandCategory}' requires high-risk policy`, commandCategory, false);
    }
    if (request.riskLevel === "high") {
      return this.result(this.config.highRiskTools, request, "High-risk tool follows highRiskTools policy", commandCategory, false);
    }
    if (!request.mutating && request.riskLevel === "low" && this.config.allowReadOnlyTools) {
      return this.result("allow", request, "Read-only low-risk tool is allowed by default", commandCategory, false);
    }
    if (request.mutating) {
      return this.result(this.config.mutatingTools, request, "Mutating tool follows mutatingTools policy", commandCategory, false);
    }
    return this.result(this.config.defaultMode, request, "Tool follows default permission mode", commandCategory, false);
  }

  private pathDecision(request: PermissionRequest): { denied: false } | { denied: true; reason: string } {
    const candidates = pathCandidates(request);
    const trustedRoots = this.trustedRoots(request.cwd);
    for (const candidate of candidates) {
      const absolute = resolve(request.cwd, candidate);
      const trustedRoot = trustedRoots.find((root) => containsPath(root, absolute));
      if (trustedRoot === undefined) return { denied: true, reason: "Target path is outside workspace or trusted directories" };
      const relativePath = relative(trustedRoot, absolute) || ".";
      if (this.config.denyGlobs.some((glob) => globMatches(glob, relativePath))) {
        return { denied: true, reason: "Target path is denied by permission policy" };
      }
    }
    return { denied: false };
  }

  private trustedRoots(cwd: string): string[] {
    const configured = this.config.trustedDirectories.length === 0 ? ["."] : this.config.trustedDirectories;
    return configured.map((entry) => (isAbsolute(entry) ? resolve(entry) : resolve(cwd, entry)));
  }

  private shellRequiresApproval(request: PermissionRequest, commandCategory: string): boolean {
    if (request.toolName !== "shell_exec") return false;
    return this.shellConfig?.requireApprovalFor.includes(commandCategory) === true;
  }

  private result(action: PermissionDecision["action"], request: PermissionRequest, reason: string, commandCategory: string | undefined, sensitivePath: boolean): PermissionDecision {
    return {
      action,
      reason,
      requirement: request.requirement,
      metadata: {
        toolName: request.toolName,
        riskLevel: request.riskLevel,
        mutating: request.mutating,
        sensitivePath,
        ...(commandCategory === undefined ? {} : { commandCategory })
      }
    };
  }
}

export function classifyCommand(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const command = value.trim();
  if (/(^|[;&|]\s*)(rm|unlink|rmdir|shred)\b/.test(command) || /\bfind\b.+\s-delete\b/.test(command)) return "delete";
  if (/\b(npm|pnpm|yarn|bun)\s+(install|add|i)\b/.test(command) || /\b(pip|pip3)\s+install\b/.test(command)) return "install";
  if (/\bgit\s+(commit|push|rebase|tag|reset|checkout|merge|clean|branch\s+-D)\b/.test(command)) return "git-write";
  if (/\b(sudo|su\s+-|chmod\s+-R|chown\s+-R|mkfs|dd\s+if=|diskutil\s+erase)\b/.test(command)) return "privilege";
  if (/\b(curl|wget|ssh|scp|nc|ncat|telnet|ftp|sftp)\b/.test(command)) return "network";
  return "shell";
}

function pathCandidates(request: PermissionRequest): string[] {
  const values = [request.call.input.path, request.call.input.filePath, request.call.input.directory, request.call.input.cwd];
  return values.filter((entry): entry is string => typeof entry === "string");
}

function containsPath(root: string, candidate: string): boolean {
  const diff = relative(root, candidate);
  return diff === "" || (!diff.startsWith("..") && !isAbsolute(diff));
}

export function globMatches(pattern: string, value: string): boolean {
  const normalized = value.replaceAll("\\", "/");
  if (pattern.startsWith("**/")) {
    const tail = pattern.slice(3);
    if (tail.endsWith("/**")) {
      const directory = tail.slice(0, -3);
      return normalized === directory || normalized.startsWith(`${directory}/`) || normalized.includes(`/${directory}/`);
    }
    return new RegExp(`(^|/)${escapeGlob(tail)}$`).test(normalized);
  }
  if (pattern.endsWith("/**")) return normalized.startsWith(pattern.slice(0, -3));
  if (pattern.includes("*")) {
    return new RegExp(`^${escapeGlob(pattern)}$`).test(normalized);
  }
  return normalized === pattern;
}

function escapeGlob(pattern: string): string {
  return pattern.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*");
}
