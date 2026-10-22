import { test, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { ROOT, VERSION, runCli, tempDir, removeDir } from "./helpers.js";

const home = tempDir("cli-home");
const temps = [home];
after(() => temps.forEach(removeDir));

const run = (args, env = {}) => runCli(args, { env: { ...process.env, ROJOHUB_HOME: home, ...env } });

test("--help and version", () => {
  const help = run(["--help"]);
  assert.equal(help.status, 0);
  assert.match(help.stdout, /rojohub add \[dir\]/);
  assert.equal(run(["version"]).stdout.trim(), VERSION);
  assert.equal(run(["bogus"]).status, 1);
});

test("install-plugin --dir installs both plugins, backs up the stock Rojo plugin, and --stock restores it", () => {
  const dest = tempDir("plugins");
  temps.push(dest);
  const managed = path.join(dest, "RojoManagedPlugin.rbxm");
  const syncPlugin = fs.readFileSync(path.join(ROOT, "plugin", "RojoHubSync.rbxm"));
  fs.writeFileSync(managed, "stock rojo plugin");

  const r = run(["install-plugin", "--dir", dest]);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(fs.readFileSync(path.join(dest, "RojoHub.server.lua"), "utf8"), fs.readFileSync(path.join(ROOT, "plugin", "RojoHub.server.lua"), "utf8"));
  assert.ok(fs.readFileSync(managed).equals(syncPlugin), "RojoManagedPlugin.rbxm is not the RojoHub sync plugin");
  assert.equal(fs.readFileSync(managed + ".rojo-backup", "utf8"), "stock rojo plugin");

  // Installing again keeps the original backup instead of backing up our own plugin.
  assert.equal(run(["install-plugin", "--dir", dest]).status, 0);
  assert.equal(fs.readFileSync(managed + ".rojo-backup", "utf8"), "stock rojo plugin");

  // --stock puts the stock plugin back and keeps the RojoHub plugin.
  const stock = run(["install-plugin", "--dir", dest, "--stock"]);
  assert.equal(stock.status, 0, stock.stderr);
  assert.match(stock.stdout, /Restored the stock Rojo plugin/);
  assert.equal(fs.readFileSync(managed, "utf8"), "stock rojo plugin");
  assert.ok(!fs.existsSync(managed + ".rojo-backup"));
  assert.ok(fs.existsSync(path.join(dest, "RojoHub.server.lua")));

  // --stock with nothing of ours installed leaves the Rojo plugin alone.
  assert.equal(run(["install-plugin", "--dir", dest, "--stock"]).status, 0);
  assert.equal(fs.readFileSync(managed, "utf8"), "stock rojo plugin");
});

test("ROJOHUB_PLUGINS_DIR overrides the Studio plugins folder", () => {
  const dest = tempDir("plugins-env");
  temps.push(dest);
  const r = run(["install-plugin", "--stock"], { ROJOHUB_PLUGINS_DIR: dest });
  assert.equal(r.status, 0, r.stderr);
  assert.ok(fs.existsSync(path.join(dest, "RojoHub.server.lua")));
  assert.ok(!fs.existsSync(path.join(dest, "RojoManagedPlugin.rbxm")));
});

test("commands that need the hub fail with a clear message when it is down", () => {
  const r = run(["status"], { ROJOHUB_PORT: "1" });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /RojoHub is not running/);
});
