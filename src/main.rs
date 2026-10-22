//! `rojohub`: one hub for many Rojo servers and Roblox Studio windows.

mod client;
mod config;
mod daemon;
mod hub;
mod mcp;
mod msgpack;
mod platform;
mod ports;
mod project_file;
mod proxy;
mod rojo;
mod state;
mod util;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use serde_json::{json, Value};

use client::{hub, HubError};
use config::{home_dir, hub_url, VERSION};
use platform::{
    install_service, open_url, service_description, studio_plugins_dir, uninstall_service,
};

const HUB_PLUGIN: &str = include_str!("../plugin/RojoHub.server.lua");
const SYNC_PLUGIN: &[u8] = include_bytes!("../plugin/RojoHubSync.rbxm");

fn help() -> String {
    format!(
        "rojohub {VERSION} — one hub for many Rojo servers and Roblox Studio windows

Setup
  rojohub install-plugin            Install the RojoHub plugin and the RojoHub sync plugin (auto-connect)
                                    into Studio's plugins folder (--dir to override, --stock to keep the stock Rojo plugin)
  rojohub engine [rojo|path]        Which Rojo binary to run for every project (default: rojo on PATH)
  rojohub install-service           Run the hub at login ({}); uninstall-service to remove
  rojohub daemon                    Run the hub in the foreground instead
  rojohub version

Projects (default: the project containing the current folder)
  rojohub add [dir] --place <id>[,<id>] [--game <id>] [--name <n>] [--project <file>] [--auto-start]
  rojohub add [dir] --from-studio   Use the PlaceId of the one unbound Studio window that's open
  rojohub remove <project>
  rojohub start|stop|restart [project]
  rojohub port [project]            Print only the Rojo port (handy in scripts)
  rojohub logs [project]
  rojohub status                    Projects, ports and connected Studio windows
  rojohub open                      Open the dashboard in your browser

Agents
  rojohub mcp                       MCP server over stdio (register it with your agent)
  rojohub mcp-config                Show how to register it with Claude Code and others
",
        service_description()
    )
}

/// `--k=v`, `--k v` and bare `--k`; everything else is positional.
struct Args {
    pos: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Args {
    fn parse(argv: &[String]) -> Args {
        let mut pos = Vec::new();
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(rest) = a.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    flags.insert(k.to_string(), Some(v.to_string()));
                } else if argv
                    .get(i + 1)
                    .map(|n| !n.starts_with("--"))
                    .unwrap_or(false)
                {
                    flags.insert(rest.to_string(), Some(argv[i + 1].clone()));
                    i += 1;
                } else {
                    flags.insert(rest.to_string(), None);
                }
            } else {
                pos.push(a.clone());
            }
            i += 1;
        }
        Args { pos, flags }
    }

    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }

    fn str(&self, k: &str) -> Option<&str> {
        self.flags.get(k).and_then(|v| v.as_deref())
    }
}

fn dot(status: &str) -> &'static str {
    match status {
        "running" => "●",
        "starting" => "◐",
        "unhealthy" => "◑",
        "stopped" => "○",
        "crashed" | "error" => "✕",
        _ => "?",
    }
}

fn s(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    })
}

fn ids(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn encoded(p: &str) -> String {
    util::percent_encode(p)
}

fn current_project(reference: Option<&String>) -> Result<String, HubError> {
    if let Some(r) = reference {
        return Ok(r.clone());
    }
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let r = hub("GET", &format!("/api/resolve?cwd={}", encoded(&cwd)), None)?;
    match r["project"]["id"].as_str() {
        Some(id) => Ok(id.to_string()),
        None => Err(HubError::new(format!(
            "{cwd} is not inside a registered project. Pass a project name or run `rojohub add`."
        ))),
    }
}

fn cmd_add(args: &Args) -> Result<(), HubError> {
    let dir = std::path::absolute(Path::new(
        args.pos.first().map(String::as_str).unwrap_or("."),
    ))
    .map_err(|e| HubError::new(e.to_string()))?;
    let project_file =
        std::path::absolute(dir.join(args.str("project").unwrap_or("default.project.json")))
            .map_err(|e| HubError::new(e.to_string()))?;
    let mut place_ids: Vec<u64> = args
        .str("place")
        .unwrap_or("")
        .split(',')
        .filter_map(|p| p.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .collect();
    let mut game_id: Option<u64> = args
        .str("game")
        .and_then(|g| g.trim().parse().ok())
        .filter(|g: &u64| *g > 0);

    if args.has("from-studio") {
        let status = hub("GET", "/api/status", None)?;
        let open: Vec<Value> = status["studios"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w["binding"]["status"] == "unbound")
            .collect();
        if open.len() != 1 {
            return Err(HubError::new(if open.is_empty() {
                "No unbound Studio window found. Open the published place in Studio (with the RojoHub plugin) first.".to_string()
            } else {
                format!(
                    "{} unbound Studio windows are open ({}). Use --place instead.",
                    open.len(),
                    open.iter()
                        .map(|w| format!("{}={}", s(&w["placeName"]), w["placeId"]))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }));
        }
        place_ids = vec![open[0]["placeId"].as_u64().unwrap_or(0)];
        game_id = game_id.or_else(|| open[0]["gameId"].as_u64().filter(|g| *g > 0));
        println!(
            "Using \"{}\" from Studio: PlaceId {}, GameId {}",
            s(&open[0]["placeName"]),
            place_ids[0],
            game_id
                .map(|g| g.to_string())
                .unwrap_or_else(|| "null".into())
        );
    }
    if place_ids.is_empty() {
        return Err(HubError::new("Pass --place <PlaceId> (find it in Studio: File > Game Settings > Places), or --from-studio."));
    }
    if !project_file.is_file() {
        return Err(HubError::new(format!(
            "No project file at {}. Use --project to point at it.",
            project_file.display()
        )));
    }
    if project_file::patch_serve_place_ids(&project_file, &place_ids, args.has("no-patch"))? {
        println!(
            "Added servePlaceIds {} to {} (backup: .rojohub-backup). Commit this change.",
            json!(place_ids),
            project_file
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
    }
    let r = hub(
        "POST",
        "/api/projects",
        Some(&json!({
            "dir": dir.to_string_lossy(), "projectFile": project_file.to_string_lossy(), "placeIds": place_ids, "gameId": game_id,
            "name": args.str("name"), "autoStart": args.has("auto-start"),
        })),
    )?;
    for w in r["warnings"].as_array().cloned().unwrap_or_default() {
        println!("warning: {}", s(&w));
    }
    let p = &r["project"];
    println!(
        "{} \"{}\" ({}) → Rojo port {}",
        if r["updated"].as_bool().unwrap_or(false) {
            "Updated"
        } else {
            "Registered"
        },
        s(&p["name"]),
        s(&p["id"]),
        p["port"]
    );
    println!("Next: rojohub start {}", s(&p["id"]));
    Ok(())
}

fn print_status(st: &Value) {
    println!(
        "Engine: {}   Ports: {}\n",
        s(&st["hub"]["engine"]),
        ids(&st["hub"]["portRange"]).replace(',', "-")
    );
    let projects = st["projects"].as_array().cloned().unwrap_or_default();
    if projects.is_empty() {
        println!("No projects yet. In a Rojo project folder run: rojohub add . --place <PlaceId>");
    }
    for p in &projects {
        let r = &p["rojo"];
        let status = s(&r["status"]);
        println!(
            "{} {:<24} port {}  {:<9} places {}{}",
            dot(&status),
            s(&p["name"]),
            p["port"],
            status,
            ids(&p["placeIds"]),
            p["gameId"]
                .as_u64()
                .map(|g| format!(" game {g}"))
                .unwrap_or_default()
        );
        println!("    {}", s(&p["dir"]));
        if let Some(e) = r["error"].as_str() {
            println!("    ! {e}");
        }
        for w in p["studios"].as_array().cloned().unwrap_or_default() {
            let id = s(&w["id"]);
            println!(
                "    ↳ Studio \"{}\" (session {})",
                s(&w["placeName"]),
                &id[..id.len().min(8)]
            );
        }
        if let Some(link) = r.get("studioLink").filter(|l| l.is_object()) {
            let auto = link["clients"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .any(|x| x["autoConnected"].as_bool().unwrap_or(false))
                })
                .unwrap_or(false);
            println!(
                "    Sync: {}",
                if link["connected"].as_bool().unwrap_or(false) {
                    format!(
                        "Studio connected{}",
                        if auto { " (auto-connect)" } else { "" }
                    )
                } else {
                    "no Studio connected yet".to_string()
                }
            );
        }
    }
    let loose: Vec<Value> = st["studios"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w["binding"]["status"] != "bound")
        .collect();
    if !loose.is_empty() {
        println!("\nStudio windows not bound to a project:");
        for w in loose {
            println!(
                "  {} (PlaceId {}) — {}: {}",
                s(&w["placeName"]),
                w["placeId"],
                s(&w["binding"]["status"]),
                s(&w["binding"]["messages"][0])
            );
        }
    }
}

fn install_plugin(args: &Args) -> Result<(), HubError> {
    let dest: PathBuf = match args.str("dir") {
        Some(d) => std::path::absolute(d).map_err(|e| HubError::new(e.to_string()))?,
        None => studio_plugins_dir()?,
    };
    fs::create_dir_all(&dest)
        .map_err(|e| HubError::new(format!("Could not create {}: {e}", dest.display())))?;
    let hub_file = dest.join("RojoHub.server.lua");
    fs::write(&hub_file, HUB_PLUGIN).map_err(|e| HubError::new(e.to_string()))?;
    println!("Installed RojoHub plugin: {}", hub_file.display());

    // Same file name as `rojo plugin install`, so the sync plugin replaces the Rojo plugin
    // instead of running beside it.
    let target = dest.join("RojoManagedPlugin.rbxm");
    let backup = dest.join("RojoManagedPlugin.rbxm.rojo-backup");
    let installed_ours = fs::read(&target).map(|b| b == SYNC_PLUGIN).unwrap_or(false);
    if args.has("stock") {
        if backup.is_file() {
            fs::copy(&backup, &target).map_err(|e| HubError::new(e.to_string()))?;
            let _ = fs::remove_file(&backup);
            println!("Restored the stock Rojo plugin: {}", target.display());
        } else if installed_ours {
            let _ = fs::remove_file(&target);
            println!("Removed the RojoHub sync plugin. Install the stock Rojo plugin with: rojo plugin install");
        } else {
            println!("Stock Rojo plugin left as is: {}", target.display());
        }
    } else {
        if target.is_file() && !backup.is_file() && !installed_ours {
            fs::copy(&target, &backup).map_err(|e| HubError::new(e.to_string()))?;
        }
        fs::write(&target, SYNC_PLUGIN).map_err(|e| HubError::new(e.to_string()))?;
        println!(
            "Installed RojoHub sync plugin (auto-connect): {}",
            target.display()
        );
        println!("  It replaces the Rojo plugin of that name; a backup was kept as .rojo-backup (restore it with --stock).");
        println!("  If you also have Rojo from the Creator Store, disable it so two sync plugins don't run.");
    }
    println!("Restart open Studio windows and allow the HTTP permission prompt for 127.0.0.1.");
    Ok(())
}

fn run(argv: Vec<String>) -> Result<i32, HubError> {
    let cmd = argv.first().map(String::as_str).unwrap_or("");
    let args = Args::parse(&argv[argv.len().min(1)..]);
    match cmd {
        "daemon" => daemon::run().map_err(HubError::new)?,
        "mcp" => mcp::run().map_err(HubError::new)?,
        "add" => cmd_add(&args)?,
        "remove" => {
            let name = args
                .pos
                .first()
                .ok_or_else(|| HubError::new("Usage: rojohub remove <project>"))?;
            hub("DELETE", &format!("/api/projects/{}", encoded(name)), None)?;
            println!("Removed {name} (its project file was left as is).");
        }
        "start" | "stop" | "restart" => {
            let id = current_project(args.pos.first())?;
            let r = hub(
                "POST",
                &format!("/api/projects/{}/{cmd}", encoded(&id)),
                None,
            )?;
            let p = &r["project"];
            println!(
                "{}: {} on port {}",
                s(&p["name"]),
                s(&p["rojo"]["status"]),
                p["port"]
            );
            if p["rojo"]["status"] == "running" {
                println!("Studio connects by itself if the RojoHub sync plugin is installed (rojohub install-plugin).");
                println!(
                    "With the stock Rojo plugin, connect it to localhost:{}.",
                    p["port"]
                );
            }
        }
        "port" => {
            let id = current_project(args.pos.first())?;
            let st = hub("GET", "/api/status", None)?;
            let p = st["projects"]
                .as_array()
                .and_then(|ps| ps.iter().find(|p| p["id"] == id).cloned())
                .ok_or_else(|| HubError::new(format!("Unknown project \"{id}\".")))?;
            println!("{}", p["port"]);
        }
        "logs" => {
            let id = current_project(args.pos.first())?;
            let r = hub("GET", &format!("/api/projects/{}/logs", encoded(&id)), None)?;
            let lines: Vec<String> = r["logs"]
                .as_array()
                .map(|a| a.iter().map(s).collect())
                .unwrap_or_default();
            let text = lines.join("\n");
            println!(
                "{}",
                if text.is_empty() {
                    "(no output yet)"
                } else {
                    &text
                }
            );
        }
        "status" | "list" | "ls" => {
            let st = hub("GET", "/api/status", None)?;
            if args.has("json") {
                println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
            } else {
                print_status(&st);
            }
        }
        "open" => open_url(&format!("{}/", hub_url())),
        "install-plugin" => install_plugin(&args)?,
        "engine" => match args.pos.first() {
            None => {
                let st = hub("GET", "/api/status", None)?;
                println!("Engine: {}", s(&st["hub"]["engine"]));
            }
            Some(e) => {
                let r = hub("POST", "/api/engine", Some(&json!({ "engine": e })))?;
                println!("Engine: {}. {}", s(&r["engine"]), s(&r["note"]));
            }
        },
        "install-service" => {
            let log_file = home_dir().join("hub.log");
            let exe = env::current_exe().map_err(|e| HubError::new(e.to_string()))?;
            let note = install_service(&exe, &log_file, &hub_url())?;
            println!(
                "{note} Logs: {}. Dashboard: {}/",
                log_file.display(),
                hub_url()
            );
        }
        "uninstall-service" => {
            // Only Windows has no service manager to stop the hub for us; ask it to stop itself.
            if cfg!(windows) {
                let _ = hub("POST", "/api/shutdown", None);
            }
            uninstall_service();
            println!("Service removed.");
        }
        "mcp-config" => {
            let exe = env::current_exe().map_err(|e| HubError::new(e.to_string()))?;
            let exe = exe.to_string_lossy();
            println!(
                "Claude Code (run once; the agent is locked to whichever project folder it starts in):\n  claude mcp add --scope user rojohub -- \"{exe}\" mcp\n\nCodex:\n  codex mcp add rojohub -- \"{exe}\" mcp\n\nAny other MCP client (JSON config):\n{}",
                serde_json::to_string_pretty(&json!({ "mcpServers": { "rojohub": { "command": exe, "args": ["mcp"] } } })).unwrap_or_default()
            );
        }
        "version" | "--version" => println!("{VERSION}"),
        _ => {
            print!("{}", help());
            if !cmd.is_empty() && cmd != "help" && cmd != "--help" {
                return Ok(1);
            }
        }
    }
    Ok(0)
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    match run(argv) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("rojohub: {}", e.message);
            exit(1);
        }
    }
}
