#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const SECRET_NAME = /(TOKEN|SECRET|PASSWORD|PRIVATE_KEY|API_KEY|ACTIONS_ID_TOKEN)/i;

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function safeEnvironment() {
  return Object.fromEntries(Object.entries(process.env).filter(([name]) => !SECRET_NAME.test(name)));
}

function run(program, args, env) {
  const result = spawnSync(program, args, { cwd: process.cwd(), env, shell: false, stdio: "inherit" });
  if (result.error) fail(result.error.message);
  if (result.signal) fail(`${program} was terminated by ${result.signal}`);
  process.exit(result.status ?? 1);
}

const args = process.argv.slice(2);
if (args[0] === "--inside") {
  if (process.platform !== "linux" || process.getuid?.() !== 0) fail("network guard did not enter the privileged Linux namespace");
  const uid = Number(args[1]);
  const gid = Number(args[2]);
  const home = args[3];
  const path = args[4];
  const program = args[5];
  if (!Number.isSafeInteger(uid) || uid < 1 || !Number.isSafeInteger(gid) || gid < 1 || !/^\/[^\0\r\n]*$/.test(home || "") || !path || /[\0\r\n]/.test(path) || !program) fail("network guard received an invalid identity or command");
  const loopback = spawnSync("ip", ["link", "set", "lo", "up"], { shell: false, stdio: "inherit" });
  if (loopback.error || loopback.status !== 0) fail(loopback.error?.message || "network guard could not enable isolated loopback");
  process.setgroups([]);
  process.setgid(gid);
  process.setuid(uid);
  run(program, args.slice(6), { ...safeEnvironment(), HOME: home, PATH: path });
}

if (args[0] === "self-test") {
  process.env.SYNAPTIC_TEST_TOKEN = "removed";
  if (safeEnvironment().SYNAPTIC_TEST_TOKEN) fail("network guard retained a secret");
  process.stdout.write('{"ok":true}\n');
  process.exit(0);
}

if (process.platform !== "linux" || process.env.GITHUB_ACTIONS !== "true") fail("the built-in network guard is only available on GitHub-hosted Linux runners");
if (process.getuid?.() === 0 || !args[0]) fail("network guard requires a non-root runner identity and a command");

run("sudo", [
  "-n",
  "-E",
  "unshare",
  "--net",
  "--fork",
  "--kill-child",
  "--",
  process.execPath,
  fileURLToPath(import.meta.url),
  "--inside",
  String(process.getuid()),
  String(process.getgid()),
  process.env.HOME || process.cwd(),
  process.env.PATH || "/usr/local/bin:/usr/bin:/bin",
  ...args
], safeEnvironment());
