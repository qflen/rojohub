//! The hub: an HTTP API on 127.0.0.1 for the CLI, the dashboard, agents and the Studio
//! plugins. Studio windows long-poll `/studio/poll`; agents post commands that the hub
//! routes only to a window bound to the agent's project.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, HOST, ORIGIN};
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::config::{engine_usable, get_token, hub_port, token_file, HUB_HOST, VERSION};
use crate::hub::{bind, Hub, Inner, Pending, Session, Waiter, Window};
use crate::msgpack::value_as_id;
use crate::platform::open_path;
use crate::ports::allocate_port;
use crate::proxy::ProxyBody;
use crate::rojo::reap_orphans;
use crate::state::{load_state, project_for_dir, read_project_file, slugify, Project};
use crate::util::{constant_time_eq, iso_now, log, now_ms, parse_query, percent_decode};

const POLL_HOLD_MS: u64 = 15_000;
const SESSION_TTL_MS: u64 = 45_000;
const DASHBOARD: &str = include_str!("dashboard.html");
const CSP: &str = "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data: https://tr.rbxcdn.com; connect-src 'self'; frame-ancestors 'none'";

type Resp = Response<ProxyBody>;

pub struct HttpError {
    pub status: u16,
    pub message: String,
    pub extra: Map<String, Value>,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> HttpError {
        HttpError {
            status,
            message: message.into(),
            extra: Map::new(),
        }
    }
}

impl From<String> for HttpError {
    fn from(message: String) -> HttpError {
        HttpError::new(500, message)
    }
}

fn body_of(text: String) -> ProxyBody {
    Full::new(Bytes::from(text))
        .map_err(|never| match never {})
        .boxed()
}

fn send(status: u16, body: &Value) -> Resp {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .body(body_of(body.to_string()))
        .unwrap_or_else(|_| Response::new(body_of(String::new())))
}

fn offline() -> bool {
    env::var("ROJOHUB_OFFLINE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn random_id() -> String {
    let b: [u8; 16] = rand::random();
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn run() -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(serve())
}

async fn serve() -> Result<(), String> {
    let token = get_token().map_err(|e| format!("Could not create the token file: {e}"))?;
    let mut state = load_state();
    if let Some(engine) = state.engine.clone() {
        if !engine_usable(&engine) {
            log(&format!(
                "Ignoring engine setting \"{engine}\": no such binary. Using rojo from PATH."
            ));
            state.engine = None;
            let _ = crate::state::save_state(&state);
        }
    }
    let project_count = state.projects.len();
    let (changes_tx, mut changes_rx) = mpsc::unbounded_channel::<()>();
    let hub = Arc::new(Hub {
        token,
        changes: changes_tx,
        inner: Mutex::new(Inner::new(state)),
    });

    reap_orphans();

    let port = hub_port();
    let listener = TcpListener::bind((HUB_HOST, port)).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("Port {port} is busy. Is another RojoHub already running? (rojohub status)")
        } else {
            e.to_string()
        }
    })?;
    log(&format!(
        "RojoHub {VERSION} listening on http://{HUB_HOST}:{port} ({project_count} projects)"
    ));

    // Proxies and supervisors report changes over a channel; coalesce them into one flush.
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            while changes_rx.recv().await.is_some() {
                while changes_rx.try_recv().is_ok() {}
                hub.lock().notify_all();
            }
        });
    }
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                hub.lock().sweep_sessions(SESSION_TTL_MS);
                hub.health_tick().await;
            }
        });
    }
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let ids: Vec<String> = hub
                .lock()
                .state
                .projects
                .iter()
                .map(|p| p.id.clone())
                .collect();
            for id in ids {
                resolve_names(hub.clone(), id).await;
            }
            load_icons(hub.clone()).await;
        });
    }
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let auto: Vec<(String, String)> = hub
                .lock()
                .state
                .projects
                .iter()
                .filter(|p| p.auto_start)
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            for (id, name) in auto {
                let hub = hub.clone();
                tokio::spawn(async move {
                    if let Err(e) = hub.start_project(&id).await {
                        log(&format!("{name}: {e}"));
                    }
                });
            }
        });
    }

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            let _ = tx.send(()).await;
        });
    }

    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r { Ok((s, _)) => s, Err(_) => continue },
            _ = shutdown_rx.recv() => break,
        };
        let _ = stream.set_nodelay(true);
        let hub = hub.clone();
        let shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let hub = hub.clone();
                let shutdown = shutdown.clone();
                async move { Ok::<_, hyper::Error>(handle(hub, shutdown, req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(20))
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
    // Also reachable as POST /api/shutdown: on Windows there is no signal that lets the hub
    // stop its Rojo children before it dies.
    log("Shutting down, stopping Rojo servers...");
    tokio::time::sleep(Duration::from_millis(200)).await;
    hub.stop_all().await;
    std::process::exit(0);
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn handle(hub: Arc<Hub>, shutdown: mpsc::Sender<()>, req: Request<Incoming>) -> Resp {
    match route(hub, shutdown, req).await {
        Ok(r) => r,
        Err(e) => {
            if e.status == 500 {
                log(&format!("Error: {}", e.message));
            }
            let mut body = Map::new();
            body.insert("error".into(), Value::String(e.message));
            for (k, v) in e.extra {
                body.insert(k, v);
            }
            send(e.status, &Value::Object(body))
        }
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Value, HttpError> {
    let collected = Limited::new(req.into_body(), 2_000_000)
        .collect()
        .await
        .map_err(|_| HttpError::new(413, "Body too large"))?
        .to_bytes();
    if collected.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(&collected).map_err(|_| HttpError::new(400, "Invalid JSON"))
}

fn header<'a>(req: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

fn guard(hub: &Hub, req: &Request<Incoming>, path: &str) -> Result<(), HttpError> {
    let port = hub_port();
    // DNS-rebinding defence: only answer to our own host names.
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return Err(HttpError::new(403, "Bad Host header"));
    }
    // Cross-site defence: browsers attach Origin to cross-origin requests; Studio does not.
    if let Some(origin) = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin != format!("http://127.0.0.1:{port}")
            && origin != format!("http://localhost:{port}")
        {
            return Err(HttpError::new(403, "Cross-origin requests are not allowed"));
        }
    }
    if path.starts_with("/studio/") {
        // A custom header forces a CORS preflight in browsers, which we never approve.
        if header(req, "x-rojohub-client") != Some("studio-plugin") {
            return Err(HttpError::new(
                403,
                "Studio endpoints are for the RojoHub plugin only",
            ));
        }
    } else if path.starts_with("/api/") {
        let auth = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let given = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
            .unwrap_or(auth)
            .trim();
        if !constant_time_eq(given.as_bytes(), hub.token.as_bytes()) {
            return Err(HttpError::new(
                401,
                format!("Missing or wrong token (see {})", token_file().display()),
            ));
        }
    }
    Ok(())
}

/// `/api/projects/{id}` and `/api/projects/{id}/{action}`.
fn project_route(path: &str) -> Option<(String, Option<&str>)> {
    let rest = path.strip_prefix("/api/projects/")?;
    let mut parts = rest.split('/');
    let id = parts.next().filter(|s| !s.is_empty())?;
    let action = parts.next();
    if parts.next().is_some() {
        return None;
    }
    match action {
        None => Some((percent_decode(id), None)),
        Some(a) if ["start", "stop", "restart", "logs", "reveal"].contains(&a) => {
            Some((percent_decode(id), Some(a)))
        }
        Some(_) => None,
    }
}

async fn route(
    hub: Arc<Hub>,
    shutdown: mpsc::Sender<()>,
    req: Request<Incoming>,
) -> Result<Resp, HttpError> {
    if req.method() == Method::OPTIONS {
        return Err(HttpError::new(403, "CORS is not supported"));
    }
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    guard(&hub, &req, &path)?;
    let method = req.method().clone();

    if method == Method::GET && path == "/" {
        let html = DASHBOARD.replacen("__ROJOHUB_TOKEN__", &hub.token, 1);
        return Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Cache-Control", "no-store")
            .header("Content-Security-Policy", CSP)
            .body(body_of(html))
            .unwrap_or_else(|_| Response::new(body_of(String::new()))));
    }
    if method == Method::GET && path == "/health" {
        return Ok(send(200, &json!({ "ok": true, "version": VERSION })));
    }

    // ----- Studio plugins -----
    if method == Method::POST && path == "/studio/poll" {
        return studio_poll(hub, req).await;
    }
    if method == Method::POST && path == "/studio/result" {
        let b = read_body(req).await?;
        let id = b["id"].as_str().unwrap_or("").to_string();
        let mut inner = hub.lock();
        let Some(pend) = inner.pending.get(&id) else {
            return Ok(send(200, &json!({ "ok": true, "stale": true })));
        };
        if Some(pend.session_id.as_str()) != b["sessionId"].as_str() {
            return Err(HttpError::new(403, "Result from the wrong session"));
        }
        if let Some(pend) = inner.pending.remove(&id) {
            let _ = pend.tx.send(json!({
                "ok": b["ok"].as_bool().unwrap_or(false),
                "result": b.get("result").cloned().unwrap_or(Value::Null),
                "output": b.get("output").cloned().unwrap_or(Value::Null),
                "error": b.get("error").cloned().unwrap_or(Value::Null),
            }));
        }
        return Ok(send(200, &json!({ "ok": true })));
    }
    if method == Method::POST && path == "/studio/action" {
        let b = read_body(req).await?;
        let sid = b["sessionId"].as_str().unwrap_or("").to_string();
        let action = b["action"].as_str().unwrap_or("").to_string();
        let project_id = {
            let inner = hub.lock();
            let s = inner
                .sessions
                .get(&sid)
                .ok_or_else(|| HttpError::new(404, "Unknown session"))?;
            let binding = inner.binding_for(s);
            if binding["status"] != "bound" {
                return Err(HttpError::new(409, "This place is not bound to a project"));
            }
            binding["project"]["id"].as_str().unwrap_or("").to_string()
        };
        match action.as_str() {
            "start" => {
                let _ = hub.start_project(&project_id).await;
            }
            "stop" => {
                hub.stop_project(&project_id).await;
            }
            _ => return Err(HttpError::new(400, "Unknown action")),
        }
        let inner = hub.lock();
        let binding = inner
            .sessions
            .get(&sid)
            .map(|s| inner.binding_for(s))
            .unwrap_or(Value::Null);
        return Ok(send(200, &json!({ "binding": binding })));
    }
    if method == Method::POST && path == "/studio/binding" {
        // The sync plugin asks which project serves this window's place, without becoming a session.
        let b = read_body(req).await?;
        let window = Window {
            id: String::new(),
            place_id: b.get("placeId").and_then(value_as_id).unwrap_or(0),
            game_id: b.get("gameId").and_then(value_as_id).unwrap_or(0),
        };
        let inner = hub.lock();
        let binding = bind(&inner.state.projects, &inner.runtimes, &[], &window);
        return Ok(send(
            200,
            &json!({ "binding": binding, "hubVersion": VERSION }),
        ));
    }

    // ----- CLI / agents / dashboard (token required) -----
    if method == Method::GET && path == "/api/status" {
        return Ok(send(200, &hub.lock().status_view()));
    }
    if method == Method::POST && path == "/api/engine" {
        let b = read_body(req).await?;
        let engine = b["engine"]
            .as_str()
            .filter(|e| !e.is_empty())
            .ok_or_else(|| HttpError::new(400, "engine must be rojo or a path"))?;
        let mut inner = hub.lock();
        inner.state.engine = Some(engine.to_string());
        inner.save();
        inner.notify_all();
        let label = crate::config::resolve_engine(None, inner.state.engine.as_deref()).label;
        return Ok(send(
            200,
            &json!({ "engine": label, "note": "Applies the next time each project's server starts." }),
        ));
    }
    if method == Method::GET && path == "/api/resolve" {
        let cwd = parse_query(&query)
            .into_iter()
            .find(|(k, _)| k == "cwd")
            .map(|(_, v)| v)
            .unwrap_or_else(|| "/nonexistent".into());
        let inner = hub.lock();
        let project = project_for_dir(&inner.state.projects, &cwd)
            .map(|p| inner.project_view_by_id(&p.id))
            .unwrap_or(Value::Null);
        return Ok(send(200, &json!({ "project": project })));
    }
    if method == Method::POST && path == "/api/projects" {
        let b = read_body(req).await?;
        return Ok(send(200, &add_project(hub, b).await?));
    }
    if let Some((id, action)) = project_route(&path) {
        match (method.clone(), action) {
            (Method::DELETE, None) => {
                let pid = hub
                    .lock()
                    .find_project(&id)
                    .map(|p| p.id.clone())
                    .ok_or_else(|| HttpError::new(404, format!("Unknown project \"{id}\".")))?;
                hub.stop_project(&pid).await;
                let mut inner = hub.lock();
                inner.state.projects.retain(|p| p.id != pid);
                inner.save();
                inner.notify_all();
                return Ok(send(200, &json!({ "removed": pid })));
            }
            (Method::POST, Some("reveal")) => {
                let dir = hub
                    .lock()
                    .find_project(&id)
                    .map(|p| p.dir.clone())
                    .ok_or_else(|| HttpError::new(404, format!("Unknown project \"{id}\".")))?;
                open_path(&dir);
                return Ok(send(200, &json!({ "ok": true })));
            }
            (Method::POST, Some(a @ ("start" | "stop" | "restart"))) => {
                let pid = hub
                    .lock()
                    .find_project(&id)
                    .map(|p| p.id.clone())
                    .ok_or_else(|| HttpError::new(404, format!("Unknown project \"{id}\".")))?;
                if a == "stop" || a == "restart" {
                    hub.stop_project(&pid).await;
                }
                if a == "start" || a == "restart" {
                    hub.start_project(&pid)
                        .await
                        .map_err(|e| HttpError::new(409, e))?;
                }
                return Ok(send(
                    200,
                    &json!({ "project": hub.lock().project_view_by_id(&pid) }),
                ));
            }
            (Method::GET, Some("logs")) => {
                let mut inner = hub.lock();
                let pid = inner
                    .find_project(&id)
                    .map(|p| p.id.clone())
                    .ok_or_else(|| HttpError::new(404, format!("Unknown project \"{id}\".")))?;
                let logs: Vec<String> = inner.runtime(&pid).logs.iter().cloned().collect();
                return Ok(send(200, &json!({ "logs": logs })));
            }
            _ => {}
        }
    }
    if method == Method::POST && path == "/api/shutdown" {
        let _ = shutdown.try_send(()); // answer first; the accept loop exits and stops Rojo
        return Ok(send(200, &json!({ "ok": true })));
    }
    if method == Method::POST && path == "/api/studio/exec" {
        let b = read_body(req).await?;
        let reference = b["project"].as_str().unwrap_or("").to_string();
        let project = hub
            .lock()
            .find_project(&reference)
            .cloned()
            .ok_or_else(|| HttpError::new(404, format!("Unknown project \"{reference}\".")))?;
        let allowed = ["identity", "run_luau", "get_tree", "get_source"];
        let op = b["op"].as_str().unwrap_or("").to_string();
        if !allowed.contains(&op.as_str()) {
            return Err(HttpError::new(
                400,
                format!("op must be one of {}", allowed.join(", ")),
            ));
        }
        let timeout_ms = b["timeoutMs"]
            .as_f64()
            .filter(|t| *t > 0.0)
            .unwrap_or(30_000.0)
            .clamp(1000.0, 120_000.0) as u64;
        let session_id = b["sessionId"].as_str().map(str::to_string);
        let args = b
            .get("args")
            .cloned()
            .filter(|a| a.is_object())
            .unwrap_or_else(|| json!({}));
        return Ok(send(
            200,
            &exec(hub, &project, session_id, &op, args, timeout_ms).await?,
        ));
    }
    Err(HttpError::new(404, "Not found"))
}

/// Removes a long-poll waiter when the Studio window gives up before an answer.
struct WaiterGuard {
    hub: Arc<Hub>,
    session_id: String,
    waiter_id: u64,
    armed: bool,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut inner = self.hub.lock();
        if let Some(s) = inner.sessions.get_mut(&self.session_id) {
            if s.waiter.as_ref().map(|w| w.id) == Some(self.waiter_id) {
                s.waiter = None;
            }
        }
    }
}

async fn studio_poll(hub: Arc<Hub>, req: Request<Incoming>) -> Result<Resp, HttpError> {
    let b = read_body(req).await?;
    let sid = b["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| HttpError::new(400, "sessionId required"))?;
    let place_id = b.get("placeId").and_then(value_as_id).unwrap_or(0);
    let game_id = b.get("gameId").and_then(value_as_id).unwrap_or(0);
    let place_name = b["placeName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string();
    let user_id = b.get("userId").and_then(value_as_id).unwrap_or(0);
    let plugin_version = b["pluginVersion"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string();
    let immediate = b["immediate"].as_bool().unwrap_or(false);

    let (mut rx, mut guard) = {
        let mut inner = hub.lock();
        let now = now_ms();
        let prev_place = inner.sessions.get(&sid).map(|s| s.place_id);
        if prev_place.is_none() {
            inner.sessions.insert(
                sid.clone(),
                Session {
                    id: sid.clone(),
                    place_id,
                    game_id,
                    place_name: place_name.clone(),
                    user_id,
                    plugin_version: plugin_version.clone(),
                    last_seen: now,
                    connected_at: now,
                    queue: Vec::new(),
                    waiter: None,
                },
            );
            log(&format!(
                "Studio connected: {place_name} (PlaceId {place_id})"
            ));
        }
        {
            let s = inner.sessions.get_mut(&sid).expect("session just inserted");
            s.place_id = place_id;
            s.game_id = game_id;
            s.place_name = place_name.clone();
            s.user_id = user_id;
            s.plugin_version = plugin_version;
            s.last_seen = now;
        }
        // Studio knows the place's name even when Roblox's API won't say (unpublished, private).
        if prev_place != Some(place_id) && place_name != "?" {
            let mut changed = false;
            if let Some(p) = inner
                .state
                .projects
                .iter_mut()
                .find(|p| p.place_ids.first() == Some(&place_id))
            {
                if p.place_name.as_deref() != Some(place_name.as_str()) {
                    p.place_name = Some(place_name.clone());
                    changed = true;
                }
            }
            if changed {
                inner.save();
            }
        }
        if let Some(prev) = prev_place {
            if prev != place_id {
                // Place identity changed under us (e.g. just published). Fail anything in flight.
                let queued: Vec<String> = inner
                    .sessions
                    .get_mut(&sid)
                    .map(|s| {
                        s.queue
                            .drain(..)
                            .filter_map(|c| c["id"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for cid in queued {
                    inner.cancel(&cid, "Studio place changed; command cancelled.");
                }
                inner.notify_all();
            }
        }
        inner.flush(&sid); // a previous poll still waiting gets answered now
        let has_commands = inner
            .sessions
            .get(&sid)
            .map(|s| !s.queue.is_empty())
            .unwrap_or(false);
        if has_commands || immediate {
            let binding = inner
                .sessions
                .get(&sid)
                .map(|s| inner.binding_for(s))
                .unwrap_or(Value::Null);
            let commands: Vec<Value> = inner
                .sessions
                .get_mut(&sid)
                .map(|s| std::mem::take(&mut s.queue))
                .unwrap_or_default();
            return Ok(send(
                200,
                &json!({ "binding": binding, "commands": commands, "hubVersion": VERSION }),
            ));
        }
        inner.next_waiter += 1;
        let waiter_id = inner.next_waiter;
        let (tx, rx) = oneshot::channel();
        if let Some(s) = inner.sessions.get_mut(&sid) {
            s.waiter = Some(Waiter { id: waiter_id, tx });
        }
        (
            rx,
            WaiterGuard {
                hub: hub.clone(),
                session_id: sid.clone(),
                waiter_id,
                armed: true,
            },
        )
    };

    let answer = tokio::select! {
        r = &mut rx => r.unwrap_or_else(|_| json!({ "binding": Value::Null, "commands": [], "hubVersion": VERSION })),
        _ = tokio::time::sleep(Duration::from_millis(POLL_HOLD_MS)) => {
            let flushed = {
                let mut inner = hub.lock();
                let mine = inner.sessions.get(&sid).and_then(|s| s.waiter.as_ref()).map(|w| w.id) == Some(guard.waiter_id);
                if mine {
                    inner.flush(&sid);
                }
                mine
            };
            let _ = flushed;
            rx.await.unwrap_or_else(|_| json!({ "binding": Value::Null, "commands": [], "hubVersion": VERSION }))
        }
    };
    guard.armed = false;
    Ok(send(200, &answer))
}

// ---------- agent -> studio command routing ----------

async fn exec(
    hub: Arc<Hub>,
    project: &Project,
    session_id: Option<String>,
    op: &str,
    args: Value,
    timeout_ms: u64,
) -> Result<Value, HttpError> {
    let (rx, cmd_id, target_sid, target_place) = {
        let mut inner = hub.lock();
        let bmap = inner.bindings();
        let mut candidates: Vec<(String, u64, String, u64)> = inner
            .sessions
            .values()
            .filter(|s| {
                bmap.get(&s.id).and_then(|b| b["project"]["id"].as_str())
                    == Some(project.id.as_str())
            })
            .map(|s| {
                (
                    s.id.clone(),
                    s.place_id,
                    s.place_name.clone(),
                    s.connected_at,
                )
            })
            .collect();
        candidates.sort_by_key(|c| c.3);
        let target = if let Some(sid) = session_id {
            let s = inner
                .sessions
                .get(&sid)
                .ok_or_else(|| HttpError::new(404, format!("No Studio session {sid}.")))?;
            if !candidates.iter().any(|c| c.0 == sid) {
                return Err(HttpError::new(
                    409,
                    format!(
                        "Studio session {sid} (PlaceId {}) is not bound to \"{}\". Refusing.",
                        s.place_id, project.name
                    ),
                ));
            }
            (s.id.clone(), s.place_id)
        } else {
            if candidates.is_empty() {
                let places = project
                    .place_ids
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join("/");
                return Err(HttpError::new(409, format!("No Roblox Studio window with {}'s place (PlaceId {places}) is open with the RojoHub plugin.", project.name)));
            }
            if candidates.len() > 1 {
                let mut e = HttpError::new(
                    409,
                    format!(
                        "Several Studio windows have \"{}\" open. Pass a session id.",
                        project.name
                    ),
                );
                e.extra.insert(
                    "sessions".into(),
                    Value::Array(
                        candidates
                            .iter()
                            .map(|c| json!({ "id": c.0, "placeName": c.2, "connectedAt": c.3 }))
                            .collect(),
                    ),
                );
                return Err(e);
            }
            (candidates[0].0.clone(), candidates[0].1)
        };
        let id = random_id();
        // The plugin re-checks these against game.PlaceId/GameId before running anything.
        let cmd = json!({
            "id": id, "op": op, "args": args,
            "expect": { "placeId": target.1, "gameId": project.game_id, "projectId": project.id },
        });
        let (tx, rx) = oneshot::channel();
        inner.pending.insert(
            id.clone(),
            Pending {
                session_id: target.0.clone(),
                tx,
            },
        );
        if let Some(s) = inner.sessions.get_mut(&target.0) {
            s.queue.push(cmd);
        }
        inner.flush(&target.0);
        (rx, id, target.0, target.1)
    };
    let answered = tokio::select! {
        r = rx => r.ok(),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => None,
    };
    match answered {
        Some(mut r) => {
            if let Value::Object(m) = &mut r {
                m.insert("session".into(), Value::String(target_sid));
                m.insert("placeId".into(), json!(target_place));
            }
            Ok(r)
        }
        None => {
            hub.lock().pending.remove(&cmd_id);
            Ok(
                json!({ "ok": false, "error": format!("Studio did not answer within {}s (is it busy or paused on a dialog?).", (timeout_ms as f64 / 1000.0).round() as u64) }),
            )
        }
    }
}

// ---------- projects ----------

fn resolve_path(p: &str) -> PathBuf {
    std::path::absolute(Path::new(p)).unwrap_or_else(|_| PathBuf::from(p))
}

async fn add_project(hub: Arc<Hub>, body: Value) -> Result<Value, HttpError> {
    let dir = resolve_path(body["dir"].as_str().unwrap_or(""));
    if !dir.is_dir() {
        return Err(HttpError::new(
            400,
            format!("Directory not found: {}", dir.display()),
        ));
    }
    let pf = body["projectFile"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("default.project.json");
    let project_file = resolve_path(&dir.join(pf).to_string_lossy());
    if !project_file.is_file() {
        return Err(HttpError::new(
            400,
            format!("Project file not found: {}", project_file.display()),
        ));
    }
    let mut place_ids: Vec<u64> = Vec::new();
    for v in body["placeIds"].as_array().cloned().unwrap_or_default() {
        if let Some(n) = value_as_id(&v).filter(|n| *n > 0 && *n <= (1u64 << 53)) {
            if !place_ids.contains(&n) {
                place_ids.push(n);
            }
        }
    }
    if place_ids.is_empty() {
        return Err(HttpError::new(400, "At least one PlaceId is required."));
    }
    let game_id = body.get("gameId").and_then(value_as_id).filter(|g| *g > 0);
    let json = read_project_file(&project_file.to_string_lossy())
        .map_err(|e| HttpError::new(400, format!("Invalid project file: {e}")))?;
    let dir_s = dir.to_string_lossy().into_owned();
    let file_s = project_file.to_string_lossy().into_owned();
    let name_arg = body["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let auto_start = body["autoStart"].as_bool().unwrap_or(false);

    let existing_id = {
        let inner = hub.lock();
        inner
            .state
            .projects
            .iter()
            .find(|p| p.dir == dir_s && p.project_file == file_s)
            .map(|p| p.id.clone())
    };
    let warnings: Vec<String> = {
        let inner = hub.lock();
        inner
            .state
            .projects
            .iter()
            .filter(|p| Some(&p.id) != existing_id.as_ref())
            .filter_map(|other| {
                let shared: Vec<String> = other
                    .place_ids
                    .iter()
                    .filter(|id| place_ids.contains(id))
                    .map(u64::to_string)
                    .collect();
                (!shared.is_empty()).then(|| {
                    format!(
                        "PlaceId {} is also bound to \"{}\". Only one of them can run at a time.",
                        shared.join(", "),
                        other.name
                    )
                })
            })
            .collect()
    };

    if let Some(id) = existing_id {
        {
            let mut inner = hub.lock();
            if let Some(p) = inner.state.projects.iter_mut().find(|p| p.id == id) {
                let moved = p.place_ids.first() != place_ids.first() || p.game_id != game_id;
                p.place_ids = place_ids.clone();
                p.game_id = game_id;
                if let Some(n) = &name_arg {
                    p.name = n.clone();
                }
                p.auto_start = auto_start || p.auto_start;
                if moved {
                    p.place_name = None;
                    p.game_name = None;
                }
            }
            inner.save();
            inner.notify_all();
        }
        tokio::spawn(resolve_names(hub.clone(), id.clone()));
        tokio::spawn(load_icons(hub.clone()));
        return Ok(
            json!({ "project": hub.lock().project_view_by_id(&id), "warnings": warnings, "updated": true }),
        );
    }

    let base_name = name_arg
        .clone()
        .or_else(|| json.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| {
            dir.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".into())
        });
    let projects = hub.lock().state.projects.clone();
    let port = allocate_port(&projects, None)
        .await
        .map_err(|e| HttpError::new(409, e))?;
    let id = {
        let mut inner = hub.lock();
        let base = slugify(&base_name);
        let mut id = base.clone();
        let mut n = 2;
        while inner.state.projects.iter().any(|p| p.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let project = Project {
            id: id.clone(),
            name: base_name.clone(),
            dir: dir_s,
            project_file: file_s,
            place_ids: place_ids.clone(),
            game_id,
            auto_start,
            port,
            added_at: Some(iso_now()),
            place_name: None,
            game_name: None,
            rojo_bin: None,
            extra: Map::new(),
        };
        inner.state.projects.push(project);
        inner.save();
        inner.notify_all();
        log(&format!(
            "Added {base_name} -> port {port}, places {}",
            place_ids
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
        id
    };
    tokio::spawn(resolve_names(hub.clone(), id.clone()));
    tokio::spawn(load_icons(hub.clone()));
    Ok(json!({ "project": hub.lock().project_view_by_id(&id), "warnings": warnings }))
}

// ---------- display names ----------
// PlaceIds are what the hub checks; names are only for people. Fetched once from Roblox's
// public APIs and cached in state.json. Private games report a placeholder title, which is
// skipped. ROJOHUB_OFFLINE=1 turns the lookups off.

fn roblox_json(url: &str) -> Option<Value> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut res = agent.get(url).call().ok()?;
    if !res.status().is_success() {
        return None;
    }
    let text = res.body_mut().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

async fn resolve_names(hub: Arc<Hub>, id: String) {
    if offline() {
        return;
    }
    let (place_id, game_id, need_place, need_game) = {
        let inner = hub.lock();
        let Some(p) = inner.find_project(&id) else {
            return;
        };
        if p.place_name.is_some() && (p.game_name.is_some() || p.game_id.is_none()) {
            return;
        }
        (
            p.place_ids.first().copied().unwrap_or(0),
            p.game_id,
            p.place_name.is_none(),
            p.game_id.is_some() && p.game_name.is_none(),
        )
    };
    let looked_up = tokio::task::spawn_blocking(move || {
        let place_name = if need_place {
            roblox_json(&format!(
                "https://economy.roblox.com/v2/assets/{place_id}/details"
            ))
            .and_then(|v| v.get("Name").and_then(Value::as_str).map(str::to_string))
        } else {
            None
        };
        let game_name = match (need_game, game_id) {
            (true, Some(g)) => roblox_json(&format!(
                "https://games.roblox.com/v1/games?universeIds={g}"
            ))
            .and_then(|v| v["data"][0]["name"].as_str().map(str::to_string))
            .filter(|n| !n.starts_with('[')),
            _ => None,
        };
        (place_name, game_name)
    })
    .await
    .unwrap_or((None, None));
    let mut inner = hub.lock();
    let mut changed = false;
    if let Some(p) = inner.state.projects.iter_mut().find(|p| p.id == id) {
        if let Some(n) = looked_up.0 {
            p.place_name = Some(n);
            changed = true;
        }
        if let Some(n) = looked_up.1 {
            p.game_name = Some(n);
            changed = true;
        }
    }
    if changed {
        inner.save();
        inner.notify_all();
    }
}

/// Game icons: Roblox's CDN URLs expire, so they live in memory and are fetched once per hub
/// start, for every project at once.
async fn load_icons(hub: Arc<Hub>) {
    if offline() {
        return;
    }
    let ids: Vec<u64> = {
        let inner = hub.lock();
        let mut ids: Vec<u64> = Vec::new();
        for p in &inner.state.projects {
            if let Some(id) = p.place_ids.first() {
                if !inner.icons.contains_key(id) && !ids.contains(id) {
                    ids.push(*id);
                }
            }
        }
        ids
    };
    if ids.is_empty() {
        return;
    }
    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let url = format!("https://thumbnails.roblox.com/v1/places/gameicons?placeIds={list}&size=150x150&format=Png&isCircular=false");
    let Some(r) = tokio::task::spawn_blocking(move || roblox_json(&url))
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let mut inner = hub.lock();
    for t in r["data"].as_array().cloned().unwrap_or_default() {
        if t["state"] == "Completed" {
            if let (Some(id), Some(u)) = (
                t.get("targetId").and_then(value_as_id),
                t["imageUrl"].as_str(),
            ) {
                inner.icons.insert(id, u.to_string());
            }
        }
    }
    inner.notify_all();
}
