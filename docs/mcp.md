# MCP tools

`rojohub mcp` runs an MCP server over stdio. It starts the hub on the first tool call if it is not running. See [For AI agents](../README.md#for-ai-agents) for how to register it with Claude Code, Codex or another client.

Every tool result is plain text. Failures come back as an error result with a message that says what to do next.

## Scope

The server is locked to the project that contains its working directory (or `ROJOHUB_PROJECT_DIR`). Naming any other project is refused:

```
This agent is working in "ZombieWave" (~/games/zombie-wave) and may only control that project. Refusing to touch "heistrush".
```

`ROJOHUB_ALLOW_ANY_PROJECT=1` lifts the lock.

## Common arguments

| Argument | Type | Used by | Meaning |
| --- | --- | --- | --- |
| `project` | string | all tools | Project id or name. Defaults to the locked project. |
| `session_id` | string | `studio_*` tools | Which Studio window to use. Needed only when several windows have the place open. |

When several windows have the place open and no `session_id` is given, `studio_*` tools fail and list the windows:

```
Several Studio windows have "HeistRush" open. Pass a session id.
Sessions: [{"id":"…","placeName":"Heist Rush","connectedAt":…}, …]
```

`rojo_info` also lists every open window with its session id.

## Tools

| Tool | Does |
| --- | --- |
| [`rojo_info`](#rojo_info) | Project, port, Rojo status and connected Studio windows |
| [`rojo_start`](#rojo_start) | Start or restart the project's Rojo server |
| [`rojo_wait_for_studio`](#rojo_wait_for_studio) | Wait until Studio is synced to that server |
| [`rojo_stop`](#rojo_stop) | Stop the project's Rojo server |
| [`rojo_logs`](#rojo_logs) | Recent Rojo output |
| [`studio_identity`](#studio_identity) | PlaceId, GameId, name and version of the bound window |
| [`studio_get_tree`](#studio_get_tree) | Instance tree under a path |
| [`studio_get_source`](#studio_get_source) | Source of one script |
| [`studio_run_luau`](#studio_run_luau) | Run Luau in edit mode |

### rojo_info

Call this first. Takes `project`.

Returns the project's name, folder, port, Rojo status, allowed PlaceIds, Studio sync state and open Studio windows.

```
Project: HeistRush (id heistrush)
Folder: ~/games/heist-rush
Dashboard: http://127.0.0.1:34870/
Rojo port: 34872  (the RojoHub sync plugin in Studio finds it automatically; stock Rojo plugin: localhost:34872)
Rojo status: running
Allowed PlaceIds: 12063218054  GameId: 4410392718
Studio sync: connected (place 12063218054, auto via hub)
Verified: Rojo 7.7.1 serving "HeistRush" only to places 12063218054
Studio windows: "Heist Rush" [session …]
```

If the folder is not a registered project, the error says to run `rojohub add . --place <PlaceId>`.

### rojo_start

| Argument | Type | Default |
| --- | --- | --- |
| `project` | string | locked project |
| `restart` | boolean | `false` |

Starts the server on the project's reserved port, or restarts it when `restart` is true. Returns the same text as `rojo_info`.

### rojo_wait_for_studio

| Argument | Type | Default |
| --- | --- | --- |
| `project` | string | locked project |
| `timeout_seconds` | number | `60` (1 to 300) |

Returns the `rojo_info` text once Studio's sync plugin is connected. Fails at once if Rojo is not running, and with a timeout message if no Studio connects in time.

### rojo_stop

Takes `project`. Stops the server and returns the `rojo_info` text.

### rojo_logs

| Argument | Type | Default |
| --- | --- | --- |
| `project` | string | locked project |
| `lines` | number | `80` |

Returns the last `lines` lines of Rojo output, or `(no output yet)`.

### studio_identity

Takes `project` and `session_id`. Returns what the Studio window reports about itself, then the project it matches:

```json
{
  "placeId": 12063218054,
  "gameId": 4410392718,
  "placeVersion": 42,
  "name": "Heist Rush",
  "creatorId": 1234567,
  "creatorType": "Enum.CreatorType.User",
  "session": "…"
}
Matches project "HeistRush" (allowed PlaceIds 12063218054).
```

### studio_get_tree

Read-only.

| Argument | Type | Default |
| --- | --- | --- |
| `project`, `session_id` | string | |
| `path` | string | `game` |
| `depth` | number | `2` (1 to 6) |

`path` is a dot path from `game`, such as `Workspace.Map` or `ServerScriptService`. Each line is `Name [ClassName]`. Scripts show their line count. Instances below the depth limit show their child count. At most 200 children are listed per instance.

```
ServerScriptService [ServerScriptService]
  Heist [Script] (148 lines)
  Systems [Folder] +6 children
```

### studio_get_source

Read-only.

| Argument | Type | Default |
| --- | --- | --- |
| `project`, `session_id` | string | |
| `path` | string | required |

Returns the script's `Source`. Fails if `path` is not a script. Scripts that Rojo syncs are better read from the files on disk.

### studio_run_luau

| Argument | Type | Default |
| --- | --- | --- |
| `project`, `session_id` | string | |
| `code` | string | required |
| `timeout_seconds` | number | `30` (1 to 120) |

Runs `code` in edit mode in the bound window, after the plugin re-checks the PlaceId and GameId. All changes are one undo step. If the code errors, they are rolled back.

Returns captured `print` and `warn` output, then the return values:

```
Output:
cars: 3
[warn] no garage spawn set
Returned: 1500
```

Tables come back as JSON and instances as their full name. On error the result holds the traceback, the output before the error and `(Changes were rolled back.)`.

Do not edit scripts that Rojo syncs with this tool. Rojo overwrites them; edit the files on disk instead.

## Errors

| Message starts with | Cause |
| --- | --- |
| `This agent is working in` | The call named another project. See [Scope](#scope). |
| `… is not inside a project registered with RojoHub` | The working directory is not a registered project. |
| `Unknown project` | No project has that id or name. |
| `Rojo is … Start it first.` | `rojo_wait_for_studio` was called before `rojo_start`. |
| `No Roblox Studio window with` | No window with the RojoHub plugin has the place open. |
| `Several Studio windows have` | Pass `session_id`. |
| `Studio session … is not bound to` | That window shows a different project. |
| `Refused: this Studio is PlaceId` | The plugin's own PlaceId check failed. |
