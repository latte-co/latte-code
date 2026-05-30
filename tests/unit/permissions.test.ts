import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { PermissionPolicy, classifyCommand, globMatches } from "../../src/permissions/policy.js";
import type { PermissionRequest } from "../../src/permissions/types.js";

function request(overrides: Partial<PermissionRequest>): PermissionRequest {
  return {
    toolName: "read_file",
    call: { id: "c1", name: "read_file", input: { path: "README.md" } },
    cwd: "/repo",
    riskLevel: "low",
    mutating: false,
    requirement: { reason: "test" },
    ...overrides
  };
}

describe("PermissionPolicy", () => {
  it("allows low-risk read-only tools by default", () => {
    const decision = new PermissionPolicy(DEFAULT_CONFIG.permissions).decide(request({}));
    expect(decision.action).toBe("allow");
    expect(decision.metadata.mutating).toBe(false);
  });

  it("asks for mutating tools and denies sensitive paths", () => {
    const policy = new PermissionPolicy(DEFAULT_CONFIG.permissions);
    expect(policy.decide(request({ toolName: "write_file", call: { id: "c2", name: "write_file", input: { path: "out.txt" } }, riskLevel: "medium", mutating: true })).action).toBe("ask");
    expect(policy.decide(request({ call: { id: "c3", name: "read_file", input: { path: ".env.local" } } })).action).toBe("deny");
  });

  it("denies paths outside workspace unless they are in a trusted directory", () => {
    const defaultPolicy = new PermissionPolicy(DEFAULT_CONFIG.permissions);
    expect(defaultPolicy.decide(request({ call: { id: "root", name: "list_directory", input: { path: "." } } })).action).toBe("allow");
    const outside = defaultPolicy.decide(request({ call: { id: "c", name: "read_file", input: { path: "../outside.txt" } } }));
    expect(outside.action).toBe("deny");
    expect(outside.reason).toContain("outside workspace");

    const trustedPolicy = new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, trustedDirectories: [".", "../trusted"] });
    expect(trustedPolicy.decide(request({ call: { id: "c", name: "read_file", input: { path: "../trusted/inside.txt" } } })).action).toBe("allow");
    expect(trustedPolicy.decide(request({ toolName: "shell_exec", call: { id: "s", name: "shell_exec", input: { command: "pwd", cwd: "../outside" } }, riskLevel: "medium", mutating: true })).action).toBe("deny");
    const absoluteTrustedPolicy = new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, trustedDirectories: [] });
    expect(absoluteTrustedPolicy.decide(request({ call: { id: "fallback", name: "read_file", input: { path: "README.md" } } })).action).toBe("allow");
    expect(new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, trustedDirectories: ["/repo"] }).decide(request({ call: { id: "absolute", name: "read_file", input: { path: "README.md" } } })).action).toBe("allow");
  });

  it("classifies dangerous shell commands", () => {
    expect(classifyCommand("npm install left-pad")).toBe("install");
    expect(classifyCommand("git push origin main")).toBe("git-write");
    expect(classifyCommand("rm -rf tmp")).toBe("delete");
    expect(classifyCommand("rm -fr tmp")).toBe("delete");
    expect(classifyCommand("sudo make install")).toBe("privilege");
    expect(classifyCommand("curl https://example.com")).toBe("network");
    expect(classifyCommand("pwd")).toBe("shell");
  });

  it("applies high-risk and default policies", () => {
    const policy = new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, allowReadOnlyTools: false, defaultMode: "ask" });
    expect(policy.decide(request({ riskLevel: "high" })).action).toBe("deny");
    expect(policy.decide(request({ riskLevel: "medium", mutating: false })).action).toBe("ask");
    expect(new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, highRiskTools: "ask" }).decide(request({ toolName: "shell_exec", call: { id: "c", name: "shell_exec", input: { command: "sudo make install" } }, riskLevel: "medium", mutating: true })).action).toBe("ask");
  });

  it("applies shell requireApprovalFor before generic mutating shell policy", () => {
    const permissiveShellPolicy = new PermissionPolicy({ ...DEFAULT_CONFIG.permissions, mutatingTools: "allow", highRiskTools: "deny" }, { ...DEFAULT_CONFIG.tools.shell, requireApprovalFor: ["network"] });
    expect(permissiveShellPolicy.decide(request({ toolName: "shell_exec", call: { id: "c", name: "shell_exec", input: { command: "curl https://example.com" } }, riskLevel: "medium", mutating: true })).action).toBe("ask");
    expect(permissiveShellPolicy.decide(request({ toolName: "shell_exec", call: { id: "c", name: "shell_exec", input: { command: "printf ok" } }, riskLevel: "medium", mutating: true })).action).toBe("allow");
    expect(permissiveShellPolicy.decide(request({ call: { id: "c", name: "read_file", input: { path: "README.md", command: "curl https://example.com" } } })).action).toBe("allow");
  });

  it("matches simple deny globs", () => {
    expect(globMatches("**/.git/**", "src/.git/config")).toBe(true);
    expect(globMatches("**/.git/**", ".git/config")).toBe(true);
    expect(globMatches("**/.env*", ".env.local")).toBe(true);
    expect(globMatches("dist/**", "dist/index.js")).toBe(true);
    expect(globMatches("README.md", "README.md")).toBe(true);
    expect(globMatches("*.ts", "index.ts")).toBe(true);
  });
});
