//! The MCP server over stdio: JSON-RPC, one message per line. An agent started inside a
//! registered project folder is locked to that project, so it can never drive another
//! project's Studio window by accident.

use std::env;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::client::{hub as hub_request, hub_answers, HubError};
use crate::config::{home_dir, hub_url, VERSION};
use crate::platform::start_daemon;

static AUTOSTARTED: AtomicBool = AtomicBool::new(false);

fn allow_any() -> bool {
    env::var("ROJOHUB_ALLOW_ANY_PROJECT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn cwd() -> String {
    env::var("ROJOHUB_PROJECT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".into())
        })
}

/// Agents often run without the login service installed (e.g. installed as a Claude Code
/// plugin), so the first tool call starts the hub if nothing answers.
fn hub(method: &str, path: &str, body: Option<&Value>) -> Result<Value, HubError> {
    match hub_request(method, path, body) {
        Ok(v) => Ok(v),
        Err(e) => {
            let no_autostart = env::var("ROJOHUB_NO_AUTOSTART")
                .map(|v| v == "1")
                .unwrap_or(false);
            if e.status != 0 || no_autostart || AUTOSTARTED.swap(true, Ordering::SeqCst) {
                return Err(e);
            }
            let exe = env::current_exe()
                .map_err(|e| HubError::new(format!("Cannot find my own executable: {e}")))?;
            start_daemon(&exe, &home_dir().join("hub.log"))
                .map_err(|e| HubError::new(format!("Could not start the hub: {e}")))?;
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(250));
                if hub_answers(&hub_url(), Duration::from_millis(500)) {
                    break;
                }
            }
            hub_request(method, path, body)
        }
    }
}

fn tools() -> Value {
    let session_arg = json!({ "session_id": { "type": "string", "description": "Only needed when several Studio windows have the same place open; the error message lists them." } });
    let project_arg = json!({ "project": { "type": "string", "description": "Project id or name. Defaults to the project containing the agent's working directory." } });
    let merge = |parts: &[&Value]| {
        let mut m = serde_json::Map::new();
        for p in parts {
            if let Some(o) = p.as_object() {
                for (k, v) in o {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(m)
    };
    json!([
        {
            "name": "rojo_info",
            "description": "Which Rojo project this workspace is, its dedicated port, whether Rojo is running and which Roblox Studio windows are connected. Call this first.",
            "inputSchema": { "type": "object", "properties": project_arg },
        },
        {
            "name": "rojo_start",
            "description": "Start (or restart) this project's Rojo server on its reserved port. Rojo is configured to refuse any place other than this project's PlaceIds.",
            "inputSchema": { "type": "object", "properties": merge(&[&project_arg, &json!({ "restart": { "type": "boolean" } })]) },
        },
        {
            "name": "rojo_wait_for_studio",
            "description": "Wait until Studio's sync plugin has actually connected to this project's server. Use after rojo_start before relying on changes appearing in Studio.",
            "inputSchema": { "type": "object", "properties": merge(&[&project_arg, &json!({ "timeout_seconds": { "type": "number" } })]) },
        },
        {
            "name": "rojo_stop",
            "description": "Stop this project's Rojo server.",
            "inputSchema": { "type": "object", "properties": project_arg },
        },
        {
            "name": "rojo_logs",
            "description": "Recent output of this project's Rojo server (build errors, sync messages).",
            "inputSchema": { "type": "object", "properties": merge(&[&project_arg, &json!({ "lines": { "type": "number" } })]) },
        },
        {
            "name": "studio_identity",
            "description": "Ask the Studio window bound to this project for its PlaceId, GameId, name and version, to confirm you are looking at the right place.",
            "inputSchema": { "type": "object", "properties": merge(&[&project_arg, &session_arg]) },
        },
        {
            "name": "studio_get_tree",
            "description": "List the instance tree under a path in this project's Studio window (e.g. 'ReplicatedStorage.Shared', 'Workspace'). Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": merge(&[&project_arg, &session_arg, &json!({
                    "path": { "type": "string", "description": "Dot path from game, e.g. 'ServerScriptService' or 'Workspace.Map'. Default 'game'." },
                    "depth": { "type": "number", "description": "Levels to descend (default 2, max 6)." },
                })]),
            },
        },
        {
            "name": "studio_get_source",
            "description": "Read the Source of a script in this project's Studio window. Read-only. Scripts synced by Rojo are best read from the files on disk.",
            "inputSchema": { "type": "object", "properties": merge(&[&project_arg, &session_arg, &json!({ "path": { "type": "string" } })]), "required": ["path"] },
        },
        {
            "name": "studio_run_luau",
            "description": "Run Luau in edit mode in this project's Studio window, after the plugin re-checks the PlaceId. print()/warn() output and return values come back. Changes are recorded as one undo step and rolled back if the code errors. Do NOT edit scripts that Rojo syncs here; edit the files on disk instead, or Rojo will overwrite your change.",
            "inputSchema": {
                "type": "object",
                "properties": merge(&[&project_arg, &session_arg, &json!({ "code": { "type": "string" }, "timeout_seconds": { "type": "number" } })]),
                "required": ["code"],
            },
        },
    ])
}

fn locked_project() -> Result<Value, HubError> {
    let r = hub(
        "GET",
        &format!("/api/resolve?cwd={}", crate::util::percent_encode(&cwd())),
        None,
    )?;
    Ok(r.get("project").cloned().unwrap_or(Value::Null))
}

fn pick_project(args: &Value) -> Result<Value, HubError> {
    let home = locked_project()?;
    if let Some(wanted) = args["project"].as_str().filter(|s| !s.is_empty()) {
        if !home.is_null() && !allow_any() && home["id"] != wanted && home["name"] != wanted {
            return Err(HubError::new(format!(
                "This agent is working in \"{}\" ({}) and may only control that project. Refusing to touch \"{wanted}\".",
                home["name"].as_str().unwrap_or(""),
                home["dir"].as_str().unwrap_or("")
            )));
        }
        let status = hub("GET", "/api/status", None)?;
        return status["projects"]
            .as_array()
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p["id"] == wanted || p["name"] == wanted)
                    .cloned()
            })
            .ok_or_else(|| HubError::new(format!("Unknown project \"{wanted}\".")));
    }
    if home.is_null() {
        return Err(HubError::new(format!(
            "{} is not inside a project registered with RojoHub. Run `rojohub add . --place <PlaceId>` there, or pass project.",
            cwd()
        )));
    }
    Ok(home)
}

fn ids(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn describe(p: &Value) -> String {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let port = p["port"].as_u64().unwrap_or(0);
    let rojo = &p["rojo"];
    let mut lines = vec![
        format!("Project: {} (id {})", s(&p["name"]), s(&p["id"])),
        format!("Folder: {}", s(&p["dir"])),
        format!("Dashboard: {}/", hub_url()),
        format!("Rojo port: {port}  (the RojoHub sync plugin in Studio finds it automatically; stock Rojo plugin: localhost:{port})"),
        format!(
            "Rojo status: {}{}",
            s(&rojo["status"]),
            rojo["error"].as_str().map(|e| format!(" — {e}")).unwrap_or_default()
        ),
        format!(
            "Allowed PlaceIds: {}{}",
            ids(&p["placeIds"]),
            p["gameId"].as_u64().map(|g| format!("  GameId: {g}")).unwrap_or_default()
        ),
    ];
    if let Some(count) = rojo["restartCount"].as_u64().filter(|c| *c > 0) {
        lines.push(format!("Rojo restarts: {count}"));
    }
    if let Some(warning) = rojo["warning"].as_str() {
        lines.push(format!("Warning: {warning}"));
    }
    if let Some(link) = rojo.get("studioLink").filter(|l| l.is_object()) {
        let text = if link["connected"].as_bool().unwrap_or(false) {
            let clients: Vec<String> = link["clients"]
                .as_array()
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            let auto = if c["autoConnected"].as_bool().unwrap_or(false) {
                                format!(", auto via {}", c["matchReason"].as_str().unwrap_or("hub"))
                            } else {
                                String::new()
                            };
                            let place = c["placeId"]
                                .as_u64()
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "unknown".into());
                            format!("place {place}{auto}")
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("connected ({})", clients.join("; "))
        } else {
            "no Studio has connected to Rojo yet".to_string()
        };
        lines.push(format!("Studio sync: {text}"));
    }
    if let Some(v) = rojo.get("verified").filter(|v| v.is_object()) {
        lines.push(format!(
            "Verified: Rojo {} serving \"{}\" only to places {}",
            s(&v["serverVersion"]),
            s(&v["projectName"]),
            ids(&v["expectedPlaceIds"])
        ));
    }
    let studios = p["studios"].as_array().cloned().unwrap_or_default();
    lines.push(if studios.is_empty() {
        "Studio windows: none open with this place (open it in Studio with the RojoHub plugin installed)".to_string()
    } else {
        format!(
            "Studio windows: {}",
            studios.iter().map(|w| format!("\"{}\" [session {}]", s(&w["placeName"]), s(&w["id"]))).collect::<Vec<_>>().join(", ")
        )
    });
    lines.join("\n")
}

fn studio(
    args: &Value,
    op: &str,
    op_args: Value,
    timeout_ms: u64,
) -> Result<(Value, Value), HubError> {
    let p = pick_project(args)?;
    let r = hub(
        "POST",
        "/api/studio/exec",
        Some(
            &json!({ "project": p["id"], "sessionId": args["session_id"], "op": op, "args": op_args, "timeoutMs": timeout_ms }),
        ),
    )?;
    if !r["ok"].as_bool().unwrap_or(false) {
        return Err(HubError::new(
            r["error"]
                .as_str()
                .unwrap_or("Studio reported an error")
                .to_string(),
        ));
    }
    Ok((p, r))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "nil".to_string(),
        other => other.to_string(),
    }
}

fn encoded(p: &str) -> String {
    crate::util::percent_encode(p)
}

fn call_tool(name: &str, args: &Value) -> Result<String, HubError> {
    match name {
        "rojo_info" => Ok(describe(&pick_project(args)?)),
        "rojo_start" => {
            let p = pick_project(args)?;
            let action = if args["restart"].as_bool().unwrap_or(false) {
                "restart"
            } else {
                "start"
            };
            let r = hub(
                "POST",
                &format!(
                    "/api/projects/{}/{action}",
                    encoded(p["id"].as_str().unwrap_or(""))
                ),
                None,
            )?;
            Ok(describe(&r["project"]))
        }
        "rojo_wait_for_studio" => {
            let p = pick_project(args)?;
            let secs = args["timeout_seconds"]
                .as_f64()
                .filter(|t| *t > 0.0)
                .unwrap_or(60.0)
                .clamp(1.0, 300.0);
            let until = Instant::now() + Duration::from_secs_f64(secs);
            while Instant::now() < until {
                let status = hub("GET", "/api/status", None)?;
                let cur = status["projects"]
                    .as_array()
                    .and_then(|ps| ps.iter().find(|x| x["id"] == p["id"]).cloned())
                    .unwrap_or(Value::Null);
                let st = cur["rojo"]["status"].as_str().unwrap_or("stopped");
                if st != "running" && st != "starting" {
                    return Err(HubError::new(format!("Rojo is {st}. Start it first.")));
                }
                if cur["rojo"]["studioLink"]["connected"]
                    .as_bool()
                    .unwrap_or(false)
                {
                    return Ok(describe(&cur));
                }
                thread::sleep(Duration::from_secs(1));
            }
            Err(HubError::new("Timed out: no Studio connected. Is the place open in Studio with the RojoHub sync plugin installed?"))
        }
        "rojo_stop" => {
            let p = pick_project(args)?;
            let r = hub(
                "POST",
                &format!(
                    "/api/projects/{}/stop",
                    encoded(p["id"].as_str().unwrap_or(""))
                ),
                None,
            )?;
            Ok(describe(&r["project"]))
        }
        "rojo_logs" => {
            let p = pick_project(args)?;
            let r = hub(
                "GET",
                &format!(
                    "/api/projects/{}/logs",
                    encoded(p["id"].as_str().unwrap_or(""))
                ),
                None,
            )?;
            let n = args["lines"]
                .as_f64()
                .filter(|n| *n >= 1.0)
                .map(|n| n.floor() as usize)
                .unwrap_or(80);
            let logs: Vec<String> = r["logs"]
                .as_array()
                .map(|a| a.iter().map(text_of).collect())
                .unwrap_or_default();
            let start = logs.len().saturating_sub(n);
            let out = logs[start..].join("\n");
            Ok(if out.is_empty() {
                "(no output yet)".to_string()
            } else {
                out
            })
        }
        "studio_identity" => {
            let (p, r) = studio(args, "identity", json!({}), 30_000)?;
            Ok(format!(
                "{}\nMatches project \"{}\" (allowed PlaceIds {}).",
                serde_json::to_string_pretty(&r["result"]).unwrap_or_default(),
                p["name"].as_str().unwrap_or(""),
                ids(&p["placeIds"])
            ))
        }
        "studio_get_tree" => {
            let depth = args["depth"]
                .as_f64()
                .filter(|d| *d > 0.0)
                .unwrap_or(2.0)
                .clamp(1.0, 6.0);
            let path = args["path"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("game");
            let (_, r) = studio(
                args,
                "get_tree",
                json!({ "path": path, "depth": depth }),
                30_000,
            )?;
            Ok(text_of(&r["result"]))
        }
        "studio_get_source" => {
            let (_, r) = studio(args, "get_source", json!({ "path": args["path"] }), 30_000)?;
            Ok(text_of(&r["result"]))
        }
        "studio_run_luau" => {
            let secs = args["timeout_seconds"]
                .as_f64()
                .filter(|t| *t > 0.0)
                .unwrap_or(30.0)
                .clamp(1.0, 120.0);
            let (_, r) = studio(
                args,
                "run_luau",
                json!({ "code": args["code"] }),
                (secs * 1000.0) as u64,
            )?;
            let out: Vec<String> = r["output"]
                .as_array()
                .map(|a| a.iter().map(text_of).collect())
                .unwrap_or_default();
            let out = out.join("\n");
            Ok(format!(
                "{}Returned: {}",
                if out.is_empty() {
                    String::new()
                } else {
                    format!("Output:\n{out}\n")
                },
                text_of(&r["result"])
            ))
        }
        other => Err(HubError::new(format!("Unknown tool {other}"))),
    }
}

fn handle(line: &str, out: &Arc<Mutex<io::Stdout>>) {
    let write = |msg: Value| {
        if let Ok(mut o) = out.lock() {
            let _ = writeln!(o, "{msg}");
            let _ = o.flush();
        }
    };
    let msg: Value = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(_) => {
            return write(
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } }),
            )
        }
    };
    let Some(id) = msg.get("id").filter(|i| !i.is_null()).cloned() else {
        return;
    }; // notification
    let method = msg["method"].as_str().unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "rojohub", "version": VERSION },
            "instructions": "Tools for the Rojo project in your working directory and the Roblox Studio window that has its place open. Edit code as files on disk; Rojo syncs them into Studio on the project's dedicated port.",
        }),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .filter(|a| a.is_object())
                .unwrap_or_else(|| json!({}));
            match call_tool(name, &args) {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => {
                    let extra = e
                        .body
                        .get("sessions")
                        .map(|s| format!("\nSessions: {s}"))
                        .unwrap_or_default();
                    json!({ "content": [{ "type": "text", "text": format!("{}{extra}", e.message) }], "isError": true })
                }
            }
        }
        "ping" => json!({}),
        other => {
            return write(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {other}") } }),
            )
        }
    };
    write(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

pub fn run() -> Result<(), String> {
    let out = Arc::new(Mutex::new(io::stdout()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let stdin = io::stdin();
    let mut handles = Vec::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let out = out.clone();
        let in_flight = in_flight.clone();
        in_flight.fetch_add(1, Ordering::SeqCst);
        handles.push(thread::spawn(move || {
            handle(&line, &out);
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }));
        handles.retain(|h| !h.is_finished());
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
