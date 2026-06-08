#!/usr/bin/env node
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, isAbsolute, join, relative, resolve, sep } from "node:path";

const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";
const projectRoot = process.cwd();
const packageJsonPath = join(projectRoot, "package.json");

const tempDir = await mkdtemp(join(tmpdir(), "fluxcode-local-install-"));
let originalPackageJson;

try {
  originalPackageJson = await readFile(packageJsonPath, "utf8");
  const packageJson = JSON.parse(originalPackageJson);
  const baseVersion = String(packageJson.version).replace(/-dev\.\d{12}$/, "");
  const stampedVersion = `${baseVersion}-dev.${formatMinuteStamp(new Date())}`;

  packageJson.version = stampedVersion;
  await writeFile(packageJsonPath, `${JSON.stringify(packageJson, null, 2)}\n`, "utf8");

  console.log(`Packing ${packageJson.name}@${stampedVersion}...`);
  const packResult = await runNpm(["pack", "--json", "--pack-destination", tempDir], {
    cwd: projectRoot,
    stdio: "pipe"
  });
  const packEntries = parseNpmPackJson(`${packResult.stdout}\n${packResult.stderr}`);
  const tarballPath = resolveTarballPath(tempDir, packEntries[0]?.filename);

  await ensureFileExists(tarballPath);
  await cleanGlobalInstallPaths(packageJson);

  console.log(`Installing ${tarballPath} globally...`);
  await runNpm(["install", "-g", tarballPath], {
    cwd: projectRoot,
    stdio: "inherit"
  });

  const verificationCommand = getVerificationCommand(packageJson);
  console.log(`Installed ${packageJson.name}@${stampedVersion}.`);
  if (verificationCommand !== undefined) {
    console.log(`Verify with: ${verificationCommand}`);
  }
} finally {
  let finalizationError;
  if (originalPackageJson !== undefined) {
    try {
      await restorePackageJson(originalPackageJson);
    } catch (error) {
      finalizationError = error;
    }
  }
  try {
    await removeTempDir();
  } catch (error) {
    if (finalizationError === undefined) {
      finalizationError = error;
    } else {
      console.error(`Also failed to remove temp directory ${tempDir}:`, error);
    }
  }
  if (finalizationError !== undefined) {
    throw finalizationError;
  }
}

function formatMinuteStamp(date) {
  const year = date.getFullYear();
  const month = pad2(date.getMonth() + 1);
  const day = pad2(date.getDate());
  const hour = pad2(date.getHours());
  const minute = pad2(date.getMinutes());
  return `${year}${month}${day}${hour}${minute}`;
}

function pad2(value) {
  return String(value).padStart(2, "0");
}

function parseNpmPackJson(output) {
  const parsedValues = extractJsonValues(output);
  const packEntries = parsedValues.find(isPackEntries);

  if (packEntries === undefined) {
    throw new Error("Could not parse npm pack JSON output.");
  }

  return packEntries;
}

function extractJsonValues(output) {
  const values = [];

  for (let index = 0; index < output.length; index += 1) {
    const char = output[index];
    if (char !== "[" && char !== "{") {
      continue;
    }

    const candidate = extractBalancedJson(output, index);
    if (candidate === undefined) {
      continue;
    }

    try {
      values.push(JSON.parse(candidate));
    } catch {
      // Keep scanning; npm may print notices before or after the JSON payload.
    }
  }

  return values;
}

function extractBalancedJson(text, startIndex) {
  const stack = [];
  let inString = false;
  let escaped = false;

  for (let index = startIndex; index < text.length; index += 1) {
    const char = text[index];

    if (inString) {
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
      continue;
    }

    if (char === "[" || char === "{") {
      stack.push(char === "[" ? "]" : "}");
      continue;
    }

    if (char !== "]" && char !== "}") {
      continue;
    }

    if (stack.pop() !== char) {
      return undefined;
    }

    if (stack.length === 0) {
      return text.slice(startIndex, index + 1);
    }
  }

  return undefined;
}

function isPackEntries(value) {
  return Array.isArray(value) && value.some((entry) => {
    return entry !== null && typeof entry === "object" && typeof entry.filename === "string";
  });
}

function resolveTarballPath(destinationDir, filename) {
  if (typeof filename !== "string" || filename.trim() === "") {
    throw new Error("npm pack JSON did not include a tarball filename.");
  }

  const tarballPath = isAbsolute(filename) ? resolve(filename) : resolve(destinationDir, filename);
  assertInsideDirectory(destinationDir, tarballPath, "npm pack produced a tarball outside the temp directory");
  return tarballPath;
}

async function ensureFileExists(filePath) {
  const fileStat = await stat(filePath);
  if (!fileStat.isFile()) {
    throw new Error(`Packed tarball is not a file: ${filePath}`);
  }
}

async function cleanGlobalInstallPaths(pkg) {
  const packageName = parsePackageName(pkg.name);
  const globalRoot = await getNpmPath(["root", "-g"]);
  const globalPrefix = await getNpmPath(["prefix", "-g"]);
  const globalBin = process.platform === "win32" ? globalPrefix : join(globalPrefix, "bin");
  const packageInstallPath = packageName.scope === undefined
    ? join(globalRoot, packageName.name)
    : join(globalRoot, packageName.scope, packageName.name);

  assertInsideDirectory(globalRoot, packageInstallPath, "Refusing to remove a package path outside npm global root");
  await rm(packageInstallPath, { recursive: true, force: true });

  if (packageName.scope !== undefined) {
    await removeEmptyScopeDirectory(join(globalRoot, packageName.scope));
  }

  for (const binName of getBinNames(pkg)) {
    await removeGlobalBin(globalBin, binName);
  }
}

async function getNpmPath(args) {
  const result = await runNpm(args, {
    cwd: projectRoot,
    stdio: "pipe"
  });
  const pathValue = result.stdout.trim().split(/\r?\n/).at(-1);

  if (pathValue === undefined || pathValue.trim() === "") {
    throw new Error(`npm ${args.join(" ")} did not return a path.`);
  }

  return pathValue.trim();
}

function parsePackageName(packageName) {
  if (typeof packageName !== "string" || packageName.trim() === "") {
    throw new Error("package.json name must be a non-empty string.");
  }

  if (packageName.startsWith("@")) {
    const [scope, name, extra] = packageName.split("/");
    if (extra !== undefined || !isSafePathSegment(scope) || !isSafePathSegment(name)) {
      throw new Error(`Unsupported scoped package name: ${packageName}`);
    }
    return { scope, name };
  }

  if (!isSafePathSegment(packageName)) {
    throw new Error(`Unsupported package name: ${packageName}`);
  }

  return { name: packageName };
}

function isSafePathSegment(value) {
  return typeof value === "string"
    && value !== ""
    && value !== "."
    && value !== ".."
    && !value.includes("/")
    && !value.includes("\\");
}

async function removeEmptyScopeDirectory(scopeDirectory) {
  if (!existsSync(scopeDirectory)) {
    return;
  }

  const entries = await readdir(scopeDirectory);
  if (entries.length === 0) {
    await rm(scopeDirectory, { recursive: false, force: true });
  }
}

async function removeGlobalBin(globalBin, binName) {
  if (!isSafePathSegment(binName)) {
    throw new Error(`Unsupported bin name: ${binName}`);
  }

  const suffixes = process.platform === "win32" ? ["", ".cmd", ".ps1"] : [""];
  for (const suffix of suffixes) {
    const binPath = join(globalBin, `${binName}${suffix}`);
    assertInsideDirectory(globalBin, binPath, "Refusing to remove a bin path outside npm global bin directory");
    await rm(binPath, { force: true });
  }
}

function getBinNames(pkg) {
  if (typeof pkg.bin === "string") {
    return [basename(String(pkg.name).startsWith("@") ? String(pkg.name).split("/")[1] : String(pkg.name))];
  }

  if (pkg.bin !== null && typeof pkg.bin === "object") {
    return Object.keys(pkg.bin);
  }

  return [];
}

function getVerificationCommand(pkg) {
  const binNames = getBinNames(pkg);
  if (binNames.length === 0) {
    return undefined;
  }

  const preferredBinName = binNames.includes("fluxcode") ? "fluxcode" : binNames[0];
  return `${preferredBinName} --version`;
}

function assertInsideDirectory(parentDirectory, targetPath, message) {
  const parent = resolve(parentDirectory);
  const target = resolve(targetPath);
  const rel = relative(parent, target);

  if (rel === "" || rel.startsWith("..") || rel.includes(`..${sep}`) || isAbsolute(rel)) {
    throw new Error(`${message}: ${target}`);
  }
}

function runNpm(args, options) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(npmCommand, args, {
      cwd: options.cwd,
      env: process.env,
      stdio: options.stdio === "inherit" ? "inherit" : ["ignore", "pipe", "pipe"]
    });

    let stdout = "";
    let stderr = "";

    if (options.stdio === "pipe") {
      child.stdout?.on("data", (chunk) => {
        stdout += chunk;
      });
      child.stderr?.on("data", (chunk) => {
        stderr += chunk;
      });
    }

    child.on("error", rejectPromise);
    child.on("close", (code, signal) => {
      if (code === 0) {
        resolvePromise({ stdout, stderr });
        return;
      }

      rejectPromise(new Error(formatCommandFailure(args, code, signal, stdout, stderr)));
    });
  });
}

function formatCommandFailure(args, code, signal, stdout, stderr) {
  const reason = signal === null ? `exit code ${code}` : `signal ${signal}`;
  const output = [stdout.trim(), stderr.trim()].filter((value) => value !== "").join("\n");
  return [`npm ${args.join(" ")} failed with ${reason}.`, output].filter((value) => value !== "").join("\n");
}

async function restorePackageJson(packageJsonContent) {
  await writeFile(packageJsonPath, packageJsonContent, "utf8");
}

async function removeTempDir() {
  await rm(tempDir, { recursive: true, force: true });
}
