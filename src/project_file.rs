//! The project file is what makes Rojo itself refuse the wrong place: `servePlaceIds`.

use std::fs;
use std::path::Path;

use serde_json::ser::PrettyFormatter;
use serde_json::{Map, Serializer, Value};

use crate::msgpack::value_as_id;
use crate::state::read_project_file;

fn ids_text(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn serve_ids(json: &Map<String, Value>) -> Option<Vec<u64>> {
    json.get("servePlaceIds").map(|v| {
        v.as_array()
            .map(|a| a.iter().filter_map(value_as_id).collect())
            .unwrap_or_default()
    })
}

/// Make sure the project file lists `place_ids`. Returns true when the file was changed.
/// The file's own indentation (tabs, or however many spaces) and line endings are kept, so
/// the commit is the one added key and not a reformat of the whole project file.
pub fn patch_serve_place_ids(
    project_file: &Path,
    place_ids: &[u64],
    no_patch: bool,
) -> Result<bool, String> {
    let shown = project_file.display();
    let parsed = fs::read_to_string(project_file)
        .map_err(|e| e.to_string())
        .and_then(|raw| {
            serde_json::from_str::<Value>(&raw)
                .map(|v| (raw, v))
                .map_err(|e| e.to_string())
        });
    let (raw, json) = match parsed {
        Ok((raw, Value::Object(json))) => (raw, json),
        Ok(_) => {
            return Err(format!(
            "Could not parse {shown} as JSON (not an object). Add \"servePlaceIds\": [{}] by hand.",
            ids_text(place_ids)
        ))
        }
        Err(e) => {
            return Err(format!(
                "Could not parse {shown} as JSON ({e}). Add \"servePlaceIds\": [{}] by hand.",
                ids_text(place_ids)
            ))
        }
    };
    let current = serve_ids(&json);
    let have = current.clone().unwrap_or_default();
    let missing: Vec<u64> = place_ids
        .iter()
        .copied()
        .filter(|id| !have.contains(id))
        .collect();
    if missing.is_empty() && current.is_some() {
        return Ok(false);
    }
    if no_patch {
        return Err(format!(
            "{shown} needs \"servePlaceIds\": [{}]. Remove --no-patch to let rojohub add it.",
            ids_text(place_ids)
        ));
    }
    fs::write(format!("{}.rojohub-backup", project_file.display()), &raw)
        .map_err(|e| e.to_string())?;

    let mut merged: Vec<u64> = Vec::new();
    for id in have.iter().chain(place_ids) {
        if !merged.contains(id) {
            merged.push(*id);
        }
    }
    let serve = Value::Array(merged.into_iter().map(Value::from).collect());
    // Insert right after "name" so the diff stays readable.
    let mut out = Map::new();
    for (k, v) in &json {
        if k == "servePlaceIds" {
            continue;
        }
        out.insert(k.clone(), v.clone());
        if k == "name" {
            out.insert("servePlaceIds".into(), serve.clone());
        }
    }
    if !out.contains_key("servePlaceIds") {
        out.insert("servePlaceIds".into(), serve);
    }

    let indent = detect_indent(&raw);
    let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let mut buf = Vec::new();
    let mut ser =
        Serializer::with_formatter(&mut buf, PrettyFormatter::with_indent(indent.as_bytes()));
    serde::Serialize::serialize(&Value::Object(out), &mut ser).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&buf).replace('\n', eol);
    if raw.ends_with('\n') {
        text.push_str(eol);
    }
    fs::write(project_file, text).map_err(|e| e.to_string())?;
    Ok(true)
}

/// The whitespace before the first quoted key on a line of its own; two spaces by default.
fn detect_indent(raw: &str) -> String {
    for line in raw.split('\n') {
        let ws: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if !ws.is_empty() && line[ws.len()..].starts_with('"') {
            return ws;
        }
    }
    "  ".to_string()
}

pub struct ProjectCheck {
    pub ok: bool,
    pub name: Option<String>,
    pub extra: Vec<u64>,
    pub error: Option<String>,
}

/// Checks that the project file on disk will make Rojo itself refuse the wrong place.
pub fn check_project_file(project_file: &str, place_ids: &[u64]) -> ProjectCheck {
    let json = match read_project_file(project_file) {
        Ok(j) => j,
        Err(e) => {
            return ProjectCheck {
                ok: false,
                name: None,
                extra: vec![],
                error: Some(format!("Cannot read {project_file}: {e}")),
            }
        }
    };
    let name = json.get("name").and_then(Value::as_str).map(str::to_string);
    let serve = serve_ids(&json);
    let have = serve.clone().unwrap_or_default();
    let missing: Vec<u64> = place_ids
        .iter()
        .copied()
        .filter(|id| !have.contains(id))
        .collect();
    if serve.is_none() || !missing.is_empty() {
        let shown = if missing.is_empty() {
            "field absent".to_string()
        } else {
            ids_text(&missing)
        };
        return ProjectCheck {
            ok: false,
            name,
            extra: vec![],
            error: Some(format!(
                "servePlaceIds in {project_file} must include {} (missing: {shown}). Run `rojohub add` again or add it manually.",
                ids_text(place_ids)
            )),
        };
    }
    let extra = have
        .into_iter()
        .filter(|id| !place_ids.contains(id))
        .collect();
    ProjectCheck {
        ok: true,
        name,
        extra,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rojohub-pf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn inserts_after_name_keeping_tabs_and_crlf() {
        let p = temp("tabs.project.json", "{\r\n\t\"name\": \"t\",\r\n\t\"tree\": {\r\n\t\t\"$className\": \"DataModel\"\r\n\t}\r\n}\r\n");
        assert!(patch_serve_place_ids(&p, &[123, 456], false).unwrap());
        let out = fs::read_to_string(&p).unwrap();
        assert_eq!(
            out,
            "{\r\n\t\"name\": \"t\",\r\n\t\"servePlaceIds\": [\r\n\t\t123,\r\n\t\t456\r\n\t],\r\n\t\"tree\": {\r\n\t\t\"$className\": \"DataModel\"\r\n\t}\r\n}\r\n"
        );
        assert!(
            fs::read_to_string(format!("{}.rojohub-backup", p.display()))
                .unwrap()
                .starts_with("{\r\n\t\"name\"")
        );
    }

    #[test]
    fn four_spaces_without_trailing_newline_and_merge() {
        let p = temp(
            "spaces.project.json",
            "{\n    \"name\": \"t\",\n    \"servePlaceIds\": [1],\n    \"tree\": {}\n}",
        );
        assert!(patch_serve_place_ids(&p, &[2, 1], false).unwrap());
        let out = fs::read_to_string(&p).unwrap();
        assert_eq!(out, "{\n    \"name\": \"t\",\n    \"servePlaceIds\": [\n        1,\n        2\n    ],\n    \"tree\": {}\n}");
    }

    #[test]
    fn noop_when_present_and_no_patch_refuses() {
        let p = temp(
            "present.project.json",
            "{\n  \"name\": \"t\",\n  \"servePlaceIds\": [5, 6]\n}\n",
        );
        assert!(!patch_serve_place_ids(&p, &[5], false).unwrap());
        assert!(
            fs::metadata(format!("{}.rojohub-backup", p.display())).is_err(),
            "no backup for a no-op"
        );
        let q = temp("nopatch.project.json", "{\n  \"name\": \"t\"\n}\n");
        let err = patch_serve_place_ids(&q, &[7], true).unwrap_err();
        assert!(err.contains("Remove --no-patch"), "{err}");
        assert_eq!(fs::read_to_string(&q).unwrap(), "{\n  \"name\": \"t\"\n}\n");
    }

    #[test]
    fn name_missing_appends_and_bad_json_errors() {
        let p = temp("noname.project.json", "{\n  \"tree\": {}\n}\n");
        assert!(patch_serve_place_ids(&p, &[9], false).unwrap());
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "{\n  \"tree\": {},\n  \"servePlaceIds\": [\n    9\n  ]\n}\n"
        );
        let bad = temp("bad.project.json", "not json");
        let err = patch_serve_place_ids(&bad, &[1], false).unwrap_err();
        assert!(err.starts_with("Could not parse"), "{err}");
    }

    #[test]
    fn checks() {
        let p = temp(
            "check.project.json",
            "{\"name\":\"n\",\"servePlaceIds\":[1,2,\"3\"]}",
        );
        let c = check_project_file(p.to_str().unwrap(), &[1, 2]);
        assert!(c.ok);
        assert_eq!(c.name.as_deref(), Some("n"));
        assert_eq!(c.extra, vec![3]);
        let c = check_project_file(p.to_str().unwrap(), &[1, 4]);
        assert!(!c.ok);
        assert!(c.error.unwrap().contains("(missing: 4)"));
        let q = temp("check2.project.json", "{\"name\":\"n\"}");
        let c = check_project_file(q.to_str().unwrap(), &[1]);
        assert!(c.error.unwrap().contains("must include 1 (missing: 1)"));
        let c = check_project_file("/nonexistent/x.json", &[1]);
        assert!(c.error.unwrap().starts_with("Cannot read"));
    }
}
