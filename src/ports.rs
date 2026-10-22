//! Reserved Rojo ports: stable per project, inside the configured range, never the hub's own.

use std::collections::HashSet;

use tokio::net::TcpListener;

use crate::config::{hub_port, port_range, HUB_HOST};
use crate::state::Project;

pub async fn is_port_free(port: u16) -> bool {
    TcpListener::bind((HUB_HOST, port)).await.is_ok()
}

/// Prefer a port no other project has claimed, so ports stay stable. With more projects
/// than ports, share a port with a project that isn't running; only live servers must differ.
pub async fn allocate_in(
    claimed: &HashSet<u16>,
    range: (u16, u16),
    hub: u16,
) -> Result<u16, String> {
    for pass in 0..2 {
        for port in range.0..=range.1 {
            if port == hub || (pass == 0 && claimed.contains(&port)) {
                continue;
            }
            if is_port_free(port).await {
                return Ok(port);
            }
        }
    }
    Err(format!(
        "All ports {}-{} are in use. Stop a project, or widen ROJOHUB_PORT_RANGE.",
        range.0, range.1
    ))
}

pub async fn allocate_port(projects: &[Project], except_id: Option<&str>) -> Result<u16, String> {
    let claimed: HashSet<u16> = projects
        .iter()
        .filter(|p| Some(p.id.as_str()) != except_id)
        .map(|p| p.port)
        .collect();
    allocate_in(&claimed, port_range(), hub_port()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A range nobody else uses in these tests; each test takes its own slice.
    async fn free_range(n: u16) -> (u16, u16) {
        // Find n consecutive free ports starting high up.
        let mut start = 41000u16;
        loop {
            let mut ok = true;
            for p in start..start + n {
                if !is_port_free(p).await {
                    ok = false;
                    break;
                }
            }
            if ok {
                return (start, start + n - 1);
            }
            start += n;
        }
    }

    #[tokio::test]
    async fn prefers_unclaimed_then_shares_then_fails() {
        let range = free_range(3).await;
        let (a, b, c) = (range.0, range.0 + 1, range.0 + 2);
        // Nothing claimed: the first port.
        assert_eq!(allocate_in(&HashSet::new(), range, 1).await.unwrap(), a);
        // The hub's own port is skipped.
        assert_eq!(allocate_in(&HashSet::new(), range, a).await.unwrap(), b);
        // Claimed ports are passed over while a free one exists.
        assert_eq!(
            allocate_in(&HashSet::from([a, b]), range, 1).await.unwrap(),
            c
        );
        // All claimed: share the first one that is not actually listening.
        assert_eq!(
            allocate_in(&HashSet::from([a, b, c]), range, 1)
                .await
                .unwrap(),
            a
        );
        // A port something else listens on is never handed out.
        let _hold = TcpListener::bind((HUB_HOST, a)).await.unwrap();
        assert_eq!(allocate_in(&HashSet::new(), range, 1).await.unwrap(), b);
        // Everything busy: a clear error.
        let _hold_b = TcpListener::bind((HUB_HOST, b)).await.unwrap();
        let _hold_c = TcpListener::bind((HUB_HOST, c)).await.unwrap();
        let err = allocate_in(&HashSet::new(), range, 1).await.unwrap_err();
        assert!(
            err.contains(&format!("All ports {a}-{c} are in use")),
            "{err}"
        );
    }
}
