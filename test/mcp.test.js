import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { VERSION, spawnCli, startDaemon, waitForExit, freePort, tempDir, removeDir } from "./helpers.js";

let hub;

before(async () => {
  hub = await startDaemon();
});
after(() => hub?.stop());

test("rojohub mcp answers initialize and tools/list over stdio, then exits 0", async (t) => {
  const child = spawnCli(["mcp"], { env: hub.env, stdio: ["pipe", "pipe", "pipe"] });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  let stderr = "";
  child.stderr.on("data", (d) => { stderr += d; });

  const responses = [];
  let buffered = "";
  const gotTwo = new Promise((resolve, reject) => {
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      buffered += chunk;
      const lines = buffered.split("\n");
      buffered = lines.pop();
      for (const line of lines) if (line.trim()) responses.push(JSON.parse(line));
      if (responses.length >= 2) resolve();
    });
    child.once("exit", () => reject(new Error(`mcp exited before answering:\n${stderr}`)));
    setTimeout(() => reject(new Error(`mcp did not answer within 10s; got ${responses.length} responses:\n${stderr}`)), 10000).unref();
  });

  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} }) + "\n");
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list" }) + "\n");
  await gotTwo;

  const init = responses.find((r) => r.id === 1);
  const list = responses.find((r) => r.id === 2);
  assert.equal(init.jsonrpc, "2.0");
  assert.equal(init.result.serverInfo.name, "rojohub");
  assert.equal(init.result.serverInfo.version, VERSION);
  assert.deepEqual(init.result.capabilities, { tools: {} });
  const names = list.result.tools.map((x) => x.name);
  assert.ok(names.includes("rojo_info"), `tools: ${names.join(", ")}`);
  assert.ok(names.includes("studio_run_luau"), `tools: ${names.join(", ")}`);

  child.stdin.end();
  assert.equal(await waitForExit(child), 0, stderr);
});

// Read JSON-RPC responses off a child's stdout until `id` arrives.
function response(child, id, ms = 15000) {
  return new Promise((resolve, reject) => {
    let buf = "";
    const onData = (chunk) => {
      buf += chunk;
      for (const line of buf.split("\n").slice(0, -1)) {
        const msg = line.trim() && JSON.parse(line);
        if (msg && msg.id === id) { child.stdout.off("data", onData); resolve(msg); }
      }
      buf = buf.slice(buf.lastIndexOf("\n") + 1);
    };
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", onData);
    setTimeout(() => reject(new Error(`no response ${id} within ${ms} ms`)), ms).unref();
  });
}

test("rojohub mcp starts the hub itself when none is running", async (t) => {
  const home = tempDir("mcp-auto");
  const port = await freePort();
  const env = { ...process.env, ROJOHUB_HOME: home, ROJOHUB_PORT: String(port), ROJOHUB_PORT_RANGE: "39030-39033", ROJOHUB_OFFLINE: "1" };
  const child = spawnCli(["mcp"], { env, cwd: home, stdio: ["pipe", "pipe", "pipe"] });
  t.after(async () => {
    if (child.exitCode === null) child.kill();
    // The hub was started detached, so stop it explicitly.
    const token = fs.readFileSync(path.join(home, "token"), "utf8").trim();
    await fetch(`http://127.0.0.1:${port}/api/shutdown`, { method: "POST", headers: { Authorization: `Bearer ${token}` } }).catch(() => {});
    await new Promise((r) => setTimeout(r, 500));
    removeDir(home);
  });

  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "rojo_info", arguments: {} } }) + "\n");
  const r = await response(child, 1);
  // Reaching the hub is the point: the answer is "this folder is not registered", not "hub not running".
  assert.match(r.result.content[0].text, /not inside a project registered/);
  assert.equal((await fetch(`http://127.0.0.1:${port}/health`)).status, 200);
});
