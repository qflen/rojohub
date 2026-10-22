//! `state.json`: the registered projects and the hub-wide engine setting.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{ensure_home, state_file};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub dir: String,
    pub project_file: String,
    pub place_ids: Vec<u64>,
    pub game_id: Option<u64>,
    #[serde(default)]
    pub auto_start: bool,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rojo_bin: Option<String>,
    /// Keys written by other versions survive a round trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub fn load_state() -> State {
    let _ = ensure_home();
    fs::read_to_string(state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &State) -> io::Result<()> {
    ensure_home()?;
    let file = state_file();
    let tmp = file.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_string_pretty(state).unwrap_or_default(),
    )?;
    fs::rename(&tmp, &file)
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

/// Compare directories as the OS does: symlinks resolved and, on Windows, case-insensitively.
pub fn canonical(dir: &str) -> String {
    let p = Path::new(dir);
    let d: PathBuf = fs::canonicalize(p)
        .unwrap_or_else(|_| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));
    let s = d.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Find the registered project whose directory contains `dir` (deepest match wins).
pub fn project_for_dir<'a>(projects: &'a [Project], dir: &str) -> Option<&'a Project> {
    let d = canonical(dir);
    let sep = std::path::MAIN_SEPARATOR;
    let mut best: Option<&Project> = None;
    let mut best_len = 0;
    for p in projects {
        let pd = canonical(&p.dir);
        if (d == pd || d.starts_with(&format!("{pd}{sep}"))) && pd.len() > best_len {
            best = Some(p);
            best_len = pd.len();
        }
    }
    best
}

pub fn read_project_file(file: &str) -> Result<Map<String, Value>, String> {
    let raw = fs::read_to_string(file).map_err(|e| e.to_string())?;
    match serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string())? {
        Value::Object(m) => Ok(m),
        _ => Err("project file is not a JSON object".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, dir: &Path) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            dir: dir.to_string_lossy().into_owned(),
            project_file: dir
                .join("default.project.json")
                .to_string_lossy()
                .into_owned(),
            place_ids: vec![1],
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
    fn slugs() {
        assert_eq!(slugify("My Game (v2)"), "my-game-v2");
        assert_eq!(slugify("---"), "project");
        assert_eq!(slugify("Ünïcode Name"), "n-code-name");
    }

    #[test]
    fn deepest_match_and_symlinks() {
        let root = std::env::temp_dir().join(format!("rojohub-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let outer = root.join("outer");
        let inner = outer.join("inner");
        let other = root.join("outerX");
        fs::create_dir_all(inner.join("src")).unwrap();
        fs::create_dir_all(&other).unwrap();
        let projects = vec![
            project("outer", &outer),
            project("inner", &inner),
            project("other", &other),
        ];

        assert_eq!(
            project_for_dir(&projects, inner.join("src").to_str().unwrap())
                .unwrap()
                .id,
            "inner"
        );
        assert_eq!(
            project_for_dir(&projects, outer.to_str().unwrap())
                .unwrap()
                .id,
            "outer"
        );
        // A sibling that merely shares a prefix is not inside.
        assert_eq!(
            project_for_dir(&projects, other.to_str().unwrap())
                .unwrap()
                .id,
            "other"
        );
        assert!(project_for_dir(&projects, root.to_str().unwrap()).is_none());

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&inner, &link).unwrap();
            assert_eq!(
                project_for_dir(&projects, link.join("src").to_str().unwrap())
                    .unwrap()
                    .id,
                "inner"
            );
        }
        #[cfg(windows)]
        {
            let upper = inner.to_string_lossy().to_uppercase();
            assert_eq!(project_for_dir(&projects, &upper).unwrap().id, "inner");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn state_round_trip_keeps_unknown_keys() {
        let raw = r#"{"projects":[{"id":"a","name":"A","dir":"/d","projectFile":"/d/p.json","placeIds":[1],"gameId":null,"port":34872,"custom":true}],"engine":"rojo","other":1}"#;
        let s: State = serde_json::from_str(raw).unwrap();
        assert_eq!(s.projects[0].extra.get("custom"), Some(&Value::Bool(true)));
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["other"], 1);
        assert_eq!(back["projects"][0]["custom"], true);
        assert_eq!(back["projects"][0]["gameId"], Value::Null);
        assert!(back["projects"][0].get("placeName").is_none());
    }
}
