//! Quota, balance and provider-reported billing for `/api/usage/sources`.
//!
//! Every source is read on its own schedule and cached as a private snapshot in the key-value
//! store. A failed read keeps the last good data and marks it stale; an unknown value is never
//! shown as 0. Quota percentages belong to one account and one window and are never added up.
//!
//! Sources:
//! - Connection-associated (automatic): Codex and Claude subscription sign-ins and Antigravity
//!   accounts. Their tokens are renewed through [`credentials::refresh`], so imported sign-ins
//!   stay read-only and gateway-owned sign-ins refresh under the account lock.
//! - Monitors (opt-in): Cursor, OpenCode Zen/Go, Codex/Claude CLI files, and organization billing
//!   for OpenAI and Anthropic with Admin API keys.
//! - Everything else (API keys, Gemini, custom providers) is listed as gateway-only with an
//!   explanation, because those credentials cannot read quota or account billing.
//!
//! Endpoint shapes for Codex `wham/usage`, Claude `oauth/usage`, Cursor `usage-summary` and the
//! OpenCode Go/console APIs follow the MIT-licensed CodexBar (Copyright (c) 2026 Peter
//! Steinberger, commit 2cfa632), used as a protocol reference; see THIRD_PARTY_NOTICES.md and
//! docs/usage-sources.md. OpenAI and Anthropic cost reports follow the vendors' Admin API docs.
use crate::{
    app::{ApiError, App, AppState},
    credentials,
    store::{Connection, hash, id, now},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::{StreamExt, future::BoxFuture, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Scheduled refresh cadence.
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);
/// A snapshot older than this is reported as `stale`.
pub const STALE_AFTER_SECS: i64 = 600;
pub const MAX_CONCURRENCY: usize = 4;
pub const MAX_SOURCES: usize = 256;
pub const MAX_MONITORS: usize = 100;
/// Deadline for one source, including credential renewal and every page it reads.
pub const SOURCE_DEADLINE: Duration = Duration::from_secs(30);
/// Largest provider response body read.
pub const MAX_BODY: usize = 1024 * 1024;
/// Largest stored snapshot.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;
/// Most windows kept per snapshot (also bounded by [`MAX_SNAPSHOT_BYTES`]).
pub const MAX_WINDOWS: usize = 1000;
const MANUAL_MIN: Duration = Duration::from_secs(30);
const CYCLE_DEADLINE: Duration = Duration::from_secs(240);
const MAX_BACKOFF_SECS: i64 = 3600;
const MAX_PAGES: usize = 12;
const SNAPSHOT_KIND: &str = "usage_snapshot";
pub(crate) const MONITOR_KIND: &str = "usage_monitor";
const MAX_CREDENTIAL: usize = 16 * 1024;
/// Native credential files are small; anything larger is not a credential file.
const MAX_NATIVE_FILE: u64 = 1024 * 1024;

pub const PROVIDERS: &[&str] = &[
    "codex",
    "claude",
    "cursor",
    "opencode",
    "opencode_go",
    "antigravity",
    "openai",
    "anthropic",
    "gemini",
];

// Constant, sanitized messages. Provider response text is never shown or stored.
const MSG_API_KEY_ONLY: &str = "API keys cannot read subscription quota or account billing. Gateway token and cost metrics for this account appear in Usage.";
const MSG_GEMINI: &str = "Gemini billing is only available from Google Cloud Billing (a service account or billing export), not from an API key. Gateway token and cost metrics appear in Usage.";
const MSG_CUSTOM: &str = "This provider has no quota or billing API that Switchyard can read. Gateway token and cost metrics appear in Usage.";
const MSG_NOT_YET: &str = "Not refreshed yet.";
const MSG_SIGN_IN: &str = "The provider rejected this sign-in. Sign in again, or let the owning app renew it, then refresh.";
const MSG_ADMIN_KEY: &str =
    "Organization billing needs an Admin API key. Ordinary inference keys cannot read costs.";
const MSG_RATE_LIMITED: &str =
    "The provider is rate limiting usage reads. Switchyard will retry after the requested delay.";
const MSG_TIMEOUT: &str = "The provider did not answer within 30 seconds.";
const MSG_NETWORK: &str = "Could not reach the provider.";
const MSG_HTTP: &str = "The provider returned an error for the usage request.";
const MSG_PARSE: &str = "The provider returned a response Switchyard does not recognize, so no values are shown rather than guessed.";
const MSG_IDENTITY: &str = "The sign-in on this machine now belongs to a different account. Import it again to monitor the new account.";
const MSG_DUPLICATE: &str =
    "This account is already monitored by another source, so it is not read twice.";
const MSG_NO_NATIVE: &str = "No signed-in session was found on this machine.";
const MSG_EXPIRED: &str = "The app's sign-in on this machine has expired. Open the app to renew it; Switchyard never refreshes sign-ins that another app owns.";
const MSG_AGY_HELPER: &str = "Antigravity quota is not available in this build.";
const MSG_OPENCODE_ZEN_KEY: &str =
    "OpenCode Zen balance needs a console session cookie. An API key cannot read billing.";
const MSG_CODEX_API_KEY: &str =
    "This Codex sign-in uses an API key, which has no subscription quota.";
const MSG_NO_COST_API: &str = "This provider has no billing API for this credential.";

// ---------------------------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------------------------

/// One quota or spend window. `percent` windows are 0 to 100.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub label: String,
    /// `percent`, `tokens`, `usd`, `requests` or `credits`.
    pub unit: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub reset_at: Option<String>,
    pub model: Option<String>,
    /// `account` or `model`.
    pub scope: String,
    /// Compact period such as `5h`, `7d`, `month` or `billing_cycle`.
    #[serde(default)]
    pub period: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub label: String,
    pub unit: String,
    pub currency: Option<String>,
    pub value: f64,
}
/// An amount the provider itself reported. Never added to gateway estimates.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportedCost {
    pub label: String,
    pub currency: String,
    pub amount: f64,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    /// `billed`, `included`, `subscription` or `on_demand`.
    pub kind: String,
}
/// One successful provider read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub windows: Vec<Window>,
    pub balances: Vec<Balance>,
    pub reported_costs: Vec<ReportedCost>,
    pub plan: Option<String>,
    /// Constant notes about limits of this reading.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Why a read failed. Each maps to a constant message.
#[derive(Clone, Debug, PartialEq)]
pub enum FetchError {
    /// HTTP 401 or 403 (or a credential known to be expired).
    Auth(u16),
    RateLimited(Option<u64>),
    Timeout,
    Network,
    Http(u16),
    Parse,
    Expired,
    IdentityChanged,
    NotFound,
    Duplicate,
    Unavailable(&'static str),
}
impl FetchError {
    pub fn message(&self, admin: bool) -> &'static str {
        match self {
            Self::Auth(_) if admin => MSG_ADMIN_KEY,
            Self::Auth(_) => MSG_SIGN_IN,
            Self::RateLimited(_) => MSG_RATE_LIMITED,
            Self::Timeout => MSG_TIMEOUT,
            Self::Network => MSG_NETWORK,
            Self::Http(_) => MSG_HTTP,
            Self::Parse => MSG_PARSE,
            Self::Expired => MSG_EXPIRED,
            Self::IdentityChanged => MSG_IDENTITY,
            Self::NotFound => MSG_NO_NATIVE,
            Self::Duplicate => MSG_DUPLICATE,
            Self::Unavailable(m) => m,
        }
    }
    fn status(&self) -> Option<&'static str> {
        match self {
            Self::Auth(_) | Self::Expired | Self::IdentityChanged | Self::NotFound => {
                Some("needs_auth")
            }
            Self::Unavailable(_) | Self::Duplicate => Some("unavailable"),
            _ => None,
        }
    }
}

/// Per-model quota from an Antigravity account (fields mirror `antigravity::ModelQuota`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelQuota {
    pub model: String,
    pub label: String,
    /// 0..=1, `None` when the provider did not report it.
    pub remaining_fraction: Option<f64>,
    /// Unix seconds.
    pub reset_at: Option<i64>,
}
/// Reads Antigravity quota for a connection. Errors carry the HTTP status (0 for no response).
pub type AntigravityFetcher =
    fn(App, Connection) -> BoxFuture<'static, Result<Vec<ModelQuota>, u16>>;
static ANTIGRAVITY: OnceLock<AntigravityFetcher> = OnceLock::new();
/// Wires the Antigravity quota helper (see docs/usage-sources.md). Set once at startup.
pub fn set_antigravity_fetcher(f: AntigravityFetcher) {
    let _ = ANTIGRAVITY.set(f);
}

// ---------------------------------------------------------------------------------------------
// Monitors and snapshots (private, stored as JSON in the key-value table)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub provider: String,
    /// `native`, `api_key`, `cookie` or `file`.
    pub credential_source: String,
    pub source_path: Option<String>,
    pub connection_id: Option<String>,
    pub enabled: bool,
    /// API key or cookie. Never returned by the API.
    #[serde(default)]
    pub credential: String,
    /// Hash of the account identity seen at import (native sources). Never the raw identity.
    #[serde(default)]
    pub identity: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// Bumped on every change; in-flight reads for an older version are discarded.
    #[serde(default)]
    pub version: u64,
}
impl Monitor {
    pub fn public(&self) -> Value {
        let present = match self.credential_source.as_str() {
            "native" => !self.identity.is_empty(),
            "file" => self.source_path.is_some(),
            _ => !self.credential.is_empty(),
        };
        json!({"id":self.id,"name":self.name,"provider":self.provider,"credential_source":self.credential_source,
            "source_path":self.source_path,"connection_id":self.connection_id,"enabled":self.enabled,
            "credential_present":present,"created_at":self.created_at,"updated_at":self.updated_at})
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    source_id: String,
    /// Version of the monitor or connection this snapshot belongs to.
    version: String,
    status: String,
    message: Option<String>,
    last_success_at: Option<String>,
    last_error_at: Option<String>,
    last_attempt_at: Option<String>,
    /// Unix seconds before which this source is not read again (429 or backoff).
    retry_at: Option<i64>,
    failures: u32,
    reading: Reading,
}

// ---------------------------------------------------------------------------------------------
// Per-App collector state
// ---------------------------------------------------------------------------------------------

pub(crate) struct Collector {
    app: Weak<AppState>,
    started: AtomicBool,
    initial: AtomicBool,
    refreshing: AtomicBool,
    pending: Mutex<HashSet<String>>,
    in_flight: Mutex<HashSet<String>>,
    manual: Mutex<HashMap<String, Instant>>,
    /// Serializes snapshot writes against monitor edits and deletions.
    pub(crate) write: tokio::sync::Mutex<()>,
    endpoints: Mutex<HashMap<String, String>>,
    manual_interval: Mutex<Duration>,
    pub(crate) native: crate::native_usage::NativeState,
}
static REGISTRY: LazyLock<Mutex<Vec<Arc<Collector>>>> = LazyLock::new(Default::default);

/// The collector state for `app`, created on first use. Entries for dropped apps are pruned.
pub(crate) fn collector(app: &App) -> Arc<Collector> {
    let mut registry = REGISTRY.lock().expect("collector registry");
    registry.retain(|c| c.app.strong_count() > 0);
    if let Some(c) = registry
        .iter()
        .find(|c| std::ptr::eq(c.app.as_ptr(), Arc::as_ptr(app)))
    {
        return c.clone();
    }
    let c = Arc::new(Collector {
        app: Arc::downgrade(app),
        started: AtomicBool::new(false),
        initial: AtomicBool::new(false),
        refreshing: AtomicBool::new(false),
        pending: Mutex::default(),
        in_flight: Mutex::default(),
        manual: Mutex::default(),
        write: tokio::sync::Mutex::new(()),
        endpoints: Mutex::default(),
        manual_interval: Mutex::new(MANUAL_MIN),
        native: Default::default(),
    });
    registry.push(c.clone());
    c
}

/// Test hook: point a provider API at a mock server for this app only. Names: `codex`,
/// `claude`, `cursor`, `cursor_admin`, `opencode`, `openai`, `anthropic_admin`.
#[doc(hidden)]
pub fn set_test_endpoint(app: &App, name: &str, base: &str) {
    collector(app)
        .endpoints
        .lock()
        .expect("endpoints")
        .insert(name.into(), base.trim_end_matches('/').into());
}
/// Test hook: change the minimum interval between manual refreshes of one source.
#[doc(hidden)]
pub fn set_test_manual_interval(app: &App, interval: Duration) {
    *collector(app).manual_interval.lock().expect("interval") = interval;
}
pub(crate) fn endpoint(c: &Collector, name: &str) -> String {
    if let Some(base) = c.endpoints.lock().expect("endpoints").get(name) {
        return base.clone();
    }
    match name {
        "codex" => "https://chatgpt.com/backend-api",
        "claude" | "anthropic_admin" => "https://api.anthropic.com",
        "cursor" => "https://cursor.com",
        "cursor_admin" => "https://api.cursor.com",
        "opencode" => "https://opencode.ai",
        "openai" => "https://api.openai.com",
        _ => "",
    }
    .into()
}

/// Starts background polling for `app`. Idempotent. The poller holds only a weak reference and
/// exits once the app is dropped.
pub fn start(app: &App) {
    let c = collector(app);
    if c.started.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(app);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            let Some(app) = weak.upgrade() else { break };
            c.initial.store(true, Ordering::SeqCst);
            trigger(&app, None, false);
            crate::native_usage::resume(&app);
            drop(app);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Target {
    Codex(Connection),
    Claude(Connection),
    Antigravity(Connection),
    OpenCodeGo(Connection),
    GatewayOnly(&'static str),
    Monitor(Monitor),
}
#[derive(Clone)]
struct Desc {
    id: String,
    name: String,
    provider: String,
    connection_id: Option<String>,
    connection_name: Option<String>,
    source: &'static str,
    caps: [bool; 4],
    enabled: bool,
    version: String,
    target: Target,
    /// Set when another source already monitors this account.
    duplicate: bool,
    notes: Vec<&'static str>,
}
impl Desc {
    fn fetchable(&self) -> bool {
        self.enabled && !self.duplicate && !matches!(self.target, Target::GatewayOnly(_))
    }
    fn admin(&self) -> bool {
        matches!(&self.target, Target::Monitor(m) if matches!(m.provider.as_str(), "openai" | "anthropic") || (m.provider == "cursor" && m.credential_source == "api_key"))
    }
}

/// Snapshot version for a connection. The account id is left out: it is filled in when a token is
/// renewed, which does not change the account. A different account identity does.
fn connection_version(c: &Connection) -> String {
    hash(&format!(
        "conn|{}|{}|{}|{}|{}|{}",
        c.id,
        c.kind,
        c.account_identity,
        c.oauth,
        c.base_url,
        if c.oauth {
            String::new()
        } else {
            hash(&c.api_key)
        }
    ))
}

fn monitor_version(m: &Monitor) -> String {
    format!("monitor|{}", m.version)
}

fn monitor_caps(m: &Monitor) -> [bool; 4] {
    // quota, cost, tokens, history
    match (m.provider.as_str(), m.credential_source.as_str()) {
        ("cursor", "api_key") => [false, true, false, false],
        ("cursor", _) => [true, true, false, true],
        ("opencode", "cookie") | ("opencode_go", "cookie") => [true, true, false, false],
        ("opencode_go", _) => [true, false, false, false],
        ("codex" | "claude", _) => [true, false, false, false],
        ("openai" | "anthropic", _) => [false, true, false, false],
        _ => [false, false, false, false],
    }
}

fn descs(app: &App) -> Vec<Desc> {
    let connections: Vec<Connection> = app.store.list("connection");
    let mut out = Vec::new();
    let mut seen_accounts = HashSet::new();
    for c in &connections {
        let (provider, target, caps): (String, Target, [bool; 4]) =
            if crate::usage::provider_for(c) == "opencode_go" {
                (
                    "opencode_go".into(),
                    Target::OpenCodeGo(c.clone()),
                    [true, false, true, false],
                )
            } else {
                match (c.kind.as_str(), c.oauth) {
                    ("codex", true) => (
                        "codex".into(),
                        Target::Codex(c.clone()),
                        [true, false, true, false],
                    ),
                    ("anthropic", true) => (
                        "claude".into(),
                        Target::Claude(c.clone()),
                        [true, false, true, false],
                    ),
                    ("antigravity", _) => (
                        "antigravity".into(),
                        Target::Antigravity(c.clone()),
                        [true, false, true, false],
                    ),
                    ("gemini", _) => (
                        "gemini".into(),
                        Target::GatewayOnly(MSG_GEMINI),
                        [false, false, true, false],
                    ),
                    ("openai" | "anthropic" | "codex", _) => (
                        c.kind.clone(),
                        Target::GatewayOnly(MSG_API_KEY_ONLY),
                        [false, false, true, false],
                    ),
                    _ => (
                        c.kind.clone(),
                        Target::GatewayOnly(MSG_CUSTOM),
                        [false, false, true, false],
                    ),
                }
            };
        let duplicate = provider == "opencode_go"
            && c.enabled
            && !seen_accounts.insert(format!(
                "opencode_go|{}",
                hash(&format!("opencode_go|{}", c.api_key))
            ));
        out.push(Desc {
            id: format!("connection:{}", c.id),
            name: c.name.clone(),
            provider,
            connection_id: Some(c.id.clone()),
            connection_name: Some(c.name.clone()),
            source: if matches!(target, Target::GatewayOnly(_)) {
                "gateway_only"
            } else {
                "provider_api"
            },
            caps,
            enabled: c.enabled,
            version: connection_version(c),
            target,
            duplicate,
            notes: Vec::new(),
        });
    }
    let auto: HashSet<&str> = connections
        .iter()
        .filter(|c| {
            c.enabled
                && (c.oauth && matches!(c.kind.as_str(), "codex" | "anthropic")
                    || c.kind == "antigravity")
        })
        .map(|c| c.id.as_str())
        .collect();
    let mut seen = seen_accounts;
    let mut monitors: Vec<Monitor> = app.store.list(MONITOR_KIND);
    monitors.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    for m in monitors.into_iter().take(MAX_MONITORS) {
        let connection = m
            .connection_id
            .as_deref()
            .and_then(|id| connections.iter().find(|c| c.id == id));
        let mut duplicate = false;
        if m.enabled && !m.identity.is_empty() {
            duplicate = !seen.insert(format!("{}|{}", m.provider, m.identity));
        }
        // A monitor attached to a connection whose quota is read automatically is redundant.
        if matches!(m.provider.as_str(), "codex" | "claude" | "antigravity")
            && m.connection_id
                .as_deref()
                .is_some_and(|id| auto.contains(id))
        {
            duplicate = true;
        }
        out.push(Desc {
            id: format!("monitor:{}", m.id),
            name: m.name.clone(),
            provider: m.provider.clone(),
            connection_id: connection.map(|c| c.id.clone()),
            connection_name: connection.map(|c| c.name.clone()),
            source: if matches!(m.credential_source.as_str(), "native" | "file") {
                "native_file"
            } else {
                "provider_api"
            },
            caps: monitor_caps(&m),
            enabled: m.enabled,
            version: monitor_version(&m),
            notes: match m.provider.as_str() {
                "cursor" => vec!["Cursor's own models are not available as an API through Switchyard; this source reads usage only."],
                "openai" | "anthropic" => vec!["Organization cost reports lag by up to a day and cover every key in the organization, including traffic the gateway already counted."],
                _ => Vec::new(),
            },
            target: Target::Monitor(m),
            duplicate,
        });
    }
    out.truncate(MAX_SOURCES);
    out
}

// ---------------------------------------------------------------------------------------------
// Refresh scheduling
// ---------------------------------------------------------------------------------------------

struct ResetOnDrop(Arc<Collector>);
impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.refreshing.store(false, Ordering::SeqCst);
    }
}

/// Starts a refresh in the background. `only` limits it to one source id. Returns false when a
/// refresh was already running; the request is then queued and runs right after it.
pub fn trigger(app: &App, only: Option<String>, manual: bool) -> bool {
    let c = collector(app);
    if c.refreshing.swap(true, Ordering::SeqCst) {
        c.pending
            .lock()
            .expect("pending")
            .insert(only.unwrap_or_else(|| "*".into()));
        return false;
    }
    let weak = Arc::downgrade(app);
    tokio::spawn(async move {
        let _reset = ResetOnDrop(c.clone());
        let mut next = Some(only);
        // Bounded: the original request plus at most three rounds of queued requests.
        for _ in 0..4 {
            let Some(only) = next.take() else { break };
            let Some(app) = weak.upgrade() else { return };
            let _ = tokio::time::timeout(CYCLE_DEADLINE, run_cycle(&app, &c, only, manual)).await;
            drop(app);
            let queued: Vec<String> = c.pending.lock().expect("pending").drain().collect();
            if queued.is_empty() {
                break;
            }
            next = Some(if queued.len() == 1 && queued[0] != "*" {
                Some(queued[0].clone())
            } else {
                None
            });
        }
    });
    true
}

fn ts_now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn parse_ts(s: &Option<String>) -> Option<i64> {
    s.as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
}

async fn run_cycle(app: &App, c: &Arc<Collector>, only: Option<String>, manual: bool) {
    let all = descs(app);
    let snapshots: HashMap<String, Snapshot> = app
        .store
        .list::<Snapshot>(SNAPSHOT_KIND)
        .into_iter()
        .map(|s| (s.source_id.clone(), s))
        .collect();
    if only.is_none() {
        // Drop snapshots for sources that no longer exist.
        let live: HashSet<&str> = all.iter().map(|d| d.id.as_str()).collect();
        let _guard = c.write.lock().await;
        for id in snapshots.keys().filter(|id| !live.contains(id.as_str())) {
            let _ = app.store.delete(SNAPSHOT_KIND, id);
        }
    }
    let now = ts_now();
    let due: Vec<Desc> = all
        .into_iter()
        .filter(|d| d.fetchable())
        .filter(|d| only.as_ref().is_none_or(|o| *o == d.id))
        .filter(|d| {
            let Some(s) = snapshots.get(&d.id).filter(|s| s.version == d.version) else {
                return true;
            };
            if s.retry_at.is_some_and(|t| t > now) {
                return false;
            }
            manual
                || parse_ts(&s.last_attempt_at)
                    .is_none_or(|t| now - t >= POLL_INTERVAL.as_secs() as i64 - 15)
        })
        .collect();
    stream::iter(due)
        .map(|d| refresh_one(app, c, d))
        .buffer_unordered(MAX_CONCURRENCY)
        .collect::<Vec<()>>()
        .await;
}

struct InFlight<'a>(&'a Collector, String);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.lock().expect("in flight").remove(&self.1);
    }
}

async fn refresh_one(app: &App, c: &Arc<Collector>, d: Desc) {
    if !c.in_flight.lock().expect("in flight").insert(d.id.clone()) {
        return;
    }
    let _flight = InFlight(c, d.id.clone());
    let result = tokio::time::timeout(SOURCE_DEADLINE, fetch(app, c, &d))
        .await
        .unwrap_or(Err(FetchError::Timeout));
    persist(app, c, &d, result).await;
}

/// The current version of the monitor or connection behind `d`, while it still exists, is
/// enabled and belongs to the same account. A connection whose identity was unknown when the read
/// started may have learned it from a token renewal during the read; that is the same account.
fn current_version(app: &App, d: &Desc) -> Option<String> {
    match &d.target {
        Target::Monitor(m) => app
            .store
            .get::<Monitor>(MONITOR_KIND, &m.id)
            .filter(|x| x.enabled && monitor_version(x) == d.version)
            .map(|x| monitor_version(&x)),
        Target::Codex(c) | Target::Claude(c) | Target::Antigravity(c) | Target::OpenCodeGo(c) => {
            app.store
                .get::<Connection>("connection", &c.id)
                .filter(|x| {
                    x.enabled
                        && (connection_version(x) == d.version
                            || (c.account_identity.is_empty()
                                && x.kind == c.kind
                                && x.oauth == c.oauth))
                })
                .map(|x| connection_version(&x))
        }
        Target::GatewayOnly(_) => None,
    }
}

fn backoff(failures: u32) -> i64 {
    if failures < 2 {
        return 0;
    }
    (POLL_INTERVAL.as_secs() as i64)
        .saturating_mul(1 << (failures - 1).min(6))
        .min(MAX_BACKOFF_SECS)
}

async fn persist(app: &App, c: &Collector, d: &Desc, result: Result<Reading, FetchError>) {
    let _guard = c.write.lock().await;
    // A deleted, disabled or edited source must not be resurrected by a read that started before.
    let Some(version) = current_version(app, d) else {
        return;
    };
    let mut s = app
        .store
        .get::<Snapshot>(SNAPSHOT_KIND, &d.id)
        .filter(|s| s.version == d.version || s.version == version)
        .unwrap_or_default();
    s.source_id = d.id.clone();
    s.version = version;
    let stamp = now();
    s.last_attempt_at = Some(stamp.clone());
    match result {
        Ok(reading) => {
            s.status = "ok".into();
            s.message = None;
            s.last_success_at = Some(stamp);
            s.failures = 0;
            s.retry_at = None;
            s.reading = reading;
        }
        Err(e) => {
            s.failures = s.failures.saturating_add(1);
            s.last_error_at = Some(stamp);
            s.message = Some(e.message(d.admin()).into());
            s.status = e
                .status()
                .unwrap_or(if s.last_success_at.is_some() {
                    "stale"
                } else {
                    "unavailable"
                })
                .into();
            let delay = match e {
                FetchError::RateLimited(Some(secs)) => (secs as i64).clamp(30, MAX_BACKOFF_SECS),
                FetchError::RateLimited(None) => backoff(s.failures.max(2)),
                _ => backoff(s.failures),
            };
            s.retry_at = (delay > 0).then(|| ts_now() + delay);
        }
    }
    cap_snapshot(&mut s);
    let _ = app.store.put(SNAPSHOT_KIND, &d.id, &s);
}

fn cap_snapshot(s: &mut Snapshot) {
    let r = &mut s.reading;
    r.windows.truncate(MAX_WINDOWS);
    r.balances.truncate(50);
    r.reported_costs.truncate(50);
    r.notes.truncate(10);
    while serde_json::to_vec(s).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_SNAPSHOT_BYTES {
        let r = &mut s.reading;
        if r.windows.len() > 1 {
            r.windows.truncate(r.windows.len() / 2);
            if !r.notes.iter().any(|n| n.starts_with("Some windows")) {
                r.notes
                    .push("Some windows were omitted to keep the snapshot small.".into());
            }
        } else {
            s.reading = Reading::default();
            break;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------------------------

fn retry_after(h: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = h.get("retry-after")?.to_str().ok()?.trim();
    v.parse::<u64>().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(v)
            .ok()
            .map(|t| (t.timestamp() - ts_now()).max(0) as u64)
    })
}

/// Sends a request and reads at most [`MAX_BODY`] bytes of JSON. Never surfaces provider text.
pub(crate) async fn send_json(rb: reqwest::RequestBuilder) -> Result<Value, FetchError> {
    let res = rb.send().await.map_err(|e| {
        if e.is_timeout() {
            FetchError::Timeout
        } else {
            FetchError::Network
        }
    })?;
    let status = res.status().as_u16();
    match status {
        401 | 403 => return Err(FetchError::Auth(status)),
        429 => return Err(FetchError::RateLimited(retry_after(res.headers()))),
        s if !(200..300).contains(&s) => return Err(FetchError::Http(s)),
        _ => {}
    }
    credentials::json_bounded(res, MAX_BODY)
        .await
        .ok_or(FetchError::Parse)
}

/// A finite number from a JSON number or numeric string.
pub(crate) fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}
/// Unix seconds from seconds, milliseconds, numeric strings or RFC 3339.
pub(crate) fn ts_of(v: &Value) -> Option<i64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(n) => n,
            Err(_) => {
                return chrono::DateTime::parse_from_rfc3339(s.trim())
                    .ok()
                    .map(|t| t.timestamp());
            }
        },
        _ => return None,
    };
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some(if n > 100_000_000_000.0 {
        (n / 1000.0) as i64
    } else {
        n as i64
    })
}
pub(crate) fn iso(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
fn round6(f: f64) -> f64 {
    (f * 1_000_000.0).round() / 1_000_000.0
}
fn usd_window(id: &str, label: &str, used: Option<f64>, limit: Option<f64>) -> Window {
    Window {
        id: id.into(),
        label: label.into(),
        unit: "usd".into(),
        used: used.map(round6),
        limit: limit.map(round6),
        remaining: match (used, limit) {
            (Some(u), Some(l)) => Some(round6((l - u).max(0.0))),
            _ => None,
        },
        scope: "account".into(),
        ..Default::default()
    }
}
fn percent_window(id: &str, label: &str, used: f64) -> Window {
    let used = used.clamp(0.0, 100.0);
    Window {
        id: id.into(),
        label: label.into(),
        unit: "percent".into(),
        used: Some(round6(used)),
        limit: Some(100.0),
        remaining: Some(round6(100.0 - used)),
        scope: "account".into(),
        ..Default::default()
    }
}
fn period_for(seconds: Option<i64>) -> (String, String) {
    match seconds {
        Some(18_000) => ("5h".into(), "5-hour".into()),
        Some(86_400) => ("1d".into(), "Daily".into()),
        Some(604_800) => ("7d".into(), "Weekly".into()),
        Some(s) if (2_419_200..=2_678_400).contains(&s) => ("month".into(), "Monthly".into()),
        Some(s) if s > 0 && s % 86_400 == 0 => {
            (format!("{}d", s / 86_400), format!("{}-day", s / 86_400))
        }
        Some(s) if s > 0 && s % 3600 == 0 => {
            (format!("{}h", s / 3600), format!("{}-hour", s / 3600))
        }
        _ => ("window".into(), "Usage".into()),
    }
}
fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    s.trim_matches('-').to_string()
}
fn short(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
fn month_start() -> chrono::DateTime<chrono::Utc> {
    use chrono::{Datelike, TimeZone};
    let n = chrono::Utc::now();
    chrono::Utc
        .with_ymd_and_hms(n.year(), n.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(n)
}

// ---------------------------------------------------------------------------------------------
// Provider parsers (public so fixtures can test them)
// ---------------------------------------------------------------------------------------------

fn codex_window(
    w: &Value,
    prefix: &str,
    label_prefix: &str,
    model: Option<&str>,
) -> Option<Window> {
    let used = num(&w["used_percent"])?;
    let (period, label) = period_for(w["limit_window_seconds"].as_i64());
    let mut win = percent_window(
        &format!("{prefix}{period}"),
        &format!("{label_prefix}{label}"),
        used,
    );
    win.period = Some(period);
    win.reset_at = ts_of(&w["reset_at"])
        .or_else(|| w["reset_after_seconds"].as_i64().map(|s| ts_now() + s))
        .and_then(iso);
    if let Some(m) = model {
        win.scope = "model".into();
        win.model = Some(m.into());
    }
    Some(win)
}

/// Codex `GET /backend-api/wham/usage`.
pub fn parse_codex(v: &Value) -> Result<Reading, FetchError> {
    if !v.is_object() {
        return Err(FetchError::Parse);
    }
    let mut r = Reading {
        plan: v["plan_type"].as_str().map(|s| short(s, 40)),
        ..Default::default()
    };
    let rl = &v["rate_limit"];
    for key in ["primary_window", "secondary_window"] {
        if let Some(w) = codex_window(&rl[key], "", "", None)
            && !r.windows.iter().any(|x| x.id == w.id)
        {
            r.windows.push(w);
        }
    }
    for entry in v["additional_rate_limits"]
        .as_array()
        .into_iter()
        .flatten()
        .take(50)
    {
        let name = entry["limit_name"]
            .as_str()
            .or_else(|| entry["metered_feature"].as_str())
            .unwrap_or("Model limit");
        let name = short(name, 80);
        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = codex_window(
                &entry["rate_limit"][key],
                &format!("model:{}:", slug(&name)),
                &format!("{name} "),
                Some(&name),
            ) && !r.windows.iter().any(|x| x.id == w.id)
            {
                r.windows.push(w);
            }
        }
    }
    let credits = &v["credits"];
    if credits["unlimited"] == true {
        r.notes.push("Credits are unlimited on this plan.".into());
    } else if credits["has_credits"] == true
        && let Some(balance) = num(&credits["balance"])
    {
        r.balances.push(Balance {
            label: "Credits".into(),
            unit: "credits".into(),
            currency: None,
            value: balance,
        });
    }
    if r.windows.is_empty() && r.balances.is_empty() && !rl.is_object() {
        return Err(FetchError::Parse);
    }
    Ok(r)
}

/// Claude `GET /api/oauth/usage`. `utilization` is a percentage; `extra_usage` amounts are in
/// minor currency units (cents).
pub fn parse_claude(v: &Value) -> Result<Reading, FetchError> {
    let Some(obj) = v.as_object() else {
        return Err(FetchError::Parse);
    };
    let mut r = Reading::default();
    let known: [(&str, &str, &str, &str, Option<&str>); 5] = [
        ("five_hour", "session", "Session (5-hour)", "5h", None),
        ("seven_day", "week", "Weekly", "7d", None),
        (
            "seven_day_opus",
            "week:opus",
            "Weekly Opus",
            "7d",
            Some("opus"),
        ),
        (
            "seven_day_sonnet",
            "week:sonnet",
            "Weekly Sonnet",
            "7d",
            Some("sonnet"),
        ),
        (
            "seven_day_oauth_apps",
            "week:oauth_apps",
            "Weekly OAuth apps",
            "7d",
            None,
        ),
    ];
    let push =
        |r: &mut Reading, w: &Value, id: &str, label: &str, period: &str, model: Option<&str>| {
            let Some(used) = num(&w["utilization"]) else {
                return;
            };
            let mut win = percent_window(id, label, used);
            win.period = Some(period.into());
            win.reset_at = ts_of(&w["resets_at"]).and_then(iso);
            if let Some(m) = model {
                win.scope = "model".into();
                win.model = Some(m.into());
            }
            if !r.windows.iter().any(|x| x.id == win.id) {
                r.windows.push(win);
            }
        };
    for (key, id, label, period, model) in known {
        push(
            &mut r,
            &obj.get(key).cloned().unwrap_or(Value::Null),
            id,
            label,
            period,
            model,
        );
    }
    // Newer windows are reported under other `five_hour_*` / `seven_day_*` keys.
    for (key, w) in obj.iter().take(100) {
        if known.iter().any(|k| k.0 == key) || !w.is_object() {
            continue;
        }
        let period = if key.starts_with("seven_day") {
            "7d"
        } else if key.starts_with("five_hour") {
            "5h"
        } else {
            continue;
        };
        let rest = key
            .trim_start_matches("seven_day")
            .trim_start_matches("five_hour")
            .trim_matches('_')
            .replace('_', " ");
        let label = format!(
            "{} {}",
            if period == "7d" { "Weekly" } else { "5-hour" },
            short(&rest, 40)
        );
        push(
            &mut r,
            w,
            &format!("{period}:{}", slug(&rest)),
            label.trim(),
            period,
            None,
        );
    }
    for entry in v["limits"].as_array().into_iter().flatten().take(50) {
        let Some(used) = num(&entry["percent"]) else {
            continue;
        };
        let name = entry["group"]
            .as_str()
            .or(entry["kind"].as_str())
            .unwrap_or("limit");
        let model = entry["scope"]["model"]["display_name"]
            .as_str()
            .or(entry["scope"]["model"]["id"].as_str());
        let id = format!("limit:{}:{}", slug(name), slug(model.unwrap_or("account")));
        if r.windows.iter().any(|x| x.id == id) {
            continue;
        }
        let mut win = percent_window(&id, &short(&name.replace('_', " "), 60), used);
        win.period = match name {
            "session" | "five_hour" => Some("5h".into()),
            "weekly" | "seven_day" => Some("7d".into()),
            _ => None,
        };
        win.reset_at = ts_of(&entry["resets_at"]).and_then(iso);
        if let Some(m) = model {
            win.scope = "model".into();
            win.model = Some(short(m, 80));
        }
        // The current API can return legacy fields and the same limits in its new array.
        // Keep one account window while retaining genuinely separate model-specific limits.
        let duplicate = r.windows.iter().any(|existing| {
            ["session", "week"].contains(&existing.id.as_str())
                && existing.period == win.period
                && existing.model == win.model
                && existing.reset_at == win.reset_at
                && existing.used == win.used
        });
        if !duplicate {
            r.windows.push(win);
        }
    }
    let extra = &v["extra_usage"];
    if extra["is_enabled"] == true {
        let currency = extra["currency"]
            .as_str()
            .map(str::trim)
            .filter(|c| c.len() == 3)
            .unwrap_or("USD")
            .to_ascii_uppercase();
        let used = num(&extra["used_credits"]).map(|c| c / 100.0);
        let limit = num(&extra["monthly_limit"]).map(|c| c / 100.0);
        if currency == "USD" && (used.is_some() || limit.is_some()) {
            let mut w = usd_window("extra_usage", "Extra usage (monthly)", used, limit);
            w.period = Some("month".into());
            r.windows.push(w);
        }
        if let Some(amount) = used {
            r.reported_costs.push(ReportedCost {
                label: "Extra usage this month".into(),
                currency,
                amount: round6(amount),
                period_start: None,
                period_end: None,
                kind: "on_demand".into(),
            });
        }
    }
    Ok(r)
}

fn opencode_window(w: &Value, id: &str, label: &str, period: &str) -> Option<Window> {
    let pct = ["usagePercent", "usedPercent", "percentUsed", "percent"]
        .iter()
        .find_map(|k| num(&w[*k]));
    let mut win = if let Some(p) = pct {
        percent_window(id, label, p)
    } else {
        // Console meters are micro-cents: 1 USD = 100,000,000.
        let used = num(&w["usedMicroCents"])?;
        let limit = num(&w["limitMicroCents"]);
        usd_window(id, label, Some(used / 1e8), limit.map(|l| l / 1e8))
    };
    win.period = Some(period.into());
    win.reset_at = ["resetInSec", "resetInSeconds", "resetSeconds"]
        .iter()
        .find_map(|k| w[*k].as_i64())
        .map(|s| ts_now() + s.max(0))
        .or_else(|| {
            ["resetAt", "resetsAt", "reset_at", "resets_at"]
                .iter()
                .find_map(|k| ts_of(&w[*k]))
        })
        .and_then(iso);
    Some(win)
}

/// OpenCode Go `GET /zen/go/v1/usage` (and the console `go/status` meters).
pub fn parse_opencode_go(v: &Value) -> Result<Reading, FetchError> {
    let mut r = Reading::default();
    let slots = [
        ("rolling", "5h", "Rolling 5-hour", "5h"),
        ("weekly", "7d", "Weekly", "7d"),
        ("monthly", "month", "Monthly", "month"),
    ];
    let usage = &v["usage"];
    let legacy = |k: &str| v[format!("{k}Usage")].clone();
    let meters = &v["access"]["meters"];
    for (key, id, label, period) in slots {
        let w = if usage[key].is_object() {
            usage[key].clone()
        } else if legacy(key).is_object() {
            legacy(key)
        } else {
            let meter = match key {
                "rolling" => &meters["fiveHour"],
                "weekly" => &meters["week"],
                _ => &meters["month"],
            };
            let mut m = meter.clone();
            if key == "monthly" && m.is_object() && m.get("resetsAt").is_none_or(Value::is_null) {
                m["resetsAt"] = v["access"]["endsAt"].clone();
            }
            m
        };
        if let Some(win) = opencode_window(&w, id, label, period) {
            r.windows.push(win);
        }
    }
    // A missing rolling window means the shape is not understood; never guess zeros.
    if !r.windows.iter().any(|w| w.id == "5h") {
        return Err(FetchError::Parse);
    }
    Ok(r)
}

/// Cursor `GET /api/usage-summary`. Percent fields are already percentages (0.36 means 0.36%);
/// amounts are cents.
pub fn parse_cursor_summary(v: &Value) -> Result<Reading, FetchError> {
    if !v.is_object()
        || !(v["individualUsage"].is_object()
            || v["teamUsage"].is_object()
            || v["billingCycleEnd"].is_string())
    {
        return Err(FetchError::Parse);
    }
    let start = ts_of(&v["billingCycleStart"]).and_then(iso);
    let end = ts_of(&v["billingCycleEnd"]).and_then(iso);
    let mut r = Reading {
        plan: v["membershipType"].as_str().map(|s| short(s, 40)),
        ..Default::default()
    };
    let cents = |x: &Value| num(x).map(|c| c / 100.0);
    let cycle = |mut w: Window| {
        w.reset_at = end.clone();
        w.period = Some("billing_cycle".into());
        w
    };
    let plan = &v["individualUsage"]["plan"];
    if let Some(p) = num(&plan["totalPercentUsed"]) {
        r.windows
            .push(cycle(percent_window("plan", "Included usage", p)));
    }
    if let Some(p) = num(&plan["autoPercentUsed"]) {
        r.windows.push(cycle(percent_window(
            "plan:cursor_models",
            "Auto and Cursor models",
            p,
        )));
    }
    if let Some(p) = num(&plan["apiPercentUsed"]) {
        r.windows.push(cycle(percent_window(
            "plan:other_models",
            "Third-party models",
            p,
        )));
    }
    let (plan_used, plan_limit) = (cents(&plan["used"]), cents(&plan["limit"]));
    if plan_used.is_some() || plan_limit.is_some() {
        r.windows.push(cycle(usd_window(
            "plan:usd",
            "Included usage (USD)",
            plan_used,
            plan_limit,
        )));
    }
    let mut cost = |label: &str, amount: Option<f64>, kind: &str| {
        if let Some(a) = amount {
            r.reported_costs.push(ReportedCost {
                label: label.into(),
                currency: "USD".into(),
                amount: round6(a),
                period_start: start.clone(),
                period_end: end.clone(),
                kind: kind.into(),
            });
        }
    };
    cost("Included usage this cycle", plan_used, "included");
    let od = &v["individualUsage"]["onDemand"];
    let (od_used, od_limit) = (cents(&od["used"]), cents(&od["limit"]));
    cost("On-demand usage this cycle", od_used, "on_demand");
    if od["enabled"] == true || od_used.is_some_and(|u| u > 0.0) {
        r.windows.push(cycle(usd_window(
            "on_demand",
            "On-demand",
            od_used,
            od_limit,
        )));
    }
    let overall = &v["individualUsage"]["overall"];
    if overall.is_object() && (overall["used"].is_number() || overall["limit"].is_number()) {
        r.windows.push(cycle(usd_window(
            "personal_cap",
            "Personal spend cap",
            cents(&overall["used"]),
            cents(&overall["limit"]),
        )));
    }
    let team_od = &v["teamUsage"]["onDemand"];
    if team_od.is_object() && (team_od["used"].is_number() || team_od["limit"].is_number()) {
        r.windows.push(cycle(usd_window(
            "team:on_demand",
            "Team on-demand",
            cents(&team_od["used"]),
            cents(&team_od["limit"]),
        )));
    }
    let pooled = &v["teamUsage"]["pooled"];
    if pooled.is_object() && (pooled["used"].is_number() || pooled["limit"].is_number()) {
        r.windows.push(cycle(usd_window(
            "team:pooled",
            "Team pooled usage",
            cents(&pooled["used"]),
            cents(&pooled["limit"]),
        )));
    }
    if v["isUnlimited"] == true {
        r.notes
            .push("Cursor reports this plan as unlimited.".into());
    }
    Ok(r)
}

/// Windows for Antigravity per-model quota. A missing fraction stays unknown, never 0.
pub fn antigravity_windows(quotas: &[ModelQuota]) -> Vec<Window> {
    quotas
        .iter()
        .take(200)
        .map(|q| {
            let f = q
                .remaining_fraction
                .filter(|f| f.is_finite())
                .map(|f| f.clamp(0.0, 1.0));
            Window {
                id: format!("model:{}", slug(&q.model)),
                label: short(
                    if q.label.is_empty() {
                        &q.model
                    } else {
                        &q.label
                    },
                    80,
                ),
                unit: "percent".into(),
                used: f.map(|f| round6((1.0 - f) * 100.0)),
                limit: Some(100.0),
                remaining: f.map(|f| round6(f * 100.0)),
                reset_at: q.reset_at.and_then(iso),
                model: Some(short(&q.model, 120)),
                scope: "model".into(),
                period: None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Fetchers
// ---------------------------------------------------------------------------------------------

async fn fetch(app: &App, c: &Arc<Collector>, d: &Desc) -> Result<Reading, FetchError> {
    match &d.target {
        Target::Codex(conn) => {
            with_connection(app, conn.clone(), |c2| async move {
                codex_usage(app, &codex_base(&c2.base_url), &c2.api_key, &c2.account_id).await
            })
            .await
        }
        Target::Claude(conn) => {
            with_connection(app, conn.clone(), |c2| async move {
                claude_usage(app, &claude_base(&c2.base_url), &c2.api_key).await
            })
            .await
        }
        Target::Antigravity(conn) => {
            let Some(f) = ANTIGRAVITY.get() else {
                return Err(FetchError::Unavailable(MSG_AGY_HELPER));
            };
            match f(app.clone(), conn.clone()).await {
                Ok(q) => Ok(Reading {
                    windows: antigravity_windows(&q),
                    ..Default::default()
                }),
                Err(401 | 403) => Err(FetchError::Auth(401)),
                Err(429) => Err(FetchError::RateLimited(None)),
                Err(0) => Err(FetchError::Network),
                Err(s) => Err(FetchError::Http(s)),
            }
        }
        Target::OpenCodeGo(conn) => {
            let monitor = Monitor {
                provider: "opencode_go".into(),
                credential_source: "api_key".into(),
                credential: conn.api_key.clone(),
                identity: hash(&format!("opencode_go|{}", conn.api_key)),
                ..Default::default()
            };
            fetch_monitor(app, c, &monitor).await
        }
        Target::GatewayOnly(m) => Err(FetchError::Unavailable(m)),
        Target::Monitor(m) => fetch_monitor(app, c, m).await,
    }
}

/// Renews the connection's token through the credential module (read-only for imported
/// sign-ins), and on a 401 retries once with a forced renewal.
async fn with_connection<'a, F, Fut>(
    app: &'a App,
    mut conn: Connection,
    call: F,
) -> Result<Reading, FetchError>
where
    F: Fn(Connection) -> Fut,
    Fut: std::future::Future<Output = Result<Reading, FetchError>> + 'a,
{
    credentials::refresh(app, &mut conn)
        .await
        .map_err(|_| FetchError::Auth(401))?;
    match call(conn.clone()).await {
        Err(FetchError::Auth(401)) if conn.oauth => {
            credentials::refresh_forced(app, &mut conn)
                .await
                .map_err(|_| FetchError::Auth(401))?;
            call(conn).await
        }
        other => other,
    }
}

fn codex_base(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    b.strip_suffix("/codex").unwrap_or(b).to_string()
}
fn claude_base(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

async fn codex_usage(
    app: &App,
    base: &str,
    token: &str,
    account_id: &str,
) -> Result<Reading, FetchError> {
    let mut rb = app
        .client
        .get(format!("{base}/wham/usage"))
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("user-agent", "switchyard");
    if !account_id.is_empty() {
        rb = rb.header("ChatGPT-Account-Id", account_id);
    }
    parse_codex(&send_json(rb).await?)
}

async fn claude_usage(app: &App, base: &str, token: &str) -> Result<Reading, FetchError> {
    let rb = app
        .client
        .get(format!("{base}/api/oauth/usage"))
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "claude-code/2.1.0");
    parse_claude(&send_json(rb).await?)
}

async fn fetch_monitor(app: &App, c: &Arc<Collector>, m: &Monitor) -> Result<Reading, FetchError> {
    match (m.provider.as_str(), m.credential_source.as_str()) {
        ("cursor", "api_key") => cursor_team_spend(app, c, &m.credential).await,
        ("cursor", _) => {
            let (cookie, identity) = cursor_cookie(m).await?;
            check_identity(m, &identity)?;
            let rb = app
                .client
                .get(format!("{}/api/usage-summary", endpoint(c, "cursor")))
                .header("cookie", cookie)
                .header("accept", "application/json");
            parse_cursor_summary(&send_json(rb).await?)
        }
        ("opencode" | "opencode_go", "cookie") => opencode_console(app, c, &m.credential).await,
        ("opencode", _) => Err(FetchError::Unavailable(MSG_OPENCODE_ZEN_KEY)),
        ("opencode_go", _) => {
            let (key, identity) = opencode_go_key(m).await?;
            check_identity(m, &identity)?;
            let rb = app
                .client
                .get(format!("{}/zen/go/v1/usage", endpoint(c, "opencode")))
                .bearer_auth(key)
                .header("accept", "application/json");
            parse_opencode_go(&send_json(rb).await?)
        }
        ("codex", _) => {
            let f = codex_file(m.source_path.as_deref()).await?;
            check_identity(m, &f.identity)?;
            not_connected(app, &f.token)?;
            codex_usage(app, &endpoint(c, "codex"), &f.token, &f.account_id).await
        }
        ("claude", _) => {
            let f = claude_file(m.source_path.as_deref()).await?;
            not_connected(app, &f.token)?;
            claude_usage(app, &endpoint(c, "claude"), &f.token).await
        }
        ("openai", "api_key") => openai_costs(app, c, &m.credential).await,
        ("anthropic", "api_key") => anthropic_costs(app, c, &m.credential).await,
        ("gemini", _) => Err(FetchError::Unavailable(MSG_GEMINI)),
        _ => Err(FetchError::Unavailable(MSG_NO_COST_API)),
    }
}

fn check_identity(m: &Monitor, identity: &str) -> Result<(), FetchError> {
    // A native session (default location or a user-chosen file) must still be the account
    // that was imported; cookies and keys entered by hand are the account by definition.
    if matches!(m.credential_source.as_str(), "native" | "file")
        && !m.identity.is_empty()
        && !identity.is_empty()
        && m.identity != identity
    {
        return Err(FetchError::IdentityChanged);
    }
    Ok(())
}
/// A CLI sign-in that is also a gateway connection is already read through that connection.
fn not_connected(app: &App, token: &str) -> Result<(), FetchError> {
    let connected = app
        .store
        .list::<Connection>("connection")
        .iter()
        .any(|c| c.enabled && !c.api_key.is_empty() && c.api_key == token);
    if connected {
        Err(FetchError::Duplicate)
    } else {
        Ok(())
    }
}

async fn openai_costs(app: &App, c: &Arc<Collector>, key: &str) -> Result<Reading, FetchError> {
    let start = month_start();
    let mut totals: HashMap<String, f64> = HashMap::new();
    let mut page: Option<String> = None;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let mut query = vec![
            ("start_time", start.timestamp().to_string()),
            ("bucket_width", "1d".into()),
            ("limit", "31".into()),
        ];
        if let Some(p) = &page {
            query.push(("page", p.clone()));
        }
        let rb = app
            .client
            .get(format!("{}/v1/organization/costs", endpoint(c, "openai")))
            .query(&query)
            .bearer_auth(key);
        let v = send_json(rb).await?;
        let Some(buckets) = v["data"].as_array() else {
            return Err(FetchError::Parse);
        };
        for result in buckets
            .iter()
            .flat_map(|b| b["results"].as_array().into_iter().flatten())
        {
            let Some(value) = num(&result["amount"]["value"]) else {
                continue;
            };
            let currency = result["amount"]["currency"]
                .as_str()
                .unwrap_or("usd")
                .to_ascii_uppercase();
            *totals.entry(short(&currency, 3)).or_default() += value;
        }
        page = v["next_page"]
            .as_str()
            .filter(|_| v["has_more"] == true)
            .map(String::from);
        if page.is_none() {
            complete = true;
            break;
        }
    }
    org_costs(totals, start, complete, "Organization cost, month to date")
}

async fn anthropic_costs(app: &App, c: &Arc<Collector>, key: &str) -> Result<Reading, FetchError> {
    let start = month_start();
    let mut totals: HashMap<String, f64> = HashMap::new();
    let mut page: Option<String> = None;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let mut query = vec![
            (
                "starting_at",
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            ("bucket_width", "1d".into()),
            ("limit", "31".into()),
        ];
        if let Some(p) = &page {
            query.push(("page", p.clone()));
        }
        let rb = app
            .client
            .get(format!(
                "{}/v1/organizations/cost_report",
                endpoint(c, "anthropic_admin")
            ))
            .query(&query)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
        let v = send_json(rb).await?;
        let Some(buckets) = v["data"].as_array() else {
            return Err(FetchError::Parse);
        };
        for result in buckets
            .iter()
            .flat_map(|b| b["results"].as_array().into_iter().flatten())
        {
            // `amount` is a decimal string in the lowest currency unit (cents for USD).
            let Some(cents) = num(&result["amount"]) else {
                continue;
            };
            let currency = result["currency"]
                .as_str()
                .unwrap_or("USD")
                .to_ascii_uppercase();
            *totals.entry(short(&currency, 3)).or_default() += cents / 100.0;
        }
        page = v["next_page"]
            .as_str()
            .filter(|_| v["has_more"] == true)
            .map(String::from);
        if page.is_none() {
            complete = true;
            break;
        }
    }
    org_costs(totals, start, complete, "Organization cost, month to date")
}

fn org_costs(
    totals: HashMap<String, f64>,
    start: chrono::DateTime<chrono::Utc>,
    complete: bool,
    label: &str,
) -> Result<Reading, FetchError> {
    if !complete {
        // A partial sum would understate spend; report nothing rather than a wrong total.
        return Err(FetchError::Parse);
    }
    let mut r = Reading::default();
    let mut totals: Vec<_> = totals.into_iter().collect();
    if totals.is_empty() {
        totals.push(("USD".into(), 0.0));
    }
    totals.sort_by(|a, b| a.0.cmp(&b.0));
    for (currency, amount) in totals {
        r.reported_costs.push(ReportedCost {
            label: label.into(),
            currency,
            amount: round6(amount),
            period_start: Some(start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            period_end: Some(now()),
            kind: "billed".into(),
        });
    }
    r.notes
        .push("Provider cost reports can lag by up to a day.".into());
    Ok(r)
}

/// Cursor Teams Admin API `POST /teams/spend` (Basic auth with the team API key).
async fn cursor_team_spend(
    app: &App,
    c: &Arc<Collector>,
    key: &str,
) -> Result<Reading, FetchError> {
    let (mut on_demand, mut overall) = (0f64, Some(0f64));
    let mut cycle_start = None;
    let mut complete = false;
    for page in 1..=20 {
        let rb = app
            .client
            .post(format!("{}/teams/spend", endpoint(c, "cursor_admin")))
            .basic_auth(key, Some(""))
            .json(&json!({"page":page,"pageSize":100}));
        let v = send_json(rb).await?;
        let Some(members) = v["teamMemberSpend"].as_array() else {
            return Err(FetchError::Parse);
        };
        for m in members {
            on_demand += num(&m["spendCents"]).unwrap_or(0.0) / 100.0;
            overall = match (overall, num(&m["overallSpendCents"])) {
                (Some(t), Some(x)) => Some(t + x / 100.0),
                _ => None,
            };
        }
        cycle_start = cycle_start.or_else(|| ts_of(&v["subscriptionCycleStart"]));
        let pages = v["totalPages"].as_u64().unwrap_or(1);
        if page as u64 >= pages || members.is_empty() {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(FetchError::Parse);
    }
    let start = cycle_start.and_then(iso);
    let mut r = Reading::default();
    r.reported_costs.push(ReportedCost {
        label: "Team on-demand spend this cycle".into(),
        currency: "USD".into(),
        amount: round6(on_demand),
        period_start: start.clone(),
        period_end: Some(now()),
        kind: "on_demand".into(),
    });
    if let Some(total) = overall.filter(|t| *t >= on_demand) {
        r.reported_costs.push(ReportedCost {
            label: "Team included usage this cycle".into(),
            currency: "USD".into(),
            amount: round6(total - on_demand),
            period_start: start,
            period_end: Some(now()),
            kind: "included".into(),
        });
    }
    Ok(r)
}

/// OpenCode console (cookie): workspace list, Go meters and Zen balance.
async fn opencode_console(
    app: &App,
    c: &Arc<Collector>,
    cookie: &str,
) -> Result<Reading, FetchError> {
    let base = endpoint(c, "opencode");
    let orgs = send_json(
        app.client
            .get(format!("{base}/console/api/orgs"))
            .header("cookie", cookie)
            .header("accept", "application/json"),
    )
    .await?;
    let rows = orgs
        .as_array()
        .or_else(|| orgs["data"].as_array())
        .ok_or(FetchError::Parse)?;
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["id"].as_str())
        .filter(|id| {
            (id.starts_with("wrk_") || id.starts_with("org_"))
                && id.len() < 100
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .collect();
    let Some(workspace) = ids.first().copied() else {
        return Err(FetchError::Parse);
    };
    let get = |path: &str| {
        app.client
            .get(format!("{base}{path}"))
            .header("cookie", cookie)
            .header("x-org-id", workspace)
            .header("accept", "application/json")
    };
    let mut r = Reading::default();
    let go = send_json(get("/console/api/go/status")).await;
    match &go {
        Ok(v) if v.is_null() || v["access"].is_null() => {}
        Ok(v) => {
            let parsed = parse_opencode_go(v)?;
            r.windows = parsed.windows;
        }
        Err(_) => {}
    }
    let billing = send_json(get("/console/api/billing/status")).await;
    if let Ok(b) = &billing
        && let Some(balance) = num(&b["balanceMicroCents"])
    {
        r.balances.push(Balance {
            label: "Zen balance".into(),
            unit: "usd".into(),
            currency: Some("USD".into()),
            value: round6(balance / 1e8),
        });
    }
    if ids.len() > 1 {
        r.notes
            .push("Showing the first of several OpenCode workspaces.".into());
    }
    match (go, billing) {
        (Err(e), Err(_)) => Err(e),
        _ => Ok(r),
    }
}

// ---------------------------------------------------------------------------------------------
// Native credentials (read-only; never refreshed or written)
// ---------------------------------------------------------------------------------------------

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
pub(crate) fn opencode_data_dir() -> Option<PathBuf> {
    env_dir("XDG_DATA_HOME")
        .map(|d| d.join("opencode"))
        .or_else(|| home().map(|h| h.join(".local/share/opencode")))
}
pub(crate) fn codex_home() -> Option<PathBuf> {
    env_dir("CODEX_HOME").or_else(|| home().map(|h| h.join(".codex")))
}
pub(crate) fn claude_home() -> Option<PathBuf> {
    env_dir("CLAUDE_CONFIG_DIR").or_else(|| home().map(|h| h.join(".claude")))
}
fn cursor_state_db() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
    } else if cfg!(windows) {
        env_dir("APPDATA").map(|d| d.join("Cursor/User/globalStorage/state.vscdb"))
    } else {
        env_dir("XDG_CONFIG_HOME")
            .or_else(|| home().map(|h| h.join(".config")))
            .map(|d| d.join("Cursor/User/globalStorage/state.vscdb"))
    }
}
/// The Cursor CLI (`cursor-agent`) credential file, used when no Keychain entry exists.
fn cursor_cli_auth() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home().map(|h| h.join(".cursor/auth.json"))
    } else if cfg!(windows) {
        env_dir("APPDATA").map(|d| d.join("Cursor/auth.json"))
    } else {
        env_dir("XDG_CONFIG_HOME")
            .or_else(|| home().map(|h| h.join(".config")))
            .map(|d| d.join("cursor/auth.json"))
    }
}

async fn read_json_file(p: &FsPath) -> Result<Value, FetchError> {
    let meta = tokio::fs::metadata(p)
        .await
        .map_err(|_| FetchError::NotFound)?;
    if !meta.is_file() || meta.len() > MAX_NATIVE_FILE {
        return Err(FetchError::NotFound);
    }
    let bytes = tokio::fs::read(p).await.map_err(|_| FetchError::NotFound)?;
    serde_json::from_slice(&bytes).map_err(|_| FetchError::Parse)
}

struct NativeToken {
    token: String,
    account_id: String,
    identity: String,
}

fn jwt_exp(token: &str) -> Option<i64> {
    credentials::jwt_claims(token).and_then(|c| ts_of(&c["exp"]))
}
fn not_expired(exp: Option<i64>) -> Result<(), FetchError> {
    if exp.is_some_and(|e| e <= ts_now() + 60) {
        Err(FetchError::Expired)
    } else {
        Ok(())
    }
}

async fn codex_file(path: Option<&str>) -> Result<NativeToken, FetchError> {
    let p = match path {
        Some(p) => PathBuf::from(p),
        None => codex_home().ok_or(FetchError::NotFound)?.join("auth.json"),
    };
    let v = read_json_file(&p).await?;
    let token = v["tokens"]["access_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err(
            if v["OPENAI_API_KEY"].as_str().is_some_and(|k| !k.is_empty()) {
                FetchError::Unavailable(MSG_CODEX_API_KEY)
            } else {
                FetchError::NotFound
            },
        );
    }
    not_expired(jwt_exp(&token))?;
    let claims = credentials::jwt_claims(&token).unwrap_or(Value::Null);
    let account_id = v["tokens"]["account_id"]
        .as_str()
        .or(claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .unwrap_or("")
        .to_string();
    let user = claims["sub"].as_str().unwrap_or("");
    Ok(NativeToken {
        identity: if user.is_empty() {
            String::new()
        } else {
            hash(&format!("codex|{user}|{account_id}"))
        },
        token,
        account_id,
    })
}

async fn claude_file(path: Option<&str>) -> Result<NativeToken, FetchError> {
    let p = match path {
        Some(p) => PathBuf::from(p),
        None => claude_home()
            .ok_or(FetchError::NotFound)?
            .join(".credentials.json"),
    };
    let v = read_json_file(&p).await?;
    let o = &v["claudeAiOauth"];
    let token = o["accessToken"].as_str().unwrap_or("").to_string();
    if token.is_empty() {
        return Err(FetchError::NotFound);
    }
    not_expired(ts_of(&o["expiresAt"]))?;
    // The credential file carries no stable account id, so no identity is claimed.
    Ok(NativeToken {
        token,
        account_id: String::new(),
        identity: String::new(),
    })
}

async fn opencode_go_key(m: &Monitor) -> Result<(String, String), FetchError> {
    if m.credential_source == "api_key" {
        let key = m.credential.trim().to_string();
        if key.is_empty() {
            return Err(FetchError::NotFound);
        }
        let identity = hash(&format!("opencode_go|{key}"));
        return Ok((key, identity));
    }
    let p = match &m.source_path {
        Some(p) => PathBuf::from(p),
        None => opencode_data_dir()
            .ok_or(FetchError::NotFound)?
            .join("auth.json"),
    };
    let v = read_json_file(&p).await?;
    let rec = &v["opencode-go"];
    let key = rec["key"]
        .as_str()
        .filter(|_| rec["type"] == "api")
        .unwrap_or("")
        .trim()
        .to_string();
    if key.is_empty() {
        return Err(FetchError::NotFound);
    }
    let identity = hash(&format!("opencode_go|{key}"));
    Ok((key, identity))
}

/// Decodes a VS Code state value (UTF-8, or BOM-less UTF-16LE) and strips JSON quotes.
fn decode_state_value(bytes: &[u8]) -> Option<String> {
    let s = if bytes.len() >= 4 && bytes.len().is_multiple_of(2) && bytes[1] == 0 && bytes[3] == 0 {
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).ok()?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    let s = s.trim().trim_matches('"').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Opens a SQLite database read-only without creating files next to it.
pub(crate) fn open_readonly(path: &FsPath) -> Result<rusqlite::Connection, FetchError> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return Err(FetchError::NotFound);
    }
    let mut url = url::Url::from_file_path(path).map_err(|_| FetchError::NotFound)?;
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    // An idle WAL database without sidecars is opened immutable so SQLite creates nothing.
    url.set_query(Some(if wal.exists() {
        "mode=ro"
    } else {
        "mode=ro&immutable=1"
    }));
    let db = rusqlite::Connection::open_with_flags(
        url.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| FetchError::NotFound)?;
    let _ = db.busy_timeout(Duration::from_secs(2));
    Ok(db)
}

fn cursor_token_from_db(path: PathBuf) -> Result<String, FetchError> {
    let db = open_readonly(&path)?;
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT CAST(value AS BLOB) FROM ItemTable WHERE key='cursorAuth/accessToken'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| FetchError::NotFound)?;
    if bytes.len() > 64 * 1024 {
        return Err(FetchError::Parse);
    }
    decode_state_value(&bytes).ok_or(FetchError::NotFound)
}

#[cfg(target_os = "macos")]
async fn cursor_token_from_keychain() -> Result<String, FetchError> {
    // Reads exactly one item (the Cursor CLI access token); never dumps the keychain.
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "cursor-access-token",
                "-a",
                "cursor-user",
                "-w",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| FetchError::NotFound)?
    .map_err(|_| FetchError::NotFound)?;
    if !out.status.success() || out.stdout.len() > 64 * 1024 {
        return Err(FetchError::NotFound);
    }
    let token = String::from_utf8(out.stdout)
        .map_err(|_| FetchError::NotFound)?
        .trim()
        .to_string();
    if token.is_empty() {
        Err(FetchError::NotFound)
    } else {
        Ok(token)
    }
}
#[cfg(not(target_os = "macos"))]
async fn cursor_token_from_keychain() -> Result<String, FetchError> {
    Err(FetchError::NotFound)
}

async fn cursor_token_from_file(path: &FsPath) -> Result<String, FetchError> {
    let v = read_json_file(path).await?;
    v["accessToken"]
        .as_str()
        .or(v["access_token"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or(FetchError::NotFound)
}

/// Finds the native Cursor access token: an explicit path (app `state.vscdb` or CLI
/// `auth.json`), else the Cursor app database, else the Cursor CLI's Keychain item or file.
async fn cursor_native_token(path: Option<&str>) -> Result<String, FetchError> {
    if let Some(p) = path {
        let p = PathBuf::from(p);
        return if p.extension().is_some_and(|e| e == "vscdb") {
            tokio::task::spawn_blocking(move || cursor_token_from_db(p))
                .await
                .map_err(|_| FetchError::NotFound)?
        } else {
            cursor_token_from_file(&p).await
        };
    }
    if let Some(db) = cursor_state_db().filter(|p| p.is_file())
        && let Ok(t) = tokio::task::spawn_blocking(move || cursor_token_from_db(db))
            .await
            .map_err(|_| FetchError::NotFound)?
    {
        return Ok(t);
    }
    if let Ok(t) = cursor_token_from_keychain().await {
        return Ok(t);
    }
    match cursor_cli_auth() {
        Some(p) => cursor_token_from_file(&p).await,
        None => Err(FetchError::NotFound),
    }
}

/// Builds the dashboard cookie from a Cursor access token. The user id is the last `|` part of
/// the JWT `sub`. Returns `(cookie, identity hash)`.
fn cursor_cookie_from_token(token: &str) -> Result<(String, String), FetchError> {
    let claims = credentials::jwt_claims(token).ok_or(FetchError::Parse)?;
    let sub = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(FetchError::Parse)?;
    let user = sub.rsplit('|').next().unwrap_or(sub);
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(FetchError::Parse);
    }
    not_expired(ts_of(&claims["exp"]))?;
    Ok((
        format!("WorkosCursorSessionToken={user}%3A%3A{token}"),
        hash(&format!("cursor|{sub}")),
    ))
}

/// The cookie header and identity hash for a Cursor monitor.
pub(crate) async fn cursor_cookie(m: &Monitor) -> Result<(String, String), FetchError> {
    match m.credential_source.as_str() {
        "cookie" => {
            let raw = m.credential.trim();
            if raw.is_empty() {
                return Err(FetchError::NotFound);
            }
            let session = raw
                .split(';')
                .find_map(|p| p.trim().strip_prefix("WorkosCursorSessionToken="))
                .or((!raw.contains('=')).then_some(raw));
            match session {
                Some(value) => {
                    let jwt = value
                        .split("%3A%3A")
                        .nth(1)
                        .or_else(|| value.split("::").nth(1))
                        .unwrap_or(value);
                    match cursor_cookie_from_token(jwt) {
                        Ok((cookie, identity)) if value == jwt => Ok((cookie, identity)),
                        Ok((_, identity)) => Ok((
                            if raw.contains('=') {
                                raw.to_string()
                            } else {
                                format!("WorkosCursorSessionToken={value}")
                            },
                            identity,
                        )),
                        Err(FetchError::Expired) => Err(FetchError::Expired),
                        Err(_) => Ok((
                            if raw.contains('=') {
                                raw.to_string()
                            } else {
                                format!("WorkosCursorSessionToken={value}")
                            },
                            hash(&format!("cursor-cookie|{value}")),
                        )),
                    }
                }
                None => Ok((raw.to_string(), hash(&format!("cursor-cookie|{raw}")))),
            }
        }
        _ => {
            let token = cursor_native_token(m.source_path.as_deref()).await?;
            cursor_cookie_from_token(&token)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP API
// ---------------------------------------------------------------------------------------------

/// Usage-source and native-history routes. Merge into the admin router (behind `admin_auth`).
pub fn router() -> Router<App> {
    Router::new()
        .route("/api/usage/sources", get(sources))
        .route("/api/usage/refresh", post(refresh))
        .route("/api/usage/providers", get(providers))
        .route(
            "/api/usage/monitors",
            get(list_monitors).post(create_monitor),
        )
        .route(
            "/api/usage/monitors/{id}",
            put(update_monitor).delete(delete_monitor),
        )
        .route("/api/usage/import", post(import))
        .merge(crate::native_usage::router())
}

fn public_source(d: &Desc, s: Option<&Snapshot>, in_flight: bool, poller: bool) -> Value {
    let s = s.filter(|s| s.version == d.version);
    let now_ts = ts_now();
    let (status, message): (String, Option<String>) = if !d.enabled {
        ("disabled".into(), None)
    } else if let Target::GatewayOnly(m) = d.target {
        ("unavailable".into(), Some(m.into()))
    } else if d.duplicate {
        ("unavailable".into(), Some(MSG_DUPLICATE.into()))
    } else if let Some(s) = s {
        let stale = s.status == "ok"
            && parse_ts(&s.last_success_at).is_none_or(|t| now_ts - t > STALE_AFTER_SECS);
        if stale {
            (
                "stale".into(),
                Some("This data is more than 10 minutes old.".into()),
            )
        } else {
            (s.status.clone(), s.message.clone())
        }
    } else if in_flight {
        ("refreshing".into(), None)
    } else {
        ("unavailable".into(), Some(MSG_NOT_YET.into()))
    };
    let reading = s.map(|s| s.reading.clone()).unwrap_or_default();
    let next = if d.fetchable() {
        s.and_then(|s| s.retry_at)
            .or_else(|| {
                poller
                    .then(|| {
                        s.and_then(|s| parse_ts(&s.last_attempt_at))
                            .map(|t| t + POLL_INTERVAL.as_secs() as i64)
                    })
                    .flatten()
            })
            .and_then(iso)
    } else {
        None
    };
    let mut notes: Vec<String> = d.notes.iter().map(|n| n.to_string()).collect();
    notes.extend(reading.notes.iter().cloned());
    json!({
        "id": d.id, "connection_id": d.connection_id, "name": d.name, "provider": d.provider,
        "status": status, "updated_at": s.and_then(|s| s.last_success_at.clone()),
        "next_refresh_at": next, "message": message,
        "windows": reading.windows, "balances": reading.balances, "reported_costs": reading.reported_costs,
        "capabilities": {"quota": d.caps[0], "cost": d.caps[1], "tokens": d.caps[2], "history": d.caps[3]},
        "source": d.source, "connection_name": d.connection_name,
        "plan": reading.plan, "last_success_at": s.and_then(|s| s.last_success_at.clone()),
        "last_error_at": s.and_then(|s| s.last_error_at.clone()),
        "refreshing": in_flight, "capability_notes": notes,
    })
}

/// Current snapshots for every source; starts a guarded first refresh when needed.
pub fn sources_view(app: &App) -> Value {
    let c = collector(app);
    let all = descs(app);
    let snapshots: HashMap<String, Snapshot> = app
        .store
        .list::<Snapshot>(SNAPSHOT_KIND)
        .into_iter()
        .map(|s| (s.source_id.clone(), s))
        .collect();
    let missing = all
        .iter()
        .any(|d| d.fetchable() && snapshots.get(&d.id).is_none_or(|s| s.version != d.version));
    if !c.initial.swap(true, Ordering::SeqCst) || (missing && !c.refreshing.load(Ordering::SeqCst))
    {
        trigger(app, None, false);
    }
    let in_flight = c.in_flight.lock().expect("in flight").clone();
    let poller = c.started.load(Ordering::SeqCst);
    let list: Vec<Value> = all
        .iter()
        .map(|d| public_source(d, snapshots.get(&d.id), in_flight.contains(&d.id), poller))
        .collect();
    json!({"sources": list, "generated_at": now(), "refreshing": c.refreshing.load(Ordering::SeqCst)})
}

async fn sources(State(app): State<App>) -> Json<Value> {
    Json(sources_view(&app))
}

#[derive(Deserialize, Default)]
struct RefreshBody {
    #[serde(default)]
    id: Option<String>,
}
async fn refresh(
    State(app): State<App>,
    body: Option<Json<RefreshBody>>,
) -> Result<Response, ApiError> {
    let only = body
        .and_then(|Json(b)| b.id)
        .filter(|s| !s.trim().is_empty());
    if let Some(id) = &only
        && !descs(&app).iter().any(|d| &d.id == id)
    {
        return Err(ApiError::new(404, "Usage source not found"));
    }
    let c = collector(&app);
    let key = only.clone().unwrap_or_else(|| "*".into());
    let min = *c.manual_interval.lock().expect("interval");
    {
        let mut manual = c.manual.lock().expect("manual");
        if manual.len() > 512 {
            manual.retain(|_, t| t.elapsed() < min);
        }
        if let Some(t) = manual.get(&key)
            && t.elapsed() < min
        {
            let wait = (min - t.elapsed()).as_secs().max(1);
            return Ok((StatusCode::OK, Json(json!({"accepted":false,"message":"This source was refreshed less than 30 seconds ago. Showing the latest results.","retry_after_seconds":wait}))).into_response());
        }
        manual.insert(key, Instant::now());
    }
    c.initial.store(true, Ordering::SeqCst);
    let started = trigger(&app, only, true);
    let message = if started {
        "Refresh queued; poll sources for results."
    } else {
        "A refresh is already running; this request was queued after it."
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted":true,"message":message})),
    )
        .into_response())
}

async fn providers() -> Json<Value> {
    let detected = |p: Option<PathBuf>| p.is_some_and(|p| p.exists());
    let cursor_native = detected(cursor_state_db())
        || detected(cursor_cli_auth())
        || detected(home().map(|h| h.join(".cursor")));
    Json(json!([
        {"provider":"codex","label":"Codex","credential_sources":["native","file"],"native_detected":detected(codex_home().map(|h| h.join("auth.json"))),"import_available":true,"capabilities":{"quota":true,"cost":false,"tokens":true,"history":true},"notes":["Connected Codex sign-ins are monitored automatically."]},
        {"provider":"claude","label":"Claude","credential_sources":["native","file"],"native_detected":detected(claude_home().map(|h| h.join(".credentials.json"))),"import_available":true,"capabilities":{"quota":true,"cost":false,"tokens":true,"history":true},"notes":["Connected Claude sign-ins are monitored automatically."]},
        {"provider":"cursor","label":"Cursor","credential_sources":["native","file","cookie","api_key"],"native_detected":cursor_native,"import_available":true,"capabilities":{"quota":true,"cost":true,"tokens":true,"history":true},"notes":["Cursor's own models are not available as an API through Switchyard.","An API key here means a Cursor Teams Admin API key."]},
        {"provider":"opencode_go","label":"OpenCode Go","credential_sources":["native","file","api_key","cookie"],"native_detected":detected(opencode_data_dir().map(|d| d.join("auth.json"))),"import_available":true,"capabilities":{"quota":true,"cost":false,"tokens":true,"history":true},"notes":[]},
        {"provider":"opencode","label":"OpenCode Zen","credential_sources":["cookie"],"native_detected":false,"import_available":false,"capabilities":{"quota":false,"cost":true,"tokens":true,"history":true},"notes":[MSG_OPENCODE_ZEN_KEY]},
        {"provider":"antigravity","label":"Antigravity","credential_sources":[],"native_detected":false,"import_available":false,"capabilities":{"quota":true,"cost":false,"tokens":true,"history":false},"notes":["Connected Antigravity accounts are monitored automatically."]},
        {"provider":"openai","label":"OpenAI organization","credential_sources":["api_key"],"native_detected":false,"import_available":false,"capabilities":{"quota":false,"cost":true,"tokens":false,"history":false},"notes":[MSG_ADMIN_KEY]},
        {"provider":"anthropic","label":"Anthropic organization","credential_sources":["api_key"],"native_detected":false,"import_available":false,"capabilities":{"quota":false,"cost":true,"tokens":false,"history":false},"notes":[MSG_ADMIN_KEY]},
        {"provider":"gemini","label":"Gemini","credential_sources":[],"native_detected":false,"import_available":false,"capabilities":{"quota":false,"cost":false,"tokens":true,"history":false},"notes":[MSG_GEMINI]}
    ]))
}

async fn list_monitors(State(app): State<App>) -> Json<Value> {
    let mut monitors: Vec<Monitor> = app.store.list(MONITOR_KIND);
    monitors.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Json(Value::Array(monitors.iter().map(Monitor::public).collect()))
}

const SOURCES_FOR: &[(&str, &[&str])] = &[
    ("cursor", &["native", "file", "cookie", "api_key"]),
    ("opencode", &["cookie"]),
    ("opencode_go", &["native", "file", "api_key", "cookie"]),
    ("codex", &["native", "file"]),
    ("claude", &["native", "file"]),
    ("openai", &["api_key"]),
    ("anthropic", &["api_key"]),
    ("gemini", &["api_key"]),
];

/// Validates a create/update body against `base` (the stored monitor on update).
fn apply_input(app: &App, body: &Value, base: Option<&Monitor>) -> Result<Monitor, ApiError> {
    let mut m = base.cloned().unwrap_or_default();
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim);
    if let Some(name) = text("name") {
        m.name = name.to_string();
    }
    if let Some(p) = text("provider") {
        m.provider = p.to_string();
    }
    if let Some(s) = text("credential_source") {
        m.credential_source = s.to_string();
    }
    if m.name.is_empty() || m.name.chars().count() > 100 {
        return Err(ApiError::bad("name must be 1 to 100 characters"));
    }
    if m.provider == "antigravity" {
        return Err(ApiError::bad(
            "provider: Antigravity quota appears automatically for connected Antigravity accounts",
        ));
    }
    let Some((_, allowed)) = SOURCES_FOR.iter().find(|(p, _)| *p == m.provider) else {
        return Err(ApiError::bad(
            "provider must be one of cursor, opencode, opencode_go, codex, claude, openai, anthropic, gemini",
        ));
    };
    if !allowed.contains(&m.credential_source.as_str()) {
        return Err(ApiError::bad(
            "credential_source is not supported for this provider",
        ));
    }
    match body.get("source_path") {
        Some(Value::String(p)) if !p.trim().is_empty() => {
            let p = p.trim();
            if p.len() > 1024 || !FsPath::new(p).is_absolute() {
                return Err(ApiError::bad("source_path must be an absolute path"));
            }
            m.source_path = Some(p.into());
        }
        Some(Value::Null) | Some(Value::String(_)) => m.source_path = None,
        Some(_) => return Err(ApiError::bad("source_path must be a string")),
        None => {}
    }
    if m.credential_source == "file" && m.source_path.is_none() {
        return Err(ApiError::bad(
            "source_path is required for a file credential",
        ));
    }
    if !matches!(m.credential_source.as_str(), "native" | "file") {
        m.source_path = None;
    }
    match body.get("connection_id") {
        Some(Value::String(id)) if !id.trim().is_empty() => {
            if app
                .store
                .get::<Connection>("connection", id.trim())
                .is_none()
            {
                return Err(ApiError::bad("connection_id does not match an account"));
            }
            m.connection_id = Some(id.trim().into());
        }
        Some(Value::Null) | Some(Value::String(_)) => m.connection_id = None,
        Some(_) => return Err(ApiError::bad("connection_id must be a string")),
        None => {}
    }
    if let Some(enabled) = body.get("enabled") {
        m.enabled = enabled
            .as_bool()
            .ok_or_else(|| ApiError::bad("enabled must be a boolean"))?;
    } else if base.is_none() {
        m.enabled = true;
    }
    // Omitted credential keeps the stored one; an explicit empty string clears it.
    match body.get("credential") {
        Some(Value::String(s)) => {
            if s.len() > MAX_CREDENTIAL || s.chars().any(|c| c == '\r' || c == '\n' || c == '\0') {
                return Err(ApiError::bad(
                    "credential must be a single line up to 16 KiB",
                ));
            }
            m.credential = s.trim().to_string();
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(ApiError::bad("credential must be a string")),
    }
    if matches!(m.credential_source.as_str(), "native" | "file") {
        m.credential.clear();
    } else if m.credential.is_empty() && base.is_none() {
        return Err(ApiError::bad(
            "credential is required for an API key or cookie",
        ));
    }
    Ok(m)
}

/// Reads the native identity for a monitor (explicitly requested by the user). Errors are
/// constant messages.
async fn native_identity(m: &Monitor) -> Result<String, FetchError> {
    match m.provider.as_str() {
        "cursor" if m.credential_source != "api_key" => cursor_cookie(m).await.map(|(_, i)| i),
        "opencode_go" if m.credential_source != "cookie" => {
            opencode_go_key(m).await.map(|(_, i)| i)
        }
        "codex" => codex_file(m.source_path.as_deref())
            .await
            .map(|f| f.identity),
        "claude" => claude_file(m.source_path.as_deref())
            .await
            .map(|f| f.identity),
        "cursor" | "opencode" | "opencode_go" if m.credential_source == "cookie" => {
            if m.provider == "cursor" {
                cursor_cookie(m).await.map(|(_, i)| i)
            } else {
                Ok(hash(&format!("{}|cookie|{}", m.provider, m.credential)))
            }
        }
        _ => Ok(if m.credential.is_empty() {
            String::new()
        } else {
            hash(&format!("{}|{}", m.provider, m.credential))
        }),
    }
}

fn duplicate_of(app: &App, m: &Monitor) -> Option<Monitor> {
    if m.identity.is_empty() {
        return None;
    }
    app.store
        .list::<Monitor>(MONITOR_KIND)
        .into_iter()
        .find(|x| x.id != m.id && x.provider == m.provider && x.identity == m.identity)
}

async fn create_monitor(
    State(app): State<App>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if app.store.list::<Monitor>(MONITOR_KIND).len() >= MAX_MONITORS {
        return Err(ApiError::bad("At most 100 usage monitors are supported"));
    }
    let mut m = apply_input(&app, &body, None)?;
    m.identity = native_identity(&m)
        .await
        .map_err(|e| ApiError::bad(e.message(false)))?;
    if duplicate_of(&app, &m).is_some() {
        return Err(ApiError::new(409, MSG_DUPLICATE));
    }
    m.id = id();
    m.created_at = now();
    m.updated_at = m.created_at.clone();
    m.version = 1;
    let c = collector(&app);
    {
        let _guard = c.write.lock().await;
        app.store
            .put(MONITOR_KIND, &m.id, &m)
            .map_err(ApiError::db)?;
    }
    if m.enabled {
        trigger(&app, Some(format!("monitor:{}", m.id)), true);
    }
    Ok(Json(m.public()))
}

async fn update_monitor(
    State(app): State<App>,
    Path(mid): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let stored = app
        .store
        .get::<Monitor>(MONITOR_KIND, &mid)
        .ok_or(ApiError::new(404, "Monitor not found"))?;
    let mut m = apply_input(&app, &body, Some(&stored))?;
    if !matches!(m.credential_source.as_str(), "native" | "file")
        && m.credential.is_empty()
        && m.enabled
    {
        return Err(ApiError::bad(
            "credential is required for an enabled API key or cookie monitor",
        ));
    }
    let changed_credential = m.provider != stored.provider
        || m.credential_source != stored.credential_source
        || m.source_path != stored.source_path
        || m.credential != stored.credential;
    if changed_credential && m.enabled {
        m.identity = native_identity(&m)
            .await
            .map_err(|e| ApiError::bad(e.message(false)))?;
        if duplicate_of(&app, &m).is_some() {
            return Err(ApiError::new(409, MSG_DUPLICATE));
        }
    } else if changed_credential {
        m.identity.clear();
    }
    let c = collector(&app);
    {
        let _guard = c.write.lock().await;
        // Optimistic guard: refuse to overwrite a monitor edited or deleted meanwhile.
        let current = app
            .store
            .get::<Monitor>(MONITOR_KIND, &mid)
            .ok_or(ApiError::new(404, "Monitor not found"))?;
        if current.version != stored.version {
            return Err(ApiError::new(
                409,
                "This monitor changed while saving. Reload and try again.",
            ));
        }
        if !m.enabled || changed_credential {
            crate::native_usage::forget_monitor(&app, &mid).map_err(ApiError::db)?;
        }
        m.version = stored.version + 1;
        m.updated_at = now();
        app.store
            .put(MONITOR_KIND, &mid, &m)
            .map_err(ApiError::db)?;
        app.store
            .delete(SNAPSHOT_KIND, &format!("monitor:{mid}"))
            .map_err(ApiError::db)?;
    }
    if m.enabled {
        trigger(&app, Some(format!("monitor:{mid}")), true);
    }
    Ok(Json(m.public()))
}

async fn delete_monitor(
    State(app): State<App>,
    Path(mid): Path<String>,
) -> Result<StatusCode, ApiError> {
    let c = collector(&app);
    let _guard = c.write.lock().await;
    if app.store.get::<Monitor>(MONITOR_KIND, &mid).is_none() {
        return Err(ApiError::new(404, "Monitor not found"));
    }
    crate::native_usage::forget_monitor(&app, &mid).map_err(ApiError::db)?;
    app.store.delete(MONITOR_KIND, &mid).map_err(ApiError::db)?;
    app.store
        .delete(SNAPSHOT_KIND, &format!("monitor:{mid}"))
        .map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ImportBody {
    provider: String,
    #[serde(default)]
    path: Option<String>,
}

async fn import(
    State(app): State<App>,
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>, ApiError> {
    let path = body
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from);
    if let Some(p) = &path
        && (p.len() > 1024 || !FsPath::new(p).is_absolute())
    {
        return Err(ApiError::bad("path must be an absolute path"));
    }
    let (provider, name) = match body.provider.as_str() {
        "cursor" => ("cursor", "Cursor on this machine"),
        "opencode" | "opencode_go" => ("opencode_go", "OpenCode Go on this machine"),
        "codex" => ("codex", "Codex CLI on this machine"),
        "claude" => ("claude", "Claude Code on this machine"),
        "antigravity" => {
            return Ok(Json(
                json!({"imported":0,"monitors":[],"skipped":[{"reason":"unsupported","message":"Antigravity quota appears automatically for connected Antigravity accounts. Add the account under Accounts."}],"message":"Import read-only; native accounts stay owned by their app."}),
            ));
        }
        _ => {
            return Err(ApiError::bad(
                "provider must be cursor, opencode, opencode_go, codex, claude or antigravity",
            ));
        }
    };
    let mut m = Monitor {
        name: name.into(),
        provider: provider.into(),
        credential_source: if path.is_some() { "file" } else { "native" }.into(),
        source_path: path,
        enabled: true,
        ..Default::default()
    };
    if app.store.list::<Monitor>(MONITOR_KIND).len() >= MAX_MONITORS {
        return Err(ApiError::bad("At most 100 usage monitors are supported"));
    }
    let skipped = |reason: &str, message: &str| {
        Json(
            json!({"imported":0,"monitors":[],"skipped":[{"reason":reason,"message":message}],"message":"Import read-only; native accounts stay owned by their app."}),
        )
    };
    m.identity = match native_identity(&m).await {
        Ok(i) => i,
        Err(e @ (FetchError::NotFound | FetchError::Expired | FetchError::Unavailable(_))) => {
            return Ok(skipped("not_found", e.message(false)));
        }
        Err(_) => return Ok(skipped("not_found", MSG_PARSE)),
    };
    // A file credential re-read each time is still the user's native session.
    if m.identity.is_empty() && provider != "claude" {
        return Ok(skipped("not_found", MSG_NO_NATIVE));
    }
    if let Some(existing) = duplicate_of(&app, &m) {
        return Ok(Json(
            json!({"imported":0,"monitors":[existing.public()],"skipped":[{"reason":"already_monitored","message":"This account is already monitored."}],"message":"Import read-only; native accounts stay owned by their app."}),
        ));
    }
    if matches!(provider, "codex" | "claude") {
        let f = if provider == "codex" {
            codex_file(m.source_path.as_deref()).await
        } else {
            claude_file(m.source_path.as_deref()).await
        };
        if let Ok(f) = f
            && not_connected(&app, &f.token).is_err()
        {
            return Ok(skipped(
                "already_monitored",
                "This sign-in is already a connected account, so its quota appears automatically.",
            ));
        }
    }
    m.id = id();
    m.created_at = now();
    m.updated_at = m.created_at.clone();
    m.version = 1;
    {
        let c = collector(&app);
        let _guard = c.write.lock().await;
        app.store
            .put(MONITOR_KIND, &m.id, &m)
            .map_err(ApiError::db)?;
    }
    trigger(&app, Some(format!("monitor:{}", m.id)), true);
    Ok(Json(
        json!({"imported":1,"monitors":[m.public()],"skipped":[],"message":"Import read-only; native accounts stay owned by their app."}),
    ))
}
