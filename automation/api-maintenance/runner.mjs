#!/usr/bin/env node

import { createHash, randomUUID } from "node:crypto";
import { access, mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const CONTRACT = Object.freeze({
  schema_version: 1,
  engine_version: "0.9.14",
  stages: ["repair", "publish"],
  terminal_states: [
    "no_change",
    "not_applicable",
    "review_required",
    "verified",
    "published",
    "failed",
    "inconclusive",
    "stale_base",
    "canceled"
  ]
});
const MAX_OUTPUT_BYTES = 8 * 1024 * 1024;
const TERMINAL_FAILURES = new Set(["repair_failed", "verification_failed"]);
const INCONCLUSIVE_STATES = new Set(["inconclusive"]);
const NON_PUBLISHING_STATES = new Set(["no_change", "not_applicable", "review_required", "planned"]);
const DEPENDENCY_AGENT = fileURLToPath(new URL("./dependency-agent.mjs", import.meta.url));
const HOSTED_NETWORK_GUARD = fileURLToPath(new URL("./network-guard.mjs", import.meta.url));

function fail(message) {
  throw new Error(message);
}

function required(name) {
  const value = process.env[name]?.trim();
  if (!value) fail(`${name} is required`);
  return value;
}

function optional(name, fallback = "") {
  return process.env[name]?.trim() || fallback;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function digest(value, label) {
  const normalized = value.toLowerCase();
  if (!/^[a-f0-9]{64}$/.test(normalized)) fail(`${label} must be a SHA-256 digest`);
  return normalized;
}

function repositoryIdentity(value) {
  if (!/^[A-Za-z0-9._-]+(?:\/[A-Za-z0-9._-]+)+$/.test(value) || value.split("/").some((part) => part === "." || part === "..")) {
    fail("SYNAPTIC_REPOSITORY_IDENTITY must be a canonical provider namespace/repository path");
  }
  return value;
}

function provider(value) {
  if (value !== "github" && value !== "gitlab") fail("SYNAPTIC_PROVIDER must be github or gitlab");
  return value;
}

function automationFamily() {
  const family = optional("SYNAPTIC_AUTOMATION_FAMILY", "api");
  if (family !== "api" && family !== "vulnerability" && family !== "dependency") fail("SYNAPTIC_AUTOMATION_FAMILY must be api, vulnerability, or dependency");
  return family;
}

function networkGuard() {
  const configured = parseArray("SYNAPTIC_NETWORK_GUARD_JSON");
  const hosted = process.platform === "linux" && process.env.GITHUB_ACTIONS === "true";
  const incompatibleHostedUnshare = configured[0] === "unshare" && configured.some((argument) => argument === "--user" || /^-[^-]*U/.test(argument) || argument.startsWith("--map-"));
  if (hosted && (configured.length === 0 || incompatibleHostedUnshare)) return [process.execPath, HOSTED_NETWORK_GUARD];
  return configured;
}

function vulnerabilityFinding() {
  const finding = optional("SYNAPTIC_VULNERABILITY_FINDING");
  if (finding && !/^vuln_finding_[A-Za-z0-9_-]{1,220}$/.test(finding)) fail("SYNAPTIC_VULNERABILITY_FINDING is invalid");
  return finding;
}

function parseArray(name) {
  const source = optional(name, "[]");
  let value;
  try {
    value = JSON.parse(source);
  } catch {
    fail(`${name} must be a JSON array`);
  }
  if (!Array.isArray(value) || value.length > 32 || value.some((item) => typeof item !== "string" || !item.trim() || /[\0\r\n]/.test(item))) {
    fail(`${name} must contain at most 32 non-empty argv strings`);
  }
  return value;
}

function verificationGateSummaries(value) {
  if (!Array.isArray(value) || value.length < 1 || value.length > 50) fail("Verified handoff must contain between 1 and 50 verification gates");
  return value.map((gate) => {
    const name = typeof gate?.gate === "string" ? gate.gate : "";
    const outcome = typeof gate?.outcome === "string" ? gate.outcome : "";
    if (!/^[A-Za-z0-9._:-]{1,120}$/.test(name) || !["passed", "failed", "inconclusive"].includes(outcome)) fail("Verified handoff contains an invalid verification gate summary");
    return { gate: name, outcome };
  });
}

function redact(value) {
  let result = String(value);
  for (const [name, secret] of Object.entries(process.env)) {
    if (secret && secret.length >= 8 && /(TOKEN|SECRET|PASSWORD|PRIVATE_KEY|API_KEY)/i.test(name)) {
      result = result.split(secret).join("[REDACTED]");
    }
  }
  return result;
}

function cloudConfiguration() {
  const rawUrl = optional("SYNAPTIC_CLOUD_URL");
  const policyId = optional("SYNAPTIC_CLOUD_POLICY_ID");
  if (!rawUrl && !policyId) return null;
  if (!rawUrl || !policyId) fail("SYNAPTIC_CLOUD_URL and SYNAPTIC_CLOUD_POLICY_ID must be configured together");
  const url = new URL(rawUrl);
  if (url.protocol !== "https:" || url.username || url.password || url.search || url.hash) fail("SYNAPTIC_CLOUD_URL must be a credential-free HTTPS URL");
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(policyId)) fail("SYNAPTIC_CLOUD_POLICY_ID must be a UUID");
  return {
    url: url.toString().replace(/\/$/, ""),
    policy_id: policyId,
    audience: optional("SYNAPTIC_CLOUD_OIDC_AUDIENCE", "synaptic-api-maintenance"),
    token: null
  };
}

async function cloudOidcToken(configuration) {
  if (configuration.token) return configuration.token;
  let token = optional("SYNAPTIC_OIDC_TOKEN");
  if (!token) {
    const requestUrl = optional("ACTIONS_ID_TOKEN_REQUEST_URL");
    const requestToken = optional("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
    if (!requestUrl || !requestToken) fail("Cloud callbacks require a GitHub or GitLab OIDC token");
    const url = new URL(requestUrl);
    url.searchParams.set("audience", configuration.audience);
    const response = await fetch(url, {
      method: "GET",
      redirect: "error",
      signal: AbortSignal.timeout(10_000),
      headers: { Authorization: `Bearer ${requestToken}`, Accept: "application/json" }
    });
    if (!response.ok) fail("GitHub OIDC token request failed");
    const payload = await response.json();
    token = typeof payload?.value === "string" ? payload.value : "";
  }
  if (!token || token.length > 16 * 1024 || /[\0\r\n]/.test(token)) fail("Cloud OIDC token is invalid");
  configuration.token = token;
  return token;
}

async function cloudRequest(configuration, path, method, body) {
  const response = await fetch(`${configuration.url}${path}`, {
    method,
    redirect: "error",
    signal: AbortSignal.timeout(15_000),
    headers: {
      Authorization: `Bearer ${await cloudOidcToken(configuration)}`,
      "Content-Type": "application/json",
      Accept: "application/json"
    },
    body: JSON.stringify(body)
  });
  const source = await response.text();
  if (Buffer.byteLength(source) > 1024 * 1024) fail("Synaptic Cloud callback response exceeded 1 MiB");
  if (!response.ok) fail(`Synaptic Cloud callback failed with HTTP ${response.status}`);
  try {
    return JSON.parse(source);
  } catch {
    fail("Synaptic Cloud callback returned invalid JSON");
  }
}

async function reportCloudRepair(configuration, repository, engine, outcome, record) {
  if (!configuration) return;
  const externalRunId = optional("GITHUB_RUN_ID") || optional("CI_PIPELINE_ID");
  if (!externalRunId) fail("Cloud callbacks require GITHUB_RUN_ID or CI_PIPELINE_ID");
  const registration = await cloudRequest(configuration, "/api/v1/automation/runs", "POST", {
    policyId: configuration.policy_id,
    repositoryIdentity: repository.identity,
    baseSha: repository.base_sha,
    eventId: String(outcome.event),
    vendor: record.vendor || optional("SYNAPTIC_VENDOR") || undefined,
    policyDigest: String(outcome.policy_digest),
    engineVersion: engine.version,
    engineDigest: engine.digest,
    externalRunId
  });
  const cloudRunId = registration?.data?.id;
  if (typeof cloudRunId !== "string") fail("Synaptic Cloud registration omitted the run id");
  record.cloud_run_id = cloudRunId;
  const state = String(outcome.state);
  if (state === "no_change") {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", { status: "no_change", sequence: 1 });
    return;
  }
  await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", { status: "repairing", sequence: 1 });
  if (state === "verified") {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", {
      status: "verified",
      sequence: 2,
      bundleDigest: record.bundle_digest,
      patchDigest: record.patch_digest,
      verificationGates: record.verification_gates
    });
  } else if (state === "not_applicable") {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", { status: "not_applicable", sequence: 2 });
  } else if (state === "review_required" || state === "planned") {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", { status: "review_required", sequence: 2 });
  } else if (state === "inconclusive") {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", {
      status: "inconclusive",
      sequence: 2,
      errorCode: "VERIFICATION_INCONCLUSIVE",
      errorMessage: "Synaptic could not reach a conclusive verification result."
    });
  } else if (TERMINAL_FAILURES.has(state)) {
    await cloudRequest(configuration, `/api/v1/automation/runs/${cloudRunId}`, "PATCH", {
      status: "failed",
      sequence: 2,
      errorCode: "ENGINE_REPORTED_FAILURE",
      errorMessage: "Synaptic reported a terminal repair or verification failure."
    });
  }
}

async function resolveExecutable(candidate) {
  if (isAbsolute(candidate)) {
    await access(candidate, constants.X_OK).catch(() => fail("SYNAPTIC_BINARY is not executable"));
    return await realpath(candidate);
  }
  const extensions = process.platform === "win32" ? (process.env.PATHEXT || ".EXE;.CMD;.BAT").split(";") : [""];
  for (const directory of (process.env.PATH || "").split(delimiter)) {
    for (const extension of extensions) {
      const path = join(directory, process.platform === "win32" && !candidate.toUpperCase().endsWith(extension.toUpperCase()) ? `${candidate}${extension}` : candidate);
      try {
        await access(path, constants.X_OK);
        return await realpath(path);
      } catch {
        // Keep looking without invoking a shell.
      }
    }
  }
  fail("SYNAPTIC_BINARY was not found on PATH");
}

async function command(executable, args, options = {}) {
  return await new Promise((accept, reject) => {
    const child = spawn(executable, args, {
      cwd: options.cwd,
      shell: false,
      windowsHide: true,
      env: options.env || process.env,
      stdio: ["ignore", "pipe", "pipe"]
    });
    const stdout = [];
    const stderr = [];
    let bytes = 0;
    const collect = (target, chunk) => {
      bytes += chunk.byteLength;
      if (bytes > MAX_OUTPUT_BYTES) {
        child.kill();
        reject(new Error("A child process exceeded the 8 MiB output limit"));
        return;
      }
      target.push(Buffer.from(chunk));
    };
    child.stdout.on("data", (chunk) => collect(stdout, chunk));
    child.stderr.on("data", (chunk) => collect(stderr, chunk));
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      const out = Buffer.concat(stdout).toString("utf8");
      const error = Buffer.concat(stderr).toString("utf8");
      if (code !== 0) reject(new Error(redact(error.trim() || `${options.label || "command"} failed (${signal || code})`)));
      else accept({ stdout: out, stderr: error });
    });
  });
}

function jsonOutput(result, label) {
  try {
    return JSON.parse(result.stdout);
  } catch {
    fail(`${label} returned invalid JSON`);
  }
}

async function engineIdentity() {
  const binary = await resolveExecutable(required("SYNAPTIC_BINARY"));
  const expectedVersion = optional("SYNAPTIC_ENGINE_VERSION", CONTRACT.engine_version);
  if (expectedVersion !== CONTRACT.engine_version) fail(`Runner contract 1 requires Synaptic ${CONTRACT.engine_version}`);
  const expectedDigest = digest(required("SYNAPTIC_ENGINE_SHA256"), "SYNAPTIC_ENGINE_SHA256");
  const actualDigest = sha256(await readFile(binary));
  if (actualDigest !== expectedDigest) fail("Synaptic engine binary digest does not match SYNAPTIC_ENGINE_SHA256");
  const version = await command(binary, ["--version"], { label: "Synaptic version check" });
  if (!new RegExp(`(^|\\s)${expectedVersion.replaceAll(".", "\\.")}($|\\s)`).test(version.stdout.trim())) {
    fail(`Synaptic engine version does not match ${expectedVersion}`);
  }
  return { binary, version: expectedVersion, digest: actualDigest };
}

async function repositoryContext() {
  const root = await realpath(resolve(required("SYNAPTIC_REPOSITORY_ROOT")));
  const identity = repositoryIdentity(required("SYNAPTIC_REPOSITORY_IDENTITY"));
  const expectedBase = required("SYNAPTIC_BASE_SHA").toLowerCase();
  if (!/^[a-f0-9]{40,64}$/.test(expectedBase)) fail("SYNAPTIC_BASE_SHA must be an immutable full commit SHA");
  const actualBase = (await command("git", ["-C", root, "rev-parse", "HEAD"], { label: "Git base check" })).stdout.trim().toLowerCase();
  if (actualBase !== expectedBase) fail("Checkout HEAD does not match SYNAPTIC_BASE_SHA");
  const providerName = provider(optional("SYNAPTIC_PROVIDER", "github"));
  const targetBranch = required("SYNAPTIC_TARGET_BRANCH");
  await command("git", ["check-ref-format", "--branch", targetBranch], { label: "Git target branch validation" });
  return {
    root,
    identity,
    base_sha: actualBase,
    provider: providerName,
    provider_base_url: optional("SYNAPTIC_PROVIDER_BASE_URL", providerName === "github" ? "https://github.com" : "https://gitlab.com"),
    target_branch: targetBranch
  };
}

async function remoteTargetHead(repository) {
  const reference = `refs/heads/${repository.target_branch}`;
  const result = await command("git", ["-C", repository.root, "ls-remote", "--exit-code", "--refs", "origin", reference], { label: "Git remote target check" });
  const matches = result.stdout.trim().split(/\r?\n/).filter(Boolean);
  if (matches.length !== 1) fail("Target branch lookup returned an ambiguous result");
  const [head, returnedReference] = matches[0].split(/\s+/);
  if (!/^[a-f0-9]{40,64}$/.test(head) || returnedReference !== reference) fail("Target branch lookup returned an invalid identity");
  return head;
}

async function prefetchCargoUpgrade(repository, finding) {
  if (String(finding?.package?.ecosystem || "").toLowerCase() !== "cargo") return;
  const name = String(finding.package?.name || "");
  const current = String(finding.resolved_version || "");
  const target = String(finding.remediation?.recommended_version || "");
  if (!/^[A-Za-z0-9_.-]{1,120}$/.test(name) || !/^[A-Za-z0-9.+_-]{1,80}$/.test(current) || !/^[A-Za-z0-9.+_-]{1,80}$/.test(target)) fail("Cargo vulnerability finding has an invalid package or version");
  const sandbox = await mkdtemp(join(tmpdir(), "synaptic-prefetch-"));
  const checkout = join(sandbox, "checkout");
  try {
    await command("git", ["-C", repository.root, "worktree", "add", "--detach", checkout, repository.base_sha], { label: "Cargo prefetch worktree creation" });
    const manifest = join(checkout, "Cargo.toml");
    try {
      await command("cargo", ["update", "--manifest-path", manifest, "-p", `${name}@${current}`, "--precise", target], { cwd: checkout, label: `Cargo target resolution ${name}@${target}` });
      await command("cargo", ["fetch", "--manifest-path", manifest, "--locked"], { cwd: checkout, label: `Cargo target prefetch ${name}@${target}` });
    } catch {
      const fallback = join(sandbox, "fallback");
      await mkdir(join(fallback, "src"), { recursive: true });
      await writeFile(join(fallback, "Cargo.toml"), `[package]\nname = "synaptic-prefetch"\nversion = "0.0.0"\nedition = "2021"\n\n[dependencies]\ntarget = { package = "${name}", version = "=${target}" }\n`, "utf8");
      await writeFile(join(fallback, "src", "lib.rs"), "", "utf8");
      await command("cargo", ["fetch", "--manifest-path", join(fallback, "Cargo.toml")], { cwd: fallback, label: `Cargo fallback prefetch ${name}@${target}` });
    }
  } finally {
    await command("git", ["-C", repository.root, "worktree", "remove", "--force", checkout], { label: "Cargo prefetch worktree cleanup" }).catch(() => undefined);
    await rm(sandbox, { recursive: true, force: true });
  }
}

async function emptyOutputDirectory(root) {
  const configured = optional("SYNAPTIC_HANDOFF_DIRECTORY", join(root, ".synaptic-ci", "handoffs"));
  const directory = resolve(configured);
  await mkdir(directory, { recursive: true });
  if ((await readdir(directory)).length !== 0) fail("SYNAPTIC_HANDOFF_DIRECTORY must be empty at repair-stage start");
  return directory;
}

async function withPublicationCheckout(repository, operation) {
  const sandbox = await mkdtemp(join(tmpdir(), "synaptic-publish-"));
  const checkout = join(sandbox, "checkout");
  try {
    await command("git", ["-C", repository.root, "worktree", "add", "--detach", checkout, repository.base_sha], { label: "Publication worktree creation" });
    return await operation(checkout);
  } finally {
    await command("git", ["-C", repository.root, "worktree", "remove", "--force", checkout], { label: "Publication worktree cleanup" }).catch(() => undefined);
    await rm(sandbox, { recursive: true, force: true });
  }
}

function dependencyBundleDigest(bundle) {
  return sha256(Buffer.from(JSON.stringify({ ...bundle, bundle_digest: "" })));
}

function dependencyBody(bundle) {
  const updates = bundle.updates.slice(0, 100).map((update) => `| \`${update.name}\` | \`${update.from}\` | \`${update.to}\` |`).join("\n");
  const gates = bundle.verification.gates.map((gate) => `- ${gate.gate}: ${gate.outcome}`).join("\n");
  return [
    "## Synaptic verified dependency maintenance",
    "",
    `Ecosystem: **${bundle.ecosystem}**`,
    "",
    "| Dependency | From | To |",
    "| --- | --- | --- |",
    updates || "| lockfile resolution | current | refreshed |",
    "",
    "### Compatibility evidence",
    "",
    gates,
    "",
    `Base: \`${bundle.base_sha}\``,
    `Patch SHA-256: \`${bundle.patch_digest}\``,
    "",
    "This pull request is intentionally draft-only. Synaptic never approves or merges it."
  ].join("\n");
}

async function dependencyOutcomes(engine, repository, mode, offline, guard, outputDirectory) {
  const outcomes = [];
  for (const ecosystem of ["cargo", "github-actions"]) {
    const fallback = sha256(`${repository.base_sha}\0${ecosystem}`).slice(0, 20);
    const policyDigest = sha256(`synaptic-dependency-maintenance-v1\0${ecosystem}`);
    if (offline) {
      outcomes.push({
        run: `dependency_run_${fallback}`,
        event: `dependency_update_${ecosystem.replaceAll("-", "_")}_${fallback}`,
        vendor: ecosystem,
        policy_digest: policyDigest,
        state: "review_required"
      });
      continue;
    }
    const result = await withPublicationCheckout(repository, async (checkout) => jsonOutput(await command(process.execPath, [DEPENDENCY_AGENT, "freshness"], {
      cwd: checkout,
      env: {
        ...process.env,
        SYNAPTIC_REPOSITORY_ROOT: checkout,
        SYNAPTIC_BINARY: engine.binary,
        SYNAPTIC_DEPENDENCY_ECOSYSTEM: ecosystem,
        SYNAPTIC_NETWORK_GUARD_JSON: JSON.stringify(guard)
      },
      label: `Synaptic ${ecosystem} freshness maintenance`
    }), `Synaptic ${ecosystem} freshness maintenance`));
    if (result?.version !== 1 || result.ecosystem !== ecosystem || !["no_change", "verified"].includes(result.state)) fail("Dependency agent returned an invalid outcome contract");
    const run = typeof result.run === "string" ? result.run : `dependency_run_${fallback}`;
    const event = typeof result.event === "string" ? result.event : `dependency_update_${ecosystem.replaceAll("-", "_")}_${fallback}`;
    const outcome = { ...result, run, event, vendor: ecosystem, policy_digest: result.policy_digest || policyDigest };
    if (result.state === "verified") {
      const verification = { gates: verificationGateSummaries(result.verification?.gates) };
      const bundle = {
        version: 1,
        engine_version: engine.version,
        repository_identity: repository.identity,
        provider: repository.provider,
        target_branch: repository.target_branch,
        base_sha: repository.base_sha,
        branch: `synaptic/deps/${ecosystem}-${sha256(result.patch).slice(0, 16)}`,
        run,
        event,
        ecosystem,
        title: result.title,
        updates: result.updates,
        files: result.files,
        patch: result.patch,
        patch_digest: result.patch_digest,
        policy_digest: outcome.policy_digest,
        verification,
        bundle_digest: ""
      };
      bundle.bundle_digest = dependencyBundleDigest(bundle);
      if (mode === "draft_change_request") {
        const bundleName = `${run}.handoff.json`;
        await writeFile(join(outputDirectory, bundleName), `${JSON.stringify(bundle, null, 2)}\n`, { flag: "wx" });
        outcome.bundle = bundleName;
        outcome.bundle_digest = bundle.bundle_digest;
        outcome.patch_digest = bundle.patch_digest;
        outcome.verification_gates = verification.gates;
      } else {
        outcome.state = "planned";
      }
    }
    outcomes.push(outcome);
  }
  return outcomes;
}

async function setOutput(name, value) {
  if (process.env.GITHUB_OUTPUT) {
    const marker = `synaptic_${randomUUID()}`;
    const { appendFile } = await import("node:fs/promises");
    await appendFile(process.env.GITHUB_OUTPUT, `${name}<<${marker}\n${value}\n${marker}\n`, "utf8");
  }
}

async function repair() {
  const engine = await engineIdentity();
  const repository = await repositoryContext();
  const cloud = cloudConfiguration();
  const outputDirectory = await emptyOutputDirectory(repository.root);
  const mode = optional("SYNAPTIC_AUTOMATION_MODE", "report_only");
  if (mode !== "report_only" && mode !== "draft_change_request") fail("SYNAPTIC_AUTOMATION_MODE is invalid");
  const family = automationFamily();
  const offline = optional("SYNAPTIC_OFFLINE", "false") === "true";
  let agentCommand = "";
  let guard = [];
  if (mode === "draft_change_request") {
    if (family !== "dependency") {
      agentCommand = required("SYNAPTIC_AGENT_COMMAND");
      if (!agentCommand.includes("{request}")) fail("SYNAPTIC_AGENT_COMMAND must contain {request}");
    }
    guard = networkGuard();
    if (guard.length === 0) fail("draft_change_request mode requires a network-isolation guard");
  }

  await command(engine.binary, ["extract", repository.root, "--no-store"], { cwd: repository.root, label: "Synaptic extraction" });
  if (family === "api" && optional("SYNAPTIC_REQUIRE_COMPLETE", "true") !== "false") {
    await command(engine.binary, ["api", "coverage", "--root", repository.root, "--graph", join(repository.root, "synaptic-out", "graph.json"), "--require-complete", "--json"], { cwd: repository.root, label: "Synaptic API coverage" });
    await command(engine.binary, ["api", "check-plan", "--root", repository.root, "--require-complete", "--json"], { cwd: repository.root, label: "Synaptic verification plan" });
  }

  let outcomes = [];
  if (family === "api") {
    const runArgs = ["api", "run", "--root", repository.root, "--defer-publish", "--provider", repository.provider, "--provider-base-url", repository.provider_base_url, "--repository", repository.identity, "--target-branch", repository.target_branch, "--json"];
    const vendor = optional("SYNAPTIC_VENDOR");
    if (vendor) runArgs.push("--vendor", vendor);
    if (offline) runArgs.push("--offline");
    if (mode === "report_only") runArgs.push("--dry-run");
    else {
      runArgs.push("--agent-command", agentCommand);
      for (const argument of guard) runArgs.push("--network-guard", argument);
    }
    const composed = jsonOutput(await command(engine.binary, runArgs, { cwd: repository.root, label: "Synaptic deferred API maintenance" }), "Synaptic API maintenance");
    if (composed.publication_deferred !== true || !Array.isArray(composed.outcomes)) fail("Synaptic did not return the deferred-publication outcome contract");
    outcomes = composed.outcomes;
  } else if (family === "vulnerability") {
    const scanArgs = ["vuln", "scan", "--root", repository.root, "--graph", join(repository.root, "synaptic-out", "graph.json"), "--record", "--json", offline ? "--offline" : "--online"];
    const scan = jsonOutput(await command(engine.binary, scanArgs, { cwd: repository.root, label: "Synaptic vulnerability scan" }), "Synaptic vulnerability scan");
    if (!Array.isArray(scan.findings)) fail("Synaptic vulnerability scan omitted findings");
    const requestedFinding = vulnerabilityFinding();
    const eligible = scan.findings.filter((finding) => finding?.verdict?.state === "applicable" && typeof finding?.remediation?.recommended_version === "string");
    const selected = requestedFinding ? eligible.filter((finding) => finding?.id === requestedFinding) : eligible;
    if (requestedFinding && selected.length !== 1) fail("The requested vulnerability is no longer an applicable finding with a fixed target");
    for (const finding of selected) {
      await prefetchCargoUpgrade(repository, finding);
      const runArgs = ["vuln", "run", String(finding.id), "--root", repository.root, "--graph", join(repository.root, "synaptic-out", "graph.json"), "--defer-publish", "--provider", repository.provider, "--provider-base-url", repository.provider_base_url, "--repository", repository.identity, "--target-branch", repository.target_branch, "--json"];
      if (mode === "report_only") runArgs.push("--dry-run");
      else {
        runArgs.push("--agent-command", agentCommand);
        for (const argument of guard) runArgs.push("--network-guard", argument);
      }
      const result = jsonOutput(await command(engine.binary, runArgs, { cwd: repository.root, label: `Synaptic vulnerability repair ${finding.id}` }), "Synaptic vulnerability maintenance");
      if (result.publication_deferred !== true || result.finding !== finding.id || typeof result.run !== "string") fail("Synaptic did not return the vulnerability deferred-publication contract");
      outcomes.push({ ...result, event: result.finding, vendor: String(finding.package || "vulnerability") });
    }
  } else {
    outcomes = await dependencyOutcomes(engine, repository, mode, offline, guard, outputDirectory);
  }

  const runs = [];
  for (const outcome of outcomes) {
    const state = String(outcome.state || "");
    if (state === "verified" && family === "dependency") {
      const record = { run: outcome.run, event: outcome.event, vendor: outcome.vendor, state, bundle: outcome.bundle, bundle_digest: outcome.bundle_digest, patch_digest: outcome.patch_digest, verification_gates: outcome.verification_gates };
      await reportCloudRepair(cloud, repository, engine, outcome, record);
      runs.push(record);
    } else if (state === "verified") {
      const bundleName = `${outcome.run}.handoff.json`;
      const bundlePath = join(outputDirectory, bundleName);
      const exported = jsonOutput(await command(engine.binary, [family === "api" ? "api" : "vuln", "export-run", "--run", String(outcome.run), "--root", repository.root, "--output", bundlePath, "--json"], { cwd: repository.root, label: "Synaptic verified-run export" }), "Synaptic verified-run export");
      const handoff = JSON.parse(await readFile(bundlePath, "utf8"));
      if (handoff.run?.repository_identity !== repository.identity || handoff.run?.base_sha !== repository.base_sha) fail("Verified handoff repository identity or base SHA mismatch");
      const record = { run: outcome.run, event: outcome.event, vendor: outcome.vendor, state, bundle: bundleName, bundle_digest: exported.bundle_digest, patch_digest: exported.patch_digest, verification_gates: verificationGateSummaries(handoff.verification?.gates) };
      await reportCloudRepair(cloud, repository, engine, outcome, record);
      runs.push(record);
    } else if (NON_PUBLISHING_STATES.has(state) || INCONCLUSIVE_STATES.has(state) || TERMINAL_FAILURES.has(state)) {
      const record = { run: outcome.run, event: outcome.event, vendor: outcome.vendor, state };
      await reportCloudRepair(cloud, repository, engine, outcome, record);
      runs.push(record);
    } else {
      fail(`Unsupported Synaptic terminal state: ${state || "missing"}`);
    }
  }

  const manifest = {
    schema_version: CONTRACT.schema_version,
    engine_version: engine.version,
    engine_digest: engine.digest,
    repository_identity: repository.identity,
    provider: repository.provider,
    provider_base_url: repository.provider_base_url,
    target_branch: repository.target_branch,
    base_sha: repository.base_sha,
    family,
    mode,
    generated_at: new Date().toISOString(),
    runs
  };
  const manifestBytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`);
  const manifestPath = join(outputDirectory, "manifest.json");
  await writeFile(manifestPath, manifestBytes, { flag: "wx" });
  const manifestDigest = sha256(manifestBytes);
  await setOutput("manifest_digest", manifestDigest);
  await setOutput("base_sha", repository.base_sha);
  await setOutput("has_verified", String(runs.some((run) => run.state === "verified")));
  await writeFile(join(outputDirectory, "repair.env"), [
    `SYNAPTIC_EXPECTED_HANDOFF_DIGEST=${manifestDigest}`,
    `SYNAPTIC_REPAIR_BASE_SHA=${repository.base_sha}`
  ].join("\n") + "\n", "utf8");
  process.stdout.write(`${JSON.stringify({ schema_version: 1, stage: "repair", manifest: manifestPath, manifest_digest: manifestDigest, base_sha: repository.base_sha, runs: runs.length })}\n`);
  if (runs.some((run) => TERMINAL_FAILURES.has(run.state) || INCONCLUSIVE_STATES.has(run.state))) fail(`One or more ${family} maintenance runs failed or were inconclusive`);
}

function verifiedDependencyBundle(value, repository, engine, expectedDigest) {
  if (!value || value.version !== 1 || value.engine_version !== engine.version) fail("Dependency handoff engine identity is invalid");
  if (value.repository_identity !== repository.identity || value.provider !== repository.provider || value.target_branch !== repository.target_branch || value.base_sha !== repository.base_sha) fail("Dependency handoff repository identity is invalid");
  if (value.ecosystem !== "cargo" && value.ecosystem !== "github-actions") fail("Dependency handoff ecosystem is invalid");
  if (!/^dependency_run_[a-f0-9]{20}$/.test(value.run) || !/^dependency_update_(?:cargo|github_actions)_[a-f0-9]{20}$/.test(value.event)) fail("Dependency handoff run identity is invalid");
  if (!/^synaptic\/deps\/(?:cargo|github-actions)-[a-f0-9]{16}$/.test(value.branch)) fail("Dependency handoff branch is invalid");
  if (typeof value.title !== "string" || value.title.length < 1 || value.title.length > 200 || /[\0\r\n]/.test(value.title)) fail("Dependency handoff title is invalid");
  if (!Array.isArray(value.updates) || value.updates.length > 500 || value.updates.some((update) => !update || ![update.name, update.from, update.to].every((item) => typeof item === "string" && item.length > 0 && item.length <= 240 && !/[\0\r\n|]/.test(item)))) fail("Dependency handoff updates are invalid");
  if (!Array.isArray(value.files) || value.files.length < 1 || value.files.length > 100 || value.files.some((file) => typeof file !== "string" || !file || file.includes("\\") || file.startsWith("/") || file.split("/").includes(".."))) fail("Dependency handoff files are invalid");
  if (typeof value.patch !== "string" || !value.patch.trim() || Buffer.byteLength(value.patch) > MAX_OUTPUT_BYTES) fail("Dependency handoff patch is invalid");
  if (sha256(value.patch) !== digest(value.patch_digest, "dependency patch digest")) fail("Dependency handoff patch digest mismatch");
  if (dependencyBundleDigest(value) !== digest(value.bundle_digest, "dependency bundle digest") || value.bundle_digest !== digest(expectedDigest, "bundle digest")) fail("Dependency handoff bundle digest mismatch");
  value.verification = { gates: verificationGateSummaries(value.verification?.gates) };
  return value;
}

async function pushDependencyBranch(repository, checkout, branch) {
  const reference = `refs/heads/${branch}`;
  const remote = await command("git", ["-C", checkout, "ls-remote", "--refs", "origin", reference], { label: "Dependency branch lookup" });
  const head = remote.stdout.trim().split(/\s+/)[0] || "";
  if (head && !/^[a-f0-9]{40,64}$/.test(head)) fail("Dependency branch lookup returned an invalid commit");
  await command("git", ["-C", checkout, "push", `--force-with-lease=${reference}:${head}`, "origin", `HEAD:${reference}`], { label: "Dependency branch push" });
}

async function publishGitHubDependency(repository, bundle) {
  const host = new URL(repository.provider_base_url).hostname.toLowerCase();
  const repositoryArgument = host === "github.com" ? repository.identity : `${host}/${repository.identity}`;
  const listed = jsonOutput(await command("gh", ["pr", "list", "--repo", repositoryArgument, "--head", bundle.branch, "--base", repository.target_branch, "--state", "open", "--limit", "1", "--json", "number,url"], { label: "GitHub dependency pull-request lookup" }), "GitHub dependency pull-request lookup");
  const existing = Array.isArray(listed) ? listed[0] : null;
  const body = dependencyBody(bundle);
  if (existing?.number && existing?.url) {
    await command("gh", ["pr", "edit", String(existing.number), "--repo", repositoryArgument, "--title", bundle.title, "--body", body], { label: "GitHub dependency pull-request update" });
    return { publish: { kind: "pull_request", number: existing.number, url: existing.url, branch: bundle.branch, draft: true } };
  }
  const created = await command("gh", ["pr", "create", "--repo", repositoryArgument, "--draft", "--base", repository.target_branch, "--head", bundle.branch, "--title", bundle.title, "--body", body], { label: "GitHub dependency pull-request creation" });
  const url = created.stdout.match(/https:\/\/[^\s]+\/pull\/\d+/)?.[0];
  const number = url?.match(/\/pull\/(\d+)/)?.[1];
  if (!url || !number) fail("GitHub dependency pull-request creation omitted its identity");
  return { publish: { kind: "pull_request", number, url, branch: bundle.branch, draft: true } };
}

async function publishGitLabDependency(repository, bundle) {
  const listed = jsonOutput(await command("glab", ["mr", "list", "--source-branch", bundle.branch, "--target-branch", repository.target_branch, "--state", "opened", "--output", "json"], { label: "GitLab dependency merge-request lookup" }), "GitLab dependency merge-request lookup");
  const existing = Array.isArray(listed) ? listed[0] : null;
  const body = dependencyBody(bundle);
  if (existing?.iid && existing?.web_url) {
    await command("glab", ["mr", "update", String(existing.iid), "--title", bundle.title, "--description", body], { label: "GitLab dependency merge-request update" });
    return { publish: { kind: "merge_request", number: existing.iid, url: existing.web_url, branch: bundle.branch, draft: true } };
  }
  const created = await command("glab", ["mr", "create", "--draft", "--source-branch", bundle.branch, "--target-branch", repository.target_branch, "--title", bundle.title, "--description", body, "--yes"], { label: "GitLab dependency merge-request creation" });
  const url = created.stdout.match(/https:\/\/[^\s]+\/-\/merge_requests\/\d+/)?.[0];
  const number = url?.match(/\/merge_requests\/(\d+)/)?.[1];
  if (!url || !number) fail("GitLab dependency merge-request creation omitted its identity");
  return { publish: { kind: "merge_request", number, url, branch: bundle.branch, draft: true } };
}

async function publishDependencyBundle(repository, engine, bundlePath, expectedDigest) {
  const source = await readFile(bundlePath, "utf8");
  if (Buffer.byteLength(source) > 16 * 1024 * 1024) fail("Dependency handoff exceeds 16 MiB");
  const bundle = verifiedDependencyBundle(JSON.parse(source), repository, engine, expectedDigest);
  return withPublicationCheckout(repository, async (checkout) => {
    const patchPath = join(dirname(checkout), `${bundle.run}.patch`);
    await writeFile(patchPath, bundle.patch, { flag: "wx" });
    await command("git", ["-C", checkout, "apply", "--check", "--binary", patchPath], { label: "Dependency patch validation" });
    await command("git", ["-C", checkout, "apply", "--binary", patchPath], { label: "Dependency patch application" });
    const changed = (await command("git", ["-C", checkout, "diff", "--name-only", "--diff-filter=ACMRTUXB"], { label: "Dependency changed-file validation" })).stdout.trim().split(/\r?\n/).filter(Boolean).map((file) => file.replaceAll("\\", "/")).sort();
    if (JSON.stringify(changed) !== JSON.stringify([...bundle.files].sort())) fail("Dependency handoff changed-file set mismatch");
    await command("git", ["-C", checkout, "switch", "-c", bundle.branch], { label: "Dependency branch creation" });
    await command("git", ["-C", checkout, "config", "user.name", "synaptic-bot"], { label: "Dependency commit author" });
    await command("git", ["-C", checkout, "config", "user.email", "synaptic-bot@users.noreply.github.com"], { label: "Dependency commit author" });
    await command("git", ["-C", checkout, "add", "--", ...bundle.files], { label: "Dependency patch staging" });
    await command("git", ["-C", checkout, "commit", "-m", bundle.title], { label: "Dependency verified commit" });
    await pushDependencyBranch(repository, checkout, bundle.branch);
    return repository.provider === "github" ? publishGitHubDependency(repository, bundle) : publishGitLabDependency(repository, bundle);
  });
}

async function publish() {
  const engine = await engineIdentity();
  const repository = await repositoryContext();
  const cloud = cloudConfiguration();
  const manifestPath = resolve(required("SYNAPTIC_HANDOFF_MANIFEST"));
  const bytes = await readFile(manifestPath);
  const expectedDigest = digest(required("SYNAPTIC_EXPECTED_HANDOFF_DIGEST"), "SYNAPTIC_EXPECTED_HANDOFF_DIGEST");
  if (sha256(bytes) !== expectedDigest) fail("Handoff manifest digest mismatch");
  const manifest = JSON.parse(bytes.toString("utf8"));
  if (manifest.schema_version !== CONTRACT.schema_version || manifest.engine_version !== engine.version || manifest.engine_digest !== engine.digest) fail("Handoff runner/engine identity mismatch");
  if (manifest.family !== "api" && manifest.family !== "vulnerability" && manifest.family !== "dependency") fail("Handoff manifest maintenance family is invalid");
  if (manifest.family !== automationFamily()) fail("Handoff maintenance family does not match the publication job");
  if (manifest.repository_identity !== repository.identity || manifest.base_sha !== repository.base_sha || manifest.provider !== repository.provider || manifest.target_branch !== repository.target_branch) fail("Handoff publication context mismatch");
  if (!Array.isArray(manifest.runs)) fail("Handoff manifest runs must be an array");
  const results = [];
  const commandFamily = manifest.family === "api" ? "api" : "vuln";
  const verifiedRuns = manifest.runs.filter((run) => run?.state === "verified");
  if (verifiedRuns.length > 0) {
    const currentBase = await remoteTargetHead(repository);
    if (currentBase !== repository.base_sha) {
      for (const run of verifiedRuns) {
        if (cloud && !run.cloud_run_id) fail("Verified Cloud-managed run omitted cloud_run_id");
        if (cloud) {
          await cloudRequest(cloud, `/api/v1/automation/runs/${run.cloud_run_id}`, "PATCH", {
            status: "stale_base",
            sequence: 3,
            errorCode: "STALE_BASE",
            errorMessage: "The target branch moved after repair verification; rerun maintenance on the new base."
          });
        }
        results.push({ run: run.run, state: "stale_base", expected_base_sha: repository.base_sha, current_base_sha: currentBase });
      }
      const resultPath = resolve(optional("SYNAPTIC_PUBLICATION_RESULT", join(dirname(manifestPath), "publication.json")));
      await writeFile(resultPath, `${JSON.stringify({ schema_version: 1, stage: "publish", results }, null, 2)}\n`, "utf8");
      process.stdout.write(`${JSON.stringify({ schema_version: 1, stage: "publish", published: 0, stale: results.length, result: resultPath })}\n`);
      return;
    }
  }
  for (const run of manifest.runs) {
    if (run.state !== "verified") continue;
    if (cloud && !run.cloud_run_id) fail("Verified Cloud-managed run omitted cloud_run_id");
    if (cloud) await cloudRequest(cloud, `/api/v1/automation/runs/${run.cloud_run_id}`, "PATCH", { status: "publishing", sequence: 3 });
    const bundlePath = resolve(dirname(manifestPath), run.bundle);
    const published = manifest.family === "dependency"
      ? await publishDependencyBundle(repository, engine, bundlePath, run.bundle_digest)
      : await withPublicationCheckout(repository, async (checkout) => {
        await command(engine.binary, [commandFamily, "import-run", "--bundle", bundlePath, "--expected-digest", digest(run.bundle_digest, "bundle digest"), "--root", checkout, "--json"], { cwd: checkout, label: "Synaptic verified-run import" });
        return jsonOutput(await command(engine.binary, [commandFamily, "publish", "--run", String(run.run), "--root", checkout, "--provider", repository.provider, "--provider-base-url", repository.provider_base_url, "--repository", repository.identity, "--target-branch", repository.target_branch, "--json"], { cwd: checkout, label: "Synaptic draft change-request publication" }), "Synaptic publication");
      });
    const change = published.publish;
    if (cloud) {
      const externalId = change?.number ?? String(change?.url || "").match(/\/(\d+)(?:[/?#]|$)/)?.[1];
      if (!change?.kind || !change?.url || !externalId) fail("Synaptic publication omitted the change request identity");
      await cloudRequest(cloud, `/api/v1/automation/runs/${run.cloud_run_id}`, "PATCH", {
        status: "published",
        sequence: 4,
        changeRequestKind: change.kind,
        changeRequestExternalId: String(externalId),
        changeRequestUrl: change.url
      });
    }
    results.push({ run: run.run, publish: published.publish });
  }
  const resultPath = resolve(optional("SYNAPTIC_PUBLICATION_RESULT", join(dirname(manifestPath), "publication.json")));
  await writeFile(resultPath, `${JSON.stringify({ schema_version: 1, stage: "publish", results }, null, 2)}\n`, "utf8");
  process.stdout.write(`${JSON.stringify({ schema_version: 1, stage: "publish", published: results.length, result: resultPath })}\n`);
}

async function main() {
  const stage = process.argv[2];
  if (stage === "contract") {
    process.stdout.write(`${JSON.stringify(CONTRACT, null, 2)}\n`);
    return;
  }
  if (stage === "repair") return await repair();
  if (stage === "publish") return await publish();
  fail("Usage: runner.mjs contract|repair|publish");
}

main().catch((error) => {
  process.stderr.write(`Synaptic automation failed: ${redact(error instanceof Error ? error.message : error)}\n`);
  process.exitCode = 1;
});
