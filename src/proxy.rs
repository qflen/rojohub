//! Each project's public port is a RojoHub proxy in front of a Rojo server on a private
//! loopback port. Every request is forwarded as is (with the Host header rewritten, because
//! Rojo only accepts its own port there). A WebSocket upgrade on `/api/socket/` is the
//! Rojo plugin subscribing to changes, so the tunnel is tracked as a connected client.
//! RojoHub's own sync plugin tags its requests with `rojohub-*` query parameters, which the
//! proxy strips before forwarding and uses to attribute the subscription to a Studio window.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, HOST, UPGRADE};
use hyper::service::service_fn;
use hyper::upgrade::OnUpgrade;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::util::{now_ms, parse_query, percent_encode};

pub type ProxyBody = BoxBody<Bytes, hyper::Error>;

#[derive(Debug, Clone, PartialEq)]
pub struct ProxyClient {
    pub place_id: Option<u64>,
    pub session: Option<String>,
    pub auto_connected: bool,
    pub match_reason: Option<String>,
    pub since: u64,
}

/// The subscriptions currently open through one project's proxy.
#[derive(Default)]
pub struct Tracker {
    next: u64,
    clients: BTreeMap<u64, ProxyClient>,
}

impl Tracker {
    pub fn add(&mut self, c: ProxyClient) -> u64 {
        self.next += 1;
        self.clients.insert(self.next, c);
        self.next
    }

    pub fn remove(&mut self, id: u64) {
        self.clients.remove(&id);
    }

    pub fn clients(&self) -> Vec<ProxyClient> {
        self.clients.values().cloned().collect()
    }

    pub fn connected(&self) -> bool {
        !self.clients.is_empty()
    }

    /// A Studio window with `place_id` is synced when a subscriber reported that place, or
    /// when a stock Rojo plugin (which reports nothing) is subscribed: Rojo itself refuses
    /// every place outside `servePlaceIds`, so that subscriber has one of the project's places.
    pub fn synced_for(&self, place_id: u64) -> bool {
        self.clients
            .values()
            .any(|c| c.place_id.is_none() || c.place_id == Some(place_id))
    }

    pub fn studio_link(&self) -> Value {
        json!({
            "connected": self.connected(),
            "clients": self.clients().iter().map(|c| json!({
                "placeId": c.place_id,
                "autoConnected": c.auto_connected,
                "matchReason": c.match_reason,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tags {
    pub client: Option<String>,
    pub place: Option<u64>,
    pub session: Option<String>,
    pub auto: Option<String>,
}

impl Tags {
    pub fn to_client(&self, since: u64) -> ProxyClient {
        let reason = self
            .auto
            .as_deref()
            .filter(|a| matches!(*a, "hub" | "scan"))
            .map(str::to_string);
        ProxyClient {
            place_id: self.place,
            session: self.session.clone(),
            auto_connected: reason.is_some(),
            match_reason: reason,
            since,
        }
    }
}

/// Split a query string into RojoHub's own tags and the query to forward to Rojo.
pub fn split_tags(query: Option<&str>) -> (Tags, Option<String>) {
    let mut tags = Tags::default();
    let mut rest = Vec::new();
    for (k, v) in parse_query(query.unwrap_or("")) {
        match k.as_str() {
            "rojohub-client" => tags.client = Some(v),
            "rojohub-place" => tags.place = v.parse().ok().filter(|n| *n > 0),
            "rojohub-session" => tags.session = Some(v),
            "rojohub-auto" => tags.auto = Some(v),
            _ => rest.push(format!("{}={}", percent_encode(&k), percent_encode(&v))),
        }
    }
    (tags, (!rest.is_empty()).then(|| rest.join("&")))
}

pub struct Proxy {
    pub private_port: u16,
    pub tracker: Arc<Mutex<Tracker>>,
    shutdown: watch::Sender<bool>,
}

impl Proxy {
    /// Close the listener and every open tunnel.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start forwarding `listener` (the project's public port) to Rojo on `private_port`.
/// `changes` is pinged whenever a subscriber connects or goes away.
pub fn spawn_proxy(
    listener: TcpListener,
    private_port: u16,
    changes: mpsc::UnboundedSender<()>,
) -> Proxy {
    let tracker = Arc::new(Mutex::new(Tracker::default()));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let proxy = Proxy {
        private_port,
        tracker: tracker.clone(),
        shutdown: shutdown_tx,
    };
    let mut accept_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                r = listener.accept() => match r { Ok((s, _)) => s, Err(_) => continue },
                _ = accept_shutdown.wait_for(|v| *v) => break,
            };
            let _ = stream.set_nodelay(true);
            let tracker = tracker.clone();
            let changes = changes.clone();
            let mut conn_shutdown = shutdown_rx.clone();
            let svc_shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    forward(
                        req,
                        private_port,
                        tracker.clone(),
                        changes.clone(),
                        svc_shutdown.clone(),
                    )
                });
                let conn = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades();
                tokio::select! {
                    _ = conn => {},
                    _ = conn_shutdown.wait_for(|v| *v) => {},
                }
            });
        }
    });
    proxy
}

fn text_response(status: StatusCode, msg: String) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(msg))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(body)
        .unwrap()
}

async fn forward(
    mut req: Request<Incoming>,
    private_port: u16,
    tracker: Arc<Mutex<Tracker>>,
    changes: mpsc::UnboundedSender<()>,
    shutdown: watch::Receiver<bool>,
) -> Result<Response<ProxyBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    let (tags, query) = split_tags(req.uri().query());
    let is_upgrade = req.headers().contains_key(UPGRADE);
    let subscribing = path.starts_with("/api/socket/");
    let server_upgrade: Option<OnUpgrade> = is_upgrade.then(|| hyper::upgrade::on(&mut req));

    let mut target = path.clone();
    if let Some(q) = &query {
        target.push('?');
        target.push_str(q);
    }
    if let Ok(uri) = target.parse::<hyper::Uri>() {
        *req.uri_mut() = uri;
    }
    if let Ok(host) = HeaderValue::from_str(&format!("127.0.0.1:{private_port}")) {
        req.headers_mut().insert(HOST, host);
    }

    let stream = match TcpStream::connect(("127.0.0.1", private_port)).await {
        Ok(s) => s,
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_GATEWAY,
                format!("RojoHub: Rojo is not answering on its private port ({e})"),
            ))
        }
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(x) => x,
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_GATEWAY,
                format!("RojoHub: could not talk to Rojo ({e})"),
            ))
        }
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let mut resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_GATEWAY,
                format!("RojoHub: Rojo did not answer ({e})"),
            ))
        }
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(server_upgrade) = server_upgrade {
            let client_upgrade = hyper::upgrade::on(&mut resp);
            let client = subscribing.then(|| tags.to_client(now_ms()));
            tokio::spawn(tunnel(
                server_upgrade,
                client_upgrade,
                client,
                tracker,
                changes,
                shutdown,
            ));
        }
    }
    Ok(resp.map(|b| b.boxed()))
}

async fn tunnel(
    server_upgrade: OnUpgrade,
    client_upgrade: OnUpgrade,
    client: Option<ProxyClient>,
    tracker: Arc<Mutex<Tracker>>,
    changes: mpsc::UnboundedSender<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (studio_side, rojo_side) = match tokio::try_join!(server_upgrade, client_upgrade) {
        Ok(pair) => pair,
        Err(_) => return,
    };
    let id = client.map(|c| {
        let id = tracker.lock().map(|mut t| t.add(c)).unwrap_or(0);
        let _ = changes.send(());
        id
    });
    let mut a = TokioIo::new(studio_side);
    let mut b = TokioIo::new(rojo_side);
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {},
        _ = shutdown.wait_for(|v| *v) => {},
    }
    if let Some(id) = id {
        if let Ok(mut t) = tracker.lock() {
            t.remove(id);
        }
        let _ = changes.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn tags_are_split_from_the_query() {
        let (tags, rest) = split_tags(Some(
            "rojohub-client=sync&rojohub-place=123&rojohub-session=s-1&rojohub-auto=hub&keep=1",
        ));
        assert_eq!(tags.client.as_deref(), Some("sync"));
        assert_eq!(tags.place, Some(123));
        assert_eq!(tags.session.as_deref(), Some("s-1"));
        assert_eq!(rest.as_deref(), Some("keep=1"));
        let c = tags.to_client(5);
        assert!(c.auto_connected);
        assert_eq!(c.match_reason.as_deref(), Some("hub"));

        let (tags, rest) = split_tags(None);
        assert_eq!(tags, Tags::default());
        assert_eq!(rest, None);
        let (tags, _) = split_tags(Some("rojohub-auto=manual&rojohub-place=0"));
        let c = tags.to_client(0);
        assert!(!c.auto_connected && c.match_reason.is_none() && c.place_id.is_none());
    }

    #[test]
    fn tracker_reports_clients() {
        let mut t = Tracker::default();
        assert!(!t.connected());
        assert_eq!(t.studio_link()["connected"], false);
        let a = t.add(ProxyClient {
            place_id: Some(1),
            session: None,
            auto_connected: true,
            match_reason: Some("hub".into()),
            since: 0,
        });
        assert!(t.synced_for(1) && !t.synced_for(2));
        let b = t.add(ProxyClient {
            place_id: None,
            session: None,
            auto_connected: false,
            match_reason: None,
            since: 0,
        });
        assert!(
            t.synced_for(2),
            "a stock plugin subscriber counts for any of the project's places"
        );
        assert_eq!(t.studio_link()["clients"].as_array().unwrap().len(), 2);
        t.remove(a);
        t.remove(b);
        assert!(!t.connected());
    }

    /// A stand-in for Rojo: answers GET with the Host header and query it saw, and accepts
    /// WebSocket-style upgrades on /api/socket/, echoing bytes back.
    async fn fake_rojo() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = service_fn(|mut req: Request<Incoming>| async move {
                        if req.headers().contains_key(UPGRADE) {
                            let up = hyper::upgrade::on(&mut req);
                            tokio::spawn(async move {
                                if let Ok(u) = up.await {
                                    let mut io = TokioIo::new(u);
                                    let mut buf = [0u8; 64];
                                    while let Ok(n) = io.read(&mut buf).await {
                                        if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            });
                            return Ok::<_, hyper::Error>(
                                Response::builder()
                                    .status(StatusCode::SWITCHING_PROTOCOLS)
                                    .header(UPGRADE, "websocket")
                                    .header("Connection", "Upgrade")
                                    .body(Full::new(Bytes::new()).map_err(|n| match n {}).boxed())
                                    .unwrap(),
                            );
                        }
                        let body = json!({
                            "host": req.headers().get(HOST).and_then(|h| h.to_str().ok()),
                            "query": req.uri().query(),
                            "path": req.uri().path(),
                        });
                        Ok(text_response(StatusCode::OK, body.to_string()))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .with_upgrades()
                        .await;
                });
            }
        });
        port
    }

    async fn raw(port: u16, request: &str) -> (TcpStream, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        (s, String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    #[tokio::test]
    async fn forwards_requests_and_tracks_subscriptions() {
        let private = fake_rojo().await;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let public = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let proxy = spawn_proxy(listener, private, tx);

        // Plain request: Host rewritten to the private port, RojoHub tags stripped, the rest kept.
        let (_s, head) = raw(public, &format!("GET /api/rojo?rojohub-place=5&x=1 HTTP/1.1\r\nHost: 127.0.0.1:{public}\r\nConnection: close\r\n\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let body = head.split("\r\n\r\n").nth(1).unwrap_or("");
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["host"], format!("127.0.0.1:{private}"));
        assert_eq!(v["query"], "x=1");
        assert_eq!(v["path"], "/api/rojo");
        assert!(!proxy.tracker.lock().unwrap().connected());

        // Upgrade on /api/socket/: tracked as a subscriber for place 123, bytes flow both ways.
        let (mut ws, head) = raw(public, &format!("GET /api/socket/0?rojohub-client=sync&rojohub-place=123&rojohub-auto=hub HTTP/1.1\r\nHost: 127.0.0.1:{public}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        rx.recv().await.unwrap();
        {
            let t = proxy.tracker.lock().unwrap();
            let clients = t.clients();
            assert_eq!(clients.len(), 1);
            assert_eq!(clients[0].place_id, Some(123));
            assert!(clients[0].auto_connected);
            assert!(t.synced_for(123) && !t.synced_for(9));
        }
        ws.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        ws.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        drop(ws);
        rx.recv().await.unwrap();
        assert!(!proxy.tracker.lock().unwrap().connected());

        // An upgrade elsewhere is forwarded but not counted as a subscription.
        let (_ws2, head) = raw(public, &format!("GET /other HTTP/1.1\r\nHost: 127.0.0.1:{public}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!proxy.tracker.lock().unwrap().connected());

        // Shutting the proxy down closes the port.
        proxy.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            TcpStream::connect(("127.0.0.1", public)).await.is_err() || {
                // Some platforms accept then reset; a request must fail either way.
                let mut s = TcpStream::connect(("127.0.0.1", public)).await.unwrap();
                s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.ok();
                let mut b = [0u8; 1];
                matches!(s.read(&mut b).await, Ok(0) | Err(_))
            }
        );
    }
}
