use crate::{
    credentials, oauth, proxy,
    store::{Connection, Route, Store, hash, id, now},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, broadcast};

pub type App = Arc<AppState>;
pub struct InstanceInUse;
impl std::fmt::Debug for InstanceInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for InstanceInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Another Switchyard process is using this data directory")
    }
}
impl std::error::Error for InstanceInUse {}

pub struct AppState {
    _instance_lock: std::fs::File,
    pub resilience: std::sync::Mutex<crate::resilience::Resilience>,
    pub store: Store,
    pub client: reqwest::Client,
    pub started: Instant,
    pub paused: AtomicBool,
    pub active: AtomicUsize,
    pub permits: Arc<Semaphore>,
    pub events: broadcast::Sender<Value>,
    pub admin_token: String,
    pub sessions: std::sync::Mutex<std::collections::HashMap<String, Instant>>,
    pub host: String,
    pub port: u16,
    pub max_in_flight: usize,
    pub timeout: Duration,
    pub cursor: AtomicUsize,
    pub refresh_locks:
        std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}
impl AppState {
    pub fn new(
        data: PathBuf,
        host: String,
        port: u16,
        max: usize,
        seconds: u64,
    ) -> Result<App, Box<dyn std::error::Error>> {
        let existed = data.exists();
        if data == std::path::Path::new("/")
            || std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .is_ok_and(|h| data == std::path::Path::new(&h))
        {
            return Err("Use a dedicated private data directory".into());
        }
        std::fs::create_dir_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !existed {
                std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))?;
            } else if std::fs::metadata(&data)?.permissions().mode() & 0o077 != 0 {
                return Err("Existing data directory must be private (chmod 700); its permissions were not changed".into());
            }
        }
        #[cfg(not(unix))]
        let _ = existed;
        for path in [
            &data,
            &data.join("admin-token"),
            &data.join("switchyard.db"),
            &data.join("switchyard.lock"),
        ] {
            if std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err("Data directory and state files must not be symlinks".into());
            }
        }
        let mut lock_options = std::fs::OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let instance_lock = lock_options.open(data.join("switchyard.lock"))?;
        instance_lock.try_lock().map_err(|_| InstanceInUse)?;
        let token_file = data.join("admin-token");
        let admin_token = if token_file.exists() {
            std::fs::read_to_string(&token_file)?.trim().to_string()
        } else {
            use std::io::Write;
            let token = format!(
                "sy_admin_{}{}",
                id().replace('-', ""),
                id().replace('-', "")
            );
            let temp = data.join(format!(".admin-token-{}", id()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &token_file)?;
            token
        };
        if admin_token.len() != 73
            || !admin_token.starts_with("sy_admin_")
            || !admin_token[9..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Invalid admin-token file; restore it from a private backup or remove that file to generate a new token".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;
        }
        let db_path = data.join("switchyard.db");
        let store = Store::open(&db_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600))?;
        }
        let resilience = crate::resilience::Resilience::open(&db_path)?;
        let paused = store.get::<bool>("setting", "paused").unwrap_or(false);
        let (events, _) = broadcast::channel(256);
        Ok(Arc::new(Self {
            _instance_lock: instance_lock,
            store,
            resilience: std::sync::Mutex::new(resilience),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(seconds))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            started: Instant::now(),
            paused: AtomicBool::new(paused),
            active: AtomicUsize::new(0),
            permits: Arc::new(Semaphore::new(max)),
            events,
            admin_token,
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            host,
            port,
            max_in_flight: max,
            timeout: Duration::from_secs(seconds),
            cursor: AtomicUsize::new(0),
            refresh_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
        }))
    }
    pub fn overview(&self) -> Value {
        let connections: Vec<Connection> = self.store.list("connection");
        let records = self.store.requests(1000);
        let mut latencies: Vec<u64> = records.iter().map(|r| r.latency_ms).collect();
        latencies.sort_unstable();
        let mut buckets = std::collections::BTreeMap::<i64, (u64, u64)>::new();
        for r in &records {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&r.timestamp) {
                let entry = buckets.entry(t.timestamp() / 60 * 60).or_default();
                entry.0 += 1;
                if r.status >= 400 {
                    entry.1 += 1;
                }
            }
        }
        let series:Vec<Value>=buckets.into_iter().map(|(timestamp,(requests,errors))|json!({"timestamp":chrono::DateTime::from_timestamp(timestamp,0).unwrap().to_rfc3339(),"requests":requests,"errors":errors})).collect();
        json!({"version":env!("CARGO_PKG_VERSION"),"uptime_seconds":self.started.elapsed().as_secs(),"paused":self.paused.load(Ordering::Relaxed),"requests_total":self.store.counter("total"),"requests_success":self.store.counter("success"),"requests_failed":self.store.counter("failed"),"active_requests":self.active.load(Ordering::Relaxed),"connections_total":connections.len(),"connections_enabled":connections.iter().filter(|c|c.enabled).count(),"latency_ms_p50":latencies.get(latencies.len()/2).copied().unwrap_or(0),"transport_counts":{"http":self.store.counter("http"),"sse":self.store.counter("sse"),"websocket":self.store.counter("websocket")},"recent_requests":records.into_iter().take(20).collect::<Vec<_>>(),"series":series})
    }
}
#[derive(Debug)]
pub struct ApiError {
    pub retry_after: Option<u64>,
    pub status: StatusCode,
    pub message: String,
}
impl ApiError {
    pub fn new(status: u16, message: &str) -> Self {
        Self {
            retry_after: None,
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            message: message.into(),
        }
    }
    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
    pub fn bad(message: &str) -> Self {
        Self::new(400, message)
    }
    pub fn upstream(message: &str) -> Self {
        Self::new(502, message)
    }
    pub fn db(_: rusqlite::Error) -> Self {
        Self::new(500, "Could not persist gateway state")
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r=(self.status,Json(json!({"error":{"message":self.message,"type":"gateway_error","retry_after_seconds":self.retry_after}}))).into_response();
        if let Some(seconds) = self.retry_after {
            r.headers_mut()
                .insert("retry-after", seconds.to_string().parse().unwrap());
        }
        r
    }
}
pub fn account_lock(app: &App, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = app.refresh_locks.lock().expect("refresh lock registry");
    if locks.len() >= 1024 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks
        .entry(id.into())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}
fn constant_eq(a: &str, b: &str) -> bool {
    bool::from(hash(a).as_bytes().ct_eq(hash(b).as_bytes()))
}
fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split_once(' ')
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token.trim())
                .filter(|t| !t.is_empty())
        })
        .or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))
}
fn local_host(app: &App, h: &HeaderMap) -> bool {
    let Some(host) = h.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    [
        format!("localhost:{}", app.port),
        format!("127.0.0.1:{}", app.port),
        format!("[::1]:{}", app.port),
    ]
    .contains(&host.to_string())
        && app
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|x| x.is_loopback())
}
fn trusted_cookie_host(app: &App, h: &HeaderMap) -> bool {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if [
        format!("localhost:{}", app.port),
        format!("127.0.0.1:{}", app.port),
        format!("[::1]:{}", app.port),
        format!("{}:{}", app.host, app.port),
    ]
    .contains(&host.to_string())
    {
        return true;
    }
    std::env::var("SWITCHYARD_PUBLIC_ORIGIN")
        .ok()
        .and_then(|s| url::Url::parse(&s).ok())
        .is_some_and(|u| {
            u.host_str().is_some_and(|name| {
                format!(
                    "{}{}",
                    name,
                    u.port().map(|p| format!(":{p}")).unwrap_or_default()
                ) == host
            })
        })
}
fn same_origin(h: &HeaderMap) -> bool {
    if h.get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return false;
    }
    if let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let Some(host) = h.get(header::HOST).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        return origin == format!("http://{host}") || origin == format!("https://{host}");
    }
    true
}
pub async fn admin_auth(State(app): State<App>, request: Request, next: Next) -> Response {
    let h = request.headers();
    if !same_origin(h) {
        return ApiError::new(403, "Cross-origin administration is disabled").into_response();
    }
    let cookie = session_cookie(h).is_some_and(|token| {
        let mut sessions = app.sessions.lock().expect("session lock");
        sessions.retain(|_, expiry| *expiry > Instant::now());
        sessions.contains_key(&hash(token))
    });
    if bearer(h).is_some_and(|t| !t.is_empty() && constant_eq(t, &app.admin_token))
        || (cookie && trusted_cookie_host(&app, h))
    {
        next.run(request).await
    } else {
        ApiError::new(401, "Admin session required").into_response()
    }
}
async fn client_auth(State(app): State<App>, request: Request, next: Next) -> Response {
    let token = bearer(request.headers())
        .or_else(|| {
            request
                .headers()
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
        })
        .unwrap_or("");
    let keys: Vec<Value> = app.store.list("key");
    if let Some(key) = keys.iter().find(|k| {
        !token.is_empty()
            && k["hash"]
                .as_str()
                .is_some_and(|x| constant_eq(x, &hash(token)))
    }) {
        let mut request = request;
        request
            .extensions_mut()
            .insert(crate::usage::ClientIdentity {
                id: key["id"].as_str().unwrap_or_default().to_owned(),
                name: key["name"].as_str().unwrap_or_default().to_owned(),
            });
        next.run(request).await
    } else {
        ApiError::new(401, "A valid Switchyard client API key is required").into_response()
    }
}
async fn security_headers(request: Request, next: Next) -> Response {
    let inference =
        request.uri().path().starts_with("/v1/") || request.uri().path().starts_with("/v1beta/");
    let limit: u64 = if inference {
        64 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    let oversized = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit);
    let http1 = matches!(
        request.version(),
        axum::http::Version::HTTP_10 | axum::http::Version::HTTP_11
    );
    let mut response = if oversized {
        ApiError::new(
            413,
            "Request exceeds the size limit (64 MiB for inference, 8 MiB for administration)",
        )
        .into_response()
    } else {
        next.run(request).await
    };
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && http1 {
        response.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("close"),
        );
    }
    response.headers_mut().insert(
        "x-content-type-options",
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "referrer-policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert("content-security-policy",header::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"));
    response
}
pub fn router(app: App) -> Router {
    let admin = Router::new()
        .route("/api/overview", get(overview))
        .route("/api/oauth/start", post(oauth_start))
        .route("/api/oauth/{id}", get(oauth_status).delete(oauth_cancel))
        .route("/api/oauth/{id}/callback", post(oauth_callback))
        .route("/api/config", get(config))
        .route("/api/settings", post(settings))
        .route("/api/connections", get(connections).post(create_connection))
        .route(
            "/api/connections/{id}",
            put(update_connection).delete(delete_connection),
        )
        .route("/api/connections/{id}/test", post(test_connection))
        .route("/api/connections/{id}/models", get(discover_models))
        .route("/api/import", post(import_credentials))
        .route("/api/models", get(models))
        .route("/api/routes", get(routes))
        .route("/api/routes/{*model}", put(save_route).delete(delete_route))
        .route("/api/requests", get(requests))
        .route("/api/requests/{id}", get(request_detail))
        .route("/api/events", get(events))
        .route("/api/keys", get(keys).post(create_key))
        .route("/api/keys/{id}", delete(delete_key))
        .route("/api/playground", post(playground))
        .route("/api/playground/ws", get(proxy::playground_ws))
        .merge(crate::usage::router())
        .merge(crate::usage_sources::router())
        .route_layer(middleware::from_fn_with_state(app.clone(), admin_auth));
    let client = Router::new()
        .route("/v1/models", get(client_models))
        .route(
            "/v1/responses",
            post(proxy::responses).get(proxy::responses_ws),
        )
        .route("/v1/chat/completions", post(proxy::chat))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route("/v1beta/models/{*action}", post(proxy::gemini))
        .route_layer(middleware::from_fn_with_state(app.clone(), client_auth))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));
    Router::new()
        .merge(admin)
        .merge(client)
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/api/hello", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/api/session", get(session).post(session).delete(logout))
        .fallback(static_asset)
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}
async fn session(State(app): State<App>, h: HeaderMap) -> Result<Response, ApiError> {
    if !same_origin(&h) {
        return Err(ApiError::new(
            403,
            "Cross-origin administration is disabled",
        ));
    }
    let token_ok = bearer(&h).is_some_and(|t| !t.is_empty() && constant_eq(t, &app.admin_token));
    let browser_ok = local_host(&app, &h)
        && same_origin(&h)
        && h.get("sec-fetch-site").is_some_and(|v| v == "same-origin");
    if !token_ok && !browser_ok {
        return Err(ApiError::new(
            401,
            "Open the local dashboard or provide the admin token",
        ));
    }
    let token = format!("{}{}", id(), id());
    {
        let mut sessions = app.sessions.lock().expect("session lock");
        sessions.retain(|_, expiry| *expiry > Instant::now());
        if sessions.len() >= 256
            && let Some(old) = sessions
                .iter()
                .min_by_key(|(_, expiry)| **expiry)
                .map(|(key, _)| key.clone())
        {
            sessions.remove(&old);
        }
        sessions.insert(hash(&token), Instant::now() + Duration::from_secs(43200));
    }
    let mut r = Json(json!({"authenticated":true})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        format!(
            "sy_session={}; HttpOnly; SameSite=Strict; Path=/api; Max-Age=43200{}",
            token,
            if std::env::var("SWITCHYARD_PUBLIC_ORIGIN").is_ok_and(|s| s.starts_with("https://")) {
                "; Secure"
            } else {
                ""
            }
        )
        .parse()
        .unwrap(),
    );
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(r)
}
fn session_cookie(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("sy_session="))
}
async fn logout(State(app): State<App>, h: HeaderMap) -> Result<Response, ApiError> {
    if !same_origin(&h) || !trusted_cookie_host(&app, &h) {
        return Err(ApiError::new(
            403,
            "Cross-origin administration is disabled",
        ));
    }
    if let Some(token) = session_cookie(&h) {
        app.sessions
            .lock()
            .expect("session lock")
            .remove(&hash(token));
    }
    let mut response = Json(json!({"authenticated":false})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_static(
            "sy_session=; HttpOnly; SameSite=Strict; Path=/api; Max-Age=0",
        ),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}
async fn oauth_start(
    State(app): State<App>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        oauth::start(app, body["provider"].as_str().unwrap_or("")).await?,
    ))
}
async fn oauth_status(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(oauth::status(&app, &id)?))
}
async fn oauth_cancel(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(oauth::cancel(&app, &id)?))
}
async fn oauth_callback(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        oauth::submit_callback(&app, &id, body["input"].as_str().unwrap_or("")).await?,
    ))
}
async fn overview(State(app): State<App>) -> Json<Value> {
    Json(app.overview())
}
async fn config(State(app): State<App>, h: HeaderMap) -> Json<Value> {
    let host = h
        .get(header::HOST)
        .and_then(|x| x.to_str().ok())
        .unwrap_or("127.0.0.1");
    let origin =
        std::env::var("SWITCHYARD_PUBLIC_ORIGIN").unwrap_or_else(|_| format!("http://{host}"));
    let ws_origin = origin.replacen("http", "ws", 1);
    Json(
        json!({"host":app.host,"port":app.port,"api_base":format!("{origin}/v1"),"websocket_url":format!("{ws_origin}/v1/responses"),"requires_api_key":true,"max_in_flight":app.max_in_flight,"request_timeout_seconds":app.timeout.as_secs()}),
    )
}
async fn settings(State(app): State<App>, Json(v): Json<Value>) -> Result<Json<Value>, ApiError> {
    let paused = v["paused"]
        .as_bool()
        .ok_or(ApiError::bad("paused must be a boolean"))?;
    app.store
        .put("setting", "paused", &paused)
        .map_err(ApiError::db)?;
    app.paused.store(paused, Ordering::Relaxed);
    let overview = app.overview();
    let _ = app.events.send(json!({"type":"overview","data":overview}));
    Ok(Json(overview))
}
/// The newest retained request involving `connection_id`, as `(timestamp, status, error)`.
///
/// A request counts if the connection served it or was attempted and failed over. For the
/// connection that finished the request, the request's own outcome is used (a stream can fail
/// after a 200 attempt); for one that was only tried, its last attempt in that request. Attempt
/// errors are constant labels, never provider text. Only the retained history (newest 1,000
/// requests) is consulted.
fn connection_history<'a>(
    records: &'a [crate::store::RequestRecord],
    connection_id: &str,
) -> Option<(&'a str, u16, Option<&'a str>)> {
    records.iter().find_map(|record| {
        if record.connection_id == connection_id {
            return Some((
                record.timestamp.as_str(),
                record.status,
                record.error.as_deref(),
            ));
        }
        record
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.connection_id == connection_id)
            .map(|attempt| {
                (
                    record.timestamp.as_str(),
                    attempt.status,
                    attempt.error.as_deref(),
                )
            })
    })
}
async fn connections(State(app): State<App>) -> Json<Value> {
    let records = app.store.requests(1000);
    let connections: Vec<Connection> = app.store.list("connection");
    let mut resilience = app.resilience.lock().expect("resilience lock");
    let values: Vec<Value> = connections
        .iter()
        .map(|connection| {
            let mut value = connection.public();
            let recent = connection_history(&records, &connection.id);
            let cooldowns = resilience.cooldowns(&connection.id);
            let account_cooling = cooldowns.iter().any(|(model, _)| model == "*");
            value["health"] = json!({
                "status": if !connection.enabled {
                    "disabled"
                } else if account_cooling {
                    "cooling"
                } else if !cooldowns.is_empty() {
                    "limited"
                } else {
                    "ready"
                },
                "cooldowns": cooldowns
                    .iter()
                    .map(|(model, seconds)| json!({"model": model, "retry_after_seconds": seconds}))
                    .collect::<Vec<_>>(),
                "last_used_at": recent.map(|(at, _, _)| at),
                "last_status": recent.map(|(_, status, _)| status),
                "last_error": recent.and_then(|(_, _, error)| error),
            });
            value["credential_expires_at"] = if connection.expires_at > 0 {
                json!(connection.expires_at)
            } else {
                Value::Null
            };
            value
        })
        .collect();
    Json(json!(values))
}
#[derive(Deserialize)]
struct ConnectionInput {
    name: String,
    kind: String,
    base_url: String,
    #[serde(default = "yes")]
    enabled: bool,
    models: Vec<String>,
    #[serde(default)]
    supports_websocket: bool,
    api_key: Option<String>,
}
fn yes() -> bool {
    true
}
fn validate(v: &ConnectionInput) -> Result<(), ApiError> {
    if v.name.trim().is_empty() || v.name.len() > 100 {
        return Err(ApiError::bad("Connection name must be 1–100 characters"));
    }
    if !["openai", "anthropic", "gemini", "codex", "antigravity"].contains(&v.kind.as_str()) {
        return Err(ApiError::bad("Unknown provider kind"));
    }
    let u = url::Url::parse(&v.base_url).map_err(|_| ApiError::bad("Invalid provider base URL"))?;
    if !["http", "https"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(ApiError::bad(
            "Use an HTTP(S) base URL without credentials, query or fragment",
        ));
    }
    if u.scheme() == "http"
        && !u.host_str().is_some_and(|s| {
            s == "localhost"
                || s.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|x| x.is_loopback())
        })
    {
        return Err(ApiError::bad(
            "HTTP providers must be on loopback. Use HTTPS for remote providers.",
        ));
    }
    if v.models.is_empty()
        || v.models.len() > 100
        || v.models.iter().any(|m| m.is_empty() || m.len() > 200)
    {
        return Err(ApiError::bad("Provide 1–100 model identifiers"));
    }
    if v.supports_websocket && !["openai", "codex"].contains(&v.kind.as_str()) {
        return Err(ApiError::bad(
            "Responses WebSocket requires an OpenAI or Codex connection",
        ));
    }
    if v.api_key.as_ref().is_some_and(|x| x.contains(['\r', '\n'])) {
        return Err(ApiError::bad("Invalid credential"));
    }
    Ok(())
}
async fn create_connection(
    State(app): State<App>,
    Json(v): Json<ConnectionInput>,
) -> Result<Json<Value>, ApiError> {
    validate(&v)?;
    let c = Connection {
        id: id(),
        name: v.name,
        kind: v.kind,
        base_url: v.base_url.trim_end_matches('/').into(),
        enabled: v.enabled,
        models: v.models,
        supports_websocket: v.supports_websocket,
        created_at: now(),
        api_key: v.api_key.unwrap_or_default(),
        refresh_token: String::new(),
        expires_at: 0,
        account_id: String::new(),
        oauth: false,
        credential_source: "api_key".into(),
        source_path: String::new(),
        account_identity: String::new(),
    };
    app.store
        .put("connection", &c.id, &c)
        .map_err(ApiError::db)?;
    Ok(Json(c.public()))
}
async fn update_connection(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(v): Json<ConnectionInput>,
) -> Result<Json<Value>, ApiError> {
    validate(&v)?;
    let lock = account_lock(&app, &id);
    let _guard = lock.lock().await;
    let mut c = app
        .store
        .get::<Connection>("connection", &id)
        .ok_or(ApiError::new(404, "Connection not found"))?;
    c.name = v.name;
    c.kind = v.kind;
    c.base_url = v.base_url.trim_end_matches('/').into();
    c.enabled = v.enabled;
    c.models = v.models;
    c.supports_websocket = v.supports_websocket;
    if let Some(key) = v.api_key.filter(|x| !x.is_empty()) {
        c.api_key = key;
        c.oauth = false;
        c.refresh_token.clear();
        c.expires_at = 0;
        c.account_id.clear();
        c.credential_source = "api_key".into();
        c.source_path.clear();
        c.account_identity.clear();
    }
    app.store.put("connection", &id, &c).map_err(ApiError::db)?;
    app.resilience.lock().expect("resilience lock").reset(&id);
    Ok(Json(c.public()))
}
async fn delete_connection(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let lock = account_lock(&app, &id);
    let _guard = lock.lock().await;
    for r in app.store.list::<Route>("route") {
        if r.targets.iter().any(|t| t.connection_id == id) {
            return Err(ApiError::new(
                409,
                "Remove this connection from its model routes first",
            ));
        }
    }
    app.store.delete("connection", &id).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn test_connection(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut c = app
        .store
        .get::<Connection>("connection", &id)
        .ok_or(ApiError::new(404, "Connection not found"))?;
    credentials::refresh(&app, &mut c)
        .await
        .map_err(management_error)?;
    let start = Instant::now();
    if c.kind == crate::antigravity::KIND {
        let result = tokio::time::timeout(
            CATALOG_DEADLINE.min(app.timeout),
            crate::antigravity::catalog(&app, &mut c),
        )
        .await;
        let (status, message) = match result {
            Ok(Ok(_)) => (200, "Provider reachable"),
            Ok(Err(error)) => (
                error.status.as_u16(),
                "Provider unavailable; check credentials and account setup",
            ),
            Err(_) => (504, "Provider check timed out"),
        };
        return Ok(Json(
            json!({"ok":status < 400,"status":status,"latency_ms":start.elapsed().as_millis(),"message":message}),
        ));
    }
    let path = if c.kind == "codex" {
        "models?client_version=0.159.3"
    } else {
        "models"
    };
    let req = proxy::upstream_headers(app.client.get(format!("{}/{}", c.base_url, path)), &c);
    let result = req.send().await;
    let (status, message) = match result {
        Ok(r) => (
            r.status().as_u16(),
            if r.status().is_success() {
                "Provider reachable"
            } else {
                "Provider returned an error; check credentials and base URL"
            },
        ),
        Err(_) => (502, "Could not reach provider"),
    };
    Ok(Json(
        json!({"ok":status<400,"status":status,"latency_ms":start.elapsed().as_millis(),"message":message}),
    ))
}
/// Bounds for one model catalog discovery (all pages together).
const CATALOG_PAGES: usize = 5;
const CATALOG_BYTES: usize = 2 * 1024 * 1024;
const CATALOG_MODELS: usize = 1000;
const CATALOG_DEADLINE: Duration = Duration::from_secs(20);
const CATALOG_UNAVAILABLE: &str =
    "Provider model catalog unavailable. Check the account or enter model identifiers manually.";

/// Remaining time before `deadline`, or a gateway timeout once it has passed.
fn catalog_remaining(deadline: Instant) -> Result<Duration, ApiError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(ApiError::new(
            504,
            "Provider model catalog timed out; enter model identifiers manually",
        ))
}
/// The catalog URL for one page. Only the configured base URL is ever requested: cursors are
/// sent as encoded query values, and URLs supplied by the provider are never followed.
fn catalog_url(connection: &Connection, cursor: Option<&str>) -> Result<url::Url, ApiError> {
    let mut url = url::Url::parse(&format!("{}/models", connection.base_url))
        .map_err(|_| ApiError::bad("Invalid provider base URL"))?;
    {
        let mut q = url.query_pairs_mut();
        match connection.kind.as_str() {
            // Documented maximum page sizes: Anthropic `limit` 1..=1000, Gemini `pageSize` <=1000.
            "anthropic" => {
                q.append_pair("limit", "1000");
                if let Some(c) = cursor {
                    q.append_pair("after_id", c);
                }
            }
            "gemini" => {
                q.append_pair("pageSize", "1000");
                if let Some(c) = cursor {
                    q.append_pair("pageToken", c);
                }
            }
            "codex" => {
                q.append_pair("client_version", "0.160.0");
            }
            _ => {}
        }
    }
    if url.query() == Some("") {
        url.set_query(None);
    }
    Ok(url)
}
/// The cursor for the next page, if the provider says there is one. Only Anthropic and Gemini
/// document catalog pagination.
fn catalog_cursor(kind: &str, page: &Value) -> Option<String> {
    let cursor = match kind {
        "anthropic" if page["has_more"] == true => page["last_id"].as_str(),
        "gemini" => page["nextPageToken"].as_str(),
        _ => None,
    }?;
    (!cursor.is_empty() && cursor.len() <= 1024 && !cursor.chars().any(char::is_control))
        .then(|| cursor.to_owned())
}
/// Reads one page within the remaining byte budget and deadline. Errors carry a status for the
/// first page; later pages only end pagination early.
async fn catalog_page(
    app: &App,
    connection: &mut Connection,
    cursor: Option<&str>,
    deadline: Instant,
    budget: usize,
    adopted: &mut bool,
) -> Result<Vec<u8>, ApiError> {
    let url = catalog_url(connection, cursor)?;
    let send = |connection: &Connection, remaining: Duration| {
        proxy::upstream_headers(app.client.get(url.clone()), connection)
            .timeout(remaining)
            .send()
    };
    let unreachable = || ApiError::upstream("Could not reach the provider's model catalog");
    let mut response = send(connection, catalog_remaining(deadline)?)
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ApiError::new(
                    504,
                    "Provider model catalog timed out; enter model identifiers manually",
                )
            } else {
                unreachable()
            }
        })?;
    // One credential renewal or source adoption per discovery, inside the same deadline.
    if response.status() == StatusCode::UNAUTHORIZED && !*adopted {
        *adopted = true;
        let rejected = connection.api_key.clone();
        let renewed = tokio::time::timeout(
            catalog_remaining(deadline)?,
            credentials::refresh_forced(app, connection),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        if renewed && connection.api_key != rejected {
            response = send(connection, catalog_remaining(deadline)?)
                .await
                .map_err(|_| unreachable())?;
        }
    }
    let status = response.status();
    if !status.is_success() {
        let code = match status.as_u16() {
            401 | 403 => 424,
            code @ 400..=599 => code,
            _ => 502,
        };
        return Err(ApiError::new(code, CATALOG_UNAVAILABLE));
    }
    let too_large = || ApiError::upstream("Provider model catalog exceeds 2 MiB");
    if response
        .content_length()
        .is_some_and(|n| n as usize > budget)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(catalog_remaining(deadline)?, response.chunk())
            .await
            .map_err(|_| {
                catalog_remaining(deadline)
                    .err()
                    .unwrap_or_else(unreachable)
            })?
            .map_err(|_| ApiError::upstream("Provider model catalog interrupted"))?;
        let Some(chunk) = chunk else { break };
        if bytes.len() + chunk.len() > budget {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
/// A catalog entry's identifier and display name, if the identifier is usable.
fn catalog_entry(model: &Value) -> Option<(String, String)> {
    let raw = model["id"]
        .as_str()
        .or(model["slug"].as_str())
        .or(model["name"].as_str())?;
    let id = raw.strip_prefix("models/").unwrap_or(raw);
    if id.is_empty() || id.len() > 200 || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let name = model["display_name"]
        .as_str()
        .or(model["displayName"].as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.len() <= 200 && !n.chars().any(char::is_control))
        .unwrap_or(id);
    Some((id.to_owned(), name.to_owned()))
}
/// Lists the provider's own model catalog for a connection, following documented pagination
/// within fixed bounds. Never changes the connection's configured models.
async fn discover_models(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let deadline = Instant::now() + CATALOG_DEADLINE.min(app.timeout);
    let mut connection = app
        .store
        .get::<Connection>("connection", &id)
        .ok_or(ApiError::new(404, "Connection not found"))?;
    if connection.kind == crate::antigravity::KIND {
        let catalog = tokio::time::timeout(
            catalog_remaining(deadline)?,
            crate::antigravity::catalog(&app, &mut connection),
        )
        .await
        .map_err(|_| ApiError::new(504, CATALOG_UNAVAILABLE))?
        .map_err(management_error)?;
        return Ok(Json(
            json!({"connection_id":id,"models":catalog.into_iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>()}),
        ));
    }
    tokio::time::timeout(
        catalog_remaining(deadline)?,
        credentials::refresh(&app, &mut connection),
    )
    .await
    .map_err(|_| {
        catalog_remaining(deadline)
            .err()
            .unwrap_or(ApiError::new(504, CATALOG_UNAVAILABLE))
    })?
    .map_err(management_error)?;

    let mut models = std::collections::BTreeMap::<String, String>::new();
    let mut seen_cursors = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    let mut used = 0usize;
    let mut adopted = false;
    let mut truncated: Option<&str> = None;
    for page_index in 0..CATALOG_PAGES {
        let bytes = match catalog_page(
            &app,
            &mut connection,
            cursor.as_deref(),
            deadline,
            CATALOG_BYTES - used,
            &mut adopted,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) if page_index == 0 => return Err(e),
            Err(_) => {
                truncated = Some(
                    "The provider's catalog could only be read in part. Add any missing models manually.",
                );
                break;
            }
        };
        used += bytes.len();
        let page: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) if page_index == 0 => {
                return Err(ApiError::upstream("Invalid provider model catalog"));
            }
            Err(_) => {
                truncated = Some(
                    "The provider's catalog could only be read in part. Add any missing models manually.",
                );
                break;
            }
        };
        let Some(entries) = page["data"]
            .as_array()
            .or_else(|| page["models"].as_array())
        else {
            if page_index == 0 {
                return Err(ApiError::upstream(
                    "Unsupported provider model catalog; enter model identifiers manually",
                ));
            }
            truncated = Some(
                "The provider's catalog could only be read in part. Add any missing models manually.",
            );
            break;
        };
        for (id, name) in entries.iter().filter_map(catalog_entry) {
            if models.len() >= CATALOG_MODELS && !models.contains_key(&id) {
                truncated = Some("Showing the first 1,000 models from the provider's catalog.");
                break;
            }
            models.insert(id, name);
        }
        if truncated.is_some() {
            break;
        }
        let next = catalog_cursor(&connection.kind, &page);
        match next {
            None => {
                // OpenAI-style lists may report more results without a documented cursor.
                if page["has_more"] == true && connection.kind != "anthropic" {
                    truncated = Some(
                        "The provider reported more models than it returned. Add any missing models manually.",
                    );
                } else if page["has_more"] == true || page["nextPageToken"].is_string() {
                    truncated = Some(
                        "The provider's catalog could only be read in part. Add any missing models manually.",
                    );
                }
                break;
            }
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    truncated = Some(
                        "The provider repeated a catalog page; showing the models read so far.",
                    );
                    break;
                }
                if page_index + 1 == CATALOG_PAGES {
                    truncated = Some(
                        "The provider's catalog has more pages than Switchyard reads. Add any missing models manually.",
                    );
                    break;
                }
                if catalog_remaining(deadline).is_err() {
                    truncated = Some(
                        "The provider's catalog took too long to read in full. Add any missing models manually.",
                    );
                    break;
                }
                cursor = Some(next);
            }
        }
    }
    let mut body = json!({
        "connection_id": connection.id,
        "models": models.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
    });
    if let Some(message) = truncated {
        body["truncated"] = json!(true);
        body["message"] = json!(message);
    }
    Ok(Json(body))
}
async fn request_detail(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = app
        .store
        .requests(1000)
        .into_iter()
        .find(|record| record.id == id)
        .ok_or(ApiError::new(
            404,
            "Request no longer retained in activity history",
        ))?;
    Ok(Json(json!(record)))
}
async fn import_credentials(
    State(app): State<App>,
    Json(v): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let source = v["source"]
        .as_str()
        .ok_or(ApiError::bad("Missing import source"))?;
    let cs = credentials::import(&app, source, v["path"].as_str()).await?;
    Ok(Json(
        json!({"imported":cs.len(),"connections":cs.iter().map(Connection::public).collect::<Vec<_>>(),"message":"Credentials imported locally. Original files were not changed."}),
    ))
}
async fn models(State(app): State<App>) -> Json<Value> {
    Json(json!(model_list(&app)))
}
fn model_list(app: &App) -> Vec<Value> {
    app.store.list::<Connection>("connection").iter().filter(|c|c.enabled).flat_map(|c|c.models.iter().map(|m|json!({"id":m,"connection_id":c.id,"connection_name":c.name,"kind":c.kind,"supports_websocket":c.supports_websocket}))).collect()
}
async fn client_models(State(app): State<App>) -> Json<Value> {
    let mut ms = std::collections::BTreeSet::new();
    for m in model_list(&app) {
        if let Some(s) = m["id"].as_str() {
            ms.insert(s.to_string());
        }
    }
    for r in app.store.list::<Route>("route") {
        ms.insert(r.model);
    }
    Json(
        json!({"object":"list","data":ms.into_iter().map(|m|json!({"id":m,"object":"model","owned_by":"switchyard"})).collect::<Vec<_>>()}),
    )
}
async fn routes(State(app): State<App>) -> Json<Value> {
    Json(json!(app.store.list::<Route>("route")))
}
async fn save_route(
    State(app): State<App>,
    Path(model): Path<String>,
    Json(v): Json<Value>,
) -> Result<Json<Route>, ApiError> {
    let r: Route = serde_json::from_value(
        json!({"model":model,"targets":v["targets"],"strategy":v["strategy"]}),
    )
    .map_err(|_| ApiError::bad("Invalid route"))?;
    if r.model.is_empty()
        || r.model.len() > 200
        || r.targets.is_empty()
        || r.targets.len() > 20
        || !["round_robin", "failover"].contains(&r.strategy.as_str())
    {
        return Err(ApiError::bad(
            "Route requires a model, 1–20 targets and a valid strategy",
        ));
    }
    for t in &r.targets {
        let c = app
            .store
            .get::<Connection>("connection", &t.connection_id)
            .ok_or(ApiError::bad("Route target connection not found"))?;
        if !c.models.contains(&t.model) {
            return Err(ApiError::bad(
                "Route target model is not configured on its connection",
            ));
        }
    }
    app.store.put("route", &r.model, &r).map_err(ApiError::db)?;
    Ok(Json(r))
}
async fn delete_route(
    State(app): State<App>,
    Path(model): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.store.delete("route", &model).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
struct RequestQuery {
    limit: Option<usize>,
    status: Option<String>,
    model: Option<String>,
}
async fn requests(State(app): State<App>, Query(q): Query<RequestQuery>) -> Json<Value> {
    Json(json!(
        app.store
            .requests(1000)
            .into_iter()
            .filter(|r| q.status.as_ref().is_none_or(|s| match s.as_str() {
                "error" => r.status >= 400,
                "success" => r.status < 400,
                _ => s.parse::<u16>().is_ok_and(|n| n == r.status),
            }))
            .filter(|r| q.model.as_ref().is_none_or(|m| r.model == *m))
            .take(q.limit.unwrap_or(100).min(1000))
            .collect::<Vec<_>>()
    ))
}
async fn events(State(app): State<App>, ws: WebSocketUpgrade) -> Response {
    let mut rx = app.events.subscribe();
    ws.on_upgrade(move|mut socket|async move{
        if socket.send(Message::Text(json!({"type":"overview","data":app.overview()}).to_string().into())).await.is_err(){return;}
        loop {tokio::select! {
            result=rx.recv()=>match result{Ok(v)=>if socket.send(Message::Text(v.to_string().into())).await.is_err(){break;},Err(broadcast::error::RecvError::Lagged(_))=>{if socket.send(Message::Text(json!({"type":"overview","data":app.overview()}).to_string().into())).await.is_err(){break;}},Err(_)=>break},
            msg=socket.recv()=>match msg{Some(Ok(Message::Ping(p)))=>if socket.send(Message::Pong(p)).await.is_err(){break;},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{}}
        }}
    })
}
async fn keys(State(app): State<App>) -> Json<Value> {
    Json(json!(
        app.store
            .list::<Value>("key")
            .into_iter()
            .map(|mut k| {
                k.as_object_mut().unwrap().remove("hash");
                k
            })
            .collect::<Vec<_>>()
    ))
}
async fn create_key(State(app): State<App>, Json(v): Json<Value>) -> Result<Json<Value>, ApiError> {
    let name = v["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 100)
        .ok_or(ApiError::bad("Key name must be 1–100 characters"))?;
    if app.store.list::<Value>("key").len() >= 100 {
        return Err(ApiError::bad("At most 100 client keys"));
    }
    let key = format!("sy_{}{}", id().replace('-', ""), id().replace('-', ""));
    let key_id = id();
    let mut v =
        json!({"id":key_id,"name":name,"prefix":&key[..11],"created_at":now(),"hash":hash(&key)});
    app.store.put("key", &key_id, &v).map_err(ApiError::db)?;
    v.as_object_mut().unwrap().remove("hash");
    v["key"] = json!(key);
    Ok(Json(v))
}
async fn delete_key(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.store.delete("key", &id).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn playground(State(app): State<App>, Json(v): Json<Value>) -> Result<Response, ApiError> {
    let model = v["model"].as_str().ok_or(ApiError::bad("Choose a model"))?;
    let input = v["input"].as_str().ok_or(ApiError::bad("Enter a prompt"))?;
    let stream = v["transport"] == "sse";
    let candidates = proxy::candidates(&app, model, false)?;
    let c = &candidates[0].0;
    let (endpoint, body) = match c.kind.as_str() {
        "anthropic" => (
            "messages",
            json!({"model":model,"max_tokens":1024,"messages":[{"role":"user","content":input}],"stream":stream}),
        ),
        "gemini" | "antigravity" => (
            "gemini",
            json!({"model":model,"contents":[{"role":"user","parts":[{"text":input}]}],"stream":stream}),
        ),
        _ => (
            "responses",
            json!({"model":model,"input":input,"stream":stream}),
        ),
    };
    let mut response = proxy::execute(app, endpoint, body, HeaderMap::new())
        .await
        .map_err(management_error)?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        *response.status_mut() = StatusCode::FAILED_DEPENDENCY;
    }
    Ok(response)
}
// Provider authentication is a dependency failure, distinct from the dashboard's own session.
fn management_error(mut error: ApiError) -> ApiError {
    if matches!(
        error.status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        error.status = StatusCode::FAILED_DEPENDENCY;
    }
    error
}
#[derive(rust_embed::RustEmbed)]
#[folder = "ui/dist/"]
struct Assets;
async fn static_asset(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path.starts_with("v1/") || path.starts_with("v1beta/") {
        return ApiError::new(404, "Endpoint not found").into_response();
    }
    let asset = Assets::get(if path.is_empty() { "index.html" } else { path }).or_else(|| {
        if !path.rsplit('/').next().unwrap_or("").contains('.') {
            Assets::get("index.html")
        } else {
            None
        }
    });
    match asset {
        Some(a) => {
            let mime = mime_guess::from_path(if path.is_empty() || !path.contains('.') {
                "index.html"
            } else {
                path
            })
            .first_or_octet_stream();
            let mut r = (
                [(header::CONTENT_TYPE, mime.to_string())],
                Body::from(a.data.into_owned()),
            )
                .into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static(if path.starts_with("assets/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                }),
            );
            r
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
