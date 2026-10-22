---
name: rojohub
description: Set up and use RojoHub for a Rojo / Roblox project - register the project with its PlaceId, start its Rojo server, wait for Studio to sync, and inspect or test the place in Studio. Use when working in a folder with a *.project.json, when the user mentions Rojo, Roblox Studio, a PlaceId, or syncing code into Studio.
---

# RojoHub

RojoHub runs one Rojo server per project on a reserved port and only lets it sync into that project's Roblox place. The `rojohub` MCP tools work on the project that contains your working directory and nothing else.

## Command line

Run `rojohub` from the project folder. While this plugin is enabled it is on the Bash PATH; the full path is `${CLAUDE_PLUGIN_ROOT}/bin/rojohub`. The first run downloads the release binary for this machine and verifies its checksum, so it can take a few seconds.

## First time in a project

1. Call `rojo_info`. If it says the folder is not registered, set it up:
2. One-time Studio plugins, if the user has not installed them: `rojohub install-plugin`. It installs the RojoHub plugin and the RojoHub sync plugin (a fork of the Rojo plugin that connects to the right project by itself). Tell the user to restart Studio and allow the HTTP prompt for 127.0.0.1.
3. Ask the user to open the published place in Studio, then run `rojohub add . --from-studio`. If they give a PlaceId instead: `rojohub add . --place <PlaceId>`. This adds `servePlaceIds` to the project file; mention that the change should be committed.
4. `rojo_start`, then `rojo_wait_for_studio`.

## Working

- Change code by editing files on disk. Rojo syncs them into Studio. Never edit synced scripts through `studio_run_luau`; Rojo overwrites them.
- Use `studio_get_tree`, `studio_get_source` and `studio_run_luau` to inspect and test. Luau runs as one undo step and is rolled back on error.
- `rojo_logs` shows Rojo's output when a sync or build fails.
- The dashboard at http://127.0.0.1:34870 shows every project, its port and whether Studio is synced. Point the user there when they ask what is running.
- If a tool says several Studio windows have the place open, ask the user which one and pass its `session_id`.
