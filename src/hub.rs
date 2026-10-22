//! The hub's in-memory model: registered projects, their Rojo runtimes, the Studio windows
//! talking to the RojoHub plugin, and the rules that bind windows to projects.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{hub_port, port_range, resolve_engine, VERSION};
use crate::proxy::Proxy;
use crate::state::{save_state, Project, State};
use crate::util::{clock_now, log, now_ms};

pub const LOG_LINES: usize = 400;
pub const LIVE: [&str; 3] = ["starting", "running", "unhealthy"];

/// A running Rojo child: how to stop it and how to know it is gone.
pub struct ProcHandle {
    pub kill: Option<oneshot::Sender<()>>,
    pub exited: watch::Receiver<bool>,
}

pub struct Runtime {
    pub status: String,
    pub logs: VecDeque<String>,
    pub error: Option<String>,
    pub pid: Option<u32>,
    pub verified: Option<Value>,
    pub failures: u32,
    pub started_at: Option<u64>,
    pub engine: Option<String>,
    pub memory: Option<u64>,
    pub stopping: bool,
    /// Bumped on every start, so a stale supervisor task never touches a newer run.
    pub generation: u64,
    pub proc: Option<ProcHandle>,
    pub proxy: Option<Proxy>,
}

impl Default for Runtime {
    fn default() -> Runtime {
        Runtime {
            status: "stopped".to_string(),
            logs: VecDeque::new(),
            error: None,
            pid: None,
            verified: None,
            failures: 0,
            started_at: None,
            engine: None,
            memory: None,
            stopping: false,
            generation: 0,
            proc: None,
            proxy: None,
        }
    }
}

impl Runtime {
    pub fn new() -> Runtime {
        Runtime::default()
    }

    pub fn is_live(&self) -> bool {
        LIVE.contains(&self.status.as_str())
    }

    pub fn push_log(&mut self, line: &str) {
        self.logs.push_back(format!("{} {}", clock_now(), line));
        while self.logs.len() > LOG_LINES {
            self.logs.pop_front();
        }
    }

    pub fn info(&self) -> Value {
        json!({
            "status": self.status,
            "error": self.error,
            "pid": self.pid,
            "verified": self.verified,
            "startedAt": self.started_at,
            "engine": self.engine,
            "studioLink": self.proxy.as_ref().map(|p| p.tracker.lock().map(|t| t.studio_link()).unwrap_or(Value::Null)),
            "memory": self.memory,
        })
    }
}

/// One Studio window running the RojoHub plugin.
pub struct Session {
    pub id: String,
    pub place_id: u64,
    pub game_id: u64,
    pub place_name: String,
    pub user_id: u64,
    pub plugin_version: String,
    pub last_seen: u64,
    pub connected_at: u64,
    pub queue: Vec<Value>,
    pub waiter: Option<Waiter>,
}

pub struct Waiter {
    pub id: u64,
    pub tx: oneshot::Sender<Value>,
}

pub struct Pending {
    pub session_id: String,
    pub tx: oneshot::Sender<Value>,
}

pub struct Inner {
    pub state: State,
    pub runtimes: HashMap<String, Runtime>,
    pub sessions: HashMap<String, Session>,
    pub pending: HashMap<String, Pending>,
    pub icons: HashMap<u64, String>,
    pub next_waiter: u64,
}

pub struct Hub {
    pub token: String,
    /// Pinged by proxies and supervisors when something a Studio window shows has changed.
    pub changes: mpsc::UnboundedSender<()>,
    pub inner: Mutex<Inner>,
}

impl Hub {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    pub fn new(state: State) -> Inner {
        Inner {
            state,
            runtimes: HashMap::new(),
            sessions: HashMap::new(),
            pending: HashMap::new(),
            icons: HashMap::new(),
            next_waiter: 0,
        }
    }

    pub fn runtime(&mut self, id: &str) -> &mut Runtime {
        self.runtimes.entry(id.to_string()).or_default()
    }

    pub fn runtime_status(&self, id: &str) -> &str {
        self.runtimes
            .get(id)
            .map(|r| r.status.as_str())
            .unwrap_or("stopped")
    }

    pub fn runtime_info(&self, id: &str) -> Value {
        self.runtimes
            .get(id)
            .map(Runtime::info)
            .unwrap_or_else(|| Runtime::new().info())
    }

    pub fn save(&self) {
        if let Err(e) = save_state(&self.state) {
            log(&format!("Could not save state: {e}"));
        }
    }

    pub fn find_project(&self, reference: &str) -> Option<&Project> {
        self.state
            .projects
            .iter()
            .find(|p| p.id == reference || p.name == reference)
    }

    pub fn set_status(&mut self, id: &str, status: &str, error: Option<String>) {
        let r = self.runtime(id);
        r.status = status.to_string();
        r.error = error;
        self.notify_all();
    }

    // ---------- binding: which project does a Studio window belong to? ----------

    pub fn binding_for(&self, s: &Session) -> Value {
        binding_for(
            &self.state.projects,
            &self.runtimes,
            self.sessions.values(),
            s,
        )
    }

    /// Every session's binding, computed once per view instead of once per project x session.
    pub fn bindings(&self) -> HashMap<String, Value> {
        self.sessions
            .values()
            .map(|s| (s.id.clone(), self.binding_for(s)))
            .collect()
    }

    pub fn session_view(&self, s: &Session, bmap: &HashMap<String, Value>) -> Value {
        json!({
            "id": s.id, "placeId": s.place_id, "gameId": s.game_id, "placeName": s.place_name, "userId": s.user_id,
            "pluginVersion": s.plugin_version, "lastSeen": s.last_seen, "connectedAt": s.connected_at,
            "binding": bmap.get(&s.id).cloned().unwrap_or(Value::Null),
        })
    }

    pub fn project_view(&self, p: &Project, bmap: &HashMap<String, Value>) -> Value {
        let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut v {
            m.insert(
                "iconUrl".into(),
                p.place_ids
                    .first()
                    .and_then(|id| self.icons.get(id))
                    .cloned()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            m.insert("rojo".into(), self.runtime_info(&p.id));
            let studios: Vec<Value> = self
                .sessions
                .values()
                .filter(|s| {
                    bmap.get(&s.id).and_then(|b| b["project"]["id"].as_str()) == Some(p.id.as_str())
                })
                .map(|s| json!({ "id": s.id, "placeId": s.place_id, "placeName": s.place_name }))
                .collect();
            m.insert("studios".into(), Value::Array(studios));
        }
        v
    }

    pub fn project_view_by_id(&self, id: &str) -> Value {
        let bmap = self.bindings();
        self.find_project(id)
            .map(|p| self.project_view(p, &bmap))
            .unwrap_or(Value::Null)
    }

    pub fn status_view(&self) -> Value {
        let bmap = self.bindings();
        let (lo, hi) = port_range();
        json!({
            "hub": {
                "version": VERSION, "port": hub_port(), "pid": std::process::id(), "portRange": [lo, hi],
                "memory": crate::rojo::own_memory(),
                "engine": resolve_engine(None, self.state.engine.as_deref()).label,
                "platform": platform_name(),
            },
            "projects": self.state.projects.iter().map(|p| self.project_view(p, &bmap)).collect::<Vec<_>>(),
            "studios": self.sessions.values().map(|s| self.session_view(s, &bmap)).collect::<Vec<_>>(),
        })
    }

    // ---------- long-poll plumbing ----------

    /// Answer a Studio window's pending long-poll with its binding and queued commands.
    pub fn flush(&mut self, session_id: &str) {
        let binding = match self.sessions.get(session_id) {
            Some(s) if s.waiter.is_some() => self.binding_for(s),
            _ => return,
        };
        let Some(s) = self.sessions.get_mut(session_id) else {
            return;
        };
        let Some(w) = s.waiter.take() else { return };
        let commands: Vec<Value> = std::mem::take(&mut s.queue);
        let _ =
            w.tx.send(json!({ "binding": binding, "commands": commands, "hubVersion": VERSION }));
    }

    pub fn notify_all(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.flush(&id);
        }
    }

    pub fn cancel(&mut self, command_id: &str, error: &str) {
        if let Some(p) = self.pending.remove(command_id) {
            let _ = p.tx.send(json!({ "ok": false, "error": error }));
        }
    }

    /// Drop Studio windows that stopped polling and fail whatever they still owed.
    pub fn sweep_sessions(&mut self, ttl_ms: u64) {
        let now = now_ms();
        let gone: Vec<String> = self
            .sessions
            .values()
            .filter(|s| s.waiter.is_none() && now.saturating_sub(s.last_seen) > ttl_ms)
            .map(|s| s.id.clone())
            .collect();
        for id in gone {
            if let Some(s) = self.sessions.remove(&id) {
                log(&format!(
                    "Studio session closed: {} ({})",
                    s.place_name, s.place_id
                ));
            }
            let owed: Vec<String> = self
                .pending
                .iter()
                .filter(|(_, p)| p.session_id == id)
                .map(|(k, _)| k.clone())
                .collect();
            for cid in owed {
                self.cancel(&cid, "Studio window went away before answering.");
            }
        }
    }
}

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

/// Minimal view of a Studio window for the binding rules (and their tests).
pub struct Window {
    pub id: String,
    pub place_id: u64,
    pub game_id: u64,
}

impl From<&Session> for Window {
    fn from(s: &Session) -> Window {
        Window {
            id: s.id.clone(),
            place_id: s.place_id,
            game_id: s.game_id,
        }
    }
}

/// The rules that decide which project a Studio window belongs to.
pub fn binding_for<'a>(
    projects: &[Project],
    runtimes: &HashMap<String, Runtime>,
    sessions: impl Iterator<Item = &'a Session>,
    s: &Session,
) -> Value {
    let others: Vec<Window> = sessions.map(Window::from).collect();
    bind(projects, runtimes, &others, &Window::from(s))
}

pub fn bind(
    projects: &[Project],
    runtimes: &HashMap<String, Runtime>,
    windows: &[Window],
    s: &Window,
) -> Value {
    let status_of = |id: &str| {
        runtimes
            .get(id)
            .map(|r| r.status.as_str())
            .unwrap_or("stopped")
    };
    let mut messages: Vec<String> = Vec::new();
    if s.place_id == 0 {
        return json!({ "status": "unpublished", "messages": ["This place is not published, so it has no PlaceId. Publish it (File > Publish to Roblox) before syncing."] });
    }
    let mut matches: Vec<&Project> = projects
        .iter()
        .filter(|p| p.place_ids.contains(&s.place_id))
        .collect();
    if matches.is_empty() {
        return json!({ "status": "unbound", "messages": [format!("No project is bound to PlaceId {}. In the project folder run: rojohub add . --place {}", s.place_id, s.place_id)] });
    }
    if matches.len() > 1 {
        let live: Vec<&Project> = matches
            .iter()
            .copied()
            .filter(|p| LIVE.contains(&status_of(&p.id)))
            .collect();
        if live.len() != 1 {
            let names: Vec<&str> = matches.iter().map(|p| p.name.as_str()).collect();
            return json!({
                "status": "conflict",
                "candidates": names,
                "messages": [format!("PlaceId {} is bound to {} projects ({}). Start exactly one of them.", s.place_id, matches.len(), names.join(", "))],
            });
        }
        messages.push(format!(
            "PlaceId is shared by {} projects; \"{}\" is the one running.",
            matches.len(),
            live[0].name
        ));
        matches = live;
    }
    let p = matches[0];
    if let Some(game_id) = p.game_id.filter(|g| *g != 0) {
        if s.game_id != game_id {
            return json!({
                "status": "mismatch",
                "messages": [format!("PlaceId matches \"{}\" but GameId is {}, expected {}. Refusing to bind.", p.name, s.game_id, game_id)],
            });
        }
    }
    let twins = windows
        .iter()
        .filter(|o| o.id != s.id && o.place_id == s.place_id)
        .count();
    if twins > 0 {
        messages.push(format!(
            "{} Studio windows have this place open. Agents must pick one by session id.",
            twins + 1
        ));
    }
    let rt = runtimes.get(&p.id);
    let synced = rt.and_then(|r| r.proxy.as_ref()).map(|px| {
        px.tracker
            .lock()
            .map(|t| t.synced_for(s.place_id))
            .unwrap_or(false)
    });
    let session = rt
        .and_then(|r| r.verified.as_ref())
        .map(|v| v["sessionId"].clone())
        .unwrap_or(Value::Null);
    json!({
        "status": "bound",
        "project": {
            "id": p.id, "name": p.name, "dir": p.dir, "port": p.port,
            "rojo": rt.map(|r| r.status.as_str()).unwrap_or("stopped"),
            "rojoError": rt.and_then(|r| r.error.clone()),
            "synced": synced,
            "session": session,
        },
        "duplicates": twins,
        "messages": messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn project(id: &str, places: &[u64], game: Option<u64>) -> Project {
        Project {
            id: id.into(),
            name: id.to_uppercase(),
            dir: format!("/p/{id}"),
            project_file: format!("/p/{id}/default.project.json"),
            place_ids: places.to_vec(),
            game_id: game,
            auto_start: false,
            port: 34872,
            added_at: None,
            place_name: None,
            game_name: None,
            rojo_bin: None,
            extra: Map::new(),
        }
    }

    fn win(id: &str, place: u64, game: u64) -> Window {
        Window {
            id: id.into(),
            place_id: place,
            game_id: game,
        }
    }

    fn running(ids: &[&str]) -> HashMap<String, Runtime> {
        ids.iter()
            .map(|id| {
                let mut r = Runtime::new();
                r.status = "running".into();
                r.verified = Some(json!({ "sessionId": format!("sess-{id}") }));
                (id.to_string(), r)
            })
            .collect()
    }

    #[test]
    fn unpublished_and_unbound() {
        let projects = vec![project("a", &[1], None)];
        let b = bind(&projects, &HashMap::new(), &[], &win("w", 0, 0));
        assert_eq!(b["status"], "unpublished");
        let b = bind(&projects, &HashMap::new(), &[], &win("w", 2, 0));
        assert_eq!(b["status"], "unbound");
        assert!(b["messages"][0]
            .as_str()
            .unwrap()
            .contains("rojohub add . --place 2"));
    }

    #[test]
    fn conflict_until_exactly_one_runs() {
        let projects = vec![project("a", &[1], None), project("b", &[1, 2], None)];
        let b = bind(&projects, &HashMap::new(), &[], &win("w", 1, 0));
        assert_eq!(b["status"], "conflict");
        assert_eq!(b["candidates"], json!(["A", "B"]));
        let both = running(&["a", "b"]);
        assert_eq!(
            bind(&projects, &both, &[], &win("w", 1, 0))["status"],
            "conflict"
        );
        let one = running(&["b"]);
        let b = bind(&projects, &one, &[], &win("w", 1, 0));
        assert_eq!(b["status"], "bound");
        assert_eq!(b["project"]["id"], "b");
        assert_eq!(b["project"]["session"], "sess-b");
        assert!(b["messages"][0]
            .as_str()
            .unwrap()
            .contains("\"B\" is the one running"));
        // The place only "b" has is never a conflict.
        assert_eq!(
            bind(&projects, &HashMap::new(), &[], &win("w", 2, 0))["project"]["id"],
            "b"
        );
    }

    #[test]
    fn game_mismatch_blocks_and_zero_game_is_ignored() {
        let projects = vec![project("a", &[1], Some(77))];
        let b = bind(&projects, &HashMap::new(), &[], &win("w", 1, 78));
        assert_eq!(b["status"], "mismatch");
        assert!(b["messages"][0].as_str().unwrap().contains("expected 77"));
        assert_eq!(
            bind(&projects, &HashMap::new(), &[], &win("w", 1, 77))["status"],
            "bound"
        );
        let loose = vec![project("a", &[1], None)];
        assert_eq!(
            bind(&loose, &HashMap::new(), &[], &win("w", 1, 5))["status"],
            "bound"
        );
    }

    #[test]
    fn bound_reports_runtime_and_twins() {
        let projects = vec![project("a", &[1], None)];
        let mut rts = running(&["a"]);
        rts.get_mut("a").unwrap().error = None;
        let windows = vec![win("w1", 1, 0), win("w2", 1, 0), win("w3", 9, 0)];
        let b = bind(&projects, &rts, &windows, &win("w1", 1, 0));
        assert_eq!(b["status"], "bound");
        assert_eq!(b["project"]["rojo"], "running");
        assert_eq!(b["project"]["port"], 34872);
        assert_eq!(
            b["project"]["synced"],
            Value::Null,
            "no proxy yet means unknown"
        );
        assert_eq!(b["duplicates"], 1);
        assert!(b["messages"][0]
            .as_str()
            .unwrap()
            .starts_with("2 Studio windows"));
        let b = bind(&projects, &HashMap::new(), &[], &win("w1", 1, 0));
        assert_eq!(b["project"]["rojo"], "stopped");
        assert_eq!(b["duplicates"], 0);
    }

    #[test]
    fn runtime_logs_are_capped() {
        let mut r = Runtime::new();
        for i in 0..LOG_LINES + 5 {
            r.push_log(&format!("line {i}"));
        }
        assert_eq!(r.logs.len(), LOG_LINES);
        assert!(r.logs.front().unwrap().ends_with("line 5"));
        assert_eq!(r.info()["studioLink"], Value::Null);
    }
}
