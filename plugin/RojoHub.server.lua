--!nonstrict
-- RojoHub Studio plugin
-- Tells this Studio window which Rojo port belongs to its place, and runs agent
-- requests only after re-checking that this really is the expected place.

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")

-- Plugins also load inside Play/Team Test data models; only the edit window should talk to the hub.
if not RunService:IsEdit() then
	return
end

local HUB = "http://127.0.0.1:34870"
local VERSION = "0.4.0"
local SESSION_ID = HttpService:GenerateGUID(false)
local HEADERS = { ["Content-Type"] = "application/json", ["X-RojoHub-Client"] = "studio-plugin" }

local alive = true
plugin.Unloading:Connect(function()
	alive = false
end)

---------------------------------------------------------------- UI

local toolbar = plugin:CreateToolbar("RojoHub")
local button = toolbar:CreateButton("RojoHub", "Show which Rojo project and port belong to this place", "")
button.ClickableWhenViewportHidden = true

local widget = plugin:CreateDockWidgetPluginGui(
	"RojoHubStatus",
	DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float, false, false, 340, 230, 280, 180)
)
widget.Title = "RojoHub"
button.Click:Connect(function()
	widget.Enabled = not widget.Enabled
end)
widget:GetPropertyChangedSignal("Enabled"):Connect(function()
	button:SetActive(widget.Enabled)
end)

local theme = settings().Studio.Theme :: any
local function color(c: Enum.StudioStyleGuideColor, m: Enum.StudioStyleGuideModifier?): Color3
	return theme:GetColor(c, m or Enum.StudioStyleGuideModifier.Default)
end

local root = Instance.new("Frame")
root.Size = UDim2.fromScale(1, 1)
root.BackgroundColor3 = color(Enum.StudioStyleGuideColor.MainBackground)
root.BorderSizePixel = 0
root.Parent = widget
local pad = Instance.new("UIPadding")
pad.PaddingTop, pad.PaddingBottom = UDim.new(0, 10), UDim.new(0, 10)
pad.PaddingLeft, pad.PaddingRight = UDim.new(0, 12), UDim.new(0, 12)
pad.Parent = root
local list = Instance.new("UIListLayout")
list.Padding = UDim.new(0, 6)
list.SortOrder = Enum.SortOrder.LayoutOrder
list.Parent = root

local function label(order: number, size: number, bold: boolean): TextLabel
	local l = Instance.new("TextLabel")
	l.LayoutOrder = order
	l.BackgroundTransparency = 1
	l.Size = UDim2.new(1, 0, 0, 0)
	l.AutomaticSize = Enum.AutomaticSize.Y
	l.TextWrapped = true
	l.TextXAlignment = Enum.TextXAlignment.Left
	l.Font = if bold then Enum.Font.SourceSansBold else Enum.Font.SourceSans
	l.TextSize = size
	l.TextColor3 = color(Enum.StudioStyleGuideColor.MainText)
	l.Text = ""
	l.Parent = root
	return l
end

local statusLabel = label(1, 20, true)
local detailLabel = label(2, 15, false)

local portBox = Instance.new("TextBox")
portBox.LayoutOrder = 3
portBox.Size = UDim2.new(1, 0, 0, 30)
portBox.ClearTextOnFocus = false
portBox.TextEditable = false
portBox.Font = Enum.Font.SourceSansBold
portBox.TextSize = 18
portBox.TextColor3 = color(Enum.StudioStyleGuideColor.MainText)
portBox.BackgroundColor3 = color(Enum.StudioStyleGuideColor.InputFieldBackground)
portBox.BorderColor3 = color(Enum.StudioStyleGuideColor.InputFieldBorder)
portBox.Visible = false
portBox.Parent = root

local startButton = Instance.new("TextButton")
startButton.LayoutOrder = 4
startButton.Size = UDim2.new(0, 140, 0, 26)
startButton.Font = Enum.Font.SourceSansSemibold
startButton.TextSize = 15
startButton.TextColor3 = color(Enum.StudioStyleGuideColor.DialogMainButtonText)
startButton.BackgroundColor3 = color(Enum.StudioStyleGuideColor.DialogMainButton)
startButton.Text = "Start Rojo"
startButton.Visible = false
startButton.Parent = root

local noteLabel = label(5, 14, false)
noteLabel.TextColor3 = color(Enum.StudioStyleGuideColor.SubText)

local OK = Color3.fromRGB(46, 160, 98)
local WARN = Color3.fromRGB(214, 142, 20)
local BAD = Color3.fromRGB(214, 64, 52)

local lastStatus = ""
local function render(binding: any?, hubError: string?)
	local status = if hubError then "offline" elseif binding then binding.status else "offline"
	portBox.Visible = false
	startButton.Visible = false
	local notes = {}

	if hubError then
		statusLabel.Text = "RojoHub is not running"
		statusLabel.TextColor3 = WARN
		detailLabel.Text = "Start it with: rojohub daemon (or rojohub install-service)"
		table.insert(notes, hubError)
	elseif binding.status == "bound" then
		local p = binding.project
		local running = p.rojo == "running"
		if running and p.synced then
			statusLabel.Text = `{p.name} — synced`
			statusLabel.TextColor3 = OK
			detailLabel.Text = `PlaceId {game.PlaceId} is verified, and Rojo on this port is connected to this window.`
		elseif running then
			statusLabel.Text = `{p.name} — Rojo ready`
			statusLabel.TextColor3 = OK
			detailLabel.Text = `PlaceId {game.PlaceId} is verified. The RojoHub sync plugin connects on its own; with the stock Rojo plugin, connect to:`
		else
			statusLabel.Text = `{p.name} — Rojo {p.rojo}`
			statusLabel.TextColor3 = if p.rojo == "starting" then WARN else BAD
			detailLabel.Text = `PlaceId {game.PlaceId} is verified for this project. Its reserved port is:`
		end
		portBox.Text = `localhost : {p.port}`
		portBox.Visible = true
		startButton.Visible = not running and p.rojo ~= "starting"
		if p.rojoError then
			table.insert(notes, p.rojoError)
		end
	else
		statusLabel.Text = if binding.status == "unbound" then "No project for this place"
			elseif binding.status == "unpublished" then "Place not published"
			elseif binding.status == "mismatch" then "Wrong game — blocked"
			else "Ambiguous project — blocked"
		statusLabel.TextColor3 = if binding.status == "unbound" then WARN else BAD
		detailLabel.Text = `PlaceId {game.PlaceId}, GameId {game.GameId}`
	end
	for _, m in (binding and binding.messages) or {} do
		table.insert(notes, m)
	end
	noteLabel.Text = table.concat(notes, "\n")

	if status ~= lastStatus then
		if status == "mismatch" or status == "conflict" then
			widget.Enabled = true -- make blocking problems impossible to miss
			warn(`[RojoHub] {statusLabel.Text}: {noteLabel.Text}`)
		elseif status == "bound" and binding then
			print(`[RojoHub] {game.Name} → project "{binding.project.name}", Rojo port {binding.project.port}`)
		end
		lastStatus = status
	end
end

---------------------------------------------------------------- HTTP

local function post(path: string, body: any): (any?, string?)
	local ok, res = pcall(function()
		return HttpService:RequestAsync({
			Url = HUB .. path,
			Method = "POST",
			Headers = HEADERS,
			Body = HttpService:JSONEncode(body),
		})
	end)
	if not ok then
		return nil, tostring(res)
	end
	local decoded = nil
	pcall(function()
		decoded = HttpService:JSONDecode(res.Body)
	end)
	if not res.Success then
		return nil, (decoded and decoded.error) or `HTTP {res.StatusCode}`
	end
	return decoded, nil
end

local currentBinding: any = nil

startButton.MouseButton1Click:Connect(function()
	startButton.Text = "Starting…"
	local r, err = post("/studio/action", { sessionId = SESSION_ID, action = "start" })
	startButton.Text = "Start Rojo"
	if r then
		currentBinding = r.binding
		render(r.binding, nil)
	else
		noteLabel.Text = err or "Failed"
	end
end)

---------------------------------------------------------------- agent operations

local function resolvePath(path: string): Instance
	local node: Instance = game
	for segment in string.gmatch(path, "[^%.]+") do
		if node == game and segment == "game" then
			continue
		end
		local child = node:FindFirstChild(segment)
		if not child and node == game then
			local ok, service = pcall(game.GetService, game, segment)
			if ok then
				child = service
			end
		end
		if not child then
			error(`No child "{segment}" under {node:GetFullName()}`)
		end
		node = child
	end
	return node
end

local function describe(inst: Instance, depth: number, indent: string, out: { string })
	local children = inst:GetChildren()
	local extra = ""
	if inst:IsA("LuaSourceContainer") then
		local ok, src = pcall(function()
			return (inst :: any).Source
		end)
		if ok then
			local _, lines = string.gsub(src, "\n", "")
			extra = ` ({lines + 1} lines)`
		end
	end
	table.insert(out, `{indent}{inst.Name} [{inst.ClassName}]{extra}{if #children > 0 and depth <= 0 then ` +{#children} children` else ""}`)
	if depth <= 0 then
		return
	end
	for i, child in children do
		if i > 200 then
			table.insert(out, `{indent}  … {#children - 200} more`)
			break
		end
		describe(child, depth - 1, indent .. "  ", out)
	end
end

local function stringify(v: any): string
	if typeof(v) == "table" then
		local ok, json = pcall(HttpService.JSONEncode, HttpService, v)
		if ok then
			return json
		end
	elseif typeof(v) == "Instance" then
		return v:GetFullName()
	end
	return tostring(v)
end

local ops = {}

function ops.identity(_args)
	return {
		placeId = game.PlaceId,
		gameId = game.GameId,
		placeVersion = game.PlaceVersion,
		name = game.Name,
		creatorId = game.CreatorId,
		creatorType = tostring(game.CreatorType),
		session = SESSION_ID,
	}, nil
end

function ops.get_tree(args)
	local out = {}
	describe(resolvePath(args.path or "game"), args.depth or 2, "", out)
	return table.concat(out, "\n"), nil
end

function ops.get_source(args)
	local inst = resolvePath(args.path)
	if not inst:IsA("LuaSourceContainer") then
		error(`{inst:GetFullName()} is a {inst.ClassName}, not a script`)
	end
	return (inst :: any).Source, nil
end

function ops.run_luau(args)
	local fn, compileErr = loadstring(args.code, "=agent")
	if not fn then
		error(`Compile error: {compileErr}`)
	end
	local output = {}
	local function capture(prefix: string, ...)
		local parts = table.pack(...)
		for i = 1, parts.n do
			parts[i] = tostring(parts[i])
		end
		table.insert(output, prefix .. table.concat(parts, "\t", 1, parts.n))
	end
	local base = getfenv(0)
	local env = setmetatable({
		print = function(...)
			capture("", ...)
			print("[agent]", ...)
		end,
		warn = function(...)
			capture("[warn] ", ...)
			warn("[agent]", ...)
		end,
	}, {
		__index = function(_, k)
			if k == "plugin" then
				return nil -- agent code does not get this plugin's own handle
			end
			return base[k]
		end,
	})
	setfenv(fn, env)

	local recording = ChangeHistoryService:TryBeginRecording("RojoHub agent", "Code run by an agent via RojoHub")
	local results = table.pack(xpcall(fn, debug.traceback))
	if recording then
		ChangeHistoryService:FinishRecording(
			recording,
			if results[1] then Enum.FinishRecordingOperation.Commit else Enum.FinishRecordingOperation.Cancel
		)
	end
	if not results[1] then
		error(`{results[2]}\nOutput before error:\n{table.concat(output, "\n")}\n(Changes were rolled back.)`)
	end
	local returned = {}
	for i = 2, results.n do
		table.insert(returned, stringify(results[i]))
	end
	return if #returned > 0 then table.concat(returned, ", ") else nil, output
end

local function execute(cmd: any)
	local reply: any = { sessionId = SESSION_ID, id = cmd.id, ok = false }
	local expect = cmd.expect or {}
	-- Second, independent check: the hub routed this here, but we only obey if our place matches.
	if expect.placeId ~= game.PlaceId or (expect.gameId and expect.gameId ~= game.GameId) then
		reply.error = `Refused: this Studio is PlaceId {game.PlaceId} / GameId {game.GameId}, `
			.. `but the request was for PlaceId {expect.placeId}{if expect.gameId then ` / GameId {expect.gameId}` else ""}.`
		warn("[RojoHub] " .. reply.error)
	elseif not currentBinding or currentBinding.status ~= "bound" or currentBinding.project.id ~= expect.projectId then
		reply.error = "Refused: this Studio window is not bound to the requesting project."
	elseif not ops[cmd.op] then
		reply.error = `Unknown operation {cmd.op}`
	else
		local ok, result, output = pcall(ops[cmd.op], cmd.args or {})
		reply.ok = ok
		if ok then
			reply.result = result
			reply.output = output
		else
			reply.error = tostring(result)
		end
	end
	post("/studio/result", reply)
end

---------------------------------------------------------------- main loop

render(nil, "Connecting…")
task.spawn(function()
	local backoff = 2
	local first = true
	while alive do
		local r, err = post("/studio/poll", {
			sessionId = SESSION_ID,
			placeId = game.PlaceId,
			gameId = game.GameId,
			placeName = game.Name,
			userId = (plugin :: any):GetStudioUserId(),
			pluginVersion = VERSION,
			immediate = first,
		})
		if not alive then
			break
		end
		if r then
			first = false
			backoff = 2
			currentBinding = r.binding
			render(r.binding, nil)
			for _, cmd in r.commands or {} do
				task.spawn(execute, cmd)
			end
		else
			render(nil, err)
			first = true
			task.wait(backoff)
			backoff = math.min(backoff * 2, 15)
		end
	end
end)
