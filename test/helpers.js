// Shared helpers: every test runs against a throwaway ROJOHUB_HOME on a port of its own,
// so the suite never touches the user's real ~/.rojohub or a hub that is already running.
// The binary under test is ROJOHUB_BIN, or the freshest `cargo build` output in target/.
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const VERSION = /^version\s*=\s*"([^"]+)"/m.exec(fs.readFileSync(path.join(ROOT, "Cargo.toml"), "utf8"))[1];

function defaultBin() {
  const exe = process.platform === "win32" ? "rojohub.exe" : "rojohub";
  const candidates = ["release", "debug"].map((p) => path.join(ROOT, "target", p, exe)).filter((p) => fs.existsSync(p));
  candidates.sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs);
  if (!candidates.length) throw new Error("No rojohub binary: set ROJOHUB_BIN or run `cargo build`.");
  return candidates[0];
}
export const BIN = process.env.ROJOHUB_BIN || defaultBin();

export const runCli = (args, opts = {}) => spawnSync(BIN, args, { encoding: "utf8", windowsHide: true, ...opts });
export const spawnCli = (args, opts = {}) => spawn(BIN, args, { windowsHide: true, ...opts });

export function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

export function tempDir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `rojohub-${prefix}-`));
}

export function removeDir(dir) {
  fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}

export function waitForExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(child.exitCode);
  return new Promise((resolve) => child.once("exit", (code) => resolve(code)));
}

// Spawn `rojohub daemon` in a fresh home and wait until /health answers.
// Callers register the returned `stop` in an `after` hook declared at module level
// (registering hooks from inside another hook attaches them to that hook instead).
// `home` lets a test pre-populate the state dir (e.g. pids.json) before the hub reads it.
export async function startDaemon({ home = tempDir("home"), portRange = "39000-39003" } = {}) {
  const port = await freePort();
  const env = {
    ...process.env,
    ROJOHUB_HOME: home,
    ROJOHUB_PORT: String(port),
    ROJOHUB_PORT_RANGE: portRange,
    NO_COLOR: "1",
    ROJOHUB_OFFLINE: "1", // no Roblox name lookups from tests
  };
  const child = spawnCli(["daemon"], { env, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  child.stdout.on("data", (d) => { output += d; });
  child.stderr.on("data", (d) => { output += d; });

  const stop = async () => {
    child.kill();
    await waitForExit(child);
    removeDir(home);
  };

  const url = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 10000;
  for (;;) {
    if (child.exitCode !== null) throw new Error(`daemon exited early (code ${child.exitCode}):\n${output}`);
    try {
      const res = await fetch(`${url}/health`, { signal: AbortSignal.timeout(500) });
      if (res.ok) break;
    } catch {}
    if (Date.now() >= deadline) {
      await stop();
      throw new Error(`daemon did not answer on ${url} within 10s:\n${output}`);
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  const token = fs.readFileSync(path.join(home, "token"), "utf8").trim();
  return { child, home, port, url, token, env, stop, output: () => output };
}

// The Rojo binary tests should use: ROJOHUB_ROJO_BIN if set, else `rojo` on PATH. null if neither runs.
export function rojoBin() {
  const bin = process.env.ROJOHUB_ROJO_BIN || "rojo";
  const r = spawnSync(bin, ["--version"], { encoding: "utf8", windowsHide: true });
  return r.status === 0 && /^Rojo \d/.test(r.stdout) ? bin : null;
}

export function writeProject(dir, placeIds) {
  fs.mkdirSync(path.join(dir, "src"), { recursive: true });
  fs.writeFileSync(path.join(dir, "src", "Hello.server.luau"), "print('hi')\n");
  const project = { name: "ci-project", tree: { $className: "DataModel", ServerScriptService: { $path: "src" } } };
  if (placeIds) project.servePlaceIds = placeIds;
  fs.writeFileSync(path.join(dir, "default.project.json"), JSON.stringify(project, null, 2));
  return dir;
}

// Open a raw WebSocket upgrade on `port` the way the Rojo plugin subscribes to a server,
// and resolve with the socket and the response head once the server has answered.
export function openSubscription(port, pathname) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(port, "127.0.0.1", () => {
      sock.write(
        `GET ${pathname} HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n` +
          `Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n`,
      );
    });
    let head = "";
    const onData = (d) => {
      head += d.toString("latin1");
      if (head.includes("\r\n\r\n")) {
        sock.off("data", onData);
        resolve({ sock, head });
      }
    };
    sock.on("data", onData);
    sock.on("error", reject);
    setTimeout(() => reject(new Error(`no upgrade response from port ${port}:\n${head}`)), 5000).unref();
  });
}
