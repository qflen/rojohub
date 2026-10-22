--[[
	This file is part of RojoHub's sync plugin, a fork of the Rojo Studio
	plugin derived from Rojo v7.7.1 (https://github.com/rojo-rbx/rojo).
	This file was added by RojoHub; it has no upstream counterpart.

	This Source Code Form is subject to the terms of the Mozilla Public
	License, v. 2.0. If a copy of the MPL was not distributed with this
	file, You can obtain one at http://mozilla.org/MPL/2.0/.

	Auto-connect decision logic. This module is pure Luau: it touches no
	Roblox globals so it can be unit-tested outside Studio (see
	sync-plugin/tests/policy.luau). All I/O lives in HubAutoConnect/init.lua.

	Every decision has one of three shapes:
	  { kind = "connect", port = number, reason = "hub" | "scan",
	    projectName = string?, session = string? }
	  { kind = "refuse", message = string }
	  { kind = "wait" }
]]

local Policy = {}

local WAIT = { kind = "wait" }

local function wait()
	return WAIT
end

local function refuse(message: string)
	return { kind = "refuse", message = message }
end

--[[
	Pause rule. After a session on `pausedPort` ends, the plugin must not
	auto-connect to that same port and server session again; a different
	server session id (the server restarted) or a manual Connect clears it.

	`pausedSession == nil` means the paused session was never identified, so
	the port stays paused only while the candidate session is unknown too.
	A candidate with an unknown session is treated as the paused one.
]]
function Policy.isPaused(state, port: number, session: string?): boolean
	if state == nil or state.pausedPort == nil or state.pausedPort ~= port then
		return false
	end

	if state.pausedSession == nil then
		return session == nil
	end

	return session == nil or session == state.pausedSession
end

--[[
	Decide from the hub's answer to POST /studio/binding.
	`binding` is the decoded `binding` object; `state` is the pause state
	({ pausedPort = number?, pausedSession = string? }).
]]
function Policy.fromBinding(binding, state)
	if type(binding) ~= "table" or type(binding.status) ~= "string" then
		return wait()
	end

	local status = binding.status

	if status == "conflict" or status == "mismatch" then
		local message
		if type(binding.messages) == "table" and type(binding.messages[1]) == "string" then
			message = binding.messages[1]
		else
			message = ("the hub reported status '%s'"):format(status)
		end

		return refuse(message)
	end

	if status ~= "bound" then
		-- unbound, unpublished, or something this plugin does not know
		return wait()
	end

	local project = binding.project
	if type(project) ~= "table" or project.rojo ~= "running" or type(project.port) ~= "number" then
		return wait()
	end

	local session = if type(project.session) == "string" then project.session else nil
	if Policy.isPaused(state, project.port, session) then
		return wait()
	end

	return {
		kind = "connect",
		port = project.port,
		reason = "hub",
		projectName = if type(project.name) == "string" then project.name else tostring(project.id),
		session = session,
	}
end

--[[
	Decide from a port scan done while the hub is unreachable.
	`servers` is { { port = number, info = <decoded /api/rojo body or nil> }, ... }.
	`state` is optional and carries the same pause state as fromBinding.
]]
function Policy.fromScan(placeId: number, servers, protocolVersion: number, state)
	if type(placeId) ~= "number" or placeId == 0 then
		return wait()
	end

	local matching = {}
	for _, server in ipairs(servers or {}) do
		local info = server.info
		if
			type(info) == "table"
			and info.protocolVersion == protocolVersion
			and type(info.expectedPlaceIds) == "table"
			and table.find(info.expectedPlaceIds, placeId) ~= nil
		then
			table.insert(matching, server)
		end
	end

	if #matching == 0 then
		return wait()
	end

	if #matching > 1 then
		local ports = {}
		for _, server in ipairs(matching) do
			table.insert(ports, server.port)
		end
		table.sort(ports)
		for i, port in ipairs(ports) do
			ports[i] = tostring(port)
		end

		return refuse(
			("%d Rojo servers on ports %s all list PlaceId %d. Stop all but one."):format(
				#matching,
				table.concat(ports, ", "),
				placeId
			)
		)
	end

	local server = matching[1]
	local session = if type(server.info.sessionId) == "string" then server.info.sessionId else nil
	if Policy.isPaused(state, server.port, session) then
		return wait()
	end

	return {
		kind = "connect",
		port = server.port,
		reason = "scan",
		projectName = if type(server.info.projectName) == "string" then server.info.projectName else nil,
		session = session,
	}
end

--[[
	Query string appended to every request ApiContext makes, so the hub's
	proxy can attribute the connection to this Studio window.
]]
function Policy.buildQuery(placeId: number, sessionGuid: string, reason: string): string
	assert(reason == "hub" or reason == "scan" or reason == "manual", "reason must be hub, scan or manual")

	return ("rojohub-client=sync&rojohub-place=%d&rojohub-session=%s&rojohub-auto=%s"):format(
		placeId,
		sessionGuid,
		reason
	)
end

return Policy
