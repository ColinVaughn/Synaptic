#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

function fail(message) {
  throw new Error(message);
}

function run(program, args) {
  return spawnSync(program, args, {
    cwd: process.cwd(),
    encoding: "utf8",
    shell: false,
    env: process.env,
    maxBuffer: 8 * 1024 * 1024
  });
}

function safeRelative(file) {
  if (typeof file !== "string" || !file || file.includes("\\") || file.startsWith("/") || file.split("/").includes("..")) {
    fail(`unsafe allowed file: ${String(file)}`);
  }
  return file;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function replacementRequirement(previous, target) {
  return previous.trim().startsWith("=") ? `=${target}` : target;
}

async function raiseManifestFloors(files, packageName, target) {
  const escaped = escapeRegex(packageName);
  let changed = 0;

  for (const file of files.filter((candidate) => candidate.endsWith("Cargo.toml"))) {
    const path = resolve(process.cwd(), file);
    const source = await readFile(path, "utf8");
    let next = source;

    next = next.replace(
      new RegExp(`^(\\s*${escaped}\\s*=\\s*")([^"]+)(".*)$`, "gm"),
      (_match, before, requirement, after) => `${before}${replacementRequirement(requirement, target)}${after}`
    );
    next = next.replace(
      new RegExp(`^(\\s*${escaped}\\s*=\\s*\\{[^}]*?\\bversion\\s*=\\s*")([^"]+)("[^}]*\\}.*)$`, "gm"),
      (_match, before, requirement, after) => `${before}${replacementRequirement(requirement, target)}${after}`
    );
    next = next.replace(
      /^(\s*[A-Za-z0-9_-]+\s*=\s*\{[^}\n]*\bpackage\s*=\s*"([^"]+)"[^}\n]*\bversion\s*=\s*")([^"]+)("[^}\n]*\}.*)$/gm,
      (match, before, aliasedPackage, requirement, after) => aliasedPackage === packageName
        ? `${before}${replacementRequirement(requirement, target)}${after}`
        : match
    );

    const table = new RegExp(`(^\\s*\\[(?:workspace\\.)?(?:dev-|build-)?dependencies\\.${escaped}\\]\\s*$)([\\s\\S]*?)(?=^\\s*\\[|\\s*$)`, "gm");
    next = next.replace(table, (section, header, body) => {
      const updated = body.replace(
        /^(\s*version\s*=\s*")([^"]+)(".*)$/m,
        (_match, before, requirement, after) => `${before}${replacementRequirement(requirement, target)}${after}`
      );
      return `${header}${updated}`;
    });

    if (next !== source) {
      await writeFile(path, next, "utf8");
      changed += 1;
    }
  }
  return changed;
}

async function main() {
  const requestPath = process.argv[2];
  if (!requestPath) fail("usage: dependency-agent.mjs <agent-request.json>");
  const request = JSON.parse(await readFile(resolve(requestPath), "utf8"));
  if (request?.version !== 1 || request?.brief?.version !== 1) fail("unsupported Synaptic request version");

  const brief = request.brief;
  const vendor = String(brief.event?.vendor || "");
  const match = /^cargo:([A-Za-z0-9_.-]+)$/.exec(vendor);
  if (!match) fail(`unsupported dependency ecosystem: ${vendor || "missing"}`);
  const packageName = match[1];
  const target = String(brief.event?.release || "");
  if (!/^[0-9A-Za-z.+_-]{1,80}$/.test(target)) fail("invalid target version");

  const allowedFiles = [...new Set((brief.allowed_files || []).map(safeRelative))];
  if (!allowedFiles.some((file) => file.endsWith("Cargo.lock"))) fail("repair brief does not allow a Cargo lockfile");
  const current = String(brief.applicability?.observed_versions?.[0] || "");
  if (current && !/^[0-9A-Za-z.+_-]{1,80}$/.test(current)) fail("invalid current version");
  const selector = current ? `${packageName}@${current}` : packageName;

  let result = run("cargo", ["update", "-p", selector, "--precise", target, "--offline"]);
  if (result.status !== 0) {
    const manifestsChanged = await raiseManifestFloors(allowedFiles, packageName, target);
    if (manifestsChanged === 0) fail((result.stderr || "cargo update failed").trim().slice(-3000));
    result = run("cargo", ["update", "-p", selector, "--precise", target, "--offline"]);
  }
  if (result.status !== 0) fail((result.stderr || "cargo update failed").trim().slice(-3000));

  const diff = run("git", ["diff", "--binary", "--", ...allowedFiles]);
  if (diff.status !== 0) fail((diff.stderr || "git diff failed").trim().slice(-3000));
  if (!diff.stdout.trim()) fail("dependency update produced no allowed-file changes");

  process.stdout.write(`${JSON.stringify({
    unified_diff: diff.stdout,
    rationale: `Raise ${vendor} to ${target}; Synaptic will independently verify resolution, graph invariants, build, tests, and repository policy.`
  })}\n`);
}

main().catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
});
