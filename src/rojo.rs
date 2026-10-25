//! Rojo process supervision: start a server on a private port behind the project's proxy,
//! verify it is the right project, keep it healthy, stop it, and reap leftovers.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::header::HOST;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

use crate::config::{
    find_tool, pids_file, port_range, resolve_engine, tool_path, user_home, HUB_HOST,
};
use crate::hub::{Hub, ProcHandle};
use crate::msgpack::{decode_rojo_body, RojoInfo};
use crate::ports::allocate_port;
use crate::project_file::check_project_file;
use crate::proxy::spawn_proxy;
use crate::state::Project;
use crate::util::{log, now_ms, strip_ansi};

static OWN_MEMORY: AtomicU64 = AtomicU64::new(0);

/// The hub's own resident memory, as last sampled by the health tick.
pub fn own_memory() -> Option<u64> {
    match OWN_MEMORY.load(Ordering::Relaxed) {
        0 => None,
        n => Some(n),
    }
}

fn ids_text(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn old_rojo_version(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts[0].parse::<u64>().is_ok_and(|major| major < 7)
}

fn record_crash(crashes: &mut VecDeque<Instant>, now: Instant) -> usize {
    while crashes
        .front()
        .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
    {
        crashes.pop_front();
    }
    crashes.push_back(now);
    crashes.len()
}

fn rokit_missing_message(dir: &str, bin: &Path) -> Option<String> {
    if bin
        != user_home()
            .join(".rokit")
            .join("bin")
            .join(if cfg!(windows) { "rojo.exe" } else { "rojo" })
    {
        return None;
    }
    for parent in Path::new(dir).ancestors() {
        let config = parent.join("rokit.toml");
        let Ok(raw) = fs::read_to_string(&config) else {
            continue;
        };
        let mut in_tools = false;
        for line in raw.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_tools = line == "[tools]";
                continue;
            }
            if !in_tools {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim().trim_matches('"') != "rojo" {
                continue;
            }
            let value = value.trim().trim_start_matches(['"', '\'']);
            let version = value
                .split(['"', '\'', ' ', '#'])
                .next()?
                .rsplit_once('@')?
                .1;
            if version.is_empty() {
                return None;
            }
            return Some(format!(
                "Rojo {version} pinned in {} is not installed. Run `rokit install` there.",
                config.display()
            ));
        }
        return None;
    }
    None
}

// ---------- pids.json: Rojo servers a crashed hub might leave behind ----------

fn read_pids() -> Map<String, Value> {
    fs::read_to_string(pids_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_pids(pids: &Map<String, Value>) {
    let _ = fs::write(pids_file(), Value::Object(pids.clone()).to_string());
}

fn remember_pid(id: &str, pid: u32) {
    let mut pids = read_pids();
    pids.insert(id.to_string(), json!(pid));
    write_pids(&pids);
}

fn forget_pid(id: &str) {
    let mut pids = read_pids();
    pids.remove(id);
    write_pids(&pids);
}

/// Kill Rojo servers left behind by a previous hub that crashed.
pub fn reap_orphans() {
    for (id, pid) in read_pids() {
        let Some(pid) = pid.as_u64() else { continue };
        let pid_s = pid.to_string();
        let listed = if cfg!(windows) {
            // tasklist only reports the image name; ps gives the whole command line.
            std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid_s}"), "/FO", "CSV", "/NH"])
                .output()
        } else {
            std::process::Command::new("ps")
                .args(["-p", &pid_s, "-o", "command="])
                .output()
        };
        let Ok(out) = listed else { continue };
        let cmd = String::from_utf8_lossy(&out.stdout).to_lowercase();
        let is_rojo = cmd.contains("rojo") && (cfg!(windows) || cmd.contains("serve"));
        if !is_rojo {
            continue;
        }
        let killed = if cfg!(windows) {
            std::process::Command::new("taskkill")
                .args(["/PID", &pid_s, "/F"])
                .output()
                .map(|o| o.status.success())
        } else {
            std::process::Command::new("kill")
                .args(["-TERM", &pid_s])
                .output()
                .map(|o| o.status.success())
        };
        if killed.unwrap_or(false) {
            log(&format!("Stopped leftover Rojo for {id} (pid {pid})"));
        }
    }
    write_pids(&Map::new());
}

/// Resident memory of the given pids, in bytes (missing = not running / unknown).
pub async fn sample_memory(pids: &[u32]) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    if pids.is_empty() {
        return out;
    }
    if cfg!(windows) {
        // tasklist takes one PID filter per call. CSV row: "rojo.exe","1234","Console","1","12,345 K"
        for pid in pids {
            let mut cmd = Command::new("tasklist");
            cmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
            hide_window(&mut cmd);
            if let Ok(o) = cmd.output().await {
                let text = String::from_utf8_lossy(&o.stdout);
                if let Some(kb) = parse_tasklist_kb(&text) {
                    out.insert(*pid, kb * 1024);
                }
            }
        }
    } else {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if let Ok(o) = Command::new("ps")
            .args(["-o", "pid=,rss=", "-p", &list])
            .output()
            .await
        {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let mut it = line.split_whitespace();
                if let (Some(Ok(pid)), Some(Ok(kb))) = (
                    it.next().map(str::parse::<u32>),
                    it.next().map(str::parse::<u64>),
                ) {
                    out.insert(pid, kb * 1024);
                }
            }
        }
    }
    out
}

/// The memory column of a tasklist CSV row, in KB: the last quoted field ends in " K".
pub fn parse_tasklist_kb(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.trim_end().ends_with("K\""))?;
    let field = line.trim_end().trim_end_matches('"').rsplit('"').next()?;
    let digits: String = field.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// One GET against a Rojo server on loopback: status, content type and body.
pub async fn http_get(port: u16, path: &str, timeout: Duration) -> Option<(u16, String, Vec<u8>)> {
    let fut = async {
        let stream = TcpStream::connect((HUB_HOST, port)).await.ok()?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .uri(path)
            .header(HOST, format!("{HUB_HOST}:{port}"))
            .body(Empty::<Bytes>::new())
            .ok()?;
        let resp = sender.send_request(req).await.ok()?;
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.into_body().collect().await.ok()?.to_bytes();
        Some((status, ct, body.to_vec()))
    };
    tokio::time::timeout(timeout, fut).await.ok().flatten()
}

/// `GET /api/rojo` decoded, or None when nothing Rojo-like answers.
pub async fn probe_rojo(port: u16, timeout: Duration) -> Option<Value> {
    let (status, ct, body) = http_get(port, "/api/rojo?client=rojohub", timeout).await?;
    if !(200..300).contains(&status) {
        return None;
    }
    decode_rojo_body(&ct, &body).ok()
}

/// Confirm the server answering is really *this* project, serving *these* places.
pub fn verify(
    project: &Project,
    expected_name: Option<&str>,
    info: &RojoInfo,
) -> Result<Value, String> {
    if let Some(expected) = expected_name {
        if info.project_name.as_deref() != Some(expected) {
            return Err(format!(
                "Port {} answered as project \"{}\", expected \"{expected}\".",
                project.port,
                info.project_name.clone().unwrap_or_default()
            ));
        }
    }
    let expected = info.expected_place_ids.clone();
    let missing: Vec<u64> = project
        .place_ids
        .iter()
        .copied()
        .filter(|id| !expected.as_deref().unwrap_or(&[]).contains(id))
        .collect();
    if expected.is_none() || !missing.is_empty() {
        return Err(format!(
            "Rojo is not restricting places (expectedPlaceIds={}). Refusing to run unguarded.",
            expected
                .map(|e| json!(e).to_string())
                .unwrap_or_else(|| "undefined".to_string())
        ));
    }
    Ok(json!({
        "projectName": info.project_name,
        "serverVersion": info.server_version,
        "expectedPlaceIds": expected,
        "sessionId": info.session_id,
    }))
}

fn signal_name(n: i32) -> String {
    match n {
        2 => "SIGINT".into(),
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

async fn terminate(child: &mut Child) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output()
                .await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    tokio::select! {
        s = child.wait() => s.ok(),
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    }
}

/// Owns a Rojo child for its whole life: collects its output, stops it on request and
/// records how it ended.
fn supervise(
    hub: Arc<Hub>,
    id: String,
    generation: u64,
    mut child: Child,
    kill_rx: oneshot::Receiver<()>,
    exited_tx: watch::Sender<bool>,
    rokit_missing: Option<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(read_lines(
                hub.clone(),
                id.clone(),
                generation,
                out,
                rokit_missing.clone(),
            )));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(read_lines(
                hub.clone(),
                id.clone(),
                generation,
                err,
                rokit_missing.clone(),
            )));
        }
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = kill_rx => terminate(&mut child).await,
        };
        let mut missing_pin = false;
        for r in readers {
            missing_pin |= r.await.unwrap_or(false);
        }
        let code = status.and_then(|s| s.code());
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.and_then(|s| s.signal()).map(signal_name)
        };
        #[cfg(not(unix))]
        let signal: Option<String> = None;
        let mut restart = false;
        {
            let mut inner = hub.lock();
            let r = inner.runtime(&id);
            if r.generation == generation {
                r.push_log(&format!(
                    "rojo exited (code {}, signal {})",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                    signal.clone().unwrap_or_else(|| "null".into())
                ));
                forget_pid(&id);
                r.proc = None;
                r.pid = None;
                r.memory = None;
                if r.stopping {
                    r.proxy.take();
                    inner.set_status(&id, "stopped", None);
                } else if missing_pin {
                    r.proxy.take();
                    let msg = rokit_missing.unwrap();
                    r.push_log(&msg);
                    inner.set_status(&id, "error", Some(msg));
                } else if r.status != "error" {
                    let how = match (&signal, code) {
                        (Some(sig), _) => format!("signal {sig}"),
                        (None, Some(c)) => format!("code {c}"),
                        (None, None) => "code null".to_string(),
                    };
                    let count = record_crash(&mut r.crashes, Instant::now());
                    if count >= 3 {
                        r.proxy.take();
                        let msg = "Rojo kept crashing (3 times in 60s). Check the logs with `rojohub logs <project>`.";
                        r.push_log(msg);
                        inner.set_status(&id, "error", Some(msg.into()));
                    } else {
                        r.restart_count += 1;
                        r.push_log(&format!(
                            "Rojo exited unexpectedly ({how}); restarting ({}).",
                            r.restart_count
                        ));
                        inner.set_status(&id, "starting", None);
                        restart = true;
                    }
                } else {
                    r.proxy.take();
                }
            }
        }
        let _ = exited_tx.send(true);
        if restart {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = hub.start_project_inner(&id, Some(generation)).await;
            });
        }
    })
}

async fn read_lines<R: tokio::io::AsyncRead + Unpin>(
    hub: Arc<Hub>,
    id: String,
    generation: u64,
    reader: R,
    rokit_missing: Option<String>,
) -> bool {
    let mut lines = BufReader::new(reader).lines();
    let mut missing_pin = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let line = strip_ansi(&line);
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if rokit_missing.is_some() && line == "ERROR No such file or directory (os error 2)" {
            missing_pin = true;
            continue;
        }
        let mut inner = hub.lock();
        let r = inner.runtime(&id);
        if r.generation == generation {
            r.push_log(line);
        }
    }
    missing_pin
}

impl Hub {
    /// Move the project to a free port in the range and persist it; returns the old port.
    async fn reassign_port(&self, project: &mut Project) -> Result<u16, String> {
        let projects = self.lock().state.projects.clone();
        let new_port = allocate_port(&projects, Some(&project.id)).await?;
        let old = project.port;
        {
            let mut inner = self.lock();
            if let Some(p) = inner.state.projects.iter_mut().find(|p| p.id == project.id) {
                p.port = new_port;
            }
            inner.save();
        }
        project.port = new_port;
        Ok(old)
    }

    pub async fn start_project(self: &Arc<Self>, id: &str) -> Result<Value, String> {
        self.start_project_inner(id, None).await
    }

    async fn start_project_inner(
        self: &Arc<Self>,
        id: &str,
        restart: Option<u64>,
    ) -> Result<Value, String> {
        let mut project = {
            let mut inner = self.lock();
            if inner.shutting_down {
                return Err("Hub is shutting down.".into());
            }
            let p = inner
                .find_project(id)
                .cloned()
                .ok_or_else(|| format!("Unknown project \"{id}\"."))?;
            let r = inner.runtime(&p.id);
            if restart.is_some_and(|generation| r.generation != generation || r.stopping) {
                return Err("Restart canceled.".into());
            }
            if r.proc.is_some() && r.is_live() {
                return Ok(r.info());
            }
            p
        };
        let pid_key = project.id.clone();

        let check = check_project_file(&project.project_file, &project.place_ids);
        if !check.ok {
            let e = check.error.unwrap_or_else(|| "Invalid project file".into());
            let mut inner = self.lock();
            inner.runtime(&pid_key).proxy.take();
            inner.set_status(&pid_key, "error", Some(e.clone()));
            return Err(e);
        }
        {
            let mut inner = self.lock();
            if !check.extra.is_empty() {
                inner.runtime(&pid_key).push_log(&format!(
                    "warning: servePlaceIds also allows {}, which RojoHub does not know about",
                    ids_text(&check.extra)
                ));
            }
            // Only one Rojo per place ID at a time, otherwise two servers fight over the same Studio.
            let others = inner.state.projects.clone();
            for other in others.iter().filter(|o| o.id != project.id) {
                if !crate::hub::LIVE.contains(&inner.runtime_status(&other.id)) {
                    continue;
                }
                let shared: Vec<u64> = other
                    .place_ids
                    .iter()
                    .copied()
                    .filter(|p| project.place_ids.contains(p))
                    .collect();
                if !shared.is_empty() {
                    let msg = format!(
                        "Refusing to start: \"{}\" is already serving place {}. Stop it first.",
                        other.name,
                        ids_text(&shared)
                    );
                    inner.runtime(&pid_key).proxy.take();
                    inner.set_status(&pid_key, "error", Some(msg.clone()));
                    return Err(msg);
                }
            }
        }

        let existing_private = self
            .lock()
            .runtimes
            .get(&pid_key)
            .and_then(|r| r.proxy.as_ref().map(|p| p.private_port));
        let (lo, hi) = port_range();
        if existing_private.is_none() && (project.port < lo || project.port > hi) {
            let old = self.reassign_port(&mut project).await?;
            self.lock().runtime(&pid_key).push_log(&format!(
                "Port {old} is outside the port range {lo}-{hi}; moved to {}.",
                project.port
            ));
        }
        let listener = if existing_private.is_some() {
            None
        } else {
            Some(match TcpListener::bind((HUB_HOST, project.port)).await {
                Ok(l) => l,
                Err(_) => {
                    let old = self.reassign_port(&mut project).await?;
                    self.lock().runtime(&pid_key).push_log(&format!(
                    "Port {old} was in use by something else; moved to {}. Update the Rojo plugin port in Studio.",
                    project.port
                ));
                    log(&format!(
                        "{}: port {old} busy, reassigned to {}",
                        project.name, project.port
                    ));
                    TcpListener::bind((HUB_HOST, project.port))
                        .await
                        .map_err(|e| format!("Could not listen on port {}: {e}", project.port))?
                }
            })
        };
        let private_port = if let Some(port) = existing_private {
            port
        } else {
            let l = TcpListener::bind((HUB_HOST, 0))
                .await
                .map_err(|e| format!("Could not pick a private port: {e}"))?;
            l.local_addr().map_err(|e| e.to_string())?.port()
        };

        let engine = {
            let inner = self.lock();
            resolve_engine(project.rojo_bin.as_deref(), inner.state.engine.as_deref())
        };
        let bin_path = find_tool(&engine.bin);
        let rokit_missing = rokit_missing_message(&project.dir, &bin_path);
        let private_s = private_port.to_string();
        let args = [
            "serve",
            project.project_file.as_str(),
            "--address",
            HUB_HOST,
            "--port",
            private_s.as_str(),
        ];

        let generation = {
            let mut inner = self.lock();
            if inner.shutting_down {
                return Err("Hub is shutting down.".into());
            }
            let changes = self.changes.clone();
            let r = inner.runtime(&pid_key);
            if restart.is_some_and(|generation| r.generation != generation || r.stopping) {
                return Err("Restart canceled.".into());
            }
            if r.proc.is_some() && r.is_live() {
                return Ok(r.info());
            }
            r.engine = Some(engine.label.clone());
            r.push_log(&format!("$ {} {}", engine.bin, args.join(" ")));
            r.status = "starting".into();
            r.error = None;
            r.warning = None;
            r.verified = None;
            r.failures = 0;
            r.stopping = false;
            r.memory = None;
            r.started_at = Some(now_ms());
            if restart.is_none() {
                r.restart_count = 0;
                r.crashes.clear();
            }
            r.generation += 1;
            let generation = r.generation;

            let mut cmd = Command::new(&bin_path);
            cmd.args(args)
                .current_dir(&project.dir)
                .env("PATH", tool_path())
                .env("NO_COLOR", "1")
                .env("CLICOLOR", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            hide_window(&mut cmd);
            match cmd.spawn() {
                Err(e) => {
                    let msg = if e.kind() == std::io::ErrorKind::NotFound {
                        format!("Could not find \"{}\". Install Rojo (e.g. via rokit), or point ROJOHUB_ROJO_BIN at it.", engine.bin)
                    } else {
                        e.to_string()
                    };
                    r.push_log(&msg);
                    r.proc = None;
                    r.proxy.take();
                    inner.set_status(&pid_key, "error", Some(msg.clone()));
                    return Err(msg);
                }
                Ok(child) => {
                    r.pid = child.id();
                    if let Some(pid) = child.id() {
                        remember_pid(&pid_key, pid);
                    }
                    let (kill_tx, kill_rx) = oneshot::channel();
                    let (exited_tx, exited_rx) = watch::channel(false);
                    r.proc = Some(ProcHandle {
                        kill: Some(kill_tx),
                        exited: exited_rx,
                    });
                    if let Some(listener) = listener {
                        r.proxy = Some(spawn_proxy(listener, private_port, changes));
                    }
                    tokio::spawn(supervise(
                        self.clone(),
                        pid_key.clone(),
                        generation,
                        child,
                        kill_rx,
                        exited_tx,
                        rokit_missing,
                    ));
                    inner.notify_all();
                    generation
                }
            }
        };

        // Confirm the server on this port is really *this* project, serving *these* places.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            {
                let inner = self.lock();
                let ours = inner
                    .runtimes
                    .get(&pid_key)
                    .map(|r| r.generation == generation && r.proc.is_some())
                    .unwrap_or(false);
                if !ours {
                    break;
                }
            }
            if let Some(raw) = probe_rojo(private_port, Duration::from_millis(1500)).await {
                let info = RojoInfo::from_value(&raw);
                if old_rojo_version(&info.server_version) {
                    let warning = format!("Rojo {} is too old for the bundled sync plugin. Run `rokit add rojo-rbx/rojo@7.7.1` and then `rokit install` in {}.", info.server_version, project.dir);
                    let mut inner = self.lock();
                    let r = inner.runtime(&pid_key);
                    if r.generation == generation {
                        r.push_log(&format!("warning: {warning}"));
                        r.warning = Some(warning);
                    }
                }
                match verify(&project, check.name.as_deref(), &info) {
                    Err(e) => {
                        self.lock().runtime(&pid_key).push_log(&e);
                        self.stop_project(&pid_key).await;
                        self.lock().set_status(&pid_key, "error", Some(e.clone()));
                        return Err(e);
                    }
                    Ok(details) => {
                        let mut inner = self.lock();
                        let r = inner.runtime(&pid_key);
                        if r.generation == generation {
                            r.verified = Some(details);
                            inner.set_status(&pid_key, "running", None);
                            log(&format!(
                                "{}: Rojo {} on port {}",
                                project.name, info.server_version, project.port
                            ));
                        }
                        return Ok(inner.runtime_info(&pid_key));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let still_ours = {
            let inner = self.lock();
            inner
                .runtimes
                .get(&pid_key)
                .map(|r| r.generation == generation && r.proc.is_some())
                .unwrap_or(false)
        };
        if still_ours {
            self.stop_project(&pid_key).await;
            self.lock().set_status(
                &pid_key,
                "error",
                Some("Rojo did not answer on its port within 30s. See logs.".into()),
            );
        }
        Err(self
            .lock()
            .runtimes
            .get(&pid_key)
            .and_then(|r| r.error.clone())
            .unwrap_or_else(|| "Rojo failed to start".into()))
    }

    pub async fn stop_project(&self, id: &str) -> Value {
        let exited = {
            let mut inner = self.lock();
            let r = inner.runtime(id);
            match r.proc.as_mut() {
                None => {
                    r.stopping = true;
                    r.generation += 1;
                    r.proxy.take();
                    inner.set_status(id, "stopped", None);
                    return inner.runtime_info(id);
                }
                Some(p) => {
                    r.stopping = true;
                    if let Some(k) = p.kill.take() {
                        let _ = k.send(());
                    }
                    p.exited.clone()
                }
            }
        };
        let mut exited = exited;
        let _ = tokio::time::timeout(Duration::from_secs(10), exited.wait_for(|v| *v)).await;
        self.lock().runtime_info(id)
    }

    pub async fn stop_all(self: &Arc<Self>) {
        let ids: Vec<String> = {
            let mut inner = self.lock();
            inner.shutting_down = true;
            inner.runtimes.keys().cloned().collect()
        };
        let mut set = tokio::task::JoinSet::new();
        for id in ids {
            let hub = self.clone();
            set.spawn(async move {
                hub.stop_project(&id).await;
            });
        }
        while set.join_next().await.is_some() {}
    }

    /// Periodic health check: the process must still answer as the same Rojo session.
    pub async fn health_tick(self: &Arc<Self>) {
        let live: Vec<(String, u32)> = {
            let inner = self.lock();
            inner
                .runtimes
                .iter()
                .filter(|(_, r)| r.proc.is_some())
                .filter_map(|(id, r)| r.pid.map(|p| (id.clone(), p)))
                .collect()
        };
        let own = std::process::id();
        let mut pids: Vec<u32> = live.iter().map(|(_, p)| *p).collect();
        pids.push(own);
        let mem = sample_memory(&pids).await;
        {
            let mut inner = self.lock();
            if let Some(m) = mem.get(&own) {
                OWN_MEMORY.store(*m, Ordering::Relaxed);
            }
            let mut changed = false;
            for (id, pid) in &live {
                let r = inner.runtime(id);
                let m = mem.get(pid).copied();
                if m != r.memory {
                    r.memory = m;
                    changed = true;
                }
            }
            if changed {
                inner.notify_all();
            }
        }
        let checks: Vec<(String, u64, u16, Value)> = {
            let inner = self.lock();
            inner
                .runtimes
                .iter()
                .filter(|(_, r)| {
                    r.proc.is_some() && (r.status == "running" || r.status == "unhealthy")
                })
                .filter_map(|(id, r)| {
                    let port = r.proxy.as_ref()?.private_port;
                    let sid = r.verified.as_ref()?.get("sessionId")?.clone();
                    Some((id.clone(), r.generation, port, sid))
                })
                .collect()
        };
        for (id, generation, port, sid) in checks {
            let info = probe_rojo(port, Duration::from_millis(1500))
                .await
                .map(|v| RojoInfo::from_value(&v));
            let mut inner = self.lock();
            let r = inner.runtime(&id);
            if r.generation != generation || r.proc.is_none() {
                continue;
            }
            match info {
                Some(info) if info.session_id == sid => {
                    r.failures = 0;
                    if r.status != "running" {
                        inner.set_status(&id, "running", None);
                    }
                }
                other => {
                    r.failures += 1;
                    if r.failures >= 3 && r.status != "unhealthy" {
                        let msg = if other.is_some() {
                            "A different Rojo session is answering on this port."
                        } else {
                            "Rojo stopped answering."
                        };
                        inner.set_status(&id, "unhealthy", Some(msg.into()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project {
            id: "p".into(),
            name: "P".into(),
            dir: "/p".into(),
            project_file: "/p/default.project.json".into(),
            place_ids: vec![123],
            game_id: None,
            auto_start: false,
            port: 34872,
            added_at: None,
            place_name: None,
            game_name: None,
            rojo_bin: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn verification_rules() {
        let info = RojoInfo::from_value(
            &json!({"sessionId": "s", "serverVersion": "7.7.1", "projectName": "ci", "expectedPlaceIds": [123, 9]}),
        );
        let ok = verify(&project(), Some("ci"), &info).unwrap();
        assert_eq!(ok["expectedPlaceIds"], json!([123, 9]));
        assert_eq!(ok["sessionId"], "s");
        let wrong = verify(&project(), Some("other"), &info).unwrap_err();
        assert!(
            wrong.contains("answered as project \"ci\", expected \"other\""),
            "{wrong}"
        );
        let open = RojoInfo::from_value(&json!({"projectName": "ci", "serverVersion": "7.7.1"}));
        let err = verify(&project(), Some("ci"), &open).unwrap_err();
        assert!(err.contains("expectedPlaceIds=undefined"), "{err}");
        let other_place = RojoInfo::from_value(
            &json!({"projectName": "ci", "serverVersion": "7.7.1", "expectedPlaceIds": [5]}),
        );
        let err = verify(&project(), None, &other_place).unwrap_err();
        assert!(err.contains("expectedPlaceIds=[5]"), "{err}");
    }

    #[test]
    fn crash_limit_resets_after_sixty_seconds() {
        let mut crashes = VecDeque::new();
        let start = Instant::now();
        assert_eq!(record_crash(&mut crashes, start), 1);
        assert_eq!(
            record_crash(&mut crashes, start + Duration::from_secs(1)),
            2
        );
        assert_eq!(
            record_crash(&mut crashes, start + Duration::from_secs(2)),
            3
        );
        assert_eq!(
            record_crash(&mut crashes, start + Duration::from_secs(62)),
            1
        );
    }

    #[test]
    fn compares_rojo_major_version() {
        for version in ["6.0.0", "6.2.1-rc.1", "0.7.7"] {
            assert!(old_rojo_version(version), "{version}");
        }
        for version in ["7.0.0", "7.7.0-rc.1", "7.7.1+build", "8.0", "?", "v6.0.0"] {
            assert!(!old_rojo_version(version), "{version}");
        }
    }

    #[test]
    fn tasklist_memory_column() {
        assert_eq!(
            parse_tasklist_kb("\"rojo.exe\",\"1234\",\"Console\",\"1\",\"12,345 K\"\r\n"),
            Some(12345)
        );
        assert_eq!(
            parse_tasklist_kb("INFO: No tasks are running which match the specified criteria.\r\n"),
            None
        );
        assert_eq!(
            parse_tasklist_kb("\"rojo.exe\",\"1\",\"Services\",\"0\",\"1\u{a0}024 K\"\n"),
            Some(1024)
        );
    }
}
