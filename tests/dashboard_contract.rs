//! Dashboard API contract: model catalog discovery, connection health, request lookup and the
//! unauthenticated probe. Real gateway, loopback mock providers, fake credentials only.

mod support;

use axum::{
    Router,
    body::Body,
    extract::Query,
    http::HeaderMap,
    response::Response,
    routing::{get, post},
};
use base64::Engine;
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use support::*;

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

fn catalog(body: Value) -> Router {
    Router::new().route(
        "/models",
        get(move || {
            let body = body.clone();
            async move { json_response(200, body) }
        }),
    )
}

async fn discover(gw: &Gateway, id: &str) -> (u16, Value, String) {
    let r = gw.admin_get(&format!("/api/connections/{id}/models")).await;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
    )
}

fn ids(v: &Value) -> Vec<String> {
    v["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

async fn stored_models(gw: &Gateway, id: &str) -> Value {
    gw.admin_json("/api/connections")
        .await
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["models"]
        .clone()
}

async fn health(gw: &Gateway, id: &str) -> Value {
    let all = gw.admin_json("/api/connections").await;
    all.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
        .clone()
}

fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}

// ---------------------------------------------------------------------------------------------
// Model catalog discovery
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn catalog_discovery_requires_admin_and_a_known_connection() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(catalog(json!({"data":[{"id":"m"}]}))).await;
    let id = gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let path = format!("/api/connections/{id}/models");

    assert_eq!(
        gw.http.get(gw.url(&path)).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        gw.http
            .get(gw.url(&path))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "client keys cannot browse catalogs"
    );
    let r = gw
        .http
        .get(gw.url(&path))
        .bearer_auth(&gw.admin)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(up.count(), 0, "rejected requests never reach the provider");
    assert_eq!(
        gw.admin_get("/api/connections/missing/models")
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn catalogs_in_every_provider_format_are_normalised_and_never_saved() {
    let (_d, gw) = setup().await;
    let openai = Upstream::start(catalog(json!({"object":"list","data":[
        {"id":"gpt-b","object":"model"},
        {"id":"gpt-a","object":"model"},
        {"id":"gpt-a","object":"model"},
        {"id":"","object":"model"},
        {"id":42},
        {"id":"x".repeat(201)},
        {"object":"model"}
    ]})))
    .await;
    let anthropic = Upstream::start(catalog(json!({"data":[
        {"type":"model","id":"claude-opus-5-5","display_name":"Claude Opus 5.5"},
        {"type":"model","id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku 4.5"}
    ],"has_more":false})))
    .await;
    let codex = Upstream::start(catalog(json!({"models":[
        {"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"},
        {"slug":"gpt-6-luna","display_name":"GPT-6 Luna"}
    ]})))
    .await;
    let gemini = Upstream::start(catalog(json!({"models":[
        {"name":"models/gemini-3-pro","displayName":"Gemini 3 Pro","supportedGenerationMethods":["generateContent"]},
        {"name":"models/text-embedding-9","displayName":"Text Embedding 9"}
    ]}))).await;

    let o = gw
        .connection("OpenAI", "openai", &openai.base(), &["configured-only"])
        .await;
    let a = gw
        .connection(
            "Anthropic",
            "anthropic",
            &anthropic.base(),
            &["configured-only"],
        )
        .await;
    let c = gw
        .connection("Codex", "codex", &codex.base(), &["configured-only"])
        .await;
    let g = gw
        .connection("Gemini", "gemini", &gemini.base(), &["configured-only"])
        .await;

    let (s, v, text) = discover(&gw, &o).await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["connection_id"], o.as_str());
    assert_eq!(
        ids(&v),
        ["gpt-a", "gpt-b"],
        "deduplicated, invalid ids dropped, sorted: {v}"
    );
    assert_eq!(
        v["models"][0],
        json!({"id":"gpt-a","name":"gpt-a"}),
        "name falls back to the id"
    );
    let req = &openai.requests()[0];
    assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/models"));
    assert_eq!(
        req.header("authorization").unwrap(),
        format!("Bearer {PROVIDER_KEY}")
    );

    let (_, v, _) = discover(&gw, &a).await;
    assert_eq!(
        v["models"],
        json!([
            {"id":"claude-haiku-4-5-20251001","name":"Claude Haiku 4.5"},
            {"id":"claude-opus-5-5","name":"Claude Opus 5.5"}
        ])
    );
    let req = &anthropic.requests()[0];
    assert_eq!(req.header("x-api-key").as_deref(), Some(PROVIDER_KEY));
    assert!(req.header("anthropic-version").is_some());

    let (_, v, _) = discover(&gw, &c).await;
    assert_eq!(
        v["models"],
        json!([
            {"id":"gpt-6-luna","name":"GPT-6 Luna"},
            {"id":"gpt-6.1-sol","name":"GPT-6.1 Sol"}
        ])
    );
    let req = &codex.requests()[0];
    assert!(
        req.query
            .as_deref()
            .is_some_and(|q| q.starts_with("client_version=")),
        "{:?}",
        req.query
    );
    assert_eq!(req.header("originator").as_deref(), Some("codex_cli_rs"));

    let (_, v, _) = discover(&gw, &g).await;
    assert_eq!(
        v["models"],
        json!([
            {"id":"gemini-3-pro","name":"Gemini 3 Pro"},
            {"id":"text-embedding-9","name":"Text Embedding 9"}
        ]),
        "models/ prefix stripped"
    );
    assert_eq!(
        gemini.requests()[0].header("x-goog-api-key").as_deref(),
        Some(PROVIDER_KEY)
    );

    // Discovery is read-only: configured models, routes and history are untouched.
    for id in [&o, &a, &c, &g] {
        assert_eq!(stored_models(&gw, id).await, json!(["configured-only"]));
    }
    assert_eq!(
        gw.request_log().await,
        json!([]).as_array().unwrap().clone(),
        "discovery is not inference traffic"
    );
    let all = gw.admin_get("/api/connections").await.text().await.unwrap();
    assert!(!all.contains(PROVIDER_KEY));
}

#[tokio::test]
async fn catalog_size_and_entry_count_are_bounded() {
    let (_d, gw) = setup().await;
    let many: Vec<Value> = (0..1500)
        .map(|i| json!({"id":format!("model-{i:04}")}))
        .collect();
    let big = Upstream::start(catalog(json!({"data":many}))).await;
    let huge = Upstream::start(Router::new().route(
        "/models",
        get(|| async {
            // About 3 MiB, streamed without Content-Length so the bound applies while reading.
            let entry = format!("{{\"id\":\"{}\"}},", "m".repeat(150));
            let s = async_stream::stream! {
                yield Ok::<_, std::io::Error>(axum::body::Bytes::from("{\"data\":["));
                for _ in 0..20_000 { yield Ok(axum::body::Bytes::from(entry.clone())); }
                yield Ok(axum::body::Bytes::from("{\"id\":\"end\"}]}"));
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from_stream(s))
                .unwrap()
        }),
    ))
    .await;
    let b = gw.connection("Big", "openai", &big.base(), &["m"]).await;
    let h = gw.connection("Huge", "openai", &huge.base(), &["m"]).await;

    let (s, v, _) = discover(&gw, &b).await;
    assert_eq!(s, 200);
    let got = ids(&v);
    assert_eq!(got.len(), 1000, "at most 1,000 entries");
    assert_eq!(got.first().unwrap(), "model-0000");
    assert_eq!(
        got.last().unwrap(),
        "model-0999",
        "the first 1,000 catalog entries are kept"
    );

    let (s, v, _) = discover(&gw, &h).await;
    assert_eq!(s, 502, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("2 MiB"),
        "{v}"
    );
}

#[tokio::test]
async fn catalog_failures_are_explained_without_leaking_provider_bodies() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(
        Router::new()
            .route(
                "/e500/models",
                get(|| async {
                    json_response(
                        500,
                        json!({"error":{"message":format!("internal; key {PROVIDER_KEY}")}}),
                    )
                }),
            )
            .route("/html/models", get(|| async { "<html>gateway</html>" }))
            .route(
                "/shape/models",
                get(|| async { json_response(200, json!({"items":[{"id":"x"}]})) }),
            )
            .route(
                "/limit/models",
                get(|| async {
                    Response::builder()
                        .status(429)
                        .header("retry-after", "30")
                        .body(Body::from(format!(
                            "{{\"error\":\"slow down {PROVIDER_KEY}\"}}"
                        )))
                        .unwrap()
                }),
            ),
    )
    .await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    let cases = [
        ("e500", format!("{}/e500", up.base()), 500, "manually"),
        (
            "html",
            format!("{}/html", up.base()),
            502,
            "Invalid provider model catalog",
        ),
        ("shape", format!("{}/shape", up.base()), 502, "manually"),
        ("limit", format!("{}/limit", up.base()), 429, "manually"),
        ("dead", dead, 502, "Could not reach"),
    ];
    for (name, base, status, needle) in cases {
        let id = gw.connection(name, "openai", &base, &["kept"]).await;
        let (s, v, text) = discover(&gw, &id).await;
        assert_eq!(s, status, "{name}: {text}");
        assert!(
            !text.contains(PROVIDER_KEY)
                && !text.contains("slow down")
                && !text.contains("internal;"),
            "{name} leaked the provider body: {text}"
        );
        assert!(
            v["error"]["message"].as_str().unwrap().contains(needle),
            "{name}: {text}"
        );
        assert_eq!(
            stored_models(&gw, &id).await,
            json!(["kept"]),
            "{name}: failed discovery must not change models"
        );
    }
}

/// A provider rejecting the account's credential must not look like an expired admin session:
/// the dashboard treats 401 from `/api/*` as "sign in to Switchyard again".
#[tokio::test]
async fn provider_auth_rejection_is_not_reported_as_an_admin_401() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/models",
        get(|| async { json_response(401, json!({"error":{"message":"invalid api key"}})) }),
    ))
    .await;
    let id = gw.connection("Revoked", "openai", &up.base(), &["m"]).await;
    let (s, v, text) = discover(&gw, &id).await;
    assert_ne!(s, 401, "provider 401 surfaced as an admin-auth 401: {text}");
    assert!(v["error"]["message"].is_string());
    // The admin session itself is still valid.
    assert_eq!(gw.admin_get("/api/overview").await.status(), 200);
}

/// An imported Codex login whose source file was rotated by the Codex CLI: discovery retries once
/// with the adopted token, and never writes to the source.
#[tokio::test]
async fn catalog_401_adopts_a_rotated_native_token_once() {
    let (_d, gw) = setup().await;
    let now = chrono::Utc::now().timestamp();
    let claims = |exp: i64, n: &str| {
        jwt(
            json!({"exp":exp,"sub":"user-1","n":n,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-1","chatgpt_plan_type":"plus"}}),
        )
    };
    let old = claims(now + 3600, "old");
    let new = claims(now + 7200, "new");
    let accepted = new.clone();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let up = Upstream::start(Router::new().route(
        "/models",
        get(
            move |h: HeaderMap, Query(_q): Query<HashMap<String, String>>| {
                let accepted = accepted.clone();
                let log = log.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    log.lock().unwrap().push(auth.clone());
                    if auth == format!("Bearer {accepted}") {
                        json_response(200, json!({"models":[{"slug":"gpt-6.1-sol"}]}))
                    } else {
                        json_response(401, json!({"detail":"token expired"}))
                    }
                }
            },
        ),
    ))
    .await;
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    let write = |access: &str| {
        std::fs::write(&file, json!({"tokens":{"access_token":access,"refresh_token":"rt-shared","account_id":"acct-1"}}).to_string()).unwrap();
    };
    write(&old);
    let r = gw
        .admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"codex","path":file}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let id = r.json::<Value>().await.unwrap()["connections"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = gw
        .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":"Codex","kind":"codex","base_url":up.base(),"models":["gpt-6.1-sol"],"supports_websocket":true}))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(health(&gw, &id).await["credential_expires_at"], now + 3600);

    // The Codex CLI rotates the token in its own file.
    write(&new);
    let before = std::fs::read(&file).unwrap();
    let (s, v, text) = discover(&gw, &id).await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(ids(&v), ["gpt-6.1-sol"]);
    let tried = seen.lock().unwrap().clone();
    assert_eq!(
        tried,
        [format!("Bearer {old}"), format!("Bearer {new}")],
        "exactly one retry with the adopted token"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "source file must not be written"
    );
    let c = health(&gw, &id).await;
    assert_eq!(c["credential_source"], "native_codex");
    assert_eq!(c["credential_expires_at"], now + 7200);
    assert!(!text.contains(&new) && !c.to_string().contains(&new));
}

// ---------------------------------------------------------------------------------------------
// Connection health
// ---------------------------------------------------------------------------------------------

/// Per-model failures: status and optional Retry-After.
type Failures = Arc<Mutex<HashMap<String, (u16, Option<&'static str>)>>>;

/// Provider that fails per model, as configured.
fn scripted(fail: Failures) -> Router {
    Router::new().route(
        "/responses",
        post(move |axum::Json(v): axum::Json<Value>| {
            let fail = fail.clone();
            async move {
                let model = v["model"].as_str().unwrap_or("").to_string();
                match fail.lock().unwrap().get(&model).copied() {
                    Some((status, retry)) => {
                        let mut r = Response::builder()
                            .status(status)
                            .header("content-type", "application/json");
                        if let Some(retry) = retry {
                            r = r.header("retry-after", retry);
                        }
                        r.body(Body::from(format!(
                            "{{\"error\":{{\"message\":\"no {PROVIDER_KEY}\"}}}}"
                        )))
                        .unwrap()
                    }
                    None => json_response(
                        200,
                        completed(&format!("resp_{model}"), "ok")["response"].clone(),
                    ),
                }
            }
        }),
    )
}

#[tokio::test]
async fn health_reflects_cooldowns_per_model_and_account_and_clears_on_save() {
    let (_d, gw) = setup().await;
    let fail_a = Arc::new(Mutex::new(HashMap::new()));
    let a = Upstream::start(scripted(fail_a.clone())).await;
    let b = Upstream::start(scripted(Arc::new(Mutex::new(HashMap::new())))).await;
    let ca = gw.connection("A", "openai", &a.base(), &["m1", "m2"]).await;
    let cb = gw.connection("B", "openai", &b.base(), &["m1", "m2"]).await;
    let t = |c: &str, m: &str| json!({"connection_id":c,"model":m});
    gw.put_route("one", "failover", json!([t(&ca, "m1"), t(&cb, "m1")]))
        .await;
    gw.put_route("two", "failover", json!([t(&ca, "m2"), t(&cb, "m2")]))
        .await;
    let (_, key) = gw.create_key("k").await;

    // Fresh accounts: ready, no history, API keys have no expiry.
    let h = health(&gw, &ca).await;
    assert_eq!(
        h["health"],
        json!({"status":"ready","cooldowns":[],"last_used_at":null,"last_status":null,"last_error":null})
    );
    assert_eq!(h["credential_expires_at"], Value::Null);

    // A model-specific rate limit: A is limited for m1 only.
    fail_a
        .lock()
        .unwrap()
        .insert("m1".into(), (429, Some("120")));
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"one","input":"x"}))
            .await
            .status(),
        200
    );
    gw.wait_for_log(1).await;
    let h = health(&gw, &ca).await["health"].clone();
    assert_eq!(h["status"], "limited", "{h}");
    let cooldowns = h["cooldowns"].as_array().unwrap();
    assert_eq!(cooldowns.len(), 1);
    assert_eq!(cooldowns[0]["model"], "m1");
    let retry = cooldowns[0]["retry_after_seconds"].as_u64().unwrap();
    assert!((115..=121).contains(&retry), "{retry}");
    let hb = health(&gw, &cb).await["health"].clone();
    assert_eq!(hb["status"], "ready", "the serving account is unaffected");
    assert_eq!(hb["last_status"], 200);
    assert!(
        hb["last_used_at"]
            .as_str()
            .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
    );
    // m2 still routes to A while it is limited on m1.
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"two","input":"x"}))
            .await
            .status(),
        200
    );
    let log = gw.wait_for_log(2).await;
    assert_eq!(log[0]["connection_name"], "A");

    // Rejected credentials bench the whole account.
    fail_a.lock().unwrap().insert("m2".into(), (401, None));
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"two","input":"x"}))
            .await
            .status(),
        200
    );
    gw.wait_for_log(3).await;
    let h = health(&gw, &ca).await["health"].clone();
    assert_eq!(h["status"], "cooling", "{h}");
    let models: Vec<_> = h["cooldowns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["model"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        models,
        ["*", "m1"],
        "account-wide cooldown is reported as model \"*\""
    );
    assert!(!h.to_string().contains(PROVIDER_KEY));

    // A disabled account reports "disabled" (saving it also clears cooldowns).
    let path = format!("/api/connections/{ca}");
    let save = |enabled: bool, key: Option<&str>| {
        let mut body = json!({"name":"A","kind":"openai","base_url":a.base(),"models":["m1","m2"],"enabled":enabled});
        if let Some(k) = key {
            body["api_key"] = json!(k);
        }
        gw.admin_send(Method::PUT, &path, body)
    };
    assert_eq!(save(false, None).await.status(), 200);
    assert_eq!(health(&gw, &ca).await["health"]["status"], "disabled");
    // Saving the account (here with a fixed key) clears its cooldowns.
    fail_a.lock().unwrap().clear();
    assert_eq!(save(true, Some("sk-fixed")).await.status(), 200);
    let h = health(&gw, &ca).await["health"].clone();
    assert_eq!(h["status"], "ready", "{h}");
    assert_eq!(h["cooldowns"], json!([]));
    assert_eq!(h["last_status"], 200, "history is kept: {h}");
}

#[tokio::test]
async fn health_reports_the_latest_failure_without_secrets() {
    let (_d, gw) = setup().await;
    let fail = Arc::new(Mutex::new(HashMap::from([(
        "bad".to_string(),
        (400u16, None),
    )])));
    let up = Upstream::start(scripted(fail)).await;
    let id = gw
        .connection("Solo", "openai", &up.base(), &["bad", "good"])
        .await;
    let (_, key) = gw.create_key("k").await;
    gw.post("/v1/responses", &key, json!({"model":"good","input":"x"}))
        .await;
    gw.wait_for_log(1).await;
    gw.post("/v1/responses", &key, json!({"model":"bad","input":"x"}))
        .await;
    gw.wait_for_log(2).await;
    let h = health(&gw, &id).await["health"].clone();
    assert_eq!(
        h["status"], "ready",
        "a 400 is a request error, not a cooldown: {h}"
    );
    assert_eq!(h["last_status"], 400);
    assert!(
        h["last_error"]
            .as_str()
            .is_some_and(|e| !e.is_empty() && !e.contains(PROVIDER_KEY)),
        "{h}"
    );
}

// ---------------------------------------------------------------------------------------------
// Request lookup and probe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn request_detail_returns_retained_metadata_and_404_after_expiry() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(scripted(Arc::new(Mutex::new(HashMap::new())))).await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let prompt = "PROMPT-CANARY-dash";
    gw.post("/v1/responses", &key, json!({"model":"m","input":prompt}))
        .await;
    let listed = gw.wait_for_log(1).await[0].clone();
    let id = listed["id"].as_str().unwrap().to_string();
    let path = format!("/api/requests/{id}");

    let r = gw.admin_get(&path).await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(!text.contains(prompt));
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        listed,
        "detail matches the history entry"
    );
    assert_eq!(
        gw.http.get(gw.url(&path)).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        gw.http
            .get(gw.url(&path))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        gw.admin_get("/api/requests/not-a-request").await.status(),
        404
    );

    // Pushed out of the 1,000-entry history: no longer retained.
    for i in 0..1000 {
        gw.app
            .store
            // Built through serde so fields added later with defaults do not break this test.
            .record(
                &serde_json::from_value::<switchyard::store::RequestRecord>(json!({
                    "id": format!("filler-{i}"), "timestamp": switchyard::store::now(), "model": "m",
                    "connection_id": "c", "connection_name": "c", "transport": "http", "status": 200,
                    "latency_ms": 1, "input_tokens": null, "output_tokens": null, "error": null
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let r = gw.admin_get(&path).await;
    assert_eq!(r.status(), 404);
    assert!(r.text().await.unwrap().contains("no longer retained"));
    assert_eq!(gw.admin_get("/api/requests/filler-999").await.status(), 200);
}

#[tokio::test]
async fn hello_probe_is_public_and_reveals_nothing() {
    let (_d, gw) = setup().await;
    let r = gw.http.get(gw.url("/api/hello")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"status":"ok"}));
    let r = gw.http.head(gw.url("/api/hello")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers().get("set-cookie").is_none(),
        "the probe must not mint sessions"
    );
}

#[tokio::test]
async fn anthropic_count_tokens_is_proxied_natively() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/messages/count_tokens",
        post(|| async { json_response(200, json!({"input_tokens":17})) }),
    ))
    .await;
    gw.connection("Claude", "anthropic", &up.base(), &["claude-x"])
        .await;
    gw.connection("OpenAI", "openai", &up.base(), &["gpt-x"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let body = json!({"model":"claude-x","messages":[{"role":"user","content":"hi"}]});
    let r = gw
        .http
        .post(gw.url("/v1/messages/count_tokens"))
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"input_tokens":17}));
    let req = &up.requests()[0];
    assert_eq!(req.path, "/messages/count_tokens");
    assert_eq!(req.header("x-api-key").as_deref(), Some(PROVIDER_KEY));
    assert_eq!(req.json()["model"], "claude-x");
    let r = gw
        .post(
            "/v1/messages/count_tokens",
            &key,
            json!({"model":"gpt-x","messages":[]}),
        )
        .await;
    assert_eq!(r.status(), 400, "token counting is Anthropic-only");
    assert_eq!(
        gw.http
            .post(gw.url("/v1/messages/count_tokens"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}
