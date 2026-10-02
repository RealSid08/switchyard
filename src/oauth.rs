//! Independent browser sign-in (OAuth 2.0 authorization code with PKCE S256) for Codex and Claude.
//!
//! Accounts created here are owned by the gateway: their refresh tokens are not shared with any
//! native CLI, so the gateway may refresh them (see credentials::refresh).
//!
//! Endpoints, public client IDs, scopes and callback ports follow the official CLIs as documented
//! by CLIProxyAPI's `internal/auth/{codex,claude}` (MIT), used as the behavioral reference. The
//! callback ports are fixed because the providers only accept these registered redirect URIs.
use crate::{
    antigravity,
    app::{ApiError, App},
    credentials::{self, Parsed, SOURCE_OAUTH},
    store::Connection,
};
use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::watch};

pub(crate) struct Provider {
    key: &'static str,
    auth_url: &'static str,
    token_url: &'static str,
    profile_url: &'static str,
    client_id: &'static str,
    scope: &'static str,
    refresh_scope: &'static str,
    callback_path: &'static str,
    port: u16,
}
static CODEX: Provider = Provider {
    key: "codex",
    auth_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    profile_url: "",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scope: "openid email profile offline_access",
    refresh_scope: "openid profile email",
    callback_path: "/auth/callback",
    port: 1455,
};
static CLAUDE: Provider = Provider {
    key: "claude",
    auth_url: "https://claude.ai/oauth/authorize",
    token_url: "https://platform.claude.com/v1/oauth/token",
    profile_url: "https://api.anthropic.com/api/oauth/profile",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    scope: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    refresh_scope: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    callback_path: "/callback",
    port: 54545,
};
/// Google sign-in for Antigravity. The client id and secret are resolved at runtime
/// ([`antigravity::oauth_client`]); `client_id` here is unused.
static ANTIGRAVITY: Provider = Provider {
    key: "antigravity",
    auth_url: antigravity::AUTH_URL,
    token_url: antigravity::TOKEN_URL,
    profile_url: antigravity::USERINFO_URL,
    client_id: "",
    scope: antigravity::SCOPES,
    refresh_scope: "",
    callback_path: antigravity::CALLBACK_PATH,
    port: antigravity::CALLBACK_PORT,
};
fn provider(name: &str) -> Option<&'static Provider> {
    match name.trim().to_ascii_lowercase().as_str() {
        "codex" | "openai" | "chatgpt" => Some(&CODEX),
        "claude" | "anthropic" => Some(&CLAUDE),
        "antigravity" => Some(&ANTIGRAVITY),
        _ => None,
    }
}

const TTL: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 8;
const MAX_FLOWS: usize = 64;
const KEEP_FINISHED: Duration = Duration::from_secs(600);
/// Upper bound on completing a sign-in once the provider redirected back: token exchange (20 s),
/// profile lookup (10 s) and the account lock (25 s), with margin. Past it the flow expires.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(60);
const CANCELLED: &str = "Sign-in cancelled.";
const EXPIRED: &str = "Sign-in expired. Start again from the control room.";
const ENDED: &str = "This sign-in already ended. Start again from the control room.";
const SAVE_FAILED: &str = "Signed in, but the account could not be saved. Start again.";
const MAX_RESPONSE: usize = 256 * 1024;

#[derive(Clone, Default)]
struct Overrides {
    port: Option<u16>,
    token_url: Option<String>,
    profile_url: Option<String>,
}
static OVERRIDES: LazyLock<Mutex<HashMap<&'static str, Overrides>>> =
    LazyLock::new(Default::default);
static TTL_OVERRIDE: Mutex<Option<Duration>> = Mutex::new(None);

/// Test hook: point a provider at a local callback port and mock token/profile endpoints.
#[doc(hidden)]
pub fn set_test_endpoints(
    provider_name: &str,
    port: u16,
    token_url: &str,
    profile_url: Option<&str>,
) {
    if let Some(p) = provider(provider_name) {
        OVERRIDES.lock().expect("oauth overrides").insert(
            p.key,
            Overrides {
                port: Some(port),
                token_url: Some(token_url.into()),
                profile_url: profile_url.map(String::from),
            },
        );
    }
}
/// Test hook: shorten the authorization TTL and the exchange limit.
#[doc(hidden)]
pub fn set_test_ttl(ttl: Option<Duration>) {
    *TTL_OVERRIDE.lock().expect("oauth ttl") = ttl;
}
fn over(p: &Provider) -> Overrides {
    OVERRIDES
        .lock()
        .expect("oauth overrides")
        .get(p.key)
        .cloned()
        .unwrap_or_default()
}
fn port(p: &Provider) -> u16 {
    over(p).port.unwrap_or(p.port)
}
fn token_url(p: &Provider) -> String {
    over(p).token_url.unwrap_or_else(|| p.token_url.into())
}
fn profile_url(p: &Provider) -> String {
    over(p).profile_url.unwrap_or_else(|| p.profile_url.into())
}
/// The exchange deadline; a test TTL override shortens it too so expiry races are testable.
fn exchange_limit() -> Duration {
    TTL_OVERRIDE
        .lock()
        .expect("oauth ttl")
        .unwrap_or(EXCHANGE_LIMIT)
}
fn ttl() -> Duration {
    TTL_OVERRIDE.lock().expect("oauth ttl").unwrap_or(TTL)
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Pending,
    Exchanging,
    Complete,
    Error,
    Expired,
}
struct Flow {
    provider: &'static Provider,
    owner: std::sync::Weak<crate::app::AppState>,
    state: String,
    verifier: String,
    redirect_uri: String,
    expires: Instant,
    phase: Phase,
    connection: Option<Value>,
    message: Option<&'static str>,
    ended: Option<Instant>,
    stop: watch::Sender<bool>,
}
impl Flow {
    fn active(&self) -> bool {
        matches!(self.phase, Phase::Pending | Phase::Exchanging)
    }
    /// The answer for a callback that arrives after the flow ended: its final outcome, not a
    /// state mismatch. `Ok` only for a completed sign-in.
    fn final_outcome(&self) -> Result<(), Failure> {
        match self.phase {
            Phase::Complete => Ok(()),
            Phase::Expired => Err((410, self.message.unwrap_or(EXPIRED))),
            Phase::Error => Err((409, self.message.unwrap_or(ENDED))),
            Phase::Pending | Phase::Exchanging => Err((409, ENDED)),
        }
    }
    fn finish(&mut self, phase: Phase, message: Option<&'static str>) {
        self.phase = phase;
        self.message = message;
        self.ended = Some(Instant::now());
        // Wipe secrets as soon as they are no longer needed.
        self.verifier.clear();
        let _ = self.stop.send(true);
    }
}
static FLOWS: LazyLock<Mutex<HashMap<String, Flow>>> = LazyLock::new(Default::default);

fn prune(flows: &mut HashMap<String, Flow>) {
    let now = Instant::now();
    for f in flows.values_mut() {
        if f.active() && f.expires <= now {
            f.finish(Phase::Expired, Some(EXPIRED));
        }
    }
    flows.retain(|_, f| {
        f.ended
            .is_none_or(|t| now.duration_since(t) < KEEP_FINISHED)
    });
    while flows.len() > MAX_FLOWS {
        let Some(oldest) = flows
            .iter()
            .filter(|(_, f)| !f.active())
            .min_by_key(|(_, f)| f.ended)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        flows.remove(&oldest);
    }
}

fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

async fn bind(port: u16) -> Result<Vec<TcpListener>, ApiError> {
    // A superseded flow releases the port asynchronously; allow it a moment.
    let mut last = None;
    for _ in 0..25 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(v4) => {
                let mut ls = vec![v4];
                // Browsers may resolve localhost to ::1 first. IPv6 is best effort.
                if let Ok(v6) = TcpListener::bind(("::1", port)).await {
                    ls.push(v6);
                }
                return Ok(ls);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            Err(e) => {
                last = Some(e);
                break;
            }
        }
    }
    let busy = last.is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse);
    Err(ApiError::new(
        409,
        &if busy {
            format!(
                "Sign-in callback port {port} is already in use. Close any running codex login, claude login or CLIProxyAPI login, then retry."
            )
        } else {
            format!("Could not open the local sign-in callback on port {port}.")
        },
    ))
}

/// Starts a browser sign-in. Returns `{id, provider, authorization_url, expires_in_seconds,
/// status:"pending"}`. Open `authorization_url` in a browser on this machine.
pub async fn start(app: App, provider_name: &str) -> Result<Value, ApiError> {
    let p = provider(provider_name).ok_or(ApiError::bad("Choose codex, claude or antigravity"))?;
    // Antigravity needs the desktop app's OAuth client, resolved before anything is bound.
    let google_client = if p.key == ANTIGRAVITY.key {
        Some(antigravity::oauth_client().ok_or(ApiError::new(424, antigravity::CLIENT_MISSING))?)
    } else {
        None
    };
    {
        let mut flows = FLOWS.lock().expect("oauth flows");
        prune(&mut flows);
        // The callback port is fixed per provider, so a newer sign-in replaces an older one.
        for f in flows.values_mut().filter(|f| {
            f.provider.key == p.key
                && f.active()
                && f.owner.ptr_eq(&std::sync::Arc::downgrade(&app))
        }) {
            if f.phase == Phase::Exchanging {
                return Err(ApiError::new(
                    409,
                    "A sign-in for this provider is finishing. Check its status in a few seconds.",
                ));
            }
            f.finish(Phase::Error, Some("Replaced by a newer sign-in."));
        }
        if flows.values().filter(|f| f.active()).count() >= MAX_PENDING {
            return Err(ApiError::new(
                429,
                "Too many sign-ins in progress. Finish or wait for one to expire.",
            )
            .retry_after(30));
        }
    }
    let port = port(p);
    let listeners = bind(port).await?;
    let id = random_hex();
    let state = format!("{}{}", random_hex(), random_hex());
    let verifier = {
        let mut raw = Vec::with_capacity(48);
        for _ in 0..3 {
            raw.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        b64(&raw)
    };
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    let redirect_uri = format!("http://localhost:{port}{}", p.callback_path);
    let mut url = url::Url::parse(p.auth_url).expect("static authorization URL");
    {
        let mut q = url.query_pairs_mut();
        if p.key == "claude" {
            q.append_pair("code", "true");
        }
        q.append_pair(
            "client_id",
            google_client
                .as_ref()
                .map_or(p.client_id, |c| c.id.as_str()),
        )
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", p.scope)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
        if p.key == ANTIGRAVITY.key {
            q.append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
        if p.key == "codex" {
            q.append_pair("prompt", "login")
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true");
        }
    }
    let ttl = ttl();
    let (stop, stop_rx) = watch::channel(false);
    {
        let mut flows = FLOWS.lock().expect("oauth flows");
        flows.insert(
            id.clone(),
            Flow {
                provider: p,
                owner: std::sync::Arc::downgrade(&app),
                state,
                verifier,
                redirect_uri,
                expires: Instant::now() + ttl,
                phase: Phase::Pending,
                connection: None,
                message: None,
                ended: None,
                stop,
            },
        );
    }
    let router = Router::new()
        .route(p.callback_path, get(callback))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(CallbackState {
            app,
            id: id.clone(),
            port,
        });
    for l in listeners {
        let mut rx = stop_rx.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let _ = axum::serve(l, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|stopped| *stopped).await;
                })
                .await;
        });
    }
    // Expires the flow at its deadline, which an exchange may extend (see EXCHANGE_LIMIT).
    // Expiry signals `stop`, which also aborts an exchange still in flight.
    let flow_id = id.clone();
    let mut rx = stop_rx;
    tokio::spawn(async move {
        loop {
            let deadline = {
                let flows = FLOWS.lock().expect("oauth flows");
                match flows.get(&flow_id) {
                    Some(f) if f.active() => f.expires,
                    _ => return,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.into()) => {}
                _ = rx.wait_for(|stopped| *stopped) => return,
            }
            let mut flows = FLOWS.lock().expect("oauth flows");
            if let Some(f) = flows.get_mut(&flow_id)
                && f.active()
                && f.expires <= Instant::now()
            {
                f.finish(Phase::Expired, Some(EXPIRED));
                return;
            }
        }
    });
    tracing::info!(provider = p.key, "Browser sign-in started");
    Ok(json!({
        "id": id,
        "provider": p.key,
        "authorization_url": url.to_string(),
        "expires_in_seconds": ttl.as_secs(),
        "status": "pending",
    }))
}

/// Reports a sign-in by id. Never returns codes or tokens.
pub fn status(app: &App, id: &str) -> Result<Value, ApiError> {
    let mut flows = FLOWS.lock().expect("oauth flows");
    prune(&mut flows);
    let f = flows.get(id).ok_or(ApiError::new(
        404,
        "Sign-in not found. It may have expired; start again.",
    ))?;
    if !f.owner.ptr_eq(&std::sync::Arc::downgrade(app)) {
        return Err(ApiError::new(404, "Sign-in not found"));
    }
    let mut v = json!({
        "id": id,
        "provider": f.provider.key,
        "status": match f.phase {
            Phase::Pending | Phase::Exchanging => "pending",
            Phase::Complete => "complete",
            Phase::Error => "error",
            Phase::Expired => "expired",
        },
    });
    if f.phase == Phase::Pending {
        v["expires_in_seconds"] = json!(
            f.expires
                .saturating_duration_since(Instant::now())
                .as_secs()
        );
    }
    if let Some(c) = &f.connection {
        v["connection"] = c.clone();
    }
    if let Some(m) = f.message {
        v["message"] = json!(m);
    }
    Ok(v)
}

/// Cancels a sign-in that has not finished, including one whose token exchange or account save
/// is in flight, and releases its callback port. Linearizable with completion: if the account was
/// already saved the sign-in stays complete; otherwise nothing is saved afterwards.
pub fn cancel(app: &App, id: &str) -> Result<Value, ApiError> {
    status(app, id)?;
    {
        let mut flows = FLOWS.lock().expect("oauth flows");
        if let Some(f) = flows.get_mut(id)
            && f.active()
        {
            f.finish(Phase::Error, Some(CANCELLED));
        }
    }
    status(app, id)
}

/// Completes a sign-in from a callback URL pasted by the user. This supports a gateway running
/// on another machine, where the browser's redirect to localhost cannot reach the gateway. Also
/// accepts Claude's manual `code#state` form.
pub async fn submit_callback(app: &App, id: &str, input: &str) -> Result<Value, ApiError> {
    let input = input.trim();
    if input.is_empty() || input.len() > 8192 {
        return Err(ApiError::bad(
            "Paste the full callback URL from the browser address bar",
        ));
    }
    let mut params = HashMap::new();
    if let Ok(u) = url::Url::parse(input) {
        for (k, v) in u.query_pairs() {
            if ["code", "state", "error"].contains(&k.as_ref()) {
                params.insert(k.into_owned(), v.into_owned());
            }
        }
    } else if let Some((code, state)) = input.split_once('#') {
        params.insert("code".into(), code.into());
        params.insert("state".into(), state.into());
    } else {
        return Err(ApiError::bad(
            "Paste the full callback URL from the browser address bar",
        ));
    }
    complete(app, id, &params)
        .await
        .map_err(|(code, msg)| ApiError::new(code, msg))?;
    status(app, id)
}

#[derive(Clone)]
struct CallbackState {
    app: App,
    id: String,
    port: u16,
}
async fn callback(
    State(s): State<CallbackState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let allowed = ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .any(|name| host == format!("{name}:{}", s.port));
    if !allowed {
        return page(
            StatusCode::BAD_REQUEST,
            "Sign-in not completed",
            "This callback only accepts loopback requests.",
        );
    }
    match complete(&s.app, &s.id, &q).await {
        Ok(()) => page(
            StatusCode::OK,
            "Signed in",
            "Your account is connected. You can close this tab and return to the Switchyard control room.",
        ),
        Err((code, msg)) => page(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            "Sign-in not completed",
            msg,
        ),
    }
}

type Failure = (u16, &'static str);

async fn complete(app: &App, id: &str, q: &HashMap<String, String>) -> Result<(), Failure> {
    const GONE: Failure = (
        404,
        "This sign-in is no longer active. Start again from the control room.",
    );
    const MISMATCH: Failure = (
        400,
        "This sign-in response does not match. Start again from the control room.",
    );
    let raw_code = q.get("code").map(String::as_str).unwrap_or("");
    // Claude may append "#state" to the code.
    let (code, fragment_state) = match raw_code.split_once('#') {
        Some((c, s)) => (c, Some(s)),
        None => (raw_code, None),
    };
    let state = q
        .get("state")
        .map(String::as_str)
        .or(fragment_state)
        .unwrap_or("");
    let (p, verifier, redirect_uri, expected, mut stopped) = {
        let mut flows = FLOWS.lock().expect("oauth flows");
        prune(&mut flows);
        let f = flows.get_mut(id).ok_or(GONE)?;
        if !f.owner.ptr_eq(&std::sync::Arc::downgrade(app)) {
            return Err(GONE);
        }
        // An ended sign-in reports how it ended (it holds no secrets and accepts nothing).
        if !f.active() {
            return f.final_outcome();
        }
        let matches = |s: &str| !s.is_empty() && bool::from(s.as_bytes().ct_eq(f.state.as_bytes()));
        if !matches(state) || fragment_state.is_some_and(|s| !matches(s)) {
            return Err(MISMATCH);
        }
        if f.phase == Phase::Exchanging {
            return Err((409, "This sign-in is already being completed."));
        }
        if q.contains_key("error") {
            f.finish(
                Phase::Error,
                Some("Sign-in was cancelled or denied at the provider."),
            );
            return Err((400, "Sign-in was cancelled or denied at the provider."));
        }
        if code.is_empty() || code.len() > 4096 || !code.bytes().all(|b| b.is_ascii_graphic()) {
            f.finish(
                Phase::Error,
                Some("The provider returned an invalid sign-in response."),
            );
            return Err((400, "The provider returned an invalid sign-in response."));
        }
        f.phase = Phase::Exchanging;
        f.expires = f.expires.max(Instant::now() + exchange_limit());
        (
            f.provider,
            std::mem::take(&mut f.verifier),
            f.redirect_uri.clone(),
            f.state.clone(),
            f.stop.subscribe(),
        )
    };
    // Exchange and save, abandoned as soon as the flow is cancelled, replaced or expires: the
    // token request and any account-lock wait are dropped rather than run to completion.
    let work = async {
        let parsed = exchange(app, p, code, &verifier, &redirect_uri, &expected, id).await?;
        credentials::upsert_gated(app, parsed, |write| commit(id, write))
            .await
            .map_err(|_| SAVE_FAILED)
    };
    let outcome = tokio::select! {
        biased;
        result = work => Some(result),
        _ = stopped.wait_for(|stopped| *stopped) => None,
    };
    if let Some(Ok(_)) = outcome {
        tracing::info!(provider = p.key, "Browser sign-in completed");
        return Ok(());
    }
    let mut flows = FLOWS.lock().expect("oauth flows");
    let Some(f) = flows.get_mut(id) else {
        return Err(GONE);
    };
    match outcome {
        // Still ours to end: the exchange or save failed.
        Some(Err(message)) if f.phase == Phase::Exchanging => {
            f.finish(Phase::Error, Some(message));
            Err((502, message))
        }
        // Cancelled, replaced, expired, or (in a lost race) completed: report the final outcome.
        _ => f.final_outcome(),
    }
}

/// Commit point of a browser sign-in. Runs the account write only while the flow is still
/// exchanging, and marks it complete under the same lock, so completion and cancellation or
/// expiry are mutually exclusive. The store write is synchronous; nothing awaits under the lock.
fn commit(id: &str, write: credentials::Write<'_>) -> Result<Connection, ApiError> {
    let mut flows = FLOWS.lock().expect("oauth flows");
    let Some(f) = flows.get_mut(id).filter(|f| f.phase == Phase::Exchanging) else {
        return Err(ApiError::new(409, ENDED));
    };
    let c = write()?;
    f.connection = Some(c.public());
    f.finish(Phase::Complete, None);
    Ok(c)
}

const UNREACHABLE: &str =
    "Could not reach the provider sign-in service. Check the network and start again.";
const REJECTED: &str = "The provider rejected this sign-in. Start again from the control room.";
const BAD_RESPONSE: &str = "The provider returned an unexpected sign-in response. Start again.";
const UNCONFIGURED: &str = antigravity::CLIENT_MISSING;

async fn exchange(
    app: &App,
    p: &'static Provider,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: &str,
    flow_id: &str,
) -> Result<Parsed, &'static str> {
    let req = app
        .client
        .post(token_url(p))
        .timeout(credentials::token_timeout())
        .header(header::ACCEPT, "application/json");
    let google_client = if p.key == ANTIGRAVITY.key {
        Some(antigravity::oauth_client().ok_or(UNCONFIGURED)?)
    } else {
        None
    };
    let req = if let Some(client) = &google_client {
        req.form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client.id.as_str()),
            ("client_secret", client.secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
    } else if p.key == "codex" {
        req.form(&[
            ("grant_type", "authorization_code"),
            ("client_id", p.client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
    } else {
        req.json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": p.client_id,
            "code_verifier": verifier,
            "state": state,
        }))
    };
    let res = req.send().await.map_err(|_| UNREACHABLE)?;
    if !res.status().is_success() {
        return Err(if res.status().is_client_error() {
            REJECTED
        } else {
            UNREACHABLE
        });
    }
    let v = credentials::json_bounded(res, MAX_RESPONSE)
        .await
        .ok_or(BAD_RESPONSE)?;
    let access = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(BAD_RESPONSE)?;
    let refresh = v["refresh_token"].as_str().unwrap_or("");
    if p.key == ANTIGRAVITY.key {
        if refresh.is_empty() {
            // Without offline access the account could not be renewed; refuse rather than save
            // an account that stops working within the hour.
            return Err(BAD_RESPONSE);
        }
        let email = antigravity::fetch_email(app, access)
            .await
            .unwrap_or_default();
        let project = antigravity::discover_project(app, access)
            .await
            .unwrap_or_default();
        let models = antigravity::fetch_models(app, access, &project)
            .await
            .map(|m| m.into_iter().map(|(id, _)| id).collect())
            .unwrap_or_else(antigravity::default_models);
        return Ok(Parsed {
            name: if email.is_empty() {
                "Antigravity account".into()
            } else {
                email.clone()
            },
            kind: antigravity::KIND,
            base: antigravity::DAILY_BASE,
            token: access.into(),
            refresh_token: refresh.into(),
            expires_at: credentials::expiry_from_response(&v, access),
            account_id: project,
            identity: if email.is_empty() {
                crate::store::hash(&format!("v1|antigravity|oauth|{flow_id}"))
            } else {
                antigravity::identity_for_email(&email)
            },
            source: SOURCE_OAUTH,
            source_path: String::new(),
            oauth: true,
            models,
        });
    }
    let mut parsed = if p.key == "codex" {
        credentials::codex_from_tokens(
            &json!({"refresh_token": refresh, "id_token": v["id_token"]}),
            access,
            SOURCE_OAUTH,
            // Only seeds identity when the tokens carry none; never collides across sign-ins.
            &format!("oauth:{flow_id}"),
        )
    } else {
        let mut uuid = v["account"]["uuid"].as_str().unwrap_or("").to_string();
        let mut email = v["account"]["email_address"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if uuid.is_empty()
            && !profile_url(p).is_empty()
            && let Ok(r) = app
                .client
                .get(profile_url(p))
                .bearer_auth(access)
                .header("anthropic-beta", "oauth-2025-04-20")
                .timeout(Duration::from_secs(10))
                .send()
                .await
            && r.status().is_success()
            && let Some(pv) = credentials::json_bounded(r, MAX_RESPONSE).await
        {
            uuid = pv["account"]["uuid"].as_str().unwrap_or("").into();
            if email.is_empty() {
                email = pv["account"]["email"].as_str().unwrap_or("").into();
            }
        }
        Parsed {
            name: if email.is_empty() {
                "Claude account".into()
            } else {
                email
            },
            kind: "anthropic",
            base: "https://api.anthropic.com/v1",
            token: access.into(),
            refresh_token: refresh.into(),
            expires_at: 0,
            account_id: String::new(),
            identity: if uuid.is_empty() {
                crate::store::hash(&format!("v1|claude|oauth|{flow_id}"))
            } else {
                credentials::claude_identity(&uuid)
            },
            source: SOURCE_OAUTH,
            source_path: String::new(),
            oauth: true,
            models: credentials::claude_models(),
        }
    };
    parsed.expires_at = credentials::expiry_from_response(&v, access);
    parsed.source_path = String::new();
    if parsed.name == "Codex" {
        parsed.name = "Codex account".into();
    }
    Ok(parsed)
}

/// New tokens from a gateway-owned refresh.
pub(crate) struct Tokens {
    pub token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
}

/// Refreshes a gateway-owned token family. Errors are bounded and never echo provider bodies.
pub(crate) async fn refresh_tokens(
    app: &App,
    kind: &str,
    refresh_token: &str,
) -> Result<Tokens, ApiError> {
    let p: &Provider = match kind {
        "codex" => &CODEX,
        "anthropic" => &CLAUDE,
        antigravity::KIND => &ANTIGRAVITY,
        _ => {
            return Err(ApiError::new(
                401,
                "This account cannot be refreshed. Sign in again.",
            ));
        }
    };
    let req = app
        .client
        .post(token_url(p))
        .timeout(credentials::token_timeout())
        .header(header::ACCEPT, "application/json");
    let req = if p.key == ANTIGRAVITY.key {
        let client =
            antigravity::oauth_client().ok_or(ApiError::new(401, antigravity::CLIENT_MISSING))?;
        req.form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client.id.as_str()),
            ("client_secret", client.secret.as_str()),
        ])
    } else if p.key == "codex" {
        req.form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", p.client_id),
            ("scope", p.refresh_scope),
        ])
    } else {
        req.json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": p.client_id,
            "scope": p.refresh_scope,
        }))
    };
    let res = req.send().await.map_err(|_| {
        ApiError::new(
            503,
            "Could not reach the provider sign-in service. Retry shortly.",
        )
        .retry_after(10)
    })?;
    let status = res.status().as_u16();
    if matches!(status, 400 | 401 | 403) {
        return Err(ApiError::new(
            401,
            "This account's sign-in was revoked or expired. Sign in again from the control room.",
        ));
    }
    if !res.status().is_success() {
        return Err(ApiError::new(
            503,
            "The provider sign-in service is unavailable. Retry shortly.",
        )
        .retry_after(if status == 429 { 60 } else { 10 }));
    }
    let v = credentials::json_bounded(res, MAX_RESPONSE)
        .await
        .ok_or(ApiError::upstream(
            "The provider returned an unexpected refresh response.",
        ))?;
    let token = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(ApiError::upstream(
            "The provider returned an unexpected refresh response.",
        ))?;
    let account_id = credentials::jwt_claims(v["id_token"].as_str().unwrap_or(""))
        .or_else(|| credentials::jwt_claims(token))
        .and_then(|c| {
            c["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default();
    Ok(Tokens {
        token: token.into(),
        refresh_token: v["refresh_token"].as_str().unwrap_or("").into(),
        expires_at: credentials::expiry_from_response(&v, token),
        account_id: if p.key == "codex" {
            account_id
        } else {
            String::new()
        },
    })
}

fn page(status: StatusCode, title: &str, message: &str) -> Response {
    // Static text only: nothing from the request is reflected.
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Switchyard: {title}</title><style>body{{font:16px/1.5 system-ui,sans-serif;background:#0b0d10;color:#e7eaee;display:grid;place-items:center;min-height:100vh;margin:0}}main{{max-width:28rem;padding:2rem;border:1px solid #2a2f36;border-radius:12px;background:#12161b}}h1{{font-size:1.25rem;margin:0 0 .5rem}}p{{margin:0;color:#aab2bd}}</style></head><body><main><h1>{title}</h1><p>{message}</p></main></body></html>"
    );
    let mut r = (status, html).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    h.insert(
        "referrer-policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        "content-security-policy",
        header::HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    r
}
