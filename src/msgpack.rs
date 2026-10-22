//! Decoding Rojo's API bodies. Rojo 7.5+ answers in MessagePack; older versions used JSON.

use serde_json::Value;

pub fn decode_msgpack(bytes: &[u8]) -> Result<Value, String> {
    rmp_serde::from_slice::<Value>(bytes).map_err(|e| format!("msgpack: {e}"))
}

/// Decode a Rojo API body whatever its encoding.
pub fn decode_rojo_body(content_type: &str, bytes: &[u8]) -> Result<Value, String> {
    if content_type.contains("json") || bytes.first() == Some(&b'{') {
        serde_json::from_slice(bytes).map_err(|e| format!("json: {e}"))
    } else {
        decode_msgpack(bytes)
    }
}

/// The parts of `GET /api/rojo` the hub checks.
#[derive(Debug, Clone, PartialEq)]
pub struct RojoInfo {
    pub session_id: Value,
    pub server_version: String,
    pub protocol_version: Option<u64>,
    pub project_name: Option<String>,
    pub expected_place_ids: Option<Vec<u64>>,
}

impl RojoInfo {
    pub fn from_value(v: &Value) -> RojoInfo {
        RojoInfo {
            session_id: v.get("sessionId").cloned().unwrap_or(Value::Null),
            server_version: v
                .get("serverVersion")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            protocol_version: v.get("protocolVersion").and_then(Value::as_u64),
            project_name: v
                .get("projectName")
                .and_then(Value::as_str)
                .map(str::to_string),
            expected_place_ids: v
                .get("expectedPlaceIds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(value_as_id).collect()),
        }
    }
}

/// Place ids as numbers, whether Rojo wrote them as numbers or someone wrote strings.
pub fn value_as_id(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expected() -> Value {
        json!({
            "sessionId": "abc",
            "expectedPlaceIds": [123, 70000, 4294967296u64],
            "projectName": "t",
            "ok": true,
            "n": null,
            "neg": -5,
            "f": 1.5,
        })
    }

    /// Hand-encoded MessagePack for `expected`, one element per line.
    fn encoded() -> Vec<u8> {
        let mut b = vec![0x87]; // fixmap, 7 entries
        b.push(0xa9);
        b.extend(b"sessionId");
        b.push(0xa3);
        b.extend(b"abc");
        b.push(0xb0);
        b.extend(b"expectedPlaceIds");
        b.push(0x93); // fixarray, 3 items
        b.push(0x7b); // positive fixint 123
        b.extend([0xce, 0x00, 0x01, 0x11, 0x70]); // uint32 70000
        b.extend([0xcf, 0, 0, 0, 1, 0, 0, 0, 0]); // uint64 4294967296
        b.push(0xab);
        b.extend(b"projectName");
        b.push(0xa1);
        b.extend(b"t");
        b.push(0xa2);
        b.extend(b"ok");
        b.push(0xc3); // true
        b.push(0xa1);
        b.extend(b"n");
        b.push(0xc0); // nil
        b.push(0xa3);
        b.extend(b"neg");
        b.push(0xfb); // negative fixint -5
        b.push(0xa1);
        b.extend(b"f");
        b.extend([0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]); // float64 1.5
        b
    }

    #[test]
    fn decodes_a_hand_built_rojo_style_map() {
        assert_eq!(decode_msgpack(&encoded()).unwrap(), expected());
    }

    #[test]
    fn rejects_truncated_input() {
        let err = decode_msgpack(&encoded()[..12]).unwrap_err();
        assert!(err.starts_with("msgpack"), "{err}");
    }

    #[test]
    fn body_decoding_picks_the_encoding() {
        let json = serde_json::to_vec(&expected()).unwrap();
        assert_eq!(
            decode_rojo_body("application/json", &json).unwrap(),
            expected()
        );
        assert_eq!(decode_rojo_body("", &json).unwrap(), expected());
        assert_eq!(
            decode_rojo_body("application/msgpack", &encoded()).unwrap(),
            expected()
        );
    }

    #[test]
    fn info_extraction() {
        let info = RojoInfo::from_value(&json!({
            "sessionId": "s1", "serverVersion": "7.7.1", "protocolVersion": 5,
            "projectName": "p", "expectedPlaceIds": [1, "2", 3.0]
        }));
        assert_eq!(info.expected_place_ids, Some(vec![1, 2, 3]));
        assert_eq!(info.server_version, "7.7.1");
        let none = RojoInfo::from_value(&json!({"serverVersion": "7.0.0"}));
        assert_eq!(none.expected_place_ids, None);
        assert_eq!(none.session_id, Value::Null);
    }
}
