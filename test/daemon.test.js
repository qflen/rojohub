import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { VERSION, startDaemon, tempDir, removeDir } from "./helpers.js";

let hub;
const PORT_RANGE = [39000, 39003];

before(async () => {
  hub = await startDaemon();
});
after(() => hub?.stop());

const api = (route, init = {}) =>
  fetch(hub.url + route, { ...init, headers: { Authorization: `Bearer ${hub.token}`, "Content-Type": "application/json", ...(init.headers || {}) } });
const studio = (route, body) =>
  fetch(hub.url + route, { method: "POST", headers: { "Content-Type": "application/json", "X-RojoHub-Client": "studio-plugin" }, body: JSON.stringify(body) });

// Plain node:http so we can send an arbitrary Host header (fetch may refuse to).
function rawGet(route, headers) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port: hub.port, path: route, method: "GET", headers }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => { body += d; });
      res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body }));
    });
    req.on("error", reject);
    req.end();
  });
}

test("GET /health reports ok and the version", async () => {
  const res = await fetch(`${hub.url}/health`);
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { ok: true, version: VERSION });
});

test("GET /api/status requires the bearer token", async () => {
  const anon = await fetch(`${hub.url}/api/status`);
  assert.equal(anon.status, 401);

  const res = await api("/api/status");
  assert.equal(res.status, 200);
  const body = await res.json();
  assert.deepEqual(body.projects, []);
  assert.deepEqual(body.hub.portRange, PORT_RANGE);
  assert.equal(typeof body.hub.engine, "string");
});

test("a foreign Host header is rejected (DNS rebinding)", async () => {
  const res = await rawGet("/health", { Host: "evil.example" });
  assert.equal(res.status, 403);
  const ok = await rawGet("/health", { Host: `127.0.0.1:${hub.port}` });
  assert.equal(ok.status, 200);
});

test("projects can be added, resolved by cwd, listed and removed", async (t) => {
  const dir = tempDir("project");
  t.after(() => removeDir(dir));
  fs.writeFileSync(path.join(dir, "default.project.json"), JSON.stringify({ name: "t", tree: { $className: "DataModel" } }));

  const added = await api("/api/projects", { method: "POST", body: JSON.stringify({ dir, placeIds: [123] }) });
  const addedText = await added.text();
  assert.equal(added.status, 200, addedText);
  const { project } = JSON.parse(addedText);
  assert.equal(project?.name, "t", addedText);
  assert.deepEqual(project.placeIds, [123]);
  assert.equal(project.dir, dir);
  assert.ok(project.port >= PORT_RANGE[0] && project.port <= PORT_RANGE[1], `port ${project.port} outside ${PORT_RANGE.join("-")}`);
  assert.equal(project.rojo.status, "stopped");

  const status = await (await api("/api/status")).json();
  assert.deepEqual(status.projects.map((p) => p.id), [project.id]);

  const sub = path.join(dir, "src", "deeper");
  fs.mkdirSync(sub, { recursive: true });
  const resolved = await (await api(`/api/resolve?cwd=${encodeURIComponent(sub)}`)).json();
  assert.equal(resolved.project?.id, project.id);

  // The sync plugin asks which project serves a place; the RojoHub plugin long-polls.
  const bound = await (await studio("/studio/binding", { placeId: 123, gameId: 0, pluginVersion: "test" })).json();
  assert.equal(bound.binding.status, "bound");
  assert.equal(bound.binding.project.id, project.id);
  assert.equal(bound.binding.project.port, project.port);
  assert.equal(bound.binding.project.rojo, "stopped");
  const mismatch = await (await studio("/studio/binding", { placeId: 123, gameId: 0 })).json();
  assert.equal(mismatch.binding.status, "bound", "a project without a GameId accepts any GameId");

  const removed = await api(`/api/projects/${encodeURIComponent(project.id)}`, { method: "DELETE" });
  assert.equal(removed.status, 200);
  assert.deepEqual(await removed.json(), { removed: project.id });

  const after = await (await api("/api/status")).json();
  assert.deepEqual(after.projects, []);
  const gone = await (await api(`/api/resolve?cwd=${encodeURIComponent(dir)}`)).json();
  assert.equal(gone.project, null);
});

test("Studio endpoints need the plugin header and classify windows", async () => {
  const noHeader = await fetch(`${hub.url}/studio/binding`, { method: "POST", body: JSON.stringify({ placeId: 5 }) });
  assert.equal(noHeader.status, 403);

  const unbound = await (await studio("/studio/binding", { placeId: 5, gameId: 0 })).json();
  assert.equal(unbound.binding.status, "unbound");
  assert.equal(unbound.hubVersion, VERSION);
  const unpublished = await (await studio("/studio/binding", { placeId: 0, gameId: 0 })).json();
  assert.equal(unpublished.binding.status, "unpublished");

  const poll = await (await studio("/studio/poll", { sessionId: "test-session", placeId: 5, gameId: 0, placeName: "Test Place", immediate: true })).json();
  assert.equal(poll.binding.status, "unbound");
  assert.deepEqual(poll.commands, []);
  const status = await (await api("/api/status")).json();
  assert.deepEqual(status.studios.map((w) => [w.id, w.placeName, w.binding.status]), [["test-session", "Test Place", "unbound"]]);

  // A command for a project nobody has open is refused before anything is queued.
  const exec = await api("/api/studio/exec", { method: "POST", body: JSON.stringify({ project: "nope", op: "identity" }) });
  assert.equal(exec.status, 404);
});

test("GET / serves the dashboard with the token inlined and a CSP", async () => {
  const res = await fetch(`${hub.url}/`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type") || "", /text\/html/);
  assert.ok(res.headers.get("content-security-policy"), "missing Content-Security-Policy header");
  const html = await res.text();
  assert.ok(html.includes(hub.token), "dashboard does not contain the token");
  assert.ok(!html.includes("__ROJOHUB_TOKEN__"), "token placeholder was not replaced");
});
