#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_OUTPUT_BYTES = 8 * 1024 * 1024;
const SECRET_NAME = /(TOKEN|SECRET|PASSWORD|PRIVATE_KEY|API_KEY|ACTIONS_ID_TOKEN)/i;

function fail(message) {
  throw new Error(message);
}

function run(program, args, options = {}) {
  const result = spawnSync(program, args, {
    cwd: options.cwd || process.cwd(),
    encoding: "utf8",
    shell: false,
    env: options.env || process.env,
    maxBuffer: MAX_OUTPUT_BYTES
  });
  if (result.error) throw result.error;
  if (result.status !== 0) fail((result.stderr || `${program} failed with ${result.status}`).trim().slice(-4_000));
  return result.stdout;
}

function safeEnvironment(extra = {}) {
  return Object.fromEntries([
    ...Object.entries(process.env).filter(([name]) => !SECRET_NAME.test(name)),
    ...Object.entries(extra)
  ]);
}

function guarded(program, args, cwd, guard) {
  const env = safeEnvironment({ CARGO_NET_OFFLINE: "true", SYNAPTIC_OFFLINE: "1" });
  return guard.length
    ? run(guard[0], [...guard.slice(1), program, ...args], { cwd, env })
    : run(program, args, { cwd, env });
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function semver(value) {
  const match = /^v?(0|[1-9][0-9]*)(?:\.(0|[1-9][0-9]*))?(?:\.(0|[1-9][0-9]*))?$/.exec(value);
  return match ? match.slice(1).map((part) => Number(part || 0)) : null;
}

export function compareSemver(left, right) {
  const a = semver(left);
  const b = semver(right);
  if (!a || !b) fail("invalid stable semantic version");
  for (let index = 0; index < 3; index += 1) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return 0;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function replacementRequirement(previous, target) {
  return previous.trim().startsWith("=") ? `=${target}` : target;
}

export function raiseManifestFloorSource(source, packageName, target) {
  const escaped = escapeRegex(packageName);
  let next = source.replace(
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
  return next.replace(table, (section, header, body) => `${header}${body.replace(
    /^(\s*version\s*=\s*")([^"]+)(".*)$/m,
    (_match, before, requirement, after) => `${before}${replacementRequirement(requirement, target)}${after}`
  )}`);
}

async function raiseManifestFloors(files, packageName, target) {
  let changed = 0;
  for (const file of files) {
    const source = await readFile(file, "utf8");
    const next = raiseManifestFloorSource(source, packageName, target);
    if (next !== source) {
      await writeFile(file, next, "utf8");
      changed += 1;
    }
  }
  return changed;
}

function directCargoDependencies(metadata, root) {
  const workspace = new Set(metadata.workspace_members || []);
  const packages = new Map(metadata.packages.map((item) => [item.id, item]));
  const direct = new Map();
  for (const node of metadata.resolve?.nodes || []) {
    if (!workspace.has(node.id)) continue;
    const owner = packages.get(node.id);
    for (const edge of node.deps || []) {
      const item = packages.get(edge.pkg);
      if (!item?.source?.includes("crates.io-index") || !semver(item.version)) continue;
      const manifest = resolve(owner.manifest_path);
      const manifestRelative = relative(root, manifest);
      if (!manifestRelative || manifestRelative.startsWith("..") || resolve(root, manifestRelative) !== manifest) continue;
      const current = direct.get(item.name) || { name: item.name, version: item.version, manifests: new Set() };
      if (compareSemver(item.version, current.version) > 0) current.version = item.version;
      current.manifests.add(manifest);
      direct.set(item.name, current);
    }
  }
  return direct;
}

async function latestCrateVersion(name) {
  const response = await fetch(`https://crates.io/api/v1/crates/${encodeURIComponent(name)}`, {
    headers: { Accept: "application/json", "User-Agent": "Synaptic dependency maintenance/1" },
    redirect: "error",
    signal: AbortSignal.timeout(15_000)
  });
  if (!response.ok) fail(`crates.io returned HTTP ${response.status} for ${name}`);
  const payload = await response.json();
  const version = payload?.crate?.max_stable_version;
  if (typeof version !== "string" || !semver(version)) fail(`crates.io omitted a stable version for ${name}`);
  return version;
}

async function mapLimit(values, limit, operation) {
  const output = new Array(values.length);
  let cursor = 0;
  await Promise.all(Array.from({ length: Math.min(limit, values.length) }, async () => {
    while (cursor < values.length) {
      const index = cursor;
      cursor += 1;
      output[index] = await operation(values[index]);
    }
  }));
  return output;
}

function cargoMetadata(root) {
  return JSON.parse(run("cargo", ["metadata", "--format-version", "1", "--locked"], { cwd: root }));
}

function versionSets(metadata) {
  const result = new Map();
  for (const item of metadata.packages) {
    if (!item.source?.includes("crates.io-index") || !semver(item.version)) continue;
    const versions = result.get(item.name) || new Set();
    versions.add(item.version);
    result.set(item.name, versions);
  }
  return result;
}

function cargoChanges(before, after) {
  const previous = versionSets(before);
  const current = versionSets(after);
  return [...current.entries()].flatMap(([name, versions]) => {
    const old = [...(previous.get(name) || [])].sort(compareSemver);
    const next = [...versions].sort(compareSemver);
    return old.join(",") !== next.join(",") ? [{ name, from: old.join(", ") || "unresolved", to: next.join(", ") }] : [];
  }).sort((left, right) => left.name.localeCompare(right.name));
}

async function updateCargo(root) {
  if (!await readFile(join(root, "Cargo.toml"), "utf8").catch(() => null)) return [];
  const before = cargoMetadata(root);
  const direct = directCargoDependencies(before, root);
  run("cargo", ["update", "--workspace"], { cwd: root });
  const compatible = cargoMetadata(root);
  const compatibleDirect = directCargoDependencies(compatible, root);
  const names = [...direct.keys()].sort();
  const latest = await mapLimit(names, 8, latestCrateVersion);
  for (let index = 0; index < names.length; index += 1) {
    const name = names[index];
    const target = latest[index];
    const original = direct.get(name);
    const resolved = compatibleDirect.get(name)?.version || original.version;
    const originalVersion = semver(original.version);
    const targetVersion = semver(target);
    if (targetVersion[0] !== originalVersion[0] || compareSemver(target, resolved) <= 0) continue;
    if (await raiseManifestFloors([...original.manifests], name, target) === 0) continue;
    run("cargo", ["update", "-p", `${name}@${resolved}`, "--precise", target], { cwd: root });
  }
  run("cargo", ["fetch", "--locked"], { cwd: root });
  return cargoChanges(before, cargoMetadata(root));
}

function actionTags(source) {
  const tags = new Map();
  for (const line of source.split(/\r?\n/)) {
    const [sha, reference] = line.trim().split(/\s+/);
    const match = /^refs\/tags\/(v(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){0,2})(\^\{\})?$/.exec(reference || "");
    if (!/^[0-9a-f]{40}$/i.test(sha || "") || !match || !semver(match[1])) continue;
    const record = tags.get(match[1]) || {};
    record[match[2] ? "commit" : "tag"] = sha.toLowerCase();
    tags.set(match[1], record);
  }
  return [...tags.entries()].map(([tag, hashes]) => ({ tag, version: semver(tag), sha: hashes.commit || hashes.tag }))
    .sort((left, right) => compareSemver(right.tag, left.tag));
}

function actionReferences(source) {
  return [...source.matchAll(/\buses:\s*["']?([A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+)@(v\d+(?:\.\d+){0,2}|[0-9a-f]{40})["']?/gi)]
    .map((match) => ({ action: match[1], ref: match[2] }));
}

export function rewriteGitHubActions(source, targets) {
  return source.replace(
    /(\buses:\s*["']?)([A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+)@(v\d+(?:\.\d+){0,2}|[0-9a-f]{40})(["']?)/gi,
    (match, prefix, action, current, suffix) => {
      const target = targets.get(action.toLowerCase());
      if (!target) return match;
      const next = /^[0-9a-f]{40}$/i.test(current)
        ? target.sha
        : current.split(".").length === 1 ? `v${target.version[0]}` : target.tag;
      return next && next !== current ? `${prefix}${action}@${next}${suffix}` : match;
    }
  );
}

async function updateGitHubActions(root) {
  const directory = join(root, ".github", "workflows");
  const names = await readdir(directory).catch(() => []);
  const files = names.filter((name) => /\.ya?ml$/i.test(name)).map((name) => join(directory, name));
  const sources = new Map(await Promise.all(files.map(async (file) => [file, await readFile(file, "utf8")])));
  const refs = [...new Map([...sources.values()].flatMap(actionReferences).map((item) => [item.action.toLowerCase(), item])).values()];
  const targets = new Map();
  for (const item of refs) {
    const remote = run("git", ["ls-remote", "--tags", `https://github.com/${item.action}.git`], { cwd: root });
    const tags = actionTags(remote);
    if (!tags.length) continue;
    targets.set(item.action.toLowerCase(), tags[0]);
  }
  const updates = [];
  for (const [file, source] of sources) {
    const next = rewriteGitHubActions(source, targets);
    if (next === source) continue;
    for (const before of actionReferences(source)) {
      const after = actionReferences(next).find((candidate) => candidate.action === before.action && candidate.ref !== before.ref);
      if (after) updates.push({ name: before.action, from: before.ref, to: after.ref });
    }
    await writeFile(file, next, "utf8");
  }
  return [...new Map(updates.map((item) => [`${item.name}:${item.from}:${item.to}`, item])).values()];
}

export function splitCommand(source) {
  const args = [];
  let current = "";
  let quote = "";
  for (let index = 0; index < source.length; index += 1) {
    const character = source[index];
    if (quote) {
      if (character === quote) quote = "";
      else if (character === "\\" && quote === '"' && index + 1 < source.length) current += source[++index];
      else current += character;
    } else if (character === '"' || character === "'") quote = character;
    else if (/\s/.test(character)) {
      if (current) {
        args.push(current);
        current = "";
      }
    } else current += character;
  }
  if (quote) fail("verification command has an unterminated quote");
  if (current) args.push(current);
  if (!args.length || args.some((item) => /[\0\r\n]/.test(item))) fail("verification command is invalid");
  return args;
}

async function verify(root, engine, guard) {
  const gates = [];
  guarded(engine, ["extract", root, "--no-store"], root, []);
  gates.push({ gate: "graph_extract", outcome: "passed" });
  const plan = JSON.parse(guarded(engine, ["api", "check-plan", "--root", root, "--json"], root, []));
  const commands = [];
  for (const project of plan.projects || []) {
    for (const source of [...(project.checks || []), ...(project.tests || [])]) {
      const key = `${project.root || ""}\0${source}`;
      if (!commands.some((item) => item.key === key)) commands.push({ key, root: project.root || "", source });
    }
  }
  if (!commands.length) fail("Synaptic found no authoritative build or test command for dependency verification");
  if (commands.length > 50) fail("Synaptic verification plan exceeds 50 commands");
  for (let index = 0; index < commands.length; index += 1) {
    const item = commands[index];
    const argv = splitCommand(item.source);
    if (argv[0] === "cargo" && !argv.includes("--locked")) argv.push("--locked");
    guarded(argv[0], argv.slice(1), resolve(root, item.root), guard);
    gates.push({ gate: `compatibility_${index + 1}`, outcome: "passed" });
  }
  run("git", ["diff", "--check"], { cwd: root });
  gates.push({ gate: "patch_policy", outcome: "passed" });
  return gates;
}

function safeChangedFile(root, file, ecosystem) {
  const normalized = file.replaceAll("\\", "/");
  if (!normalized || normalized.startsWith("/") || normalized.split("/").includes("..")) fail("dependency update produced an unsafe path");
  if (ecosystem === "cargo" && normalized !== "Cargo.lock" && !normalized.endsWith("Cargo.toml")) fail(`Cargo update changed disallowed file ${normalized}`);
  if (ecosystem === "github-actions" && !/^\.github\/workflows\/[^/]+\.ya?ml$/i.test(normalized)) fail(`GitHub Actions update changed disallowed file ${normalized}`);
  const absolute = resolve(root, normalized);
  if (relative(root, absolute).startsWith("..")) fail("dependency update escaped the repository");
  return normalized;
}

async function freshness() {
  const root = resolve(process.env.SYNAPTIC_REPOSITORY_ROOT || ".");
  const engine = resolve(process.env.SYNAPTIC_BINARY || "");
  const ecosystem = process.env.SYNAPTIC_DEPENDENCY_ECOSYSTEM;
  if (ecosystem !== "cargo" && ecosystem !== "github-actions") fail("SYNAPTIC_DEPENDENCY_ECOSYSTEM must be cargo or github-actions");
  const guard = JSON.parse(process.env.SYNAPTIC_NETWORK_GUARD_JSON || "[]");
  if (!Array.isArray(guard) || guard.some((item) => typeof item !== "string" || !item)) fail("SYNAPTIC_NETWORK_GUARD_JSON is invalid");
  const updates = ecosystem === "cargo" ? await updateCargo(root) : await updateGitHubActions(root);
  const files = run("git", ["diff", "--name-only", "--diff-filter=ACMRTUXB"], { cwd: root }).trim().split(/\r?\n/).filter(Boolean).map((file) => safeChangedFile(root, file, ecosystem));
  if (!files.length) {
    process.stdout.write(`${JSON.stringify({ version: 1, ecosystem, state: "no_change", updates: [] })}\n`);
    return;
  }
  const gates = await verify(root, engine, guard);
  const patch = run("git", ["diff", "--binary", "--", ...files], { cwd: root });
  if (!patch.trim() || Buffer.byteLength(patch) > MAX_OUTPUT_BYTES) fail("dependency patch is empty or exceeds 8 MiB");
  const identity = sha256(JSON.stringify({ ecosystem, updates, patch })).slice(0, 20);
  process.stdout.write(`${JSON.stringify({
    version: 1,
    ecosystem,
    state: "verified",
    run: `dependency_run_${identity}`,
    event: `dependency_update_${ecosystem.replaceAll("-", "_")}_${identity}`,
    vendor: ecosystem,
    title: ecosystem === "cargo" ? "chore(deps): update Cargo dependencies" : "chore(deps): update GitHub Actions",
    updates,
    files,
    patch,
    patch_digest: sha256(patch),
    policy_digest: sha256(`synaptic-dependency-maintenance-v1\0${ecosystem}`),
    verification: { gates }
  })}\n`);
}

async function vulnerabilityAgent(requestPath) {
  const request = JSON.parse(await readFile(resolve(requestPath), "utf8"));
  if (request?.version !== 1 || request?.brief?.version !== 1) fail("unsupported Synaptic request version");
  const brief = request.brief;
  const match = /^cargo:([A-Za-z0-9_.-]+)$/.exec(String(brief.event?.vendor || ""));
  if (!match) fail("the bundled vulnerability agent supports Cargo upgrades only");
  const target = String(brief.event?.release || "");
  if (!semver(target)) fail("invalid target version");
  const allowed = [...new Set((brief.allowed_files || []).filter((file) => typeof file === "string" && !file.includes("\\") && !file.startsWith("/") && !file.split("/").includes("..")))];
  if (!allowed.some((file) => file.endsWith("Cargo.lock"))) fail("repair brief does not allow a Cargo lockfile");
  const manifests = allowed.filter((file) => file.endsWith("Cargo.toml")).map((file) => resolve(process.cwd(), file));
  const current = String(brief.applicability?.observed_versions?.[0] || "");
  let selector = current ? `${match[1]}@${current}` : match[1];
  let result = spawnSync("cargo", ["update", "-p", selector, "--precise", target, "--offline"], { cwd: process.cwd(), encoding: "utf8", shell: false, maxBuffer: MAX_OUTPUT_BYTES });
  if (result.status !== 0 && await raiseManifestFloors(manifests, match[1], target)) {
    result = spawnSync("cargo", ["update", "-p", selector, "--precise", target, "--offline"], { cwd: process.cwd(), encoding: "utf8", shell: false, maxBuffer: MAX_OUTPUT_BYTES });
  }
  if (result.status !== 0) fail((result.stderr || "cargo update failed").trim().slice(-3_000));
  const patch = run("git", ["diff", "--binary", "--", ...allowed]);
  if (!patch.trim()) fail("dependency update produced no allowed-file changes");
  process.stdout.write(`${JSON.stringify({ unified_diff: patch, rationale: `Raise cargo:${match[1]} to ${target}; Synaptic will independently verify resolution, graph invariants, build, tests, and repository policy.` })}\n`);
}

function selfTest() {
  if (compareSemver("1.2.3", "1.2.2") <= 0) fail("semver comparison failed");
  const manifest = raiseManifestFloorSource('demo = { version = "0.9", features = ["x"] }\n', "demo", "0.10.1");
  if (!manifest.includes('version = "0.10.1"')) fail("Cargo manifest rewrite failed");
  const targets = new Map([["actions/checkout", { tag: "v7.1.0", version: [7, 1, 0], sha: "a".repeat(40) }]]);
  const workflow = rewriteGitHubActions("- uses: actions/checkout@v5\n", targets);
  if (!workflow.includes("actions/checkout@v7")) fail("GitHub Action rewrite failed");
  if (splitCommand('cargo test --package "hello world"').at(-1) !== "hello world") fail("command parsing failed");
  process.stdout.write('{"ok":true}\n');
}

async function main() {
  const action = process.argv[2];
  if (action === "freshness") return freshness();
  if (action === "self-test") return selfTest();
  if (action) return vulnerabilityAgent(action);
  fail("usage: dependency-agent.mjs freshness|self-test|<agent-request.json>");
}

if (resolve(process.argv[1] || "") === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
