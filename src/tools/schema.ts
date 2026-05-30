import { isRecord } from "../shared/types.js";

export type SchemaType = "string" | "number" | "boolean" | "object" | "array";

export interface LightweightSchema {
  type: SchemaType;
  required?: string[];
  properties?: Record<string, LightweightSchema>;
  items?: LightweightSchema;
  enum?: readonly string[];
  additionalProperties?: boolean;
}

export class SchemaValidationError extends Error {
  readonly issues: string[];

  constructor(issues: string[]) {
    super(`Schema validation failed: ${issues.join("; ")}`);
    this.issues = issues;
  }
}

export function validateSchema(schema: LightweightSchema, value: unknown, path = "$."): void {
  const issues: string[] = [];
  visit(schema, value, path, issues);
  if (issues.length > 0) throw new SchemaValidationError(issues);
}

function visit(schema: LightweightSchema, value: unknown, path: string, issues: string[]): void {
  if (schema.type === "array") {
    if (!Array.isArray(value)) {
      issues.push(`${path} expected array`);
      return;
    }
    if (schema.items !== undefined) {
      value.forEach((entry, index) => visit(schema.items as LightweightSchema, entry, `${path}${index}.`, issues));
    }
    return;
  }

  if (schema.type === "object") {
    if (!isRecord(value)) {
      issues.push(`${path} expected object`);
      return;
    }
    for (const required of schema.required ?? []) {
      if (!(required in value)) issues.push(`${path}${required} is required`);
    }
    const properties = schema.properties ?? {};
    for (const [key, entry] of Object.entries(value)) {
      const child = properties[key];
      if (child !== undefined) visit(child, entry, `${path}${key}.`, issues);
      else if (schema.additionalProperties === false) issues.push(`${path}${key} is not allowed`);
    }
    return;
  }

  if (typeof value !== schema.type) {
    issues.push(`${path} expected ${schema.type}`);
    return;
  }
  if (schema.enum !== undefined && typeof value === "string" && !schema.enum.includes(value)) {
    issues.push(`${path} must be one of ${schema.enum.join(", ")}`);
  }
}
