export function stripJsonComments(input: string): string {
  let output = "";
  let inString = false;
  let escaped = false;
  let inLineComment = false;
  let inBlockComment = false;

  for (let index = 0; index < input.length; index += 1) {
    /* v8 ignore next -- loop bounds guarantee this access; fallback satisfies noUncheckedIndexedAccess. */
    const char = input[index] ?? "";
    const next = input[index + 1] ?? "";

    if (inLineComment) {
      if (char === "\n") {
        inLineComment = false;
        output += char;
      }
      continue;
    }

    if (inBlockComment) {
      if (char === "*" && next === "/") {
        inBlockComment = false;
        index += 1;
      } else if (char === "\n") {
        output += "\n";
      }
      continue;
    }

    if (inString) {
      output += char;
      if (escaped) {
        escaped = false;
      } else if (char === "\\") {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }

    if (char === '"') {
      inString = true;
      output += char;
      continue;
    }
    if (char === "/" && next === "/") {
      inLineComment = true;
      index += 1;
      continue;
    }
    if (char === "/" && next === "*") {
      inBlockComment = true;
      index += 1;
      continue;
    }
    output += char;
  }

  return output;
}

export function removeTrailingCommas(input: string): string {
  let output = "";
  let inString = false;
  let escaped = false;
  for (let index = 0; index < input.length; index += 1) {
    /* v8 ignore next -- loop bounds guarantee this access; fallback satisfies noUncheckedIndexedAccess. */
    const char = input[index] ?? "";
    if (inString) {
      output += char;
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') inString = false;
      continue;
    }
    if (char === '"') {
      inString = true;
      output += char;
      continue;
    }
    if (char === ",") {
      let cursor = index + 1;
      /* v8 ignore next -- out-of-range fallback is only for noUncheckedIndexedAccess. */
      while (/\s/.test(input[cursor] ?? "")) cursor += 1;
      /* v8 ignore next -- out-of-range fallback is only for noUncheckedIndexedAccess. */
      const next = input[cursor] ?? "";
      if (next === "}" || next === "]") continue;
    }
    output += char;
  }
  return output;
}

export function parseJsonc(input: string): unknown {
  return JSON.parse(removeTrailingCommas(stripJsonComments(input)));
}
