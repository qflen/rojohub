//! Windows counterpart of `bin/rojohub` for the Claude Code plugin, committed as
//! `bin/rojohub.exe`. Claude Code spawns `${CLAUDE_PLUGIN_ROOT}/bin/rojohub`; on Windows that
//! resolves to this executable. On first run it downloads the release binary for this machine
//! with the system's `curl.exe` (PowerShell as a fallback), checks it against the release's
//! SHA256SUMS with `certutil`, keeps it in the plugin's data directory, then runs it with the
//! same arguments and stdio. It has no dependencies, so it stays small and reproducible.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn fail(msg: &str) -> ! {
    eprintln!("rojohub: {msg}");
    process::exit(1)
}

fn nonempty(v: OsString) -> Option<OsString> {
    (!v.is_empty()).then_some(v)
}

fn data_dir() -> PathBuf {
    if let Some(d) = env::var_os("CLAUDE_PLUGIN_DATA").and_then(nonempty) {
        return PathBuf::from(d);
    }
    if let Some(d) = env::var_os("ROJOHUB_HOME").and_then(nonempty) {
        return PathBuf::from(d).join("bin");
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".rojohub").join("bin")
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let _ = fs::remove_file(dest);
    let curl = Command::new("curl.exe")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(dest)
        .arg(url)
        .status();
    if matches!(curl, Ok(s) if s.success()) && dest.is_file() {
        return Ok(());
    }
    let script = format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{}'",
        dest.display()
    );
    let ps = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status();
    if matches!(ps, Ok(s) if s.success()) && dest.is_file() {
        return Ok(());
    }
    Err(format!("could not download {url}"))
}

fn sha256(path: &Path) -> Result<String, String> {
    let out = Command::new("certutil")
        .arg("-hashfile")
        .arg(path)
        .arg("SHA256")
        .output()
        .map_err(|e| format!("certutil: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() == 64 && compact.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(compact.to_ascii_lowercase());
        }
    }
    Err("certutil printed no SHA-256".to_string())
}

fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset).then(|| hash.to_ascii_lowercase())
    })
}

fn run(bin: &Path, args: &[OsString]) -> ! {
    match Command::new(bin).args(args).status() {
        Ok(status) => process::exit(status.code().unwrap_or(1)),
        Err(e) => fail(&format!("could not run {}: {e}", bin.display())),
    }
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if let Some(b) = env::var_os("ROJOHUB_BIN").and_then(nonempty) {
        run(Path::new(&b), &args);
    }
    let repo = env::var("ROJOHUB_REPO")
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "qflen/rojohub".to_string());
    if env::consts::ARCH != "x86_64" && env::consts::ARCH != "aarch64" {
        fail(&format!("unsupported CPU {}. Download a binary from https://github.com/{repo}/releases and set ROJOHUB_BIN.", env::consts::ARCH));
    }
    // Windows on ARM runs the x64 build through emulation; there is no native arm64 asset.
    let asset = "rojohub-windows-x64.exe";
    let dir = data_dir().join(VERSION);
    let bin = dir.join("rojohub.exe");
    if !bin.is_file() {
        fs::create_dir_all(&dir)
            .unwrap_or_else(|e| fail(&format!("could not create {}: {e}", dir.display())));
        let base = format!("https://github.com/{repo}/releases/download/v{VERSION}");
        eprintln!("rojohub: downloading {asset} {VERSION} from {base}");
        let tmp = dir.join(format!("download-{}.tmp", process::id()));
        let sums = dir.join(format!("sums-{}.tmp", process::id()));
        let result = download(&format!("{base}/{asset}"), &tmp)
            .and_then(|_| download(&format!("{base}/SHA256SUMS"), &sums))
            .and_then(|_| fs::read_to_string(&sums).map_err(|e| e.to_string()))
            .and_then(|text| {
                expected_sum(&text, asset)
                    .ok_or_else(|| format!("{asset} is not listed in SHA256SUMS"))
            })
            .and_then(|want| {
                let got = sha256(&tmp)?;
                if want == got {
                    Ok(())
                } else {
                    Err(format!(
                        "checksum mismatch for {asset} (expected {want}, got {got})"
                    ))
                }
            })
            .and_then(|_| fs::rename(&tmp, &bin).map_err(|e| e.to_string()));
        let _ = fs::remove_file(&sums);
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            fail(&e);
        }
    }
    run(&bin, &args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_asset_line() {
        let sums = "abc  rojohub-linux-x64\nDEF  *rojohub-windows-x64.exe\n";
        assert_eq!(
            expected_sum(sums, "rojohub-windows-x64.exe").as_deref(),
            Some("def")
        );
        assert_eq!(expected_sum(sums, "rojohub-macos-arm64"), None);
    }
}
