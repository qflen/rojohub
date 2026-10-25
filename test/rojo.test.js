// End-to-end against a real Rojo binary: the hub starts `rojo serve` on a private port behind
// the project's proxy, verifies the server (project name + expectedPlaceIds, decoded from
// Rojo's real MessagePack), forwards traffic, notices a Studio subscription, keeps it healthy,
// stops it, and reaps a leftover server from a previous hub. Skipped when no Rojo is
// available (CI downloads one; locally put `rojo` on PATH or set ROJOHUB_ROJO_BIN).
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import { startDaemon, tempDir, removeDir, rojoBin, writeProject, waitForExit, openSubscription } from "./helpers.js";

const ROJO = rojoBin();
const PORT_RANGE = [39010, 39013];
let hub, dir;
const temps = [];

before(async () => {
  if (!ROJO) return;
  dir = writeProject(tempDir("proj"), [123]);
  temps.push(dir);
  hub = await startDaemon({ portRange: PORT_RANGE.join("-") });
});
after(async () => {
  await hub?.stop();
  temps.forEach(removeDir);
});

const api = (route, init = {}) =>
  fetch(hub.url + route, { ...init, headers: { Authorization: `Bearer ${hub.token}`, "Content-Type": "application/json" } });

async function until(cond, ms, what) {
  const t = Date.now() + ms;
  let last;
  while (Date.now() < t) {
    last = await cond();
    if (last) return last;
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`timed out waiting for ${what}`);
}

test("hub refuses to start a project whose file lacks servePlaceIds", { skip: !ROJO && "no rojo binary" }, async () => {
  const bare = writeProject(tempDir("bare"), null);
  temps.push(bare);
  const add = await api("/api/projects", { method: "POST", body: JSON.stringify({ dir: bare, placeIds: [123], name: "bare" }) });
  assert.equal(add.status, 200);
  const start = await api("/api/projects/bare/start", { method: "POST" });
  assert.equal(start.status, 409);
  assert.match((await start.json()).error, /servePlaceIds/);
  await api("/api/projects/bare", { method: "DELETE" });
});

test("start, verify, proxy, detect a Studio subscription, health, logs and stop a real Rojo server", { skip: !ROJO && "no rojo binary" }, async () => {
  const add = await (await api("/api/projects", { method: "POST", body: JSON.stringify({ dir, placeIds: [123] }) })).json();
  const id = add.project.id;
  assert.ok(add.project.port >= PORT_RANGE[0] && add.project.port <= PORT_RANGE[1]);

  const started = await api(`/api/projects/${id}/start`, { method: "POST" });
  const startedBody = await started.text();
  assert.equal(started.status, 200, startedBody);
  const { project } = JSON.parse(startedBody);
  assert.equal(project.rojo.status, "running");
  assert.equal(project.rojo.verified.projectName, "ci-project");
  assert.deepEqual(project.rojo.verified.expectedPlaceIds, [123]);
  assert.match(project.rojo.verified.serverVersion, /^\d+\.\d+/);
  assert.deepEqual(project.rojo.studioLink, { connected: false, clients: [] });

  // The public port is the hub's proxy: Rojo's own API answers through it.
  const direct = await fetch(`http://127.0.0.1:${project.port}/api/rojo`);
  assert.equal(direct.status, 200);
  assert.match(direct.headers.get("content-type") || "", /msgpack|json/);
  assert.ok((await direct.arrayBuffer()).byteLength > 10);

  const { logs } = await (await api(`/api/projects/${id}/logs`)).json();
  assert.ok(logs.some((l) => / serve .*--port /.test(l)), logs.join("\n"));

  // A Studio subscription is a WebSocket upgrade on /api/socket/ through the proxy. RojoHub's
  // sync plugin tags it with the window's place; the hub reports the project as synced.
  const { sock, head } = await openSubscription(project.port, "/api/socket/0?rojohub-client=sync&rojohub-place=123&rojohub-session=t-1&rojohub-auto=hub");
  assert.ok(head.startsWith("HTTP/1.1 101"), head);
  const synced = await until(async () => {
    const p = (await (await api("/api/status")).json()).projects.find((x) => x.id === id);
    return p.rojo.studioLink.connected ? p : null;
  }, 5000, "studioLink.connected");
  assert.deepEqual(synced.rojo.studioLink.clients, [{ placeId: 123, autoConnected: true, matchReason: "hub" }]);
  sock.destroy();
  await until(async () => {
    const p = (await (await api("/api/status")).json()).projects.find((x) => x.id === id);
    return p.rojo.studioLink.connected ? null : p;
  }, 5000, "studioLink to drop");

  // A stock Rojo plugin subscribes without tags and still counts as connected.
  const stock = await openSubscription(project.port, "/api/socket/0");
  assert.ok(stock.head.startsWith("HTTP/1.1 101"), stock.head);
  const plain = await until(async () => {
    const p = (await (await api("/api/status")).json()).projects.find((x) => x.id === id);
    return p.rojo.studioLink.connected ? p : null;
  }, 5000, "stock subscription");
  assert.deepEqual(plain.rojo.studioLink.clients, [{ placeId: null, autoConnected: false, matchReason: null }]);
  stock.sock.destroy();

  // The health tick (every 5 s) samples the server's resident memory via ps / tasklist.
  let mem = null;
  for (const t = Date.now() + 9000; Date.now() < t && mem == null;) {
    await new Promise((r) => setTimeout(r, 500));
    mem = (await (await api("/api/status")).json()).projects.find((x) => x.id === id).rojo.memory;
  }
  assert.ok(Number.isFinite(mem) && mem > 1_000_000, `expected a memory reading for the Rojo process, got ${mem}`);

  // Starting again is a no-op, not a second server.
  const again = await (await api(`/api/projects/${id}/start`, { method: "POST" })).json();
  assert.equal(again.project.rojo.pid, project.rojo.pid);

  const stopped = await (await api(`/api/projects/${id}/stop`, { method: "POST" })).json();
  assert.equal(stopped.project.rojo.status, "stopped");
  assert.equal(stopped.project.rojo.studioLink, null);
  await assert.rejects(fetch(`http://127.0.0.1:${project.port}/api/rojo`, { signal: AbortSignal.timeout(1000) }));
});

test("a crashed Rojo restarts behind the same public port", { skip: !ROJO && "no rojo binary" }, async () => {
  const crashDir = writeProject(tempDir("crash"), [456]);
  temps.push(crashDir);
  const added = await (await api("/api/projects", { method: "POST", body: JSON.stringify({ dir: crashDir, placeIds: [456] }) })).json();
  const id = added.project.id;
  const started = await (await api(`/api/projects/${id}/start`, { method: "POST" })).json();
  assert.equal(started.project.rojo.status, "running");
  const { port } = started.project;
  const oldPid = started.project.rojo.pid;

  process.kill(oldPid, "SIGKILL");
  const recovered = await until(async () => {
    const p = (await (await api("/api/status")).json()).projects.find((x) => x.id === id);
    return p.rojo.status === "running" && p.rojo.pid !== oldPid ? p : null;
  }, 15000, "Rojo to restart");
  assert.equal(recovered.port, port);
  assert.equal(recovered.rojo.restartCount, 1);
  assert.deepEqual(recovered.rojo.verified.expectedPlaceIds, [456]);
  assert.equal((await fetch(`http://127.0.0.1:${port}/api/rojo`)).status, 200);
  const { logs } = await (await api(`/api/projects/${id}/logs`)).json();
  assert.ok(logs.some((line) => line.includes("restarting (1)")), logs.join("\n"));
  process.kill(recovered.rojo.pid, "SIGKILL");
  const stopped = await (await api(`/api/projects/${id}/stop`, { method: "POST" })).json();
  assert.equal(stopped.project.rojo.status, "stopped");
  await new Promise((r) => setTimeout(r, 700));
  const stillStopped = (await (await api("/api/status")).json()).projects.find((x) => x.id === id);
  assert.equal(stillStopped.rojo.status, "stopped");
  assert.equal(stillStopped.rojo.pid, null);
});

test("a Rojo left behind by a crashed hub is reaped on startup", { skip: !ROJO && "no rojo binary" }, async () => {
  const home = tempDir("home");
  temps.push(home);
  const orphan = spawn(ROJO, ["serve", path.join(dir, "default.project.json"), "--port", String(PORT_RANGE[1])], { cwd: dir, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
  let orphanOut = "";
  orphan.stdout.on("data", (d) => { orphanOut += d; });
  orphan.stderr.on("data", (d) => { orphanOut += d; });
  await new Promise((r) => setTimeout(r, 1500));
  assert.equal(orphan.exitCode, null, `orphan rojo should be running before the hub starts:\n${orphanOut}`);
  fs.writeFileSync(path.join(home, "pids.json"), JSON.stringify({ old: orphan.pid }));

  const second = await startDaemon({ home, portRange: PORT_RANGE.join("-") });
  try {
    const code = await Promise.race([waitForExit(orphan), new Promise((r) => setTimeout(() => r("timeout"), 5000))]);
    assert.notEqual(code, "timeout", "hub did not kill the orphan rojo");
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(home, "pids.json"), "utf8")), {});
  } finally {
    if (orphan.exitCode === null) orphan.kill();
    await second.stop();
  }
});
