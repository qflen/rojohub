//! The CLI's and the MCP server's view of the hub: JSON over loopback HTTP with the token.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::config::{get_token, hub_url};

#[derive(Debug, Clone)]
pub struct HubError {
    pub message: String,
    /// HTTP status, or 0 when the hub could not be reached at all.
    pub status: u16,
    pub body: Value,
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl HubError {
    pub fn new(message: impl Into<String>) -> HubError {
        HubError {
            message: message.into(),
            status: 0,
            body: Value::Null,
        }
    }
}

impl From<String> for HubError {
    fn from(message: String) -> HubError {
        HubError::new(message)
    }
}

static TOKEN: OnceLock<String> = OnceLock::new();

fn token() -> Result<&'static str, HubError> {
    if let Some(t) = TOKEN.get() {
        return Ok(t);
    }
    let t = get_token().map_err(|e| HubError::new(format!("Could not read the hub token: {e}")))?;
    Ok(TOKEN.get_or_init(|| t))
}

fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

pub fn hub(method: &str, path: &str, body: Option<&Value>) -> Result<Value, HubError> {
    hub_with_timeout(method, path, body, Duration::from_millis(130_000))
}

pub fn hub_with_timeout(
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, HubError> {
    let token = token()?;
    let url = format!("{}{}", hub_url(), path);
    let auth = format!("Bearer {token}");
    let agent = agent(timeout);
    let sent = match (method, body) {
        ("GET", _) => agent.get(&url).header("Authorization", &auth).call(),
        ("DELETE", _) => agent.delete(&url).header("Authorization", &auth).call(),
        (_, Some(b)) => agent
            .post(&url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(b.to_string().as_str()),
        (_, None) => agent.post(&url).header("Authorization", &auth).send_empty(),
    };
    let mut res = sent.map_err(|_| {
        HubError::new(format!(
            "RojoHub is not running at {}. Start it with `rojohub daemon` (or `rojohub install-service`).",
            hub_url()
        ))
    })?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    let data: Value =
        serde_json::from_str(&text).unwrap_or_else(|_| Value::Object(Default::default()));
    if !(200..300).contains(&status) {
        let message = data
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(HubError {
            message,
            status,
            body: data,
        });
    }
    Ok(data)
}

/// Does a hub answer on `url`?
pub fn hub_answers(url: &str, timeout: Duration) -> bool {
    agent(timeout)
        .get(&format!("{url}/health"))
        .call()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
