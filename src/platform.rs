//! The few places where macOS, Windows and Linux differ: opening a browser, where Studio keeps
//! its plugins, and how to run the hub at login.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::client::hub_answers;
use crate::config::{ensure_home, user_home};

/// Spawn a detached process and forget it; errors are reported to the caller.
fn spawn_detached(cmd: &mut Command) -> io::Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map(|_| ())
}

pub fn open_url(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    if spawn_detached(&mut cmd).is_err() {
        println!("Open {url} in your browser.");
    }
}

/// The same commands open a folder in Finder / Explorer / the file manager.
pub fn open_path(path: &str) {
    open_url(path)
}

pub fn studio_plugins_dir() -> Result<PathBuf, String> {
    if let Some(d) = env::var_os("ROJOHUB_PLUGINS_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let h = user_home();
    if cfg!(target_os = "macos") {
        return Ok(h.join("Documents").join("Roblox").join("Plugins"));
    }
    if cfg!(windows) {
        let local = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join("AppData").join("Local"));
        return Ok(local.join("Roblox").join("Plugins"));
    }
    // Studio has no Linux build; Vinegar runs it under Wine, so look inside its prefix.
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_default();
    let vinegar = h
        .join(".local/share/vinegar/prefixes/studio/drive_c/users")
        .join(user)
        .join("AppData/Local/Roblox/Plugins");
    if vinegar.is_dir() {
        return Ok(vinegar);
    }
    Err("Could not find Studio's plugins folder. Pass --dir <path> or set ROJOHUB_PLUGINS_DIR to it.".to_string())
}

// ---------- run at login ----------

fn plist_path() -> PathBuf {
    user_home()
        .join("Library")
        .join("LaunchAgents")
        .join("dev.rojohub.hub.plist")
}
fn unit_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".config"))
        .join("systemd")
        .join("user")
        .join("rojohub.service")
}
fn startup_vbs_path() -> PathBuf {
    env::var_os("APPDATA")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join("AppData").join("Roaming"))
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join("RojoHub.vbs")
}

/// ROJOHUB_* settings in effect when the service is installed are baked into it, so a hub
/// installed with ROJOHUB_PORT=35000 keeps that port at login.
fn hub_env() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = env::vars()
        .filter(|(k, _)| k.starts_with("ROJOHUB_"))
        .collect();
    v.sort();
    v
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn service_description() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        "Startup folder"
    } else {
        "systemd user service"
    }
}

fn launchd_plist(exe: &str, log_file: &str) -> String {
    let env: String = hub_env()
        .iter()
        .map(|(k, v)| format!("<key>{}</key><string>{}</string>", xml(k), xml(v)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.rojohub.hub</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key>
  <dict>{env}</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(exe),
        xml(log_file),
        xml(log_file)
    )
}

fn systemd_unit(exe: &str, log_file: &str) -> String {
    let env: Vec<String> = hub_env()
        .iter()
        .map(|(k, v)| format!("Environment=\"{k}={}\"", v.replace('"', "\\\"")))
        .collect();
    format!(
        "[Unit]\nDescription=RojoHub\n\n[Service]\nExecStart=\"{exe}\" daemon\n{}\nRestart=always\nRestartSec=2\nStandardOutput=append:{log_file}\nStandardError=append:{log_file}\n\n[Install]\nWantedBy=default.target\n",
        env.join("\n")
    )
}

/// A VBScript in the Startup folder runs at login without admin rights and without a console
/// window flashing up (style 0 = hidden, False = don't wait).
fn startup_vbs(exe: &str, log_file: &str) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let cmd = format!("cmd /c \"\"{exe}\" daemon >> \"{log_file}\" 2>&1\"");
    let mut lines = vec!["Set sh = CreateObject(\"WScript.Shell\")".to_string()];
    for (k, v) in hub_env() {
        lines.push(format!(
            "sh.Environment(\"PROCESS\")({}) = {}",
            q(&k),
            q(&v)
        ));
    }
    lines.push(format!("sh.Run {}, 0, False", q(&cmd)));
    lines.join("\r\n") + "\r\n"
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(
            "systemctl not found, so the hub can't be installed as a service. Run `rojohub daemon` in a terminal instead.".to_string(),
        ),
        Err(e) => Err(format!("systemctl --user {} failed: {e}. Run `rojohub daemon` in a terminal instead.", args.join(" "))),
        Ok(o) if !o.status.success() => Err(format!(
            "systemctl --user {} failed: {}. Run `rojohub daemon` in a terminal instead.",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Ok(_) => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(user_home())
        .map(|m| m.uid().to_string())
        .unwrap_or_else(|_| "501".to_string())
}

/// Start the hub in the background, detached from the caller, logging to `log_file`.
pub fn start_daemon(exe: &Path, log_file: &Path) -> io::Result<()> {
    ensure_home()?;
    #[cfg(windows)]
    {
        // A direct CreateProcess would hand the hub every inheritable handle of this process,
        // including the stdio pipes of whoever spawned us (a test runner, Claude Code), and
        // that parent would then wait on those pipes until the hub exits. Start-Process without
        // redirection goes through ShellExecute, which inherits nothing; cmd does the logging.
        use std::os::windows::process::CommandExt;
        let q = |s: &str| s.replace('\'', "''");
        let command = format!(
            "\"\"{}\" daemon >> \"{}\" 2>&1\"",
            exe.to_string_lossy(),
            log_file.to_string_lossy()
        );
        let script = format!(
            "Start-Process -WindowStyle Hidden -FilePath $env:ComSpec -ArgumentList '/c', '{}'",
            q(&command)
        );
        let status = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .status()?;
        if !status.success() {
            return Err(io::Error::other("powershell could not start the hub"));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let log = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_file)?;
        let mut cmd = Command::new(exe);
        cmd.arg("daemon")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        cmd.spawn().map(|_| ())
    }
}

pub fn install_service(exe: &Path, log_file: &Path, hub_url: &str) -> Result<String, String> {
    ensure_home().map_err(|e| e.to_string())?; // the log file lives there; keep its 0700 mode
    let exe_s = exe.to_string_lossy();
    let log_s = log_file.to_string_lossy();
    if cfg!(target_os = "macos") {
        let plist = plist_path();
        fs::create_dir_all(plist.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&plist, launchd_plist(&exe_s, &log_s)).map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        {
            let target = format!("gui/{}", uid());
            let p = plist.to_string_lossy().into_owned();
            let _ = run("launchctl", &["bootout", &target, &p]);
            run("launchctl", &["bootstrap", &target, &p])
                .map_err(|e| format!("launchctl bootstrap failed: {e}"))?;
        }
        return Ok("RojoHub will now run at login (launchd).".to_string());
    }
    if cfg!(windows) {
        let vbs = startup_vbs_path();
        fs::create_dir_all(vbs.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&vbs, startup_vbs(&exe_s, &log_s)).map_err(|e| e.to_string())?;
        if !hub_answers(hub_url, Duration::from_secs(1)) {
            start_daemon(exe, log_file).map_err(|e| e.to_string())?;
        }
        return Ok("RojoHub will now run at login (Startup folder).".to_string());
    }
    let unit = unit_path();
    fs::create_dir_all(unit.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&unit, systemd_unit(&exe_s, &log_s)).map_err(|e| e.to_string())?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "rojohub"])?;
    systemctl(&["restart", "rojohub"])?; // starts it, or picks up the rewritten unit on reinstall
    Ok("RojoHub will now run at login (systemd user service).".to_string())
}

/// On macOS and Linux the service manager stops the hub; on Windows the caller asks the hub
/// to shut down first (POST /api/shutdown), so its Rojo servers stop with it.
pub fn uninstall_service() {
    if cfg!(target_os = "macos") {
        #[cfg(target_os = "macos")]
        {
            let _ = run(
                "launchctl",
                &[
                    "bootout",
                    &format!("gui/{}", uid()),
                    &plist_path().to_string_lossy(),
                ],
            );
        }
        let _ = fs::remove_file(plist_path());
    } else if cfg!(windows) {
        let _ = fs::remove_file(startup_vbs_path());
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", "rojohub"]);
        let _ = fs::remove_file(unit_path());
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_files_quote_paths() {
        let plist = launchd_plist("/Apps/Rojo & Co/rojohub", "/tmp/hub.log");
        assert!(
            plist.contains("<string>/Apps/Rojo &amp; Co/rojohub</string><string>daemon</string>")
        );
        let unit = systemd_unit("/usr/local/bin/rojohub", "/home/me/.rojohub/hub.log");
        assert!(unit.contains("ExecStart=\"/usr/local/bin/rojohub\" daemon\n"));
        let vbs = startup_vbs("C:\\Tools\\rojohub.exe", "C:\\Users\\me\\.rojohub\\hub.log");
        assert!(vbs.contains("sh.Run \"cmd /c \"\"\"\"C:\\Tools\\rojohub.exe\"\" daemon >> \"\"C:\\Users\\me\\.rojohub\\hub.log\"\" 2>&1\"\"\", 0, False\r\n"));
    }
}
