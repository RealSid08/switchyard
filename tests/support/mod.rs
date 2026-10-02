//! Shared harness for gateway integration tests: a real Switchyard server on loopback,
//! recording mock upstream providers, and small HTTP/WebSocket helpers.
#![allow(dead_code)]

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::HeaderMap,
    middleware::{self, Next},
    response::Response,
};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use switchyard::app::{App, AppState, router};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

pub const PROVIDER_KEY: &str = "sk-upstream-secret-DO-NOT-LEAK-7f3a";

/// A request observed by a mock upstream.
#[derive(Clone, Debug)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}
impl Captured {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("upstream received JSON")
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }
}

/// A loopback Axum server standing in for a provider. Every request is recorded.
pub struct Upstream {
    pub addr: SocketAddr,
    pub captured: Arc<Mutex<Vec<Captured>>>,
    handle: JoinHandle<()>,
}
impl Upstream {
    pub async fn start(app: Router) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let log = captured.clone();
        let app = app.layer(middleware::from_fn(move |req: Request, next: Next| {
            let log = log.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                log.lock().unwrap().push(Captured {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().map(String::from),
                    headers: parts.headers.clone(),
                    body: body.clone(),
                });
                next.run(Request::from_parts(parts, Body::from(body))).await
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            captured,
            handle,
        }
    }
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
    pub fn requests(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }
    pub fn count(&self) -> usize {
        self.captured.lock().unwrap().len()
    }
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A running gateway bound to an ephemeral loopback port.
pub struct Gateway {
    pub app: App,
    pub port: u16,
    pub admin: String,
    pub data: PathBuf,
    pub http: reqwest::Client,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}
impl Gateway {
    pub async fn start(data: &Path) -> Self {
        Self::start_with(data, 64, 30).await
    }
    pub async fn start_with(data: &Path, max_in_flight: usize, timeout: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let app = AppState::new(
            data.to_path_buf(),
            "127.0.0.1".into(),
            port,
            max_in_flight,
            timeout,
        )
        .expect("gateway state");
        let admin = std::fs::read_to_string(data.join("admin-token"))
            .unwrap()
            .trim()
            .to_string();
        let (tx, rx) = oneshot::channel::<()>();
        let service = router(app.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, service)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            app,
            port,
            admin,
            data: data.to_path_buf(),
            http: client(),
            shutdown: Some(tx),
            handle: Some(handle),
        }
    }
    /// Gracefully stop the server and start a fresh process-equivalent on the same data dir.
    pub async fn restart(mut self) -> Self {
        let data = self.data.clone();
        self.stop().await;
        drop(self);
        Self::start(&data).await
    }
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
    }
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://127.0.0.1:{}{}", self.port, path)
    }
    pub async fn admin_get(&self, path: &str) -> reqwest::Response {
        self.http
            .get(self.url(path))
            .bearer_auth(&self.admin)
            .send()
            .await
            .unwrap()
    }
    pub async fn admin_json(&self, path: &str) -> Value {
        let r = self.admin_get(path).await;
        assert!(r.status().is_success(), "GET {path} -> {}", r.status());
        r.json().await.unwrap()
    }
    pub async fn admin_send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> reqwest::Response {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.admin)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    pub async fn add_connection(&self, body: Value) -> Value {
        let r = self
            .admin_send(reqwest::Method::POST, "/api/connections", body.clone())
            .await;
        let status = r.status();
        let v: Value = r.json().await.unwrap();
        assert_eq!(status, 200, "create connection {body} -> {v}");
        v
    }
    pub async fn connection(&self, name: &str, kind: &str, base: &str, models: &[&str]) -> String {
        self.add_connection(
            json!({"name":name,"kind":kind,"base_url":base,"models":models,"api_key":PROVIDER_KEY}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    pub async fn put_route(
        &self,
        model: &str,
        strategy: &str,
        targets: Value,
    ) -> reqwest::Response {
        self.admin_send(
            reqwest::Method::PUT,
            &format!("/api/routes/{model}"),
            json!({"strategy":strategy,"targets":targets}),
        )
        .await
    }
    pub async fn create_key(&self, name: &str) -> (String, String) {
        let v: Value = self
            .admin_send(reqwest::Method::POST, "/api/keys", json!({"name":name}))
            .await
            .json()
            .await
            .unwrap();
        (
            v["id"].as_str().unwrap().to_string(),
            v["key"]
                .as_str()
                .expect("raw key returned once")
                .to_string(),
        )
    }
    pub async fn post(&self, path: &str, key: &str, body: Value) -> reqwest::Response {
        self.http
            .post(self.url(path))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    pub async fn request_log(&self) -> Vec<Value> {
        self.admin_json("/api/requests?limit=1000")
            .await
            .as_array()
            .unwrap()
            .clone()
    }
    /// Request records are written when a request guard drops, which can race the response.
    pub async fn wait_for_log(&self, count: usize) -> Vec<Value> {
        for _ in 0..200 {
            let log = self.request_log().await;
            if log.len() >= count {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "request log never reached {count} records: {:?}",
            self.request_log().await
        );
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// Build an SSE body from JSON events.
pub fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {}\n\n",
                e["type"].as_str().unwrap_or("message"),
                e
            )
        })
        .collect()
}

/// A streaming body that emits each chunk separately with a pause, so the gateway sees
/// genuinely split frames (mid-line and mid-UTF-8-codepoint).
pub fn chunked_body(chunks: Vec<Vec<u8>>) -> Body {
    let stream = async_stream::stream! {
        for c in chunks {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(c));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    Body::from_stream(stream)
}

/// Split bytes into fixed-size pieces.
pub fn split_every(bytes: &[u8], n: usize) -> Vec<Vec<u8>> {
    bytes.chunks(n).map(<[u8]>::to_vec).collect()
}

pub fn sse_response(body: Body) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

pub fn json_response(status: u16, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

/// Recursively scan every file in a directory for a byte pattern.
pub fn dir_contains(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            hits.extend(dir_contains(&p, needle));
        } else if let Ok(b) = std::fs::read(&p)
            && b.windows(needle.len()).any(|w| w == needle)
        {
            hits.push(p);
        }
    }
    hits
}

pub fn completed(id: &str, text: &str) -> Value {
    json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],
        "usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}})
}
