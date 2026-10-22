// install-service / uninstall-service for real, on the OS running the test. Only under CI=1:
// the macOS LaunchAgent and the systemd unit name are per user and cannot be sandboxed, so
// running this on a developer machine would replace their actual RojoHub service.
import { test, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { runCli, freePort, tempDir, removeDir } from "./helpers.js";

const P = process.platform;
const skip = !process.env.CI && "set CI=1 to run; it installs a real login service";
const temps = [];
let env, url;
after(async () => {
  if (!env) return;
  // Belt and braces: never leave the service behind, whatever the asserts did.
  runCli(["uninstall-service"], { env });
  temps.forEach(removeDir);
});

const run = (args) => runCli(args, { env });
async function healthy() {
  try { return (await fetch(url + "/health", { signal: AbortSignal.timeout(500) })).ok; } catch { return false; }
}
async function until(cond, ms) {
  const t = Date.now() + ms;
  while (Date.now() < t) { if ((await cond()) === true) return true; await new Promise((r) => setTimeout(r, 200)); }
  return false;
}

// Where each OS keeps the login entry. Only Windows can be pointed at a temp dir (APPDATA);
// launchd and the systemd user manager read their own fixed locations.
function entryPath(tmp) {
  if (P === "darwin") return path.join(os.homedir(), "Library", "LaunchAgents", "dev.rojohub.hub.plist");
  if (P === "win32") return path.join(tmp, "Microsoft", "Windows", "Start Menu", "Programs", "Startup", "RojoHub.vbs");
  return path.join(process.env.XDG_CONFIG_HOME || path.join(os.homedir(), ".config"), "systemd", "user", "rojohub.service");
}

test(`install-service starts the hub at login on ${P}`, { skip }, async () => {
  const home = tempDir("svc-home");
  const tmp = tempDir("svc-entry");
  temps.push(home, tmp);
  const port = await freePort();
  url = `http://127.0.0.1:${port}`;
  env = { ...process.env, ROJOHUB_HOME: home, ROJOHUB_PORT: String(port), ROJOHUB_PORT_RANGE: "39020-39023", APPDATA: tmp };
  if (P === "linux" && spawnSync("systemctl", ["--user", "show-environment"], { env }).status !== 0) {
    console.log("systemd user session unavailable; skipping");
    return;
  }

  const install = run(["install-service"]);
  assert.equal(install.status, 0, install.stderr);
  assert.match(install.stdout, /run at login/);
  const entry = entryPath(tmp);
  assert.ok(fs.existsSync(entry), `no login entry at ${entry}`);
  assert.ok(await until(healthy, 15000), `hub never answered on ${url}; log:\n${fs.existsSync(path.join(home, "hub.log")) ? fs.readFileSync(path.join(home, "hub.log"), "utf8") : "(none)"}`);

  // Re-running is idempotent.
  assert.equal(run(["install-service"]).status, 0);
  assert.ok(await until(healthy, 15000));

  const un = run(["uninstall-service"]);
  assert.equal(un.status, 0, un.stderr);
  assert.ok(!fs.existsSync(entry), "login entry still present after uninstall");
  assert.ok(await until(async () => !(await healthy()), 10000), "hub still answering after uninstall");
});
