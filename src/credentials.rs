//! Credential import and lifecycle.
//!
//! Ownership rules (see docs/oauth.md):
//! - `native_codex`, `native_claude` and `cliproxy` connections are *source-owned*. Their refresh
//!   tokens are shared with another program, and OAuth refresh tokens are single-use, so the
//!   gateway never refreshes them. When the access token expires it rereads the read-only source
//!   and adopts a newer token written there by the owning CLI; otherwise it fails with a clear 401.
//! - `oauth` connections came from the gateway's own PKCE sign-in (src/oauth.rs). The gateway owns
//!   that token family and refreshes it under the per-account lock.
//! - `api_key` connections never refresh.
use crate::{
    app::{ApiError, App, account_lock},
    oauth,
    store::{Connection, hash, id, now},
};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
#[cfg(target_os = "macos")]
use tokio::process::Command;

pub const SOURCE_NATIVE_CODEX: &str = "native_codex";
pub const SOURCE_NATIVE_CLAUDE: &str = "native_claude";
pub const SOURCE_CLIPROXY: &str = "cliproxy";
pub const SOURCE_OAUTH: &str = "oauth";
pub const SOURCE_API_KEY: &str = "api_key";
/// `source_path` marker for the macOS Keychain record written by Claude Code.
pub const KEYCHAIN_CLAUDE: &str = "keychain:Claude Code-credentials";

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";
const OPENAI_BASE: &str = "https://api.openai.com/v1";
const LOCK_WAIT: Duration = Duration::from_secs(25);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);
/// Tokens expiring within this window are treated as expired.
const SKEW: i64 = 60;

/// A credential parsed from a source, before it is matched against stored connections.
#[derive(Clone)]
pub(crate) struct Parsed {
    pub name: String,
    pub kind: &'static str,
    pub base: &'static str,
    pub token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
    pub identity: String,
    pub source: &'static str,
    pub source_path: String,
    pub oauth: bool,
    pub models: Vec<String>,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}
fn str_of<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|k| v[*k].as_str().filter(|s| !s.trim().is_empty()))
        .unwrap_or("")
}

/// Decodes (without verifying) the payload of a JWT. Only used to read identity and expiry
/// claims from tokens that the provider itself will validate.
pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    if payload.len() > 64 * 1024 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}

/// Unix seconds from a number (seconds or milliseconds) or an RFC3339 string.
fn timestamp(v: &Value) -> Option<i64> {
    let n = match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?,
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                n
            } else {
                return chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|t| t.timestamp())
                    .filter(|t| *t > 0);
            }
        }
        _ => return None,
    };
    // Milliseconds are 13 digits for any date after 1973.
    let n = if n > 100_000_000_000 { n / 1000 } else { n };
    (n > 0).then_some(n)
}

/// Expiry from `expiresAt`, `expires_at`, `expired` or `expiry` (numeric or RFC3339),
/// falling back to the access token's JWT `exp`. 0 means unknown.
pub(crate) fn parse_expiry(v: &Value, token: &str) -> i64 {
    ["expiresAt", "expires_at", "expired", "expiry", "expire"]
        .iter()
        .find_map(|k| timestamp(&v[*k]))
        .or_else(|| jwt_claims(token).and_then(|c| timestamp(&c["exp"])))
        .unwrap_or(0)
}

/// Stable Codex identity: ChatGPT user (JWT `sub`, else `chatgpt_user_id`, else email) plus
/// workspace. Several people can share one workspace, so the workspace alone is never enough.
fn codex_identity(claims: &[Option<Value>], account_id: &str, email: &str) -> Option<String> {
    let user = claims.iter().flatten().find_map(|c| {
        let auth = &c["https://api.openai.com/auth"];
        [
            c["sub"].as_str(),
            auth["chatgpt_user_id"].as_str(),
            auth["user_id"].as_str(),
        ]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map(String::from)
    });
    let user =
        user.or_else(|| (!email.is_empty()).then(|| format!("email:{}", email.to_lowercase())))?;
    Some(hash(&format!("v1|codex|{user}|{account_id}")))
}
fn codex_account(claims: &[Option<Value>]) -> String {
    claims
        .iter()
        .flatten()
        .find_map(|c| {
            c["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default()
}
fn codex_email(claims: &[Option<Value>]) -> String {
    claims
        .iter()
        .flatten()
        .find_map(|c| {
            c["email"]
                .as_str()
                .or(c["https://api.openai.com/profile"]["email"].as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}
fn codex_plan(claims: &[Option<Value>]) -> String {
    claims
        .iter()
        .flatten()
        .find_map(|c| {
            c["https://api.openai.com/auth"]["chatgpt_plan_type"]
                .as_str()
                .map(str::to_lowercase)
        })
        .unwrap_or_default()
}
pub(crate) fn codex_models(plan: &str) -> Vec<String> {
    let ms: &[&str] = if plan == "free" {
        &["gpt-6-luna", "gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.5"]
    } else {
        &[
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.5",
        ]
    };
    ms.iter().map(|s| s.to_string()).collect()
}
pub(crate) fn claude_models() -> Vec<String> {
    [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "claude-haiku-4-5-20251001",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
pub(crate) fn claude_identity(account_uuid: &str) -> String {
    hash(&format!("v1|claude|account|{account_uuid}"))
}
/// Builds a Codex credential from a token document (`refresh_token`, `id_token`, `account_id`,
/// `email` and expiry fields are read from `doc` when present).
pub(crate) fn codex_from_tokens(
    doc: &Value,
    access: &str,
    source: &'static str,
    source_path: &str,
) -> Parsed {
    let (refresh, account_hint, email_hint) = (
        str_of(doc, &["refresh_token"]),
        str_of(doc, &["account_id"]),
        str_of(doc, &["email"]),
    );
    let claims = [jwt_claims(str_of(doc, &["id_token"])), jwt_claims(access)];
    let account_id = if account_hint.is_empty() {
        codex_account(&claims)
    } else {
        account_hint.to_string()
    };
    let email = if email_hint.is_empty() {
        codex_email(&claims)
    } else {
        email_hint.to_string()
    };
    let identity = codex_identity(&claims, &account_id, &email)
        .unwrap_or_else(|| hash(&format!("v1|codex|source|{source_path}|{account_id}")));
    Parsed {
        name: if email.is_empty() {
            "Codex".into()
        } else {
            email.clone()
        },
        kind: "codex",
        base: CODEX_BASE,
        token: access.into(),
        refresh_token: refresh.into(),
        expires_at: parse_expiry(doc, access),
        account_id,
        identity,
        source,
        source_path: source_path.into(),
        oauth: true,
        models: codex_models(&codex_plan(&claims)),
    }
}
fn api_key(name: &str, key: &str, source_path: &str) -> Parsed {
    Parsed {
        name: name.into(),
        kind: "openai",
        base: OPENAI_BASE,
        token: key.into(),
        refresh_token: String::new(),
        expires_at: 0,
        account_id: String::new(),
        identity: hash(&format!("v1|openai|key|{}", hash(key))),
        source: SOURCE_API_KEY,
        source_path: source_path.into(),
        oauth: false,
        models: codex_models(""),
    }
}

fn parse_native_codex(v: &Value, source_path: &str) -> Result<Parsed, ApiError> {
    let t = if v["tokens"].is_object() {
        &v["tokens"]
    } else {
        v
    };
    let access = str_of(t, &["access_token"]);
    if !access.is_empty() {
        return Ok(codex_from_tokens(
            t,
            access,
            SOURCE_NATIVE_CODEX,
            source_path,
        ));
    }
    let key = str_of(v, &["OPENAI_API_KEY"]);
    if !key.is_empty() {
        return Ok(api_key("OpenAI API key", key, source_path));
    }
    Err(ApiError::bad(
        "No Codex access token or API key found. Run codex login first.",
    ))
}

fn parse_native_claude(
    v: &Value,
    source_path: &str,
    profile: Option<&Value>,
) -> Result<Parsed, ApiError> {
    let t = if v["claudeAiOauth"].is_object() {
        &v["claudeAiOauth"]
    } else {
        v
    };
    let token = str_of(t, &["accessToken", "access_token"]);
    if token.is_empty() {
        return Err(ApiError::bad(
            "No Claude access token found. Run claude auth login first.",
        ));
    }
    let account = profile.map(|p| &p["oauthAccount"]);
    let uuid = account.map(|a| str_of(a, &["accountUuid"])).unwrap_or("");
    let email = account.map(|a| str_of(a, &["emailAddress"])).unwrap_or("");
    Ok(Parsed {
        name: if email.is_empty() {
            "Claude Code".into()
        } else {
            email.into()
        },
        kind: "anthropic",
        base: ANTHROPIC_BASE,
        token: token.into(),
        refresh_token: str_of(t, &["refreshToken", "refresh_token"]).into(),
        expires_at: parse_expiry(t, token),
        account_id: String::new(),
        // Claude access tokens are opaque and rotate, so identity is the account UUID when the
        // Claude Code profile is available, otherwise the source record itself.
        identity: if uuid.is_empty() {
            hash(&format!("v1|claude|source|{source_path}"))
        } else {
            claude_identity(uuid)
        },
        source: SOURCE_NATIVE_CLAUDE,
        source_path: source_path.into(),
        oauth: true,
        models: claude_models(),
    })
}

fn parse_cliproxy(v: &Value, source_path: &str) -> Option<Parsed> {
    let token = str_of(v, &["access_token"]);
    let email = str_of(v, &["email"]);
    match v["type"].as_str().unwrap_or("") {
        "codex" if !token.is_empty() => {
            let mut p = codex_from_tokens(v, token, SOURCE_CLIPROXY, source_path);
            if p.name == "Codex" {
                p.name = "Imported account".into();
            }
            Some(p)
        }
        "claude" if !token.is_empty() => {
            let uuid = str_of(v, &["account_uuid"]);
            Some(Parsed {
                name: if email.is_empty() {
                    "Imported account".into()
                } else {
                    email.into()
                },
                kind: "anthropic",
                base: ANTHROPIC_BASE,
                token: token.into(),
                refresh_token: str_of(v, &["refresh_token"]).into(),
                expires_at: parse_expiry(v, token),
                account_id: String::new(),
                identity: if !uuid.is_empty() {
                    claude_identity(uuid)
                } else if !email.is_empty() {
                    hash(&format!("v1|claude|email|{}", email.to_lowercase()))
                } else {
                    hash(&format!("v1|claude|source|{source_path}"))
                },
                source: SOURCE_CLIPROXY,
                source_path: source_path.into(),
                oauth: true,
                models: claude_models(),
            })
        }
        "openai" | "codex" => {
            let key = str_of(v, &["api_key"]);
            (!key.is_empty()).then(|| {
                api_key(
                    if email.is_empty() {
                        "Imported account"
                    } else {
                        email
                    },
                    key,
                    source_path,
                )
            })
        }
        _ => None,
    }
}

fn home() -> Result<PathBuf, ApiError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(ApiError::bad(
            "Home directory is not set (HOME or USERPROFILE)",
        ))
}
fn codex_default() -> Result<PathBuf, ApiError> {
    Ok(match std::env::var_os("CODEX_HOME") {
        Some(h) => PathBuf::from(h).join("auth.json"),
        None => home()?.join(".codex/auth.json"),
    })
}
fn claude_dir() -> Result<PathBuf, ApiError> {
    Ok(match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(h) => PathBuf::from(h),
        None => home()?.join(".claude"),
    })
}
/// Claude Code keeps account metadata (not secrets) in `~/.claude.json`. Only consulted for the
/// default credential location so explicit test or alternate files never touch the real profile.
async fn claude_profile() -> Option<Value> {
    let p = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(h) => PathBuf::from(h).join(".claude.json"),
        None => home().ok()?.join(".claude.json"),
    };
    // The profile can be large (project history); read it with a higher bound, metadata only.
    read_bounded(&p, 16 * 1024 * 1024).await.ok()
}
async fn canonical(p: &Path) -> String {
    tokio::fs::canonicalize(p)
        .await
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub async fn import(
    app: &App,
    source: &str,
    path: Option<&str>,
) -> Result<Vec<Connection>, ApiError> {
    let path = path.map(str::trim).filter(|p| !p.is_empty());
    let mut values = Vec::new();
    match source {
        "codex" => {
            let p = match path {
                Some(p) => PathBuf::from(p),
                None => codex_default()?,
            };
            let v = read(&p).await?;
            values.push(parse_native_codex(&v, &canonical(&p).await)?);
        }
        "claude" => {
            let (v, marker, profile) = if let Some(p) = path {
                let p = PathBuf::from(p);
                (read(&p).await?, canonical(&p).await, None)
            } else {
                let p = claude_dir()?.join(".credentials.json");
                if tokio::fs::try_exists(&p).await.unwrap_or(false) {
                    (read(&p).await?, canonical(&p).await, claude_profile().await)
                } else {
                    (
                        keychain().await?,
                        KEYCHAIN_CLAUDE.to_string(),
                        claude_profile().await,
                    )
                }
            };
            values.push(parse_native_claude(&v, &marker, profile.as_ref())?);
        }
        "cliproxy" => {
            let p = PathBuf::from(path.ok_or(ApiError::bad(
                "Choose a CLIProxyAPI auth JSON file or directory",
            ))?);
            let mut paths = Vec::new();
            if tokio::fs::metadata(&p).await.is_ok_and(|m| m.is_dir()) {
                let mut entries = tokio::fs::read_dir(&p)
                    .await
                    .map_err(|_| ApiError::bad("Cannot read auth directory"))?;
                while let Some(e) = entries
                    .next_entry()
                    .await
                    .map_err(|_| ApiError::bad("Cannot read auth directory"))?
                {
                    if e.path().extension().is_some_and(|x| x == "json") {
                        if paths.len() >= 100 {
                            return Err(ApiError::bad("Import at most 100 account files at once"));
                        }
                        paths.push(e.path());
                    }
                }
                paths.sort();
            } else {
                paths.push(p);
            }
            for p in paths {
                let v = read(&p).await?;
                if let Some(parsed) = parse_cliproxy(&v, &canonical(&p).await) {
                    values.push(parsed);
                }
            }
        }
        _ => return Err(ApiError::bad("Supported imports: codex, claude, cliproxy")),
    }
    if values.is_empty() {
        return Err(ApiError::bad("No supported credentials found"));
    }
    // Several files for one account in a batch: keep the freshest token.
    let mut unique: Vec<Parsed> = Vec::new();
    for p in values {
        match unique
            .iter_mut()
            .find(|u| u.kind == p.kind && u.identity == p.identity)
        {
            Some(u) if p.expires_at > u.expires_at => *u = p,
            Some(_) => {}
            None => unique.push(p),
        }
    }
    let mut out = Vec::new();
    for p in unique {
        out.push(upsert(app, p).await?);
    }
    Ok(out)
}

fn find_existing(app: &App, p: &Parsed) -> Option<Connection> {
    let all: Vec<Connection> = app.store.list("connection");
    all.iter()
        .find(|x| {
            x.kind == p.kind && !x.account_identity.is_empty() && x.account_identity == p.identity
        })
        .or_else(|| {
            // Rows imported before identities existed.
            all.iter().find(|x| {
                x.kind == p.kind
                    && x.account_identity.is_empty()
                    && (x.api_key == p.token
                        || (!p.account_id.is_empty()
                            && x.account_id == p.account_id
                            && x.source_path == p.source_path))
            })
        })
        .cloned()
}

async fn lock_account(app: &App, id: &str) -> Result<tokio::sync::OwnedMutexGuard<()>, ApiError> {
    tokio::time::timeout(LOCK_WAIT, account_lock(app, id).lock_owned())
        .await
        .map_err(|_| {
            ApiError::new(503, "This account is busy refreshing. Retry shortly.").retry_after(5)
        })
}

fn patch_tokens(c: &mut Connection, p: &Parsed) {
    c.api_key = p.token.clone();
    c.refresh_token = p.refresh_token.clone();
    c.expires_at = p.expires_at;
    if !p.account_id.is_empty() || c.account_id.is_empty() {
        c.account_id = p.account_id.clone();
    }
    c.oauth = p.oauth;
    c.credential_source = p.source.into();
    c.source_path = p.source_path.clone();
    c.account_identity = p.identity.clone();
}

/// Inserts or updates the connection for a parsed credential. User configuration (name, enabled,
/// base URL, WebSocket support, models) on an existing connection always wins.
pub(crate) async fn upsert(app: &App, p: Parsed) -> Result<Connection, ApiError> {
    let Some(old) = find_existing(app, &p) else {
        let c = Connection {
            id: id(),
            name: p.name.chars().take(100).collect(),
            kind: p.kind.into(),
            base_url: p.base.into(),
            enabled: true,
            models: p.models.clone(),
            supports_websocket: p.kind == "codex",
            created_at: now(),
            api_key: String::new(),
            refresh_token: String::new(),
            expires_at: 0,
            account_id: String::new(),
            oauth: false,
            credential_source: String::new(),
            source_path: String::new(),
            account_identity: String::new(),
        };
        let mut c = c;
        patch_tokens(&mut c, &p);
        app.store
            .put("connection", &c.id, &c)
            .map_err(ApiError::db)?;
        return Ok(c);
    };
    let _guard = lock_account(app, &old.id).await?;
    // Reread under the lock so a concurrent refresh or edit is not overwritten.
    let Some(mut latest) = app.store.get::<Connection>("connection", &old.id) else {
        return Err(ApiError::new(
            409,
            "The matching connection was removed during import. Retry.",
        ));
    };
    if p.source == SOURCE_OAUTH || !keeps_newer_tokens(&latest, &p) {
        patch_tokens(&mut latest, &p);
        app.store
            .put("connection", &latest.id, &latest)
            .map_err(ApiError::db)?;
        app.resilience
            .lock()
            .expect("resilience lock")
            .reset(&latest.id);
    }
    Ok(latest)
}

/// True when the stored tokens should survive an import from a shared source.
fn keeps_newer_tokens(latest: &Connection, p: &Parsed) -> bool {
    if latest.credential_source == SOURCE_OAUTH && latest.oauth {
        // An independent sign-in is never replaced by a shared copy while the gateway can still
        // renew it; a revoked one is replaced only by a usable token.
        let live = latest.expires_at == 0 || latest.expires_at > now_ts() + SKEW;
        let renewable = live || !latest.refresh_token.is_empty();
        return renewable || (p.expires_at != 0 && p.expires_at <= now_ts() + SKEW);
    }
    // Same token family from a shared source: only refuse a strictly older token.
    latest.api_key != p.token
        && latest.expires_at != 0
        && p.expires_at != 0
        && p.expires_at < latest.expires_at
}

async fn read(p: &Path) -> Result<Value, ApiError> {
    read_bounded(p, 1024 * 1024).await
}
async fn read_bounded(p: &Path, max: u64) -> Result<Value, ApiError> {
    let metadata = tokio::fs::metadata(p)
        .await
        .map_err(|_| ApiError::bad("Credential file not found or unreadable"))?;
    if !metadata.is_file() {
        return Err(ApiError::bad("Credential path is not a file"));
    }
    if metadata.len() > max {
        return Err(ApiError::bad(if max == 1024 * 1024 {
            "Credential file exceeds 1 MiB"
        } else {
            "Credential file is too large"
        }));
    }
    let b = tokio::fs::read(p)
        .await
        .map_err(|_| ApiError::bad("Cannot read credential file"))?;
    if b.len() as u64 > max {
        return Err(ApiError::bad("Credential file is too large"));
    }
    serde_json::from_slice(&b).map_err(|_| ApiError::bad("Invalid credential JSON"))
}
async fn keychain() -> Result<Value, ApiError> {
    #[cfg(target_os = "macos")]
    {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("security")
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .args([
                    "find-generic-password",
                    "-s",
                    "Claude Code-credentials",
                    "-w",
                ])
                .output(),
        )
        .await
        .map_err(|_| ApiError::bad("Keychain read timed out; import a credential file instead"))?
        .map_err(|_| ApiError::bad("Cannot read Claude Keychain credentials"))?;
        if !out.status.success() {
            return Err(ApiError::bad(
                "Claude credentials not found. Run claude auth login or provide a credential file.",
            ));
        }
        if out.stdout.len() > 1024 * 1024 {
            return Err(ApiError::bad("Claude Keychain credential exceeds 1 MiB"));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|_| ApiError::bad("Invalid Claude Keychain credential format"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(ApiError::bad(
            "Claude credential file not found. Run claude auth login first.",
        ))
    }
}

/// Reads at most `max` bytes of a JSON response body.
pub(crate) async fn json_bounded(res: reqwest::Response, max: usize) -> Option<Value> {
    if res.content_length().is_some_and(|n| n as usize > max) {
        return None;
    }
    let mut body = res.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk.ok()?);
        if buf.len() > max {
            return None;
        }
    }
    serde_json::from_slice(&buf).ok()
}

fn reauth_message(c: &Connection) -> &'static str {
    match (c.credential_source.as_str(), c.kind.as_str()) {
        (SOURCE_NATIVE_CODEX, _) => {
            "Codex sign-in for this account has expired. Run codex once (or codex login) on this machine, then retry. For an account the gateway refreshes itself, sign in from the control room."
        }
        (SOURCE_NATIVE_CLAUDE, _) => {
            "Claude Code sign-in for this account has expired. Run claude once (or claude auth login) on this machine, then retry. For an account the gateway refreshes itself, sign in from the control room."
        }
        (SOURCE_CLIPROXY, _) => {
            "This CLIProxyAPI account file has an expired token. Let CLIProxyAPI refresh it, or sign in from the control room, then retry."
        }
        (SOURCE_OAUTH, _) => {
            "This account's sign-in was revoked or expired. Sign in again from the control room."
        }
        (_, "anthropic" | "codex") if c.oauth => {
            "This account was imported by an older version and has expired. Reimport it or sign in from the control room."
        }
        _ => "The provider rejected this API key. Update the key in the control room.",
    }
}
fn reauth(c: &Connection) -> ApiError {
    ApiError::new(401, reauth_message(c))
}

/// Ensures a usable access token before an upstream request. Cheap unless the token is known to
/// be expired. On success `c` holds the latest stored connection, including any configuration
/// edits made while the request was queued.
pub async fn refresh(app: &App, c: &mut Connection) -> Result<(), ApiError> {
    if !c.oauth || c.credential_source == SOURCE_API_KEY {
        return Ok(());
    }
    if c.expires_at == 0 || c.expires_at > now_ts() + SKEW {
        return Ok(());
    }
    renew(app, c, false).await
}

/// Called once after the provider rejected `c`'s access token (HTTP 401). Adopts a newer token
/// from the owner, or refreshes a gateway-owned token. Returns an error when no different usable
/// token exists, so callers must not retry in that case.
pub async fn refresh_forced(app: &App, c: &mut Connection) -> Result<(), ApiError> {
    if !c.oauth || c.credential_source == SOURCE_API_KEY {
        return Err(reauth(c));
    }
    renew(app, c, true).await
}

async fn renew(app: &App, c: &mut Connection, forced: bool) -> Result<(), ApiError> {
    let rejected = c.api_key.clone();
    let _guard = lock_account(app, &c.id).await?;
    let mut latest = app
        .store
        .get::<Connection>("connection", &c.id)
        .ok_or(ApiError::new(
            409,
            "This account was removed. Retry to use another account.",
        ))?;
    let usable = |x: &Connection| x.expires_at == 0 || x.expires_at > now_ts() + SKEW;
    // Another request already renewed while this one waited for the lock.
    if latest.api_key != rejected && usable(&latest) {
        *c = latest;
        return Ok(());
    }
    if !forced && usable(&latest) {
        *c = latest;
        return Ok(());
    }
    match latest.credential_source.as_str() {
        SOURCE_NATIVE_CODEX | SOURCE_NATIVE_CLAUDE | SOURCE_CLIPROXY => {
            let p = reread_source(&latest).await.map_err(|_| {
                ApiError::new(
                    401,
                    "This account's sign-in source could not be read. Reimport it or sign in from the control room.",
                )
            })?;
            if !latest.account_identity.is_empty() && p.identity != latest.account_identity {
                return Err(ApiError::new(
                    401,
                    "The sign-in source now belongs to a different account. Reimport to add it as a new account.",
                ));
            }
            let fresh = p.expires_at == 0 || p.expires_at > now_ts() + SKEW;
            if p.token == rejected || p.token == latest.api_key || !fresh {
                return Err(reauth(&latest));
            }
            if latest.expires_at != 0 && p.expires_at != 0 && p.expires_at < latest.expires_at {
                return Err(reauth(&latest));
            }
            let config = latest.clone();
            patch_tokens(&mut latest, &p);
            // A source never changes who owns the row or how the user configured it.
            latest.credential_source = config.credential_source;
            latest.source_path = config.source_path;
            app.store
                .put("connection", &latest.id, &latest)
                .map_err(ApiError::db)?;
            app.resilience
                .lock()
                .expect("resilience lock")
                .reset(&latest.id);
            *c = latest;
            Ok(())
        }
        SOURCE_OAUTH => {
            let used = latest.refresh_token.clone();
            if used.is_empty() {
                return Err(reauth(&latest));
            }
            let p = oauth::refresh_tokens(app, &latest.kind, &used).await?;
            // The lock serialises gateway writers; still refuse to clobber a row that changed.
            let mut now_stored =
                app.store
                    .get::<Connection>("connection", &c.id)
                    .ok_or(ApiError::new(
                        409,
                        "This account was removed during refresh. Sign in again if needed.",
                    ))?;
            if now_stored.refresh_token != used || now_stored.credential_source != SOURCE_OAUTH {
                return Err(ApiError::new(
                    409,
                    "This account changed during refresh. Retry the request.",
                ));
            }
            now_stored.api_key = p.token;
            if !p.refresh_token.is_empty() {
                now_stored.refresh_token = p.refresh_token;
            }
            now_stored.expires_at = p.expires_at;
            if !p.account_id.is_empty() {
                now_stored.account_id = p.account_id;
            }
            app.store
                .put("connection", &now_stored.id, &now_stored)
                .map_err(ApiError::db)?;
            app.resilience
                .lock()
                .expect("resilience lock")
                .reset(&now_stored.id);
            *c = now_stored;
            Ok(())
        }
        _ => Err(reauth(&latest)),
    }
}

/// Reads a source-owned credential again, read-only.
async fn reread_source(c: &Connection) -> Result<Parsed, ApiError> {
    if c.source_path.is_empty() {
        return Err(ApiError::bad("No source"));
    }
    match c.credential_source.as_str() {
        SOURCE_NATIVE_CODEX => {
            let v = read(Path::new(&c.source_path)).await?;
            parse_native_codex(&v, &c.source_path)
        }
        SOURCE_NATIVE_CLAUDE => {
            let default = claude_dir().ok().map(|d| d.join(".credentials.json"));
            let is_default = c.source_path == KEYCHAIN_CLAUDE
                || match &default {
                    Some(d) => canonical(d).await == c.source_path,
                    None => false,
                };
            let v = if c.source_path == KEYCHAIN_CLAUDE {
                keychain().await?
            } else {
                read(Path::new(&c.source_path)).await?
            };
            let profile = if is_default {
                claude_profile().await
            } else {
                None
            };
            parse_native_claude(&v, &c.source_path, profile.as_ref())
        }
        SOURCE_CLIPROXY => {
            let v = read(Path::new(&c.source_path)).await?;
            parse_cliproxy(&v, &c.source_path)
                .filter(|p| p.kind == c.kind)
                .ok_or(ApiError::bad("Unsupported source"))
        }
        _ => Err(ApiError::bad("Unsupported source")),
    }
}

/// Bounded wait used by OAuth token requests.
pub(crate) const fn token_timeout() -> Duration {
    REFRESH_TIMEOUT
}
pub(crate) fn expiry_from_response(v: &Value, access: &str) -> i64 {
    jwt_claims(access)
        .and_then(|c| timestamp(&c["exp"]))
        .or_else(|| {
            v["expires_in"]
                .as_i64()
                .filter(|s| (1..=10 * 365 * 86400).contains(s))
                .map(|s| now_ts() + s)
        })
        .unwrap_or_else(|| now_ts() + 3600)
}
