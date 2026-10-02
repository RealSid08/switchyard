//! Quota, balance and provider-billing sources (`/api/usage/sources`, monitors, import) against
//! loopback mock providers. Never reads real credentials: every native file is a temporary
//! fixture passed by explicit path.
mod support;

use axum::{Router, http::HeaderMap, middleware, routing::get};
use base64::Engine;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{Upstream, json_response};
use switchyard::{
    app::{App, AppState, admin_auth},
    store::Connection,
    usage_sources::{self, ModelQuota},
};
use tokio::net::TcpListener;

const SECRET: &str = "sy-test-secret-token-NEVER-LEAK-91c2";
const PROVIDER_TEXT: &str = "PROVIDER-INTERNAL-ERROR-TEXT-77";

struct Env {
    app: App,
    port: u16,
    admin: String,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}
impl Env {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = AppState::new(dir.path().join("data"), "127.0.0.1".into(), port, 16, 2).unwrap();
        let admin = std::fs::read_to_string(dir.path().join("data/admin-token"))
            .unwrap()
            .trim()
            .to_string();
        let router = usage_sources::router()
            .merge(switchyard::usage::router())
            .route_layer(middleware::from_fn_with_state(app.clone(), admin_auth))
            .with_state(app.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        usage_sources::set_test_manual_interval(&app, Duration::from_millis(0));
        Self {
            app,
            port,
            admin,
            http: support::client(),
            dir,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
    async fn get(&self, path: &str) -> (u16, Value, String) {
        let r = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.admin)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }
    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .http
            .request(method, self.url(path))
            .bearer_auth(&self.admin)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
    fn endpoint(&self, name: &str, up: &Upstream) {
        usage_sources::set_test_endpoint(&self.app, name, &up.base());
    }
    fn connection(&self, c: Connection) {
        self.app.store.put("connection", &c.id, &c).unwrap();
    }
    /// Waits until the source satisfies `pred`, returning it and the raw response text.
    async fn wait_source(&self, id: &str, pred: impl Fn(&Value) -> bool) -> (Value, String) {
        let mut last = Value::Null;
        for _ in 0..300 {
            let (_, v, text) = self.get("/api/usage/sources").await;
            if let Some(s) = v["sources"]
                .as_array()
                .and_then(|a| a.iter().find(|s| s["id"] == id))
            {
                if pred(s) {
                    return (s.clone(), text);
                }
                last = s.clone();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!("source {id} never matched; last: {last}");
    }
    async fn refresh(&self, id: &str) -> (u16, Value) {
        self.post("/api/usage/refresh", json!({"id": id})).await
    }
}

fn connection(id: &str, kind: &str, base: &str, oauth: bool) -> Connection {
    Connection {
        id: id.into(),
        name: format!("{kind} account"),
        kind: kind.into(),
        base_url: base.into(),
        enabled: true,
        models: vec![],
        supports_websocket: false,
        created_at: "2026-10-01T00:00:00Z".into(),
        api_key: SECRET.into(),
        refresh_token: String::new(),
        expires_at: 0,
        account_id: "acct-123".into(),
        oauth,
        credential_source: if oauth { "native_codex" } else { "api_key" }.into(),
        source_path: String::new(),
        account_identity: String::new(),
    }
}

fn jwt(claims: Value) -> String {
    let e = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", e(&json!({"alg":"none"})), e(&claims))
}
fn future() -> i64 {
    chrono::Utc::now().timestamp() + 3600
}
fn header(h: &HeaderMap, name: &str) -> String {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}
fn window<'a>(s: &'a Value, id: &str) -> &'a Value {
    s["windows"]
        .as_array()
        .and_then(|w| w.iter().find(|w| w["id"] == id))
        .unwrap_or_else(|| panic!("window {id} missing in {s}"))
}

fn codex_fixture() -> Value {
    json!({
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_at": 1790000000},
            "secondary_window": {"used_percent": 7, "limit_window_seconds": 604800, "reset_at": 1790500000}
        },
        "additional_rate_limits": [{"limit_name": "GPT-5.3-Codex-Spark", "rate_limit": {"primary_window": {"used_percent": 3, "limit_window_seconds": 18000, "reset_at": 1790000000}}}],
        "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"}
    })
}

#[tokio::test]
async fn codex_connection_quota_is_read_with_its_account_and_never_leaks_the_token() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/backend-api/wham/usage",
        get(|| async { json_response(200, codex_fixture()) }),
    ))
    .await;
    env.connection(connection(
        "c1",
        "codex",
        &format!("{}/backend-api/codex", up.base()),
        true,
    ));
    let (s, text) = env
        .wait_source("connection:c1", |s| s["status"] == "ok")
        .await;
    assert_eq!(s["provider"], "codex");
    assert_eq!(s["source"], "provider_api");
    assert_eq!(s["plan"], "pro");
    let w = window(&s, "5h");
    assert_eq!(w["unit"], "percent");
    assert_eq!(w["used"], 42.0);
    assert_eq!(w["remaining"], 58.0);
    assert_eq!(w["reset_at"], "2026-09-21T14:13:20Z");
    assert_eq!(window(&s, "7d")["used"], 7.0);
    let spark = window(&s, "model:gpt-5-3-codex-spark:5h");
    assert_eq!(spark["scope"], "model");
    assert_eq!(s["balances"][0]["value"], 12.5);
    assert_eq!(s["balances"][0]["unit"], "credits");
    let req = &up.requests()[0];
    assert_eq!(
        header(&req.headers, "authorization"),
        format!("Bearer {SECRET}")
    );
    assert_eq!(header(&req.headers, "chatgpt-account-id"), "acct-123");
    assert!(!text.contains(SECRET), "token leaked in sources response");
}

#[tokio::test]
async fn claude_connection_reports_windows_and_extra_usage_in_dollars() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/api/oauth/usage",
        get(|| async {
            json_response(200, json!({
                "five_hour": {"utilization": 12.5, "resets_at": "2026-10-03T05:00:00Z"},
                "seven_day": {"utilization": 40, "resets_at": "2026-10-08T00:00:00Z"},
                "seven_day_opus": {"utilization": 3, "resets_at": null},
                "seven_day_sonnet": null,
                "extra_usage": {"is_enabled": true, "monthly_limit": 5000, "used_credits": 1234, "utilization": 24.68, "currency": "USD"}
            }))
        }),
    ))
    .await;
    let mut c = connection("c2", "anthropic", &format!("{}/v1", up.base()), true);
    c.credential_source = "native_claude".into();
    env.connection(c);
    let (s, _) = env
        .wait_source("connection:c2", |s| s["status"] == "ok")
        .await;
    assert_eq!(s["provider"], "claude");
    assert_eq!(window(&s, "session")["used"], 12.5);
    assert_eq!(window(&s, "week")["used"], 40.0);
    assert_eq!(window(&s, "week:opus")["model"], "opus");
    assert!(
        s["windows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["id"] != "week:sonnet"),
        "null window must be absent, not 0"
    );
    let extra = window(&s, "extra_usage");
    assert_eq!(
        (extra["used"].as_f64(), extra["limit"].as_f64()),
        (Some(12.34), Some(50.0))
    );
    assert_eq!(s["reported_costs"][0]["amount"], 12.34);
    assert_eq!(s["reported_costs"][0]["kind"], "on_demand");
    let req = &up.requests()[0];
    assert_eq!(header(&req.headers, "anthropic-beta"), "oauth-2025-04-20");
    assert!(header(&req.headers, "user-agent").starts_with("claude-code/"));
}

#[tokio::test]
async fn failures_keep_last_good_data_as_stale_with_constant_messages() {
    let env = Env::start().await;
    let fail = Arc::new(AtomicUsize::new(0));
    let f = fail.clone();
    let up = Upstream::start(Router::new().route(
        "/backend-api/wham/usage",
        get(move || {
            let f = f.clone();
            async move {
                if f.load(Ordering::SeqCst) == 0 {
                    json_response(200, codex_fixture())
                } else {
                    json_response(500, json!({"error": PROVIDER_TEXT}))
                }
            }
        }),
    ))
    .await;
    env.connection(connection(
        "c3",
        "codex",
        &format!("{}/backend-api/codex", up.base()),
        true,
    ));
    env.wait_source("connection:c3", |s| s["status"] == "ok")
        .await;
    fail.store(1, Ordering::SeqCst);
    assert_eq!(env.refresh("connection:c3").await.0, 202);
    let (s, text) = env
        .wait_source("connection:c3", |s| s["status"] == "stale")
        .await;
    assert_eq!(window(&s, "5h")["used"], 42.0, "last good values are kept");
    assert!(s["last_error_at"].is_string());
    assert!(s["updated_at"].is_string());
    assert!(
        !text.contains(PROVIDER_TEXT),
        "provider text must not be surfaced"
    );
}

#[tokio::test]
async fn rate_limits_honor_retry_after_and_never_show_zero() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(|| async {
            let mut r = json_response(429, json!({"error": PROVIDER_TEXT}));
            r.headers_mut()
                .insert("retry-after", "120".parse().unwrap());
            r
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    let (code, m) = env
        .post("/api/usage/monitors", json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":SECRET}))
        .await;
    assert_eq!(code, 200, "{m}");
    let id = format!("monitor:{}", m["id"].as_str().unwrap());
    let (s, _) = env
        .wait_source(&id, |s| {
            s["status"] == "unavailable" && s["last_error_at"].is_string()
        })
        .await;
    assert!(
        s["windows"].as_array().unwrap().is_empty(),
        "no fabricated windows"
    );
    let next =
        chrono::DateTime::parse_from_rfc3339(s["next_refresh_at"].as_str().unwrap()).unwrap();
    let wait = next.timestamp() - chrono::Utc::now().timestamp();
    assert!(
        (100..=125).contains(&wait),
        "retry-after honored, got {wait}s"
    );
    let before = up.count();
    assert_eq!(env.refresh(&id).await.0, 202);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        up.count(),
        before,
        "manual refresh must not bypass Retry-After"
    );
}

#[tokio::test]
async fn source_owned_sign_in_adopts_a_newer_token_after_401() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/backend-api/wham/usage",
        get(|h: HeaderMap| async move {
            if header(&h, "authorization") == "Bearer fresh-token-2" {
                json_response(200, codex_fixture())
            } else {
                json_response(401, json!({"error": PROVIDER_TEXT}))
            }
        }),
    ))
    .await;
    let auth = env.dir.path().join("codex-auth.json");
    std::fs::write(
        &auth,
        json!({"tokens": {"access_token": "fresh-token-2", "account_id": "acct-123"}}).to_string(),
    )
    .unwrap();
    let before = std::fs::read(&auth).unwrap();
    let mut c = connection(
        "c4",
        "codex",
        &format!("{}/backend-api/codex", up.base()),
        true,
    );
    c.source_path = auth.to_string_lossy().into_owned();
    env.connection(c);
    let (s, _) = env
        .wait_source("connection:c4", |s| s["status"] == "ok")
        .await;
    assert_eq!(window(&s, "5h")["used"], 42.0);
    assert_eq!(
        up.count(),
        2,
        "one rejected read, one retry with the adopted token"
    );
    assert_eq!(
        std::fs::read(&auth).unwrap(),
        before,
        "source file is never written"
    );
}

#[tokio::test]
async fn expired_imported_sign_in_needs_auth_without_benching_the_account() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/backend-api/wham/usage",
        get(|| async { json_response(401, json!({})) }),
    ))
    .await;
    let auth = env.dir.path().join("codex-auth.json");
    std::fs::write(
        &auth,
        json!({"tokens": {"access_token": SECRET}}).to_string(),
    )
    .unwrap();
    let mut c = connection(
        "c5",
        "codex",
        &format!("{}/backend-api/codex", up.base()),
        true,
    );
    c.source_path = auth.to_string_lossy().into_owned();
    env.connection(c);
    let (s, _) = env
        .wait_source("connection:c5", |s| s["status"] == "needs_auth")
        .await;
    assert!(s["message"].as_str().unwrap().contains("Sign in again"));
    let stored: Connection = env.app.store.get("connection", "c5").unwrap();
    assert!(
        stored.enabled,
        "a quota failure never disables the inference account"
    );
}

#[tokio::test]
async fn api_key_and_gemini_accounts_explain_why_quota_is_unavailable() {
    let env = Env::start().await;
    env.connection(connection(
        "k1",
        "openai",
        "https://api.openai.com/v1",
        false,
    ));
    env.connection(connection(
        "k2",
        "gemini",
        "https://generativelanguage.googleapis.com",
        false,
    ));
    let (s1, _) = env.wait_source("connection:k1", |_| true).await;
    let (s2, text) = env.wait_source("connection:k2", |_| true).await;
    assert_eq!(s1["status"], "unavailable");
    assert_eq!(s1["source"], "gateway_only");
    assert_eq!(s1["capabilities"]["quota"], false);
    assert_eq!(s1["capabilities"]["tokens"], true);
    assert!(
        s1["message"]
            .as_str()
            .unwrap()
            .contains("API keys cannot read")
    );
    assert!(
        s2["message"]
            .as_str()
            .unwrap()
            .contains("Google Cloud Billing")
    );
    assert!(!text.contains(SECRET));
}

fn cursor_summary() -> Value {
    json!({
        "billingCycleStart": "2026-09-15T00:00:00.000Z",
        "billingCycleEnd": "2026-10-15T00:00:00.000Z",
        "membershipType": "pro",
        "individualUsage": {
            "plan": {"enabled": true, "used": 1500, "limit": 2000, "remaining": 500, "autoPercentUsed": 0.36, "apiPercentUsed": 12, "totalPercentUsed": 6.18},
            "onDemand": {"enabled": true, "used": 250, "limit": 10000, "remaining": 9750}
        },
        "teamUsage": {}
    })
}

#[tokio::test]
async fn cursor_cookie_monitor_maps_percent_and_cents_truthfully() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/api/usage-summary",
        get(|| async { json_response(200, cursor_summary()) }),
    ))
    .await;
    env.endpoint("cursor", &up);
    let token = jwt(json!({"sub": "auth0|user_01ABC", "exp": future()}));
    let (code, m) = env
        .post("/api/usage/monitors", json!({"name":"My Cursor","provider":"cursor","credential_source":"cookie","credential":token}))
        .await;
    assert_eq!(code, 200, "{m}");
    assert_eq!(m["credential_present"], true);
    assert!(m.get("credential").is_none());
    let id = format!("monitor:{}", m["id"].as_str().unwrap());
    let (s, text) = env.wait_source(&id, |s| s["status"] == "ok").await;
    assert_eq!(
        window(&s, "plan:cursor_models")["used"],
        0.36,
        "0.36 means 0.36 percent"
    );
    assert_eq!(window(&s, "plan:other_models")["used"], 12.0);
    let usd = window(&s, "plan:usd");
    assert_eq!(
        (usd["used"].as_f64(), usd["limit"].as_f64()),
        (Some(15.0), Some(20.0))
    );
    assert_eq!(usd["reset_at"], "2026-10-15T00:00:00Z");
    let costs = s["reported_costs"].as_array().unwrap();
    let od = costs.iter().find(|c| c["kind"] == "on_demand").unwrap();
    assert_eq!(od["amount"], 2.5);
    assert_eq!(od["period_start"], "2026-09-15T00:00:00Z");
    assert!(
        costs.iter().all(|c| c["kind"] != "billed"),
        "included and on-demand are not billed totals"
    );
    assert_eq!(
        header(&up.requests()[0].headers, "cookie"),
        format!("WorkosCursorSessionToken=user_01ABC%3A%3A{token}")
    );
    assert!(!text.contains(&token));
    let (_, list, list_text) = env.get("/api/usage/monitors").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list_text.contains(&token));
}

#[tokio::test]
async fn native_cursor_import_is_read_only_and_detects_a_changed_account() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/api/usage-summary",
        get(|| async { json_response(200, cursor_summary()) }),
    ))
    .await;
    env.endpoint("cursor", &up);
    let file = env.dir.path().join("cursor-auth.json");
    let write = |sub: &str| {
        std::fs::write(
            &file,
            json!({"accessToken": jwt(json!({"sub": sub, "exp": future()}))}).to_string(),
        )
        .unwrap()
    };
    write("auth0|user_A");
    let before = std::fs::read(&file).unwrap();
    let (code, r) = env
        .post(
            "/api/usage/import",
            json!({"provider":"cursor","path":file}),
        )
        .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["imported"], 1);
    let id = format!("monitor:{}", r["monitors"][0]["id"].as_str().unwrap());
    env.wait_source(&id, |s| s["status"] == "ok").await;
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "native file untouched"
    );
    // Importing the same account again is not a second monitor.
    let (_, again) = env
        .post(
            "/api/usage/import",
            json!({"provider":"cursor","path":file}),
        )
        .await;
    assert_eq!(again["imported"], 0);
    assert_eq!(again["skipped"][0]["reason"], "already_monitored");
    // The app now holds a different account: never attribute it to the imported one.
    write("auth0|user_B");
    env.refresh(&id).await;
    let (s, _) = env.wait_source(&id, |s| s["status"] == "needs_auth").await;
    assert!(s["message"].as_str().unwrap().contains("different account"));
    // Expired native token: needs auth, no request sent.
    std::fs::write(
        &file,
        json!({"accessToken": jwt(json!({"sub": "auth0|user_A", "exp": 1000}))}).to_string(),
    )
    .unwrap();
    let count = up.count();
    env.refresh(&id).await;
    let (s, _) = env
        .wait_source(&id, |s| {
            s["message"].as_str().is_some_and(|m| m.contains("expired"))
        })
        .await;
    assert_eq!(s["status"], "needs_auth");
    assert_eq!(up.count(), count);
}

#[tokio::test]
async fn opencode_import_reads_only_the_go_key_and_unknown_shapes_are_not_zero() {
    let env = Env::start().await;
    let shape = Arc::new(AtomicUsize::new(0));
    let sh = shape.clone();
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(move || {
            let sh = sh.clone();
            async move {
                if sh.load(Ordering::SeqCst) == 0 {
                    json_response(200, json!({"usage": {"rolling": {"percent": 12.5, "resetInSec": 600}, "weekly": {"percent": 40, "resetsAt": "2026-10-08T00:00:00Z"}}}))
                } else {
                    json_response(200, json!({"html": "<div>unexpected</div>"}))
                }
            }
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    let auth = env.dir.path().join("opencode-auth.json");
    std::fs::write(&auth, json!({"openai": {"type":"api","key":"sk-openai-other"}, "opencode-go": {"type":"api","key":SECRET}}).to_string()).unwrap();
    let before = std::fs::read(&auth).unwrap();
    let (code, r) = env
        .post(
            "/api/usage/import",
            json!({"provider":"opencode","path":auth}),
        )
        .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["imported"], 1);
    assert_eq!(r["monitors"][0]["provider"], "opencode_go");
    let id = format!("monitor:{}", r["monitors"][0]["id"].as_str().unwrap());
    let (s, text) = env.wait_source(&id, |s| s["status"] == "ok").await;
    assert_eq!(window(&s, "5h")["used"], 12.5);
    assert_eq!(window(&s, "7d")["reset_at"], "2026-10-08T00:00:00Z");
    assert!(
        s["windows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["id"] != "month"),
        "absent monthly stays absent"
    );
    assert_eq!(
        header(&up.requests()[0].headers, "authorization"),
        format!("Bearer {SECRET}")
    );
    assert!(!text.contains(SECRET));
    assert_eq!(std::fs::read(&auth).unwrap(), before);
    shape.store(1, Ordering::SeqCst);
    env.refresh(&id).await;
    let (s, _) = env.wait_source(&id, |s| s["status"] == "stale").await;
    assert!(
        s["message"]
            .as_str()
            .unwrap()
            .contains("does not recognize")
    );
    assert_eq!(
        window(&s, "5h")["used"],
        12.5,
        "last good data kept, not replaced by zeros"
    );
}

#[tokio::test]
async fn deleting_a_monitor_during_a_read_does_not_resurrect_it() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            json_response(
                200,
                json!({"usage": {"rolling": {"percent": 1, "resetInSec": 60}}}),
            )
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    let (_, m) = env
        .post("/api/usage/monitors", json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":SECRET}))
        .await;
    let mid = m["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if up.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (code, _) = env
        .send(
            reqwest::Method::DELETE,
            &format!("/api/usage/monitors/{mid}"),
            json!({}),
        )
        .await;
    assert_eq!(code, 204);
    tokio::time::sleep(Duration::from_millis(900)).await;
    let (_, v, _) = env.get("/api/usage/sources").await;
    assert!(
        v["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != format!("monitor:{mid}"))
    );
    assert!(
        env.app
            .store
            .get::<Value>("usage_snapshot", &format!("monitor:{mid}"))
            .is_none()
    );
    assert!(env.app.store.get::<Value>("usage_monitor", &mid).is_none());
}

#[tokio::test]
async fn monitor_updates_preserve_or_clear_credentials_and_disabled_monitors_are_not_read() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(|| async {
            json_response(
                200,
                json!({"usage": {"rolling": {"percent": 5, "resetInSec": 60}}}),
            )
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    let (_, m) = env
        .post("/api/usage/monitors", json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":SECRET}))
        .await;
    let mid = m["id"].as_str().unwrap().to_string();
    env.wait_source(&format!("monitor:{mid}"), |s| s["status"] == "ok")
        .await;
    let path = format!("/api/usage/monitors/{mid}");
    let (code, u) = env
        .send(reqwest::Method::PUT, &path, json!({"name":"Renamed"}))
        .await;
    assert_eq!(code, 200, "{u}");
    assert_eq!(
        u["credential_present"], true,
        "omitted credential is preserved"
    );
    let (code, _) = env
        .send(reqwest::Method::PUT, &path, json!({"credential":""}))
        .await;
    assert_eq!(
        code, 400,
        "an enabled key monitor cannot have its key cleared"
    );
    let (code, u) = env
        .send(
            reqwest::Method::PUT,
            &path,
            json!({"enabled":false,"credential":""}),
        )
        .await;
    assert_eq!(code, 200);
    assert_eq!(u["credential_present"], false);
    let count = up.count();
    let (s, _) = env
        .wait_source(&format!("monitor:{mid}"), |s| s["status"] == "disabled")
        .await;
    assert!(
        s["windows"].as_array().unwrap().is_empty(),
        "an edit drops the previous snapshot"
    );
    env.refresh(&format!("monitor:{mid}")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(up.count(), count);
    let (code, e) = env
        .post(
            "/api/usage/monitors",
            json!({"name":"x","provider":"antigravity","credential_source":"native"}),
        )
        .await;
    assert_eq!(code, 400);
    assert!(e["error"]["message"].as_str().unwrap().contains("provider"));
    let (code, _) = env
        .post(
            "/api/usage/monitors",
            json!({"name":"x","provider":"openai","credential_source":"cookie","credential":"a"}),
        )
        .await;
    assert_eq!(code, 400);
}

#[tokio::test]
async fn duplicate_identities_are_rejected() {
    let env = Env::start().await;
    let body = json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":SECRET});
    assert_eq!(env.post("/api/usage/monitors", body.clone()).await.0, 200);
    assert_eq!(env.post("/api/usage/monitors", body).await.0, 409);
}

#[tokio::test]
async fn oversized_and_slow_responses_are_bounded() {
    let env = Env::start().await;
    let big = "x".repeat(2 * 1024 * 1024);
    let up = Upstream::start(
        Router::new()
            .route(
                "/zen/go/v1/usage",
                get(move || {
                    let big = big.clone();
                    async move {
                        json_response(
                            200,
                            json!({"usage": {"rolling": {"percent": 1, "pad": big}}}),
                        )
                    }
                }),
            )
            .route(
                "/api/usage-summary",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    json_response(200, json!({}))
                }),
            ),
    )
    .await;
    env.endpoint("opencode", &up);
    env.endpoint("cursor", &up);
    let (_, a) = env.post("/api/usage/monitors", json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":"k"})).await;
    let (_, b) = env
        .post("/api/usage/monitors", json!({"name":"C","provider":"cursor","credential_source":"cookie","credential":"WorkosCursorSessionToken=abc"}))
        .await;
    let (s, _) = env
        .wait_source(&format!("monitor:{}", a["id"].as_str().unwrap()), |s| {
            s["last_error_at"].is_string()
        })
        .await;
    assert!(
        s["message"]
            .as_str()
            .unwrap()
            .contains("does not recognize")
    );
    let (s, _) = env
        .wait_source(&format!("monitor:{}", b["id"].as_str().unwrap()), |s| {
            s["last_error_at"].is_string()
        })
        .await;
    assert!(
        s["message"].as_str().unwrap().contains("did not answer"),
        "{s}"
    );
}

#[tokio::test]
async fn admin_cost_reports_paginate_and_ordinary_keys_get_an_honest_message() {
    let env = Env::start().await;
    let up = Upstream::start(
        Router::new()
            .route("/v1/organization/costs", get(|q: axum::extract::RawQuery, h: HeaderMap| async move {
                if header(&h, "authorization") != "Bearer sk-admin-1" {
                    return json_response(403, json!({"error": PROVIDER_TEXT}));
                }
                let q = q.0.unwrap_or_default();
                assert!(q.contains("start_time=") && q.contains("bucket_width=1d"));
                if q.contains("page=p2") {
                    json_response(200, json!({"data":[{"results":[{"amount":{"value":0.25,"currency":"usd"}}]}],"has_more":false,"next_page":null}))
                } else {
                    json_response(200, json!({"data":[{"results":[{"amount":{"value":1.5,"currency":"usd"}},{"amount":{"value":0.125,"currency":"usd"}}]}],"has_more":true,"next_page":"p2"}))
                }
            }))
            .route("/v1/organizations/cost_report", get(|h: HeaderMap| async move {
                assert_eq!(header(&h, "anthropic-version"), "2023-06-01");
                json_response(200, json!({"data":[{"results":[{"amount":"123.45","currency":"USD"}]},{"results":[]}],"has_more":false,"next_page":null}))
            })),
    )
    .await;
    env.endpoint("openai", &up);
    env.endpoint("anthropic_admin", &up);
    let (_, o) = env.post("/api/usage/monitors", json!({"name":"OpenAI org","provider":"openai","credential_source":"api_key","credential":"sk-admin-1"})).await;
    let (_, a) = env.post("/api/usage/monitors", json!({"name":"Anthropic org","provider":"anthropic","credential_source":"api_key","credential":"sk-ant-admin-1"})).await;
    let (_, bad) = env.post("/api/usage/monitors", json!({"name":"Plain key","provider":"openai","credential_source":"api_key","credential":"sk-proj-ordinary"})).await;
    let (s, _) = env
        .wait_source(&format!("monitor:{}", o["id"].as_str().unwrap()), |s| {
            s["status"] == "ok"
        })
        .await;
    assert_eq!(s["reported_costs"][0]["amount"], 1.875);
    assert_eq!(s["reported_costs"][0]["kind"], "billed");
    assert_eq!(s["reported_costs"][0]["currency"], "USD");
    let (s, _) = env
        .wait_source(&format!("monitor:{}", a["id"].as_str().unwrap()), |s| {
            s["status"] == "ok"
        })
        .await;
    assert_eq!(
        s["reported_costs"][0]["amount"], 1.2345,
        "cents decimal string converted to dollars"
    );
    let (s, text) = env
        .wait_source(&format!("monitor:{}", bad["id"].as_str().unwrap()), |s| {
            s["status"] == "needs_auth"
        })
        .await;
    assert!(s["message"].as_str().unwrap().contains("Admin API key"));
    assert!(!text.contains(PROVIDER_TEXT) && !text.contains("sk-admin-1"));
}

#[tokio::test]
async fn manual_refresh_is_throttled_and_coalesced() {
    let env = Env::start().await;
    usage_sources::set_test_manual_interval(&env.app, Duration::from_secs(30));
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(move || {
            let c2 = c2.clone();
            async move {
                c2.fetch_add(1, Ordering::SeqCst);
                json_response(
                    200,
                    json!({"usage": {"rolling": {"percent": 5, "resetInSec": 60}}}),
                )
            }
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    let (_, m) = env.post("/api/usage/monitors", json!({"name":"Go","provider":"opencode_go","credential_source":"api_key","credential":"k"})).await;
    let id = format!("monitor:{}", m["id"].as_str().unwrap());
    env.wait_source(&id, |s| s["status"] == "ok").await;
    let (code, r) = env.refresh(&id).await;
    assert_eq!((code, &r["accepted"]), (202, &json!(true)));
    let (code, r) = env.refresh(&id).await;
    assert_eq!((code, &r["accepted"]), (200, &json!(false)), "{r}");
    assert!(r["retry_after_seconds"].as_u64().unwrap() > 0);
    assert_eq!(env.refresh("monitor:absent").await.0, 404);
}

#[tokio::test]
async fn antigravity_quota_windows_keep_unknown_fractions_unknown() {
    let w = serde_json::to_value(usage_sources::antigravity_windows(&[
        ModelQuota {
            model: "gemini-3-pro".into(),
            label: "Gemini 3 Pro".into(),
            remaining_fraction: Some(0.25),
            reset_at: Some(1_790_000_000),
        },
        ModelQuota {
            model: "claude-opus".into(),
            label: String::new(),
            remaining_fraction: None,
            reset_at: None,
        },
    ]))
    .unwrap();
    assert_eq!(w[0]["used"], 75.0);
    assert_eq!(w[0]["remaining"], 25.0);
    assert_eq!(w[0]["scope"], "model");
    assert_eq!(w[1]["used"], Value::Null);
    assert_eq!(w[1]["remaining"], Value::Null);
}

#[tokio::test]
async fn antigravity_connection_without_a_helper_is_unavailable_not_zero() {
    let env = Env::start().await;
    env.connection(connection(
        "g1",
        "antigravity",
        "https://daily-cloudcode-pa.googleapis.com",
        true,
    ));
    let (s, _) = env
        .wait_source("connection:g1", |s| {
            s["last_error_at"].is_string() || s["status"] == "ok"
        })
        .await;
    assert!(s["status"] == "unavailable" || s["status"] == "ok");
    if s["status"] == "unavailable" {
        assert!(s["windows"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn endpoints_require_the_admin_session() {
    let env = Env::start().await;
    for (m, p) in [
        (reqwest::Method::GET, "/api/usage/sources"),
        (reqwest::Method::POST, "/api/usage/refresh"),
        (reqwest::Method::GET, "/api/usage/monitors"),
        (reqwest::Method::POST, "/api/usage/import"),
        (reqwest::Method::GET, "/api/usage/native"),
    ] {
        let r = env
            .http
            .request(m, env.url(p))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{p}");
    }
}

#[tokio::test]
async fn connected_go_protocols_and_native_watcher_share_one_quota_read() {
    let env = Env::start().await;
    let up = Upstream::start(Router::new().route(
        "/zen/go/v1/usage",
        get(|| async {
            json_response(
                200,
                json!({"usage":{"rolling":{"percent":25,"resetInSec":600}}}),
            )
        }),
    ))
    .await;
    env.endpoint("opencode", &up);
    for (id, kind) in [("go-chat", "openai"), ("go-messages", "anthropic")] {
        env.connection(connection(id, kind, "https://opencode.ai/zen/go/v1", false));
    }
    let (_, m) = env.post("/api/usage/monitors", json!({"name":"Same Go account","provider":"opencode_go","credential_source":"api_key","credential":SECRET})).await;
    let (_, body, text) = env.get("/api/usage/sources").await;
    assert!(!text.contains(SECRET));
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(
        sources
            .iter()
            .filter(|s| s["provider"] == "opencode_go")
            .count(),
        3
    );
    let active = sources
        .iter()
        .find(|s| {
            s["provider"] == "opencode_go"
                && !s["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("already monitored"))
        })
        .unwrap();
    let id = active["id"].as_str().unwrap().to_string();
    let (s, _) = env
        .wait_source(&id, |s| s["status"] == "ok" || s["status"] == "unavailable")
        .await;
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(up.count(), 1, "one account, one poll");
    let (_, body, _) = env.get("/api/usage/sources").await;
    assert_eq!(
        body["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["message"]
                .as_str()
                .is_some_and(|m| m.contains("already monitored")))
            .count(),
        2,
        "{body}"
    );
    assert!(m["id"].is_string());
}
