--[[
	This file is part of RojoHub's sync plugin, a fork of the Rojo Studio
	plugin derived from Rojo v7.7.1 (https://github.com/rojo-rbx/rojo).
	This file was added by RojoHub; it has no upstream counterpart.

	This Source Code Form is subject to the terms of the Mozilla Public
	License, v. 2.0. If a copy of the MPL was not distributed with this
	file, You can obtain one at http://mozilla.org/MPL/2.0/.

	Unit tests for HubAutoConnect/Policy.lua. Run from the repo root with:
	    lune run sync-plugin/tests/policy.luau
	Exits non-zero when any test fails.
]]

local process = require("@lune/process")

local Policy = require("../overlay/plugin/src/HubAutoConnect/Policy")

local PROTOCOL = 5
local PLACE = 123456789

local passed, failed = 0, 0

local function test(name: string, body: () -> ())
	local ok, err = pcall(body)
	if ok then
		passed += 1
	else
		failed += 1
		print(("FAIL %s\n     %s"):format(name, tostring(err)))
	end
end

local function expect(condition: boolean, message: string)
	if not condition then
		error(message, 3)
	end
end

local function expectEq(actual, expected, label: string)
	if actual ~= expected then
		error(("%s: expected %s, got %s"):format(label, tostring(expected), tostring(actual)), 3)
	end
end

-- Sentinel for "override this field to nil" (a nil value in a table literal creates no key).
local NONE = {}

local function bound(project)
	return { status = "bound", messages = {}, project = project }
end

local function running(overrides)
	local project = {
		id = "proj-1",
		name = "My Game",
		port = 34873,
		rojo = "running",
		rojoError = nil,
		session = "sess-A",
		synced = false,
	}
	for key, value in pairs(overrides or {}) do
		project[key] = if value == NONE then nil else value
	end
	return project
end

local function server(port: number, info)
	return { port = port, info = info }
end

local function info(overrides)
	local body = {
		sessionId = "srv-" .. tostring(overrides and overrides.port or "x"),
		serverVersion = "7.7.1",
		protocolVersion = PROTOCOL,
		expectedPlaceIds = { PLACE },
		projectName = "Scanned",
	}
	for key, value in pairs(overrides or {}) do
		body[key] = value
	end
	body.port = nil
	return body
end

local noPause = { pausedPort = nil, pausedSession = nil }

-- fromBinding -----------------------------------------------------------------

test("bound + running connects with reason hub", function()
	local decision = Policy.fromBinding(bound(running()), noPause)
	expectEq(decision.kind, "connect", "kind")
	expectEq(decision.port, 34873, "port")
	expectEq(decision.reason, "hub", "reason")
	expectEq(decision.projectName, "My Game", "projectName")
	expectEq(decision.session, "sess-A", "session")
end)

test("bound + stopped waits", function()
	expectEq(Policy.fromBinding(bound(running({ rojo = "stopped" })), noPause).kind, "wait", "stopped")
	expectEq(Policy.fromBinding(bound(running({ rojo = "starting" })), noPause).kind, "wait", "starting")
	expectEq(Policy.fromBinding(bound(running({ rojo = "crashed" })), noPause).kind, "wait", "crashed")
	expectEq(Policy.fromBinding(bound(running({ rojo = "unhealthy" })), noPause).kind, "wait", "unhealthy")
	expectEq(Policy.fromBinding(bound(running({ rojo = "error" })), noPause).kind, "wait", "error")
end)

test("bound without a numeric port waits", function()
	expectEq(Policy.fromBinding(bound(running({ port = "34873" })), noPause).kind, "wait", "string port")
	expectEq(Policy.fromBinding({ status = "bound", messages = {} }, noPause).kind, "wait", "no project")
end)

test("conflict refuses with the first message", function()
	local decision = Policy.fromBinding({
		status = "conflict",
		messages = { "two projects claim PlaceId 123456789", "second message" },
	}, noPause)
	expectEq(decision.kind, "refuse", "kind")
	expectEq(decision.message, "two projects claim PlaceId 123456789", "message")
end)

test("mismatch refuses with the first message", function()
	local decision = Policy.fromBinding({
		status = "mismatch",
		messages = { "place belongs to a different universe" },
		project = running(),
	}, noPause)
	expectEq(decision.kind, "refuse", "kind")
	expectEq(decision.message, "place belongs to a different universe", "message")
end)

test("conflict without messages still refuses", function()
	local decision = Policy.fromBinding({ status = "conflict", messages = {} }, noPause)
	expectEq(decision.kind, "refuse", "kind")
	expect(type(decision.message) == "string" and #decision.message > 0, "message must be a non-empty string")
end)

test("unbound and unpublished wait", function()
	expectEq(Policy.fromBinding({ status = "unbound", messages = {} }, noPause).kind, "wait", "unbound")
	expectEq(Policy.fromBinding({ status = "unpublished", messages = { "save first" } }, noPause).kind, "wait", "unpublished")
end)

test("unknown or malformed bindings wait", function()
	expectEq(Policy.fromBinding({ status = "something-new" }, noPause).kind, "wait", "unknown status")
	expectEq(Policy.fromBinding(nil, noPause).kind, "wait", "nil")
	expectEq(Policy.fromBinding("bound", noPause).kind, "wait", "string")
	expectEq(Policy.fromBinding({}, noPause).kind, "wait", "no status")
end)

-- pause rule ----------------------------------------------------------------

test("pause: same port + same session waits", function()
	local state = { pausedPort = 34873, pausedSession = "sess-A" }
	expectEq(Policy.fromBinding(bound(running()), state).kind, "wait", "same session")
end)

test("pause: same port + different session connects", function()
	local state = { pausedPort = 34873, pausedSession = "sess-A" }
	local decision = Policy.fromBinding(bound(running({ session = "sess-B" })), state)
	expectEq(decision.kind, "connect", "kind")
	expectEq(decision.session, "sess-B", "session")
end)

test("pause: different port connects", function()
	local state = { pausedPort = 34872, pausedSession = "sess-A" }
	expectEq(Policy.fromBinding(bound(running()), state).kind, "connect", "other port")
end)

test("pause: unknown candidate session on the paused port waits", function()
	local state = { pausedPort = 34873, pausedSession = "sess-A" }
	expectEq(Policy.fromBinding(bound(running({ session = NONE })), state).kind, "wait", "nil candidate")
end)

test("pause: port paused without a session id only blocks unknown sessions", function()
	local state = { pausedPort = 34873, pausedSession = nil }
	expectEq(Policy.fromBinding(bound(running({ session = NONE })), state).kind, "wait", "nil candidate")
	expectEq(Policy.fromBinding(bound(running({ session = "sess-A" })), state).kind, "connect", "known candidate")
end)

test("pause: nil state means no pause", function()
	expectEq(Policy.fromBinding(bound(running()), nil).kind, "connect", "nil state")
end)

-- fromScan -------------------------------------------------------------------

test("scan: no servers waits", function()
	expectEq(Policy.fromScan(PLACE, {}, PROTOCOL, noPause).kind, "wait", "empty")
	local none = { server(34872, nil), server(34873, nil) }
	expectEq(Policy.fromScan(PLACE, none, PROTOCOL, noPause).kind, "wait", "all nil")
end)

test("scan: exactly one matching server connects with reason scan", function()
	local servers = {
		server(34872, nil),
		server(34875, info({ port = 34875 })),
		server(34876, nil),
	}
	local decision = Policy.fromScan(PLACE, servers, PROTOCOL, noPause)
	expectEq(decision.kind, "connect", "kind")
	expectEq(decision.port, 34875, "port")
	expectEq(decision.reason, "scan", "reason")
	expectEq(decision.projectName, "Scanned", "projectName")
	expectEq(decision.session, "srv-34875", "session")
end)

test("scan: two matching servers refuse with the exact message", function()
	local servers = {
		server(34879, info({ port = 34879 })),
		server(34872, info({ port = 34872 })),
		server(34873, nil),
	}
	local decision = Policy.fromScan(PLACE, servers, PROTOCOL, noPause)
	expectEq(decision.kind, "refuse", "kind")
	expectEq(
		decision.message,
		"2 Rojo servers on ports 34872, 34879 all list PlaceId 123456789. Stop all but one.",
		"message"
	)
end)

test("scan: three matching servers count correctly", function()
	local servers = {
		server(34872, info({ port = 34872 })),
		server(34873, info({ port = 34873 })),
		server(34874, info({ port = 34874 })),
	}
	local decision = Policy.fromScan(PLACE, servers, PROTOCOL, noPause)
	expectEq(decision.kind, "refuse", "kind")
	expectEq(
		decision.message,
		"3 Rojo servers on ports 34872, 34873, 34874 all list PlaceId 123456789. Stop all but one.",
		"message"
	)
end)

test("scan: wrong protocol version is ignored", function()
	local servers = {
		server(34872, info({ port = 34872, protocolVersion = 4 })),
		server(34873, info({ port = 34873 })),
	}
	local decision = Policy.fromScan(PLACE, servers, PROTOCOL, noPause)
	expectEq(decision.kind, "connect", "kind")
	expectEq(decision.port, 34873, "port")

	local onlyWrong = { server(34872, info({ port = 34872, protocolVersion = 6 })) }
	expectEq(Policy.fromScan(PLACE, onlyWrong, PROTOCOL, noPause).kind, "wait", "only wrong protocol")
end)

test("scan: nil expectedPlaceIds is ignored", function()
	local open = info({ port = 34872 })
	open.expectedPlaceIds = nil
	local servers = { server(34872, open), server(34874, info({ port = 34874 })) }
	local decision = Policy.fromScan(PLACE, servers, PROTOCOL, noPause)
	expectEq(decision.kind, "connect", "kind")
	expectEq(decision.port, 34874, "port")

	expectEq(Policy.fromScan(PLACE, { server(34872, open) }, PROTOCOL, noPause).kind, "wait", "only open server")
end)

test("scan: expectedPlaceIds without our place is ignored", function()
	local servers = { server(34872, info({ port = 34872, expectedPlaceIds = { 1, 2, 3 } })) }
	expectEq(Policy.fromScan(PLACE, servers, PROTOCOL, noPause).kind, "wait", "other places")
	local multi = { server(34872, info({ port = 34872, expectedPlaceIds = { 1, PLACE, 3 } })) }
	expectEq(Policy.fromScan(PLACE, multi, PROTOCOL, noPause).kind, "connect", "listed among others")
end)

test("scan: placeId 0 never connects", function()
	local servers = { server(34872, info({ port = 34872, expectedPlaceIds = { 0 } })) }
	expectEq(Policy.fromScan(0, servers, PROTOCOL, noPause).kind, "wait", "placeId 0")
	expectEq(Policy.fromScan(nil :: any, servers, PROTOCOL, noPause).kind, "wait", "placeId nil")
end)

test("scan: pause rule applies to the single candidate", function()
	local servers = { server(34875, info({ port = 34875 })) }
	local paused = { pausedPort = 34875, pausedSession = "srv-34875" }
	expectEq(Policy.fromScan(PLACE, servers, PROTOCOL, paused).kind, "wait", "same session")
	local other = { pausedPort = 34875, pausedSession = "srv-old" }
	expectEq(Policy.fromScan(PLACE, servers, PROTOCOL, other).kind, "connect", "different session")
	expectEq(Policy.fromScan(PLACE, servers, PROTOCOL).kind, "connect", "no state")
end)

-- buildQuery -----------------------------------------------------------------

test("buildQuery produces the exact string in key order", function()
	local guid = "3F2504E0-4F89-11D3-9A0C-0305E82C3301"
	expectEq(
		Policy.buildQuery(123, guid, "hub"),
		"rojohub-client=sync&rojohub-place=123&rojohub-session=3F2504E0-4F89-11D3-9A0C-0305E82C3301&rojohub-auto=hub",
		"hub"
	)
	expectEq(
		Policy.buildQuery(PLACE, guid, "scan"),
		"rojohub-client=sync&rojohub-place=123456789&rojohub-session=" .. guid .. "&rojohub-auto=scan",
		"scan"
	)
	expectEq(
		Policy.buildQuery(0, guid, "manual"),
		"rojohub-client=sync&rojohub-place=0&rojohub-session=" .. guid .. "&rojohub-auto=manual",
		"manual"
	)
end)

test("buildQuery rejects unknown reasons", function()
	local ok = pcall(Policy.buildQuery, 1, "guid", "auto")
	expect(not ok, "expected an error for an unknown reason")
end)

-- summary --------------------------------------------------------------------

print(("policy tests: %d passed, %d failed"):format(passed, failed))
if failed > 0 then
	process.exit(1)
end
