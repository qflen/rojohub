# RojoHub Sync plugin

RojoHub's Studio sync plugin is a fork of the stock [Rojo](https://github.com/rojo-rbx/rojo) Studio plugin with hub-driven auto-connect added. Everything else (sync, confirmation, settings, update check) is upstream Rojo unchanged.

- Upstream: `rojo-rbx/rojo`, tag **`v7.7.1`**, commit `26b6cc6`.
- Built artifact: `plugin/RojoHubSync.rbxm` (committed to this repo).
- This directory stores only an overlay: the handful of files that differ from upstream, at their upstream-relative paths. The build script fetches upstream at the pinned tag, copies the overlay over it and runs `rojo build`.

## Files

Modified upstream files (full copies with a header noting the modification; every changed line is marked `-- RojoHub`):

- `overlay/plugin/src/App/init.lua`: creates the `HubAutoConnect` instance, starts it in edit mode, stops it on unmount, passes the request query to `ApiContext.new` in `startSession()`, reports session ends to it, and renames the toolbar and widget to "RojoHub Sync".
- `overlay/plugin/src/ApiContext.lua`: `ApiContext.new(baseUrl, query)` gains an optional `query` string that is appended as `?<query>` to every request URL (`/api/rojo`, `/api/read/...`, `/api/write`, `/api/socket/<cursor>`, `/api/open/...`, `/api/serialize`, `/api/ref-patch`).

Added files:

- `overlay/plugin/src/HubAutoConnect/Policy.lua`: pure decision logic (no Roblox globals): `fromBinding`, `fromScan`, `buildQuery`, `isPaused`.
- `overlay/plugin/src/HubAutoConnect/init.lua`: Studio-side glue: the 5 s hub poll, the 30 s port-scan fallback, notifications, pause bookkeeping.
- `tests/policy.luau`: unit tests for `Policy`, run with [lune](https://github.com/lune-org/lune).
- `LICENSE.txt`: Rojo's license (MPL-2.0), verbatim.
- `../scripts/build-sync-plugin.sh`: the build script.

## Behaviour

In edit mode, while no session is active and the Team Create sync lock is free, the plugin asks the hub every 5 seconds:

```
POST http://127.0.0.1:34870/studio/binding
Content-Type: application/json
X-RojoHub-Client: studio-plugin
{"placeId": <game.PlaceId>, "gameId": <game.GameId>, "pluginVersion": "7.7.1"}
```

- `bound` with `project.rojo == "running"`: connect to `localhost:<project.port>` (reason `hub`) and show "RojoHub: connecting to project '<name>' on port <port>".
- `bound` but not running, `unbound`, `unpublished`: wait.
- `conflict` or `mismatch`: never connect; show "RojoHub will not auto-connect: <first message>" once per distinct message.
- Hub unreachable (non-200, connection failure, malformed body): at most every 30 seconds, probe `GET /api/rojo` on `localhost:34872..34881` (skipped when `game.PlaceId == 0`). Servers with the right `protocolVersion` whose `expectedPlaceIds` list the current place are candidates. One candidate: connect (reason `scan`). Several: refuse with "RojoHub will not auto-connect: <n> Rojo servers on ports <a, b> all list PlaceId <id>. Stop all but one."

Pause rule: when a session ends (Disconnect button, an aborted confirmation, or an error) the plugin remembers that port and server session id and does not auto-connect to the same pair again. A different session id on that port (the server restarted) or a manual Connect clears the pause. A manual attempt that never reached a server pauses nothing.

Every request a session makes carries `?rojohub-client=sync&rojohub-place=<placeId>&rojohub-session=<guid>&rojohub-auto=<hub|scan|manual>`, where the guid is generated once per plugin load. The stock Connect button keeps working as upstream and is tagged `manual`.

## Build

Requires `git` and `rojo` (7.7.x) on `PATH`, or `ROJO_BIN` pointing at a rojo binary. From the repo root:

```sh
scripts/build-sync-plugin.sh
```

The script clones `rojo-rbx/rojo` at `v7.7.1` (shallow, with submodules) into `.cache/rojo-upstream-v7.7.1` (gitignored; reused on later runs after being reset to pristine upstream), copies `sync-plugin/overlay` over it, runs `rojo build plugin.project.json -o plugin/RojoHubSync.rbxm` and prints the SHA-256 of the result. If `lune` is on `PATH` (or `LUNE_BIN` is set) it also runs the policy tests; otherwise it says so and skips them. The build is deterministic: two runs produce byte-identical output.

Install the plugin by copying `plugin/RojoHubSync.rbxm` into Studio's local plugins folder (Studio > Plugins > Plugins Folder).

## Test

```sh
lune run sync-plugin/tests/policy.luau
```

Prints `policy tests: N passed, M failed` and exits non-zero on failure. Lune 0.10.x is known to work.

## License

The Rojo plugin is licensed under the Mozilla Public License 2.0; see `LICENSE.txt`. This fork keeps that license: every modified or added file carries the MPL-2.0 Exhibit A notice and says whether it is modified from or added to upstream Rojo v7.7.1. The modified files are the two listed above; the rest of the plugin is upstream Rojo as built from the pinned tag.
