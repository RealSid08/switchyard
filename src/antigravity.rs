//! Antigravity (Google Cloud Code `v1internal`) accounts and protocol.
//!
//! What lives here:
//! - Resolution of the Antigravity desktop app's Google OAuth client (never embedded; see
//!   [`oauth_client`]), account identity and Cloud Code project discovery for sign-in.
//! - The `v1internal:{generateContent,streamGenerateContent}` envelope around a Gemini
//!   `GenerateContentRequest`, bounded unwrapping of JSON and SSE responses, and request headers.
//! - Translation between that Gemini dialect and the client protocols Switchyard serves
//!   ([`translate`]: OpenAI Chat Completions and Anthropic Messages), plus tool-schema cleaning
//!   for Claude models ([`schema`]).
//! - The per-model quota helper ([`fetch_quota`]) used by the usage collector.
//!
//! Protocol shapes (endpoints, field names, client metadata, the Hub user agent family, the
//! `VALIDATED` tool mode and thought-signature placement) follow two MIT-licensed references:
//! CLIProxyAPI (`internal/auth/antigravity`, `internal/runtime/executor/antigravity_*`,
//! `internal/translator/antigravity/*`, Copyright (c) Luis Pater) and CodexBar
//! (`Sources/CodexBarCore/Providers/Antigravity`, Copyright (c) 2026 Peter Steinberger). This is an
//! independent implementation; no code is copied. The endpoints are unofficial and can change.
use crate::{
    app::{ApiError, App},
    store::{Connection, hash},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

pub mod schema;
pub mod translate;

pub const KIND: &str = "antigravity";
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
pub const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
pub const CALLBACK_PORT: u16 = 51121;
pub const CALLBACK_PATH: &str = "/oauth-callback";
/// Control plane (project discovery, models, quota).
pub const PROD_BASE: &str = "https://cloudcode-pa.googleapis.com";
/// Inference and onboarding for consumer accounts, as used by the native client.
pub const DAILY_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// Antigravity Hub version advertised in the user agent (the Hub client family is required).
const HUB_VERSION: &str = "2.9.1";
/// Largest JSON body read from control-plane calls.
const CONTROL_MAX: usize = 1024 * 1024;
/// Largest single provider event or non-streaming response (matches the gateway's 16 MiB bound).
pub const EVENT_MAX: usize = 16 * 1024 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// Total budget for project discovery during a sign-in or first use (onboarding included).
const PROJECT_BUDGET: Duration = Duration::from_secs(25);

/// Suggested models for a new account before its live catalog is read. Editable starting points
/// (from CLIProxyAPI's registry), never an availability guarantee.
pub fn default_models() -> Vec<String> {
    [
        "gemini-3.1-pro-low",
        "gemini-pro-agent",
        "gemini-3-flash",
        "gemini-3.8-flash-high",
        "gemini-3.5-flash-lite",
        "claude-sonnet-4-6",
        "claude-opus-4-6-thinking",
        "gpt-oss-120b-medium",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// True for Anthropic models served through Antigravity, which need `VALIDATED` tool calling,
/// cleaned tool schemas and real (never dummy) thinking signatures.
pub fn is_claude_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

// ---------------------------------------------------------------------------------------------
// Test hooks
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct TestOverrides {
    client: Option<OAuthClient>,
    base: Option<String>,
}
static OVERRIDES: LazyLock<Mutex<TestOverrides>> = LazyLock::new(Default::default);

/// Test hook: a fixed OAuth client instead of the environment or installed app.
#[doc(hidden)]
pub fn set_test_client(client: Option<(&str, &str)>) {
    OVERRIDES.lock().expect("antigravity overrides").client =
        client.map(|(id, secret)| OAuthClient {
            id: id.into(),
            secret: secret.into(),
        });
}
/// Test hook: serve the Cloud Code control plane and userinfo from `base` (`{base}/userinfo`).
#[doc(hidden)]
pub fn set_test_base(base: Option<&str>) {
    OVERRIDES.lock().expect("antigravity overrides").base =
        base.map(|b| b.trim_end_matches('/').to_string());
}
fn test_base() -> Option<String> {
    OVERRIDES
        .lock()
        .expect("antigravity overrides")
        .base
        .clone()
}
fn control_base() -> String {
    test_base().unwrap_or_else(|| PROD_BASE.into())
}
fn onboard_base() -> String {
    test_base().unwrap_or_else(|| DAILY_BASE.into())
}
fn userinfo_url() -> String {
    test_base().map_or_else(|| USERINFO_URL.into(), |b| format!("{b}/userinfo"))
}

// ---------------------------------------------------------------------------------------------
// OAuth client
// ---------------------------------------------------------------------------------------------

/// The Antigravity desktop app's installed-application OAuth client. Google documents that such
/// client secrets are not confidential, but Switchyard still never ships one: it is supplied by
/// `SWITCHYARD_ANTIGRAVITY_CLIENT_ID` / `SWITCHYARD_ANTIGRAVITY_CLIENT_SECRET`, or read from an
/// installed `Antigravity.app` (the approach CodexBar takes).
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub id: String,
    pub secret: String,
}
impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("id", &self.id)
            .field("secret", &"[redacted]")
            .finish()
    }
}

pub const CLIENT_MISSING: &str = "Antigravity sign-in needs the Antigravity app's OAuth client. Install Antigravity, or set SWITCHYARD_ANTIGRAVITY_CLIENT_ID and SWITCHYARD_ANTIGRAVITY_CLIENT_SECRET for the Switchyard process. Importing an existing Antigravity login works without it.";

/// The OAuth client to use, if one is configured or installed.
pub fn oauth_client() -> Option<OAuthClient> {
    if let Some(c) = OVERRIDES
        .lock()
        .expect("antigravity overrides")
        .client
        .clone()
    {
        return Some(c);
    }
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let (Some(id), Some(secret)) = (
        env("SWITCHYARD_ANTIGRAVITY_CLIENT_ID"),
        env("SWITCHYARD_ANTIGRAVITY_CLIENT_SECRET"),
    ) {
        return Some(OAuthClient { id, secret });
    }
    static INSTALLED: LazyLock<Option<OAuthClient>> = LazyLock::new(discover_installed_client);
    INSTALLED.clone()
}

/// Reads the client from an installed Antigravity app bundle (macOS). Only the app's own
/// JavaScript bundle is read, bounded, and only the client id and secret are extracted.
fn discover_installed_client() -> Option<OAuthClient> {
    let mut roots = vec![std::path::PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(std::path::PathBuf::from(home).join("Applications"));
    }
    for root in roots {
        let file = root.join("Antigravity.app/Contents/Resources/app/out/main.js");
        let Ok(meta) = std::fs::metadata(&file) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 128 * 1024 * 1024 {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&file)
            && let Some(client) = client_from_bundle(&bytes)
        {
            return Some(client);
        }
    }
    None
}

/// Extracts `<digits>-<id>.apps.googleusercontent.com` and the `GOCSPX-` secret that follow the
/// app's `oauthClient` module marker.
pub fn client_from_bundle(bytes: &[u8]) -> Option<OAuthClient> {
    const MARKER: &[u8] = b"vs/platform/cloudCode/common/oauthClient";
    let start = find(bytes, MARKER).unwrap_or(0);
    let window = &bytes[start..bytes.len().min(start + 8192)];
    let id = {
        let suffix = b".apps.googleusercontent.com";
        let end = find(window, suffix)?;
        let mut begin = end;
        while begin > 0
            && (window[begin - 1].is_ascii_alphanumeric()
                || matches!(window[begin - 1], b'-' | b'_'))
        {
            begin -= 1;
        }
        let id = std::str::from_utf8(&window[begin..end + suffix.len()]).ok()?;
        (id.split('-').next()?.bytes().all(|b| b.is_ascii_digit()) && id.contains('-'))
            .then(|| id.to_string())?
    };
    let secret = {
        let begin = find(window, b"GOCSPX-")?;
        let mut end = begin + 7;
        while end < window.len()
            && (window[end].is_ascii_alphanumeric() || matches!(window[end], b'-' | b'_'))
        {
            end += 1;
        }
        let s = std::str::from_utf8(&window[begin..end]).ok()?;
        (s.len() > 12).then(|| s.to_string())?
    };
    Some(OAuthClient { id, secret })
}
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------------------------

/// The Antigravity Hub user agent with this machine's OS and architecture. Cloud Code requires
/// the Hub client family for these endpoints (see CodexBar's note on the same requirement).
pub fn user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    };
    let arch = if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "amd64"
    };
    format!("antigravity/hub/{HUB_VERSION} {os}/{arch}")
}

/// Upstream headers for an Antigravity request. Never forwards client headers.
pub fn apply_headers(req: reqwest::RequestBuilder, access_token: &str) -> reqwest::RequestBuilder {
    req.bearer_auth(access_token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, user_agent())
}

/// Reads at most `max` bytes of a JSON body before `deadline`.
async fn read_json(res: reqwest::Response, max: usize, deadline: Instant) -> Option<Value> {
    if res.content_length().is_some_and(|n| n as usize > max) {
        return None;
    }
    let mut res = res;
    let mut buf = Vec::new();
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(remaining, res.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                if buf.len() + chunk.len() > max {
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => break,
            _ => return None,
        }
    }
    serde_json::from_slice(&buf).ok()
}

/// Outcome of a control-plane call: HTTP status (0 when no response) and the parsed body.
struct Control {
    status: u16,
    body: Option<Value>,
}

async fn control_post(
    app: &App,
    url: &str,
    access: &str,
    body: &Value,
    deadline: Instant,
) -> Control {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Control {
            status: 0,
            body: None,
        };
    };
    let req = apply_headers(app.client.post(url), access)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(remaining.min(CONTROL_TIMEOUT))
        .json(body);
    match req.send().await {
        Ok(res) => {
            let status = res.status().as_u16();
            let body = read_json(res, CONTROL_MAX, deadline).await;
            Control { status, body }
        }
        Err(_) => Control {
            status: 0,
            body: None,
        },
    }
}

fn client_metadata() -> Value {
    json!({"ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI"})
}

// ---------------------------------------------------------------------------------------------
// Account identity and project discovery
// ---------------------------------------------------------------------------------------------

/// Stable account identity shared by sign-in and every import: the Google account email
/// (lowercased). Two sources for the same Google account therefore update one connection.
pub fn identity_for_email(email: &str) -> String {
    hash(&format!(
        "v1|antigravity|email|{}",
        email.trim().to_lowercase()
    ))
}
/// Identity when the email is unknown: the source record itself, never shared.
pub fn identity_for_source(source: &str) -> String {
    hash(&format!("v1|antigravity|source|{source}"))
}

/// Email of the signed-in Google account, from the userinfo endpoint.
pub(crate) async fn fetch_email(app: &App, access: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let res = apply_headers(app.client.get(userinfo_url()), access)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let v = read_json(res, 64 * 1024, deadline).await?;
    v["email"]
        .as_str()
        .map(str::trim)
        .filter(|e| e.contains('@'))
        .map(String::from)
}

fn project_from(v: &Value) -> Option<String> {
    ["cloudaicompanionProject", "projectId", "project"]
        .iter()
        .find_map(|k| {
            let p = &v[*k];
            p.as_str()
                .or_else(|| p["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
                .map(String::from)
        })
}
fn default_tier(v: &Value) -> String {
    v["allowedTiers"]
        .as_array()
        .and_then(|tiers| tiers.iter().find(|t| t["isDefault"] == true))
        .and_then(|t| t["id"].as_str())
        .or_else(|| v["currentTier"]["id"].as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("free-tier")
        .to_string()
}

/// Discovers the account's Cloud Code project: `loadCodeAssist`, then `onboardUser` polled
/// until done, within [`PROJECT_BUDGET`]. `None` when it cannot be determined in time; requests
/// retry discovery later.
pub(crate) async fn discover_project(app: &App, access: &str) -> Option<String> {
    let deadline = Instant::now() + PROJECT_BUDGET;
    let load = control_post(
        app,
        &format!("{}/v1internal:loadCodeAssist", control_base()),
        access,
        &json!({"metadata": client_metadata()}),
        deadline,
    )
    .await;
    let load_body = load.body.filter(|_| (200..300).contains(&load.status))?;
    if let Some(project) = project_from(&load_body) {
        return Some(project);
    }
    let tier = default_tier(&load_body);
    for _ in 0..5 {
        let onboard = control_post(
            app,
            &format!("{}/v1internal:onboardUser", onboard_base()),
            access,
            &json!({"tier_id": tier, "metadata": {"ide_type": "ANTIGRAVITY", "ide_name": "antigravity", "ide_version": HUB_VERSION}}),
            deadline,
        )
        .await;
        let body = onboard.body.filter(|_| onboard.status == 200)?;
        if body["done"] == true {
            return project_from(&body["response"]);
        }
        let pause = Duration::from_secs(2);
        if deadline
            .checked_duration_since(Instant::now())
            .is_none_or(|r| r <= pause)
        {
            return None;
        }
        tokio::time::sleep(pause).await;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Models and quota
// ---------------------------------------------------------------------------------------------

/// Model ids that the catalog marks as internal (tab completion, experiments) and that do not
/// serve chat requests; from CLIProxyAPI's model fetcher.
const INTERNAL_MODELS: &[&str] = &[
    "chat_20706",
    "chat_23310",
    "tab_flash_lite_preview",
    "tab_jump_flash_lite_preview",
];

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.chars().any(|c| c.is_control() || c.is_whitespace())
        && !INTERNAL_MODELS.contains(&id)
}

/// `(id, display name)` pairs from a `fetchAvailableModels` body, sorted by id.
pub fn parse_models(v: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = v["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, _)| valid_model_id(id))
        .take(1000)
        .map(|(id, m)| {
            let name = m["displayName"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty() && n.len() <= 200 && !n.chars().any(char::is_control))
                .unwrap_or(id);
            (id.clone(), name.to_string())
        })
        .collect();
    out.sort();
    out
}

/// Remaining quota for one model. `remaining_fraction` is 0.0 to 1.0 when the provider reports
/// it; `reset_at` is Unix seconds. Per model and per account only: never sum across either.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ModelQuota {
    pub model: String,
    pub label: String,
    pub remaining_fraction: Option<f64>,
    pub reset_at: Option<i64>,
}

fn fraction(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|f| f.is_finite())
        .map(|f| f.clamp(0.0, 1.0))
}
fn reset(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|t| t.timestamp())
}

/// Per-model quota from a `fetchAvailableModels` body (`models.<id>.quotaInfo`).
pub fn parse_quota(v: &Value) -> Vec<ModelQuota> {
    let mut out: Vec<ModelQuota> = v["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, m)| valid_model_id(id) && m["quotaInfo"].is_object())
        .take(1000)
        .map(|(id, m)| ModelQuota {
            model: id.clone(),
            label: m["displayName"]
                .as_str()
                .or(m["label"].as_str())
                .map(str::trim)
                .filter(|n| !n.is_empty() && n.len() <= 200)
                .unwrap_or(id)
                .to_string(),
            remaining_fraction: fraction(&m["quotaInfo"]["remainingFraction"]),
            reset_at: reset(&m["quotaInfo"]["resetTime"]),
        })
        .collect();
    out.sort_by(|a, b| a.model.cmp(&b.model));
    out
}

/// Per-model quota from a `retrieveUserQuota` body (`buckets[]`). Several buckets for one model
/// keep the most constrained (lowest remaining) one.
pub fn parse_quota_buckets(v: &Value) -> Vec<ModelQuota> {
    let mut by_model: std::collections::BTreeMap<String, ModelQuota> = Default::default();
    for b in v["buckets"].as_array().into_iter().flatten().take(1000) {
        let Some(model) = b["modelId"]
            .as_str()
            .map(str::trim)
            .filter(|m| valid_model_id(m))
        else {
            continue;
        };
        let next = ModelQuota {
            model: model.into(),
            label: model.into(),
            remaining_fraction: fraction(&b["remainingFraction"]),
            reset_at: reset(&b["resetTime"]),
        };
        match by_model.get(model) {
            Some(existing)
                if existing.remaining_fraction.unwrap_or(f64::MAX)
                    <= next.remaining_fraction.unwrap_or(f64::MAX) => {}
            _ => {
                by_model.insert(model.into(), next);
            }
        }
    }
    by_model.into_values().collect()
}

/// Reads the account's per-model quota. Renews the token first through
/// [`crate::credentials::refresh`] (imported tokens are only re-read from their source, never
/// rotated), discovers the project if it is not stored, and falls back to `retrieveUserQuota`
/// when the model catalog carries no quota. Errors carry constant messages only.
pub async fn fetch_quota(
    app: &App,
    connection: &mut Connection,
) -> Result<Vec<ModelQuota>, ApiError> {
    if connection.kind != KIND {
        return Err(ApiError::bad("Not an Antigravity connection"));
    }
    crate::credentials::refresh(app, connection).await?;
    let project = if connection.account_id.is_empty() {
        discover_project(app, &connection.api_key)
            .await
            .unwrap_or_default()
    } else {
        connection.account_id.clone()
    };
    let body = if project.is_empty() {
        json!({})
    } else {
        json!({"project": project})
    };
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    let models = control_post(
        app,
        &format!("{}/v1internal:fetchAvailableModels", control_base()),
        &connection.api_key,
        &body,
        deadline,
    )
    .await;
    match models.status {
        200..=299 => {}
        401 | 403 => {
            return Err(ApiError::new(
                424,
                "Antigravity rejected this account's credential. Sign in again or reimport.",
            ));
        }
        429 => {
            return Err(ApiError::new(
                429,
                "Antigravity is rate limiting quota checks. Try again later.",
            )
            .retry_after(60));
        }
        0 => return Err(ApiError::upstream("Could not reach Antigravity for quota.")),
        _ => return Err(ApiError::upstream("Antigravity quota is unavailable.")),
    }
    let quota = models.body.as_ref().map(parse_quota).unwrap_or_default();
    if quota.iter().any(|q| q.remaining_fraction.is_some()) {
        return Ok(quota);
    }
    let buckets = control_post(
        app,
        &format!("{}/v1internal:retrieveUserQuota", control_base()),
        &connection.api_key,
        &body,
        Instant::now() + CONTROL_TIMEOUT,
    )
    .await;
    Ok(buckets
        .body
        .filter(|_| (200..300).contains(&buckets.status))
        .map(|b| parse_quota_buckets(&b))
        .unwrap_or(quota))
}

/// Lists the account's models (`fetchAvailableModels`). Used to seed a new account's models.
pub(crate) async fn fetch_models(
    app: &App,
    access: &str,
    project: &str,
) -> Option<Vec<(String, String)>> {
    let body = if project.is_empty() {
        json!({})
    } else {
        json!({"project": project})
    };
    let r = control_post(
        app,
        &format!("{}/v1internal:fetchAvailableModels", control_base()),
        access,
        &body,
        Instant::now() + Duration::from_secs(10),
    )
    .await;
    r.body
        .filter(|_| (200..300).contains(&r.status))
        .map(|b| parse_models(&b))
        .filter(|m| !m.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Request envelope
// ---------------------------------------------------------------------------------------------

/// Inference URL for a connection base (`DAILY_BASE` for consumer accounts).
pub fn inference_url(base: &str, stream: bool) -> String {
    let base = base.trim_end_matches('/');
    if stream {
        format!("{base}/v1internal:streamGenerateContent?alt=sse")
    } else {
        format!("{base}/v1internal:generateContent")
    }
}

/// A stable per-conversation session id: a hash of the first user text, so retries and
/// follow-ups of one conversation share it. Nothing about the text is stored.
pub fn session_id(request: &Value) -> String {
    let first = request["contents"].as_array().and_then(|cs| {
        cs.iter()
            .filter(|c| c["role"] == "user")
            .find_map(|c| c["parts"][0]["text"].as_str().filter(|t| !t.is_empty()))
    });
    let digest = match first {
        Some(text) => Sha256::digest(text.as_bytes()),
        None => Sha256::digest(uuid::Uuid::new_v4().as_bytes()),
    };
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes")) & 0x7fff_ffff_ffff_ffff;
    format!("-{n}")
}

/// Wraps a Gemini `GenerateContentRequest` in the `v1internal` envelope for `model` and
/// `project`. Client routing fields are removed; Claude models get `VALIDATED` tool calling.
pub fn wrap(mut request: Value, model: &str, project: &str) -> Value {
    if let Some(o) = request.as_object_mut() {
        for k in ["model", "stream"] {
            o.remove(k);
        }
    }
    if request["sessionId"].as_str().is_none_or(str::is_empty) {
        request["sessionId"] = json!(session_id(&request));
    }
    if is_claude_model(model) && request["tools"].as_array().is_some_and(|t| !t.is_empty()) {
        let mode = request["toolConfig"]["functionCallingConfig"]["mode"]
            .as_str()
            .unwrap_or("AUTO");
        if mode.eq_ignore_ascii_case("AUTO") {
            request["toolConfig"]["functionCallingConfig"]["mode"] = json!("VALIDATED");
        }
    }
    let mut envelope = json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": if model.contains("image") { "image_gen" } else { "agent" },
        "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
        "request": request,
    });
    if !project.is_empty() {
        envelope["project"] = json!(project);
    }
    envelope
}

/// The Gemini response inside a `v1internal` response or SSE event (`{"response": ...}`).
/// Bodies that are already bare Gemini responses pass through.
pub fn unwrap(v: Value) -> Value {
    match v {
        Value::Object(mut o) if o.get("response").is_some_and(Value::is_object) => {
            o.remove("response").expect("checked")
        }
        other => other,
    }
}

/// Incremental SSE reader for `streamGenerateContent?alt=sse`: yields each event's unwrapped
/// Gemini response. Follows the event-stream rules (LF or CRLF, comments, multi-line `data:`).
/// An event larger than [`EVENT_MAX`] is an error, never truncated.
#[derive(Default)]
pub struct SseUnwrapper {
    pending: Vec<u8>,
    data: Vec<u8>,
}
impl SseUnwrapper {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ApiError> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                self.dispatch(&mut out);
            } else if let Some(d) = line.strip_prefix(b"data:") {
                let d = d.strip_prefix(b" ").unwrap_or(d);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(d);
            }
            if self.data.len() > EVENT_MAX {
                return Err(ApiError::upstream("Provider event exceeds 16 MiB"));
            }
        }
        if self.pending.len() + self.data.len() > EVENT_MAX {
            return Err(ApiError::upstream("Provider event exceeds 16 MiB"));
        }
        Ok(out)
    }
    /// Flushes a final event that was not followed by a blank line.
    pub fn finish(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let line = rest.strip_suffix(b"\r").unwrap_or(&rest);
            if let Some(d) = line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(d.strip_prefix(b" ").unwrap_or(d));
            }
        }
        self.dispatch(&mut out);
        out
    }
    fn dispatch(&mut self, out: &mut Vec<Value>) {
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data);
        if let Ok(v) = serde_json::from_slice::<Value>(&data) {
            out.push(unwrap(v));
        }
    }
}

/// Whether a Gemini response chunk ends the turn (a candidate `finishReason` or a prompt block).
pub fn is_terminal(chunk: &Value) -> bool {
    chunk["candidates"]
        .as_array()
        .is_some_and(|cs| cs.iter().any(|c| c["finishReason"].is_string()))
        || chunk["promptFeedback"]["blockReason"].is_string()
}

// ---------------------------------------------------------------------------------------------
// Gateway hooks
// ---------------------------------------------------------------------------------------------

/// Makes sure `connection` knows its Cloud Code project (stored in `account_id` for this kind),
/// discovering and persisting it on first use. The write happens under the account lock and only
/// if the stored row still has no project, so it never overwrites a concurrent change.
pub async fn ensure_project(app: &App, connection: &mut Connection) -> Result<(), ApiError> {
    if connection.kind != KIND || !connection.account_id.is_empty() {
        return Ok(());
    }
    let project = discover_project(app, &connection.api_key).await.ok_or(ApiError::new(
        424,
        "Could not determine this Antigravity account's Cloud Code project. Open Antigravity once with this account, then retry.",
    ))?;
    let lock = crate::app::account_lock(app, &connection.id);
    let _guard = tokio::time::timeout(Duration::from_secs(25), lock.lock())
        .await
        .map_err(|_| ApiError::new(503, "This account is busy. Retry shortly.").retry_after(5))?;
    if let Some(mut stored) = app.store.get::<Connection>("connection", &connection.id) {
        if stored.kind != connection.kind
            || stored.account_identity != connection.account_identity
            || stored.api_key != connection.api_key
        {
            return Err(ApiError::new(
                409,
                "The account changed during project discovery. Retry with the current connection.",
            ));
        }
        if stored.account_id.is_empty() {
            stored.account_id = project.clone();
            app.store
                .put("connection", &stored.id, &stored)
                .map_err(ApiError::db)?;
        }
        connection.account_id = if stored.account_id.is_empty() {
            project
        } else {
            stored.account_id
        };
    } else {
        return Err(ApiError::new(
            404,
            "Connection was removed during project discovery",
        ));
    }
    Ok(())
}

/// The account's model catalog for the dashboard (`fetchAvailableModels`): sorted
/// `(id, display name)` pairs, at most 1,000. Never changes the connection's configured models.
pub async fn catalog(
    app: &App,
    connection: &mut Connection,
) -> Result<Vec<(String, String)>, ApiError> {
    crate::credentials::refresh(app, connection).await?;
    let project = if connection.account_id.is_empty() {
        discover_project(app, &connection.api_key)
            .await
            .unwrap_or_default()
    } else {
        connection.account_id.clone()
    };
    let body = if project.is_empty() {
        json!({})
    } else {
        json!({"project": project})
    };
    let r = control_post(
        app,
        &format!("{}/v1internal:fetchAvailableModels", control_base()),
        &connection.api_key,
        &body,
        Instant::now() + CONTROL_TIMEOUT,
    )
    .await;
    match r.status {
        200..=299 => r
            .body
            .map(|b| parse_models(&b))
            .ok_or(ApiError::upstream("Invalid Antigravity model catalog")),
        401 | 403 => Err(ApiError::new(
            424,
            "Antigravity rejected this account's credential. Sign in again or reimport.",
        )),
        0 => Err(ApiError::upstream(
            "Could not reach the Antigravity model catalog",
        )),
        _ => Err(ApiError::upstream(
            "Antigravity model catalog unavailable. Enter model identifiers manually.",
        )),
    }
}

/// Seconds from a Google `RetryInfo` detail (`error.details[].retryDelay`, e.g. `"30s"` or
/// `"1.5s"`), rounded up and clamped to one hour. Complements the HTTP `Retry-After` header.
pub fn retry_after(body: &Value) -> Option<u64> {
    body["error"]["details"].as_array()?.iter().find_map(|d| {
        let delay = d["retryDelay"].as_str()?.trim().strip_suffix('s')?;
        let secs: f64 = delay
            .parse()
            .ok()
            .filter(|s: &f64| s.is_finite() && *s >= 0.0)?;
        Some((secs.ceil() as u64).clamp(1, 3600))
    })
}
