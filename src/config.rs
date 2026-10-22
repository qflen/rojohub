//! Settings: ports, the state directory, the API token and which Rojo to run.
//! Everything is read from the environment on demand, so the service files that bake in
//! `ROJOHUB_*` variables keep working.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const HUB_HOST: &str = "127.0.0.1";
pub const DEFAULT_HUB_PORT: u16 = 34870;
pub const DEFAULT_PORT_RANGE: (u16, u16) = (34872, 34881);

pub fn hub_port() -> u16 {
    env::var("ROJOHUB_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DEFAULT_HUB_PORT)
}

/// `ROJOHUB_PORT_RANGE=lo-hi`; anything else means the default range.
pub fn parse_range(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.trim().split_once('-')?;
    let (lo, hi): (u16, u16) = (a.parse().ok()?, b.parse().ok()?);
    (lo <= hi).then_some((lo, hi))
}

pub fn port_range() -> (u16, u16) {
    env::var("ROJOHUB_PORT_RANGE")
        .ok()
        .and_then(|s| parse_range(&s))
        .unwrap_or(DEFAULT_PORT_RANGE)
}

pub fn user_home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn home_dir() -> PathBuf {
    env::var_os("ROJOHUB_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".rojohub"))
}

pub fn state_file() -> PathBuf {
    home_dir().join("state.json")
}
pub fn pids_file() -> PathBuf {
    home_dir().join("pids.json")
}
pub fn token_file() -> PathBuf {
    home_dir().join("token")
}
pub fn hub_url() -> String {
    format!("http://{HUB_HOST}:{}", hub_port())
}

pub fn ensure_home() -> io::Result<()> {
    let dir = home_dir();
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// The bearer token shared by the CLI, the dashboard and agents. Created on first use.
pub fn get_token() -> io::Result<String> {
    ensure_home()?;
    let file = token_file();
    if let Ok(t) = fs::read_to_string(&file) {
        let t = t.trim();
        if t.len() >= 32 {
            return Ok(t.to_string());
        }
    }
    let bytes: [u8; 32] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&file, format!("{token}\n").as_bytes())?;
    Ok(token)
}

/// Write a file readable only by the user (mode 0600 where modes exist).
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)
    }
}

/// Login-item and GUI-launched processes get a minimal PATH; add the usual toolchain
/// managers and the system bin dirs for the current OS.
pub fn tool_path() -> std::ffi::OsString {
    let h = user_home();
    let mut dirs: Vec<PathBuf> = vec![
        h.join(".rokit").join("bin"),
        h.join(".aftman").join("bin"),
        h.join(".foreman").join("bin"),
        h.join(".cargo").join("bin"),
    ];
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    } else if cfg!(target_os = "linux") {
        dirs.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
        dirs.push(h.join(".local").join("bin"));
    }
    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }
    let mut seen = Vec::new();
    for d in dirs {
        if d.as_os_str().is_empty() || seen.contains(&d) {
            continue;
        }
        seen.push(d);
    }
    env::join_paths(seen).unwrap_or_default()
}

/// Find `name` in the directories of [`tool_path`], the way a shell would, so the hub
/// finds Rojo installed by rokit, aftman, foreman or cargo even when started at login.
pub fn find_tool(name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.components().count() > 1 {
        return p.to_path_buf();
    }
    let candidates: Vec<String> = if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for dir in env::split_paths(&tool_path()) {
        for c in &candidates {
            let full = dir.join(c);
            if full.is_file() {
                return full;
            }
        }
    }
    p.to_path_buf()
}

/// Is this engine setting usable: "rojo" from PATH, or a path to an existing binary?
pub fn engine_usable(engine: &str) -> bool {
    engine == "rojo" || find_tool(engine).is_file()
}

pub struct Engine {
    pub bin: String,
    pub label: String,
}

/// Which Rojo binary to run: a per-project override, then `ROJOHUB_ROJO_BIN`, then the
/// hub-wide engine setting ("rojo" on PATH, or a path to a binary).
pub fn resolve_engine(project_rojo_bin: Option<&str>, global_engine: Option<&str>) -> Engine {
    if let Some(b) = project_rojo_bin.filter(|b| !b.is_empty()) {
        return Engine {
            bin: b.to_string(),
            label: b.to_string(),
        };
    }
    if let Ok(b) = env::var("ROJOHUB_ROJO_BIN") {
        if !b.is_empty() {
            return Engine {
                bin: b.clone(),
                label: b,
            };
        }
    }
    let engine = global_engine.filter(|e| !e.is_empty()).unwrap_or("rojo");
    Engine {
        bin: engine.to_string(),
        label: engine.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("34872-34881"), Some((34872, 34881)));
        assert_eq!(parse_range(" 1-1 "), Some((1, 1)));
        assert_eq!(parse_range("9-1"), None);
        assert_eq!(parse_range("abc"), None);
        assert_eq!(parse_range(""), None);
    }

    #[test]
    fn engine_resolution() {
        let e = resolve_engine(Some("/x/rojo"), Some("/y/rojo"));
        assert_eq!(e.bin, "/x/rojo");
        let e = resolve_engine(None, None);
        assert_eq!((e.bin.as_str(), e.label.as_str()), ("rojo", "rojo"));
        let e = resolve_engine(None, Some("/opt/rojo"));
        assert_eq!(e.label, "/opt/rojo");
    }
}
