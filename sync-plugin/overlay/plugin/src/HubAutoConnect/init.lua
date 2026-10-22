--[[
	This file is part of RojoHub's sync plugin, a fork of the Rojo Studio
	plugin derived from Rojo v7.7.1 (https://github.com/rojo-rbx/rojo).
	This file was added by RojoHub; it has no upstream counterpart.

	This Source Code Form is subject to the terms of the Mozilla Public
	License, v. 2.0. If a copy of the MPL was not distributed with this
	file, You can obtain one at http://mozilla.org/MPL/2.0/.

	Hub-driven auto-connect: the Studio-side glue (HTTP, timers,
	notifications) around HubAutoConnect/Policy.lua. App creates one
	instance, starts it in edit mode, asks it for the request query when a
	session starts and tells it when a session ends.
]]

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Log = require(Packages.Log)
local Http = require(Packages.Http)
local Promise = require(Packages.Promise)

local Config = require(Plugin.Config)
local Version = require(Plugin.Version)
local Policy = require(script.Policy)

local HUB_BINDING_URL = "http://127.0.0.1:34870/studio/binding"
local HUB_POLL_SECONDS = 5
local SCAN_INTERVAL_SECONDS = 30
local SCAN_FIRST_PORT = 34872
local SCAN_LAST_PORT = 34881
local REQUEST_TIMEOUT_SECONDS = 3

local function requestAsync(params)
	-- Promise.new runs its executor in its own coroutine, so yielding here is fine.
	return Promise.new(function(resolve, reject)
		local ok, response = pcall(HttpService.RequestAsync, HttpService, params)
		if ok then
			resolve(response)
		else
			reject(response)
		end
	end)
end

local HubAutoConnect = {}
HubAutoConnect.__index = HubAutoConnect

function HubAutoConnect.new(app)
	local self = {
		app = app,
		-- One guid per plugin load; identifies this Studio window to the hub proxy.
		sessionGuid = HttpService:GenerateGUID(false),
		thread = nil,
		inFlight = false,
		lastScanAt = nil,
		shownRefusals = {},
		-- Set just before an auto-connect calls App:startSession(), consumed by takeQuery().
		pendingReason = nil,
		pendingSession = nil,
		-- Whether the current session was auto-started, and the session id the hub/scan reported for it.
		autoStarted = false,
		autoSession = nil,
		-- Pause state (see Policy.isPaused).
		pausedPort = nil,
		pausedSession = nil,
	}

	return setmetatable(self, HubAutoConnect)
end

function HubAutoConnect:start()
	if self.thread ~= nil or not RunService:IsEdit() then
		return
	end

	Log.trace("RojoHub: starting auto-connect polling")
	self.thread = task.spawn(function()
		while true do
			local ok, err = pcall(self.tick, self)
			if not ok then
				Log.warn("RojoHub auto-connect error: {}", tostring(err))
			end
			task.wait(HUB_POLL_SECONDS)
		end
	end)
end

function HubAutoConnect:stop()
	if self.thread ~= nil then
		Log.trace("RojoHub: stopping auto-connect polling")
		task.cancel(self.thread)
		self.thread = nil
	end
end

--[[
	Called by App:startSession() for every session, auto or manual. Returns
	the query string ApiContext appends to each request and records which
	kind of session is starting. A manual Connect clears the pause.
]]
function HubAutoConnect:takeQuery(): string
	local reason = self.pendingReason or "manual"
	self.autoStarted = reason ~= "manual"
	self.autoSession = self.pendingSession
	self.pendingReason = nil
	self.pendingSession = nil

	if reason == "manual" then
		self.pausedPort = nil
		self.pausedSession = nil
	end

	return Policy.buildQuery(game.PlaceId, self.sessionGuid, reason)
end

--[[
	Called by App when a session reaches Disconnected (Disconnect button,
	abort, or error). Pauses auto-connect for that port + server session so
	the plugin does not immediately re-enter a session the user just left or
	loop on a persistent error. A manual attempt that never reached a server
	pauses nothing.
]]
function HubAutoConnect:onSessionEnded(port: string, apiContext)
	local session = apiContext.__sessionId or self.autoSession
	local wasAuto = self.autoStarted
	self.autoStarted = false
	self.autoSession = nil

	if session == nil and not wasAuto then
		return
	end

	self.pausedPort = tonumber(port)
	self.pausedSession = session
	Log.trace("RojoHub: pausing auto-connect for port {} session {}", self.pausedPort, tostring(session))
end

function HubAutoConnect:pauseState()
	return { pausedPort = self.pausedPort, pausedSession = self.pausedSession }
end

function HubAutoConnect:tick()
	if self.app.serveSession ~= nil or self.inFlight then
		return
	end
	if not self.app:isSyncLockAvailable() then
		return
	end

	self.inFlight = true
	self:askHub()
		:andThen(function(binding)
			self:apply(Policy.fromBinding(binding, self:pauseState()))
		end, function(err)
			Log.trace("RojoHub: hub unreachable ({}), falling back to port scan", tostring(err))
			return self:scan()
		end)
		:catch(function(err)
			Log.warn("RojoHub auto-connect failed: {}", tostring(err))
		end)
		:finally(function()
			self.inFlight = false
		end)
end

function HubAutoConnect:askHub()
	local body = HttpService:JSONEncode({
		placeId = game.PlaceId,
		gameId = game.GameId,
		pluginVersion = Version.display(Config.version),
	})

	return requestAsync({
		Url = HUB_BINDING_URL,
		Method = "POST",
		Headers = {
			["Content-Type"] = "application/json",
			["X-RojoHub-Client"] = "studio-plugin",
		},
		Body = body,
	})
		:timeout(REQUEST_TIMEOUT_SECONDS)
		:andThen(function(response)
			if response.StatusCode ~= 200 then
				return Promise.reject(("HTTP %d"):format(response.StatusCode))
			end

			local ok, decoded = pcall(HttpService.JSONDecode, HttpService, response.Body)
			if
				not ok
				or type(decoded) ~= "table"
				or type(decoded.binding) ~= "table"
				or type(decoded.binding.status) ~= "string"
			then
				return Promise.reject("malformed hub response")
			end

			return decoded.binding
		end)
end

function HubAutoConnect:scan()
	local placeId = game.PlaceId
	if placeId == 0 then
		return nil
	end

	local now = os.clock()
	if self.lastScanAt ~= nil and now - self.lastScanAt < SCAN_INTERVAL_SECONDS then
		return nil
	end
	self.lastScanAt = now

	local probes = {}
	for port = SCAN_FIRST_PORT, SCAN_LAST_PORT do
		local url = ("http://%s:%d/api/rojo"):format(Config.defaultHost, port)
		table.insert(
			probes,
			Http.get(url)
				:timeout(REQUEST_TIMEOUT_SECONDS)
				:andThen(Http.Response.msgpack)
				:andThen(function(info)
					return { port = port, info = info }
				end)
				:catch(function()
					return { port = port, info = nil }
				end)
		)
	end

	return Promise.all(probes):andThen(function(servers)
		self:apply(Policy.fromScan(placeId, servers, Config.protocolVersion, self:pauseState()))
	end)
end

function HubAutoConnect:apply(decision)
	if decision.kind == "connect" then
		self:connect(decision)
	elseif decision.kind == "refuse" then
		self:refuse(decision.message)
	end
end

function HubAutoConnect:connect(decision)
	if self.app.serveSession ~= nil then
		-- A session started while we were asking around; leave it alone.
		return
	end

	local projectName = tostring(decision.projectName)
	Log.info("RojoHub: auto-connecting to project '{}' on port {} (reason: {})", projectName, decision.port, decision.reason)
	self.app:addNotification({
		text = ("RojoHub: connecting to project '%s' on port %d"):format(projectName, decision.port),
		timeout = 5,
	})

	self.pendingReason = decision.reason
	self.pendingSession = decision.session
	self.app.setHost("")
	self.app.setPort(tostring(decision.port))
	self.app:startSession()
	-- startSession() consumes these through takeQuery(); clear them in case it returned early.
	self.pendingReason = nil
	self.pendingSession = nil
end

function HubAutoConnect:refuse(message: string)
	if self.shownRefusals[message] then
		return
	end
	self.shownRefusals[message] = true

	Log.warn("RojoHub will not auto-connect: {}", message)
	self.app:addNotification({
		text = "RojoHub will not auto-connect: " .. message,
		timeout = 30,
	})
end

return HubAutoConnect
