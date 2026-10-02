//! End-to-end gateway tests. Every test runs a real Switchyard server on loopback against
//! real loopback Axum mock providers, with state in a private tempdir. No network access.

mod support;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::ws::{Message as AxMessage, WebSocketUpgrade},
    response::Response,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

// ---------------------------------------------------------------------------------------------
// Mock providers
// ---------------------------------------------------------------------------------------------

/// OpenAI-compatible provider answering /responses and /chat/completions with its `name`.
fn openai_mock(name: &'static str) -> Router {
    Router::new()
        .route(
            "/responses",
            post(move |Json(v): Json<Value>| async move {
                let id = format!("resp_{name}");
                let text = format!("hello from {name}");
                if v["stream"] == true {
                    let body = sse(&[
                        json!({"type":"response.created","response":{"id":id}}),
                        json!({"type":"response.output_text.delta","delta":text}),
                        completed(&id, &text),
                    ]);
                    sse_response(Body::from(body))
                } else {
                    let mut r = completed(&id, &text)["response"].clone();
                    r["model"] = v["model"].clone();
                    json_response(200, r)
                }
            }),
        )
        .route(
            "/chat/completions",
            post(move |Json(v): Json<Value>| async move {
                json_response(200, json!({"id":"chatcmpl-1","object":"chat.completion","model":v["model"],
                    "choices":[{"index":0,"message":{"role":"assistant","content":format!("chat from {name}")},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}))
            }),
        )
        .route("/models", get(|| async { Json(json!({"data":[]})) }))
}

/// Provider that always answers with a fixed status and body (and a Retry-After header).
fn failing_mock(status: u16, body: &'static str) -> Router {
    let handler = move || async move {
        Response::builder()
            .status(status)
            .header("retry-after", "7")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    Router::new()
        .route("/responses", post(handler))
        .route("/chat/completions", post(handler))
        .route("/messages", post(handler))
}

/// A loopback port with nothing listening on it.
async fn dead_port() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

/// Split so that every multi-byte UTF-8 sequence is cut after its first byte, plus every `n` bytes.
fn split_mid_utf8(bytes: &[u8], n: usize) -> Vec<Vec<u8>> {
    let mut cuts: Vec<usize> = (n..bytes.len()).step_by(n).collect();
    cuts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b >= 0xC0)
            .map(|(i, _)| i + 1),
    );
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    let mut last = 0;
    for c in cuts {
        out.push(bytes[last..c].to_vec());
        last = c;
    }
    out.push(bytes[last..].to_vec());
    assert!(
        out.iter().any(|c| std::str::from_utf8(c).is_err()),
        "split must cut through a codepoint"
    );
    out
}

fn data_events(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(String::from)
        .collect()
}

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

fn assert_no_secret(label: &str, text: &str) {
    assert!(
        !text.contains(PROVIDER_KEY),
        "{label} leaked the provider credential: {text}"
    );
}

// ---------------------------------------------------------------------------------------------
// Admin authentication and origin protection
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn health_is_public_and_responses_carry_security_headers() {
    let (_d, gw) = setup().await;
    let r = gw.http.get(gw.url("/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(r.headers()["referrer-policy"], "no-referrer");
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    // Unknown API paths are JSON 404s, never the SPA shell.
    let r = gw.admin_get("/api/does-not-exist").await;
    assert_eq!(r.status(), 404);
    let r = gw.http.get(gw.url("/v1/nope")).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn admin_api_requires_admin_token_and_blocks_cross_origin() {
    let (_d, gw) = setup().await;
    let (_, client_key) = gw.create_key("app").await;

    assert!(gw.admin.starts_with("sy_admin_") && gw.admin.len() > 40);
    let r = gw.http.get(gw.url("/api/overview")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .bearer_auth("sy_admin_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // A client API key must not grant administration.
    let r = gw
        .http
        .get(gw.url("/api/keys"))
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(gw.admin_get("/api/overview").await.status(), 200);

    // Even with a valid token, browsers on other origins are refused.
    for (name, value) in [
        ("origin", "https://evil.example".to_string()),
        ("origin", "null".to_string()),
        (
            "origin",
            format!("http://127.0.0.1:{}.evil.example", gw.port),
        ),
        ("sec-fetch-site", "cross-site".to_string()),
        ("sec-fetch-site", "same-site".to_string()),
    ] {
        let r = gw
            .http
            .post(gw.url("/api/keys"))
            .bearer_auth(&gw.admin)
            .header(name, &value)
            .json(&json!({"name":"x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{name}: {value}");
    }
    let keys = gw.admin_json("/api/keys").await;
    assert_eq!(
        keys.as_array().unwrap().len(),
        1,
        "blocked requests must not have side effects"
    );

    // Same-origin requests from the dashboard pass.
    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .bearer_auth(&gw.admin)
        .header("origin", format!("http://127.0.0.1:{}", gw.port))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn browser_session_cookie_is_local_only_and_strict() {
    let (_d, gw) = setup().await;
    let local_origin = format!("http://127.0.0.1:{}", gw.port);

    // Plain clients without the token cannot mint a session.
    let r = gw.http.post(gw.url("/api/session")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = gw
        .http
        .post(gw.url("/api/session"))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert!([401, 403].contains(&r.status().as_u16()), "{}", r.status());
    assert!(r.headers().get("set-cookie").is_none());
    // DNS rebinding: same-origin from the browser's view, but the Host is attacker-controlled.
    let r = gw
        .http
        .post(gw.url("/api/session"))
        .header("host", format!("evil.example:{}", gw.port))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // The local dashboard gets an HttpOnly, SameSite=Strict cookie.
    let r = gw
        .http
        .post(gw.url("/api/session"))
        .header("sec-fetch-site", "same-origin")
        .header("origin", &local_origin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    let cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    assert!(
        !cookie.contains(&gw.admin),
        "session cookie must not be the admin token"
    );
    let pair = cookie.split(';').next().unwrap().to_string();

    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .header("cookie", &pair)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .header("cookie", &pair)
        .header("host", format!("evil.example:{}", gw.port))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        401,
        "cookie must not work through a rebinding Host"
    );
    let r = gw
        .http
        .post(gw.url("/api/keys"))
        .header("cookie", &pair)
        .header("origin", "https://evil.example")
        .json(&json!({"name":"csrf"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .header("cookie", "sy_session=forged")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Sessions are per-process: a restart invalidates old cookies, while the token file persists.
    let admin = gw.admin.clone();
    let gw = gw.restart().await;
    assert_eq!(gw.admin, admin);
    let r = gw
        .http
        .get(gw.url("/api/overview"))
        .header("cookie", &pair)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn events_websocket_requires_admin_and_same_origin() {
    let (_d, gw) = setup().await;
    let url = gw.ws_url("/api/events");

    let err = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, tungstenite::Error::Http(r) if r.status() == 401),
        "{err:?}"
    );

    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", gw.admin).parse().unwrap(),
    );
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(
        matches!(&err, tungstenite::Error::Http(r) if r.status() == 403),
        "{err:?}"
    );

    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", gw.admin).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let first: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(first["type"], "overview");

    // Live request events are pushed to the dashboard.
    let up = Upstream::start(openai_mock("a")).await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"m","input":"hi"}))
            .await
            .status(),
        200
    );
    let ev = loop {
        let v: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        if v["type"] == "request" {
            break v;
        }
    };
    assert_eq!(ev["data"]["model"], "m");
    assert_eq!(ev["data"]["status"], 200);
}

// ---------------------------------------------------------------------------------------------
// Client keys
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn client_keys_are_hashed_shown_once_and_revocable() {
    let (dir, gw) = setup().await;
    let (id, key) = gw.create_key("laptop").await;
    let (_, other) = gw.create_key("ci").await;
    assert!(key.starts_with("sy_") && key.len() >= 40, "{key}");

    let listed = gw.admin_json("/api/keys").await;
    let text = listed.to_string();
    assert!(!text.contains(&key), "listing must not reveal raw keys");
    assert!(
        !text.contains("\"hash\""),
        "listing must not reveal key hashes: {text}"
    );
    let entry = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == id.as_str())
        .unwrap();
    assert_eq!(entry["name"], "laptop");
    assert!(key.starts_with(entry["prefix"].as_str().unwrap()));
    assert!(entry["prefix"].as_str().unwrap().len() < 16);

    // Bearer and x-api-key are both accepted; nothing else is.
    let models = |k: Option<&str>, header: &'static str| {
        let mut r = gw.http.get(gw.url("/v1/models"));
        if let Some(k) = k {
            r = if header == "bearer" {
                r.bearer_auth(k)
            } else {
                r.header(header, k)
            };
        }
        r.send()
    };
    assert_eq!(models(Some(&key), "bearer").await.unwrap().status(), 200);
    assert_eq!(models(Some(&key), "x-api-key").await.unwrap().status(), 200);
    assert_eq!(models(None, "bearer").await.unwrap().status(), 401);
    assert_eq!(models(Some(""), "bearer").await.unwrap().status(), 401);
    assert_eq!(
        models(Some(&format!("{key}x")), "bearer")
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        models(Some(&gw.admin), "bearer").await.unwrap().status(),
        401,
        "admin token is not a client key"
    );
    let body: Value = models(None, "bearer").await.unwrap().json().await.unwrap();
    assert_eq!(body["error"]["type"], "gateway_error");

    // At rest only the SHA-256 digest exists, across the DB and its WAL.
    let digest = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(key.as_bytes()))
    };
    let r = gw.admin_get("/api/overview").await; // force some DB activity
    assert_eq!(r.status(), 200);
    assert!(
        dir_contains(dir.path(), key.as_bytes()).is_empty(),
        "raw client key stored on disk"
    );
    assert!(
        !dir_contains(dir.path(), digest.as_bytes()).is_empty(),
        "key digest should be persisted"
    );

    // Revocation is immediate, survives restart, and leaves other keys intact.
    let r = gw
        .admin_send(Method::DELETE, &format!("/api/keys/{id}"), json!({}))
        .await;
    assert_eq!(r.status(), 204);
    assert_eq!(models(Some(&key), "bearer").await.unwrap().status(), 401);
    let gw = gw.restart().await;
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = gw
        .admin_send(Method::POST, "/api/keys", json!({"name":"   "}))
        .await;
    assert_eq!(r.status(), 400);
}

// ---------------------------------------------------------------------------------------------
// Connections, credentials and routes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_credentials_are_never_returned() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(openai_mock("a").route(
        "/broken/models",
        get(|| async { json_response(401, json!({"error":PROVIDER_KEY})) }),
    ))
    .await;
    let created = gw
        .add_connection(json!({"name":"A","kind":"openai","base_url":format!("{}/",up.base()),"models":["m"],"api_key":PROVIDER_KEY}))
        .await;
    assert_no_secret("create", &created.to_string());
    assert_eq!(created["credential_present"], true);
    assert_eq!(
        created["base_url"],
        up.base(),
        "trailing slash is normalised"
    );
    let id = created["id"].as_str().unwrap().to_string();

    // Updating without a key keeps the stored credential.
    let r = gw
        .admin_send(
            Method::PUT,
            &format!("/api/connections/{id}"),
            json!({"name":"A2","kind":"openai","base_url":up.base(),"models":["m","m2"]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let updated: Value = r.json().await.unwrap();
    assert_no_secret("update", &updated.to_string());
    assert_eq!(updated["credential_present"], true);

    let r = gw
        .admin_send(
            Method::POST,
            &format!("/api/connections/{id}/test"),
            json!({}),
        )
        .await;
    let tested: Value = r.json().await.unwrap();
    assert_eq!(tested["ok"], true, "{tested}");
    assert_eq!(
        up.requests()
            .last()
            .unwrap()
            .header("authorization")
            .unwrap(),
        format!("Bearer {PROVIDER_KEY}")
    );

    let (_, key) = gw.create_key("k").await;
    gw.post("/v1/responses", &key, json!({"model":"m","input":"hi"}))
        .await;

    for path in [
        "/api/connections",
        "/api/models",
        "/api/overview",
        "/api/config",
        "/api/requests",
        "/api/routes",
        "/api/keys",
    ] {
        let r = gw.admin_get(path).await;
        assert_eq!(r.status(), 200, "{path}");
        assert_no_secret(path, &r.text().await.unwrap());
    }
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_no_secret("/v1/models", &r.text().await.unwrap());

    // A provider echoing the credential in an error body must not reflect it to the client.
    let bad = gw
        .connection("B", "openai", &format!("{}/broken", up.base()), &["b"])
        .await;
    let r = gw
        .admin_send(
            Method::POST,
            &format!("/api/connections/{bad}/test"),
            json!({}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert_no_secret("test failure", &text);
    assert!(text.contains("\"ok\":false"), "{text}");
}

#[tokio::test]
async fn connection_input_is_validated() {
    let (_d, gw) = setup().await;
    let ok = json!({"name":"x","kind":"openai","base_url":"http://127.0.0.1:9","models":["m"]});
    let cases = [
        ("kind", json!("mystery")),
        ("base_url", json!("http://api.example.com/v1")),
        ("base_url", json!("https://user:pw@api.example.com/v1")),
        ("base_url", json!("https://api.example.com/v1?key=1")),
        ("base_url", json!("file:///etc/passwd")),
        ("base_url", json!("not a url")),
        ("models", json!([])),
        ("models", json!([""])),
        ("name", json!("")),
        ("api_key", json!("abc\r\nx-injected: 1")),
    ];
    for (field, value) in cases {
        let mut body = ok.clone();
        body[field] = value.clone();
        let r = gw.admin_send(Method::POST, "/api/connections", body).await;
        assert_eq!(r.status(), 400, "{field}={value}");
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
    }
    let r = gw
        .admin_send(Method::POST, "/api/connections", json!({"name":"x","kind":"anthropic","base_url":"https://api.anthropic.com/v1","models":["c"],"supports_websocket":true}))
        .await;
    assert_eq!(r.status(), 400, "WebSocket is only valid for OpenAI/Codex");
    // Valid: HTTPS remote, and HTTP on localhost.
    for base in [
        "https://api.example.com/v1",
        "http://localhost:1234/v1",
        "http://[::1]:1234",
    ] {
        let mut body = ok.clone();
        body["base_url"] = json!(base);
        assert_eq!(
            gw.admin_send(Method::POST, "/api/connections", body)
                .await
                .status(),
            200,
            "{base}"
        );
    }
    let r = gw
        .admin_send(Method::PUT, "/api/connections/missing", ok)
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn connections_routes_keys_and_settings_survive_restart() {
    let (_d, gw) = setup().await;
    let a = gw
        .connection("Alpha", "openai", "http://127.0.0.1:1/v1", &["gpt-a"])
        .await;
    let b = gw
        .connection("Beta", "codex", "http://127.0.0.1:2", &["gpt-b"])
        .await;
    let r = gw
        .put_route(
            "team/fast",
            "failover",
            json!([{"connection_id":a,"model":"gpt-a"},{"connection_id":b,"model":"gpt-b"}]),
        )
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let (_, key) = gw.create_key("persist").await;
    assert_eq!(
        gw.admin_send(Method::POST, "/api/settings", json!({"paused":true}))
            .await
            .status(),
        200
    );

    let gw = gw.restart().await;
    let conns = gw.admin_json("/api/connections").await;
    let names: Vec<_> = conns
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"Alpha".into()) && names.contains(&"Beta".into()));
    assert!(
        conns
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["credential_present"] == true)
    );
    let routes = gw.admin_json("/api/routes").await;
    assert_eq!(routes[0]["model"], "team/fast");
    assert_eq!(routes[0]["strategy"], "failover");
    assert_eq!(routes[0]["targets"][1]["connection_id"], b.as_str());
    assert_eq!(gw.admin_json("/api/overview").await["paused"], true);
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    let ids: Vec<Value> = r.json::<Value>().await.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![json!("gpt-a"), json!("gpt-b"), json!("team/fast")]
    );

    // Paused gateways refuse traffic without touching providers.
    let r = gw
        .post("/v1/responses", &key, json!({"model":"gpt-a","input":"x"}))
        .await;
    assert_eq!(r.status(), 503);

    // Referenced connections cannot be deleted until the route is removed.
    let r = gw
        .admin_send(Method::DELETE, &format!("/api/connections/{a}"), json!({}))
        .await;
    assert_eq!(r.status(), 409);
    assert_eq!(
        gw.admin_send(Method::DELETE, "/api/routes/team/fast", json!({}))
            .await
            .status(),
        204
    );
    assert_eq!(
        gw.admin_send(Method::DELETE, &format!("/api/connections/{a}"), json!({}))
            .await
            .status(),
        204
    );

    let gw = gw.restart().await;
    assert_eq!(gw.admin_json("/api/routes").await, json!([]));
    assert_eq!(
        gw.admin_json("/api/connections")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn route_errors_are_reported_clearly() {
    let (_d, gw) = setup().await;
    let a = gw
        .connection("A", "openai", "http://127.0.0.1:1", &["m"])
        .await;
    let cases = [
        (
            "unknown strategy",
            "random",
            json!([{"connection_id":a,"model":"m"}]),
        ),
        ("no targets", "failover", json!([])),
        (
            "unknown connection",
            "failover",
            json!([{"connection_id":"nope","model":"m"}]),
        ),
        (
            "model not on connection",
            "failover",
            json!([{"connection_id":a,"model":"other"}]),
        ),
        ("malformed target", "failover", json!([{"model":"m"}])),
    ];
    for (label, strategy, targets) in cases {
        let r = gw.put_route("alias", strategy, targets).await;
        assert_eq!(r.status(), 400, "{label}");
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"]["message"].as_str().is_some_and(|m| m.len() > 5),
            "{label}: {v}"
        );
    }
    assert_eq!(gw.admin_json("/api/routes").await, json!([]));

    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"ghost","input":"x"}))
        .await;
    assert_eq!(r.status(), 404);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No enabled connection"),
        "{v}"
    );
    let r = gw.post("/v1/responses", &key, json!({"input":"x"})).await;
    assert_eq!(r.status(), 400);
    let r = gw
        .http
        .post(gw.url("/v1/responses"))
        .bearer_auth(&key)
        .body("{not json")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error());
    let r = gw
        .http
        .post(gw.url("/v1/responses"))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body(vec![b' '; 65 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413, "inference bodies are capped at 64 MiB");
    // Admin endpoints keep the tighter 8 MiB cap.
    let r = gw
        .http
        .post(gw.url("/api/keys"))
        .bearer_auth(&gw.admin)
        .header("content-type", "application/json")
        .body(vec![b' '; 9 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
}

#[tokio::test]
async fn disabled_providers_receive_no_traffic() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(openai_mock("a")).await;
    let (_, key) = gw.create_key("k").await;
    let c = gw
        .add_connection(json!({"name":"A","kind":"openai","base_url":up.base(),"models":["m"],"api_key":PROVIDER_KEY,"enabled":false}))
        .await;
    let id = c["id"].as_str().unwrap();
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["data"], json!([]));
    assert_eq!(gw.admin_json("/api/models").await, json!([]));
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 404);

    // A route whose only target is disabled is unavailable too.
    assert_eq!(
        gw.put_route(
            "alias",
            "round_robin",
            json!([{"connection_id":id,"model":"m"}])
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"alias","input":"x"}))
            .await
            .status(),
        404
    );
    assert_eq!(up.count(), 0);

    let r = gw
        .admin_send(
            Method::PUT,
            &format!("/api/connections/{id}"),
            json!({"name":"A","kind":"openai","base_url":up.base(),"models":["m"],"enabled":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"alias","input":"x"}))
            .await
            .status(),
        200
    );
    assert_eq!(up.count(), 1);
}

// ---------------------------------------------------------------------------------------------
// HTTP proxying
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn responses_http_rewrites_model_and_never_forwards_client_auth() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(openai_mock("a")).await;
    let a = gw
        .connection("A", "openai", &up.base(), &["gpt-real"])
        .await;
    gw.put_route(
        "my-alias",
        "round_robin",
        json!([{"connection_id":a,"model":"gpt-real"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .http
        .post(gw.url("/v1/responses"))
        .bearer_auth(&key)
        .header("cookie", "sy_session=abc; other=1")
        .header("x-custom-secret", "nope")
        .header("openai-beta", "responses=v1")
        .header("idempotency-key", "idem-1")
        .json(&json!({"model":"my-alias","input":"hi","temperature":0.2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["id"], "resp_a");
    assert_eq!(body["model"], "gpt-real");

    let req = &up.requests()[0];
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/responses")
    );
    assert_eq!(
        req.header("authorization").unwrap(),
        format!("Bearer {PROVIDER_KEY}")
    );
    assert_eq!(req.json()["model"], "gpt-real");
    assert_eq!(
        req.json()["temperature"],
        0.2,
        "OpenAI payloads pass through untouched"
    );
    assert_eq!(req.header("openai-beta").as_deref(), Some("responses=v1"));
    assert_eq!(req.header("idempotency-key").as_deref(), Some("idem-1"));
    for h in ["cookie", "x-custom-secret", "x-api-key"] {
        assert!(req.header(h).is_none(), "{h} must not be forwarded");
    }
    for (_, v) in req.headers.iter() {
        assert!(
            !v.to_str().unwrap_or("").contains(&key),
            "client key forwarded upstream"
        );
    }

    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["model"], "my-alias");
    assert_eq!(log[0]["status"], 200);
    assert_eq!(log[0]["transport"], "http");
    assert_eq!(log[0]["connection_name"], "A");
    assert_eq!(log[0]["input_tokens"], 11);
    assert_eq!(log[0]["output_tokens"], 7);

    // Chat completions to an OpenAI connection are a passthrough.
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"my-alias","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "chat from a"
    );
    assert_eq!(up.requests()[1].path, "/chat/completions");
    let overview = gw.admin_json("/api/overview").await;
    assert_eq!(overview["requests_total"], 2);
    assert_eq!(overview["transport_counts"]["http"], 2);
}

#[tokio::test]
async fn responses_sse_is_streamed_byte_exact_across_split_utf8_chunks() {
    let (_d, gw) = setup().await;
    let events = [
        json!({"type":"response.created","response":{"id":"resp_1"}}),
        json!({"type":"response.output_text.delta","delta":"Grüße 🦀 "}),
        json!({"type":"response.output_text.delta","delta":"世界 ✓"}),
        completed("resp_1", "Grüße 🦀 世界 ✓"),
    ];
    let body = sse(&events).into_bytes();
    let expected = body.clone();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(chunked_body(split_mid_utf8(&body, 13))) }
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"m","input":"x","stream":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let mut got = Vec::new();
    let mut stream = r.bytes_stream();
    let mut frames = 0;
    while let Some(b) = stream.next().await {
        got.extend_from_slice(&b.unwrap());
        frames += 1;
    }
    assert!(frames > 1, "response should be streamed incrementally");
    assert_eq!(
        String::from_utf8(got).unwrap(),
        String::from_utf8(expected).unwrap()
    );

    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["transport"], "sse");
    assert_eq!(log[0]["status"], 200);
    assert_eq!(
        log[0]["input_tokens"], 11,
        "usage parsed from split SSE: {}",
        log[0]
    );
}

#[tokio::test]
async fn codex_chat_stream_is_shaped_and_translated_across_split_chunks() {
    let (_d, gw) = setup().await;
    let events = [
        json!({"type":"response.created","response":{"id":"resp_c"}}),
        json!({"type":"response.output_text.delta","delta":"Héllo 🦀"}),
        json!({"type":"response.output_text.delta","delta":" 世界"}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"city\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"Zürich\"}"}),
        completed("resp_c", "Héllo 🦀 世界"),
    ];
    // CRLF line endings are legal SSE and used by some proxies.
    let body = sse(&events).replace('\n', "\r\n").into_bytes();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(chunked_body(split_mid_utf8(&body, 11))) }
        }),
    ))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","stream":true,"temperature":0.5,"max_tokens":99,"stream_options":{"include_usage":true},
                "messages":[{"role":"system","content":"Be terse."},{"role":"user","content":"Weather?"}],
                "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    let data = data_events(&text);
    assert_eq!(data.last().map(String::as_str), Some("[DONE]"), "{text}");
    let chunks: Vec<Value> = data[..data.len() - 1]
        .iter()
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk" && c["model"] == "gpt-codex")
    );
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Héllo 🦀 世界");
    let args: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect();
    assert_eq!(args, "{\"city\":\"Zürich\"}");
    let call = chunks
        .iter()
        .find(|c| c["choices"][0]["delta"]["tool_calls"][0]["id"] == "call_1")
        .expect("tool call header");
    assert_eq!(
        call["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        last["usage"]["completion_tokens"], 7,
        "Chat usage uses Chat field names"
    );

    // Upstream payload shaping for the ChatGPT Codex backend.
    let req = &up.requests()[0];
    assert_eq!(req.path, "/responses");
    assert_eq!(req.header("accept").as_deref(), Some("text/event-stream"));
    assert_eq!(req.header("originator").as_deref(), Some("codex_cli_rs"));
    assert!(
        req.header("user-agent")
            .unwrap()
            .starts_with("codex_cli_rs/")
    );
    assert_eq!(
        req.header("authorization").unwrap(),
        format!("Bearer {PROVIDER_KEY}")
    );
    let p = req.json();
    assert_eq!(p["model"], "gpt-codex");
    assert_eq!(p["stream"], true);
    assert_eq!(p["store"], false);
    assert_eq!(p["instructions"], "Be terse.");
    assert_eq!(p["input"], json!([{"role":"user","content":"Weather?"}]));
    assert_eq!(
        p["tools"],
        json!([{"type":"function","name":"get_weather","parameters":{"type":"object"}}])
    );
    for k in [
        "temperature",
        "max_tokens",
        "max_output_tokens",
        "stream_options",
        "messages",
    ] {
        assert!(p.get(k).is_none(), "{k} must be stripped for Codex: {p}");
    }
}

#[tokio::test]
async fn codex_non_streaming_requests_collect_the_completed_response() {
    let (_d, gw) = setup().await;
    let body = sse(&[
        json!({"type":"response.created","response":{"id":"resp_x"}}),
        json!({"type":"response.output_text.delta","delta":"done ✓"}),
        completed("resp_x", "done ✓"),
    ])
    .into_bytes();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(chunked_body(split_mid_utf8(&body, 17))) }
        }),
    ))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw.post("/v1/responses", &key, json!({"model":"gpt-codex","input":"plain string","max_output_tokens":50,"instructions":"Custom."})).await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("application/json")
    );
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["id"], "resp_x");
    assert_eq!(v["output"][0]["content"][0]["text"], "done ✓");
    let p = up.requests()[0].json();
    assert_eq!(p["stream"], true);
    assert_eq!(p["store"], false);
    assert_eq!(p["instructions"], "Custom.");
    assert_eq!(
        p["input"],
        json!([{"role":"user","content":[{"type":"input_text","text":"plain string"}]}])
    );
    assert!(p.get("max_output_tokens").is_none());

    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "done ✓");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 11);
    assert_eq!(
        up.requests()[1].json()["instructions"],
        "You are a helpful coding assistant.",
        "default instructions when none given"
    );
}

#[tokio::test]
async fn codex_stream_failure_is_reported() {
    let (_d, gw) = setup().await;
    let body = sse(&[
        json!({"type":"response.created","response":{"id":"r"}}),
        json!({"type":"response.failed","response":{"id":"r","error":{"message":"quota"}}}),
    ]);
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(Body::from(body)) }
        }),
    ))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 502);
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x","stream":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("response.failed"));
    let log = gw.wait_for_log(2).await;
    assert!(log.iter().all(|r| r["status"] == 502), "{log:?}");
}

#[tokio::test]
async fn anthropic_native_messages_use_provider_auth_and_accept_sdk_headers() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/messages",
        post(|Json(v): Json<Value>| async move {
            if v["stream"] == true {
                sse_response(Body::from(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ))
            } else {
                json_response(200, json!({"id":"msg_1","type":"message","model":v["model"],"content":[{"type":"text","text":"bonjour"}],"usage":{"input_tokens":5,"output_tokens":2}}))
            }
        }),
    ))
    .await;
    gw.connection("Claude", "anthropic", &up.base(), &["claude-opus-5-5"])
        .await;
    let (_, key) = gw.create_key("k").await;

    // The Anthropic SDK authenticates with x-api-key.
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "tools-2024-04-04")
        .json(&json!({"model":"claude-opus-5-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["content"][0]["text"],
        "bonjour"
    );
    let req = &up.requests()[0];
    assert_eq!(req.path, "/messages");
    assert_eq!(req.header("x-api-key").as_deref(), Some(PROVIDER_KEY));
    assert!(
        req.header("authorization").is_none(),
        "API-key Anthropic connections use x-api-key only"
    );
    assert_eq!(
        req.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        req.header("anthropic-beta").as_deref(),
        Some("tools-2024-04-04")
    );

    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .json(&json!({"model":"claude-opus-5-5","max_tokens":10,"stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("text_delta"));

    // Anthropic-only models are not served on OpenAI endpoints.
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"claude-opus-5-5","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("/v1/messages"));
    assert_eq!(up.count(), 2);
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", "sy_wrong")
        .json(&json!({"model":"claude-opus-5-5"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

fn gemini_mock() -> Router {
    Router::new().route(
        "/models/{action}",
        post(|axum::extract::Path(action): axum::extract::Path<String>, Json(_v): Json<Value>| async move {
            let reply = json!({"candidates":[{"content":{"parts":[{"text":"hola"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2}});
            if action.ends_with(":streamGenerateContent") {
                sse_response(Body::from(format!("data: {reply}\r\n\r\n")))
            } else {
                json_response(200, reply)
            }
        }),
    )
}

#[tokio::test]
async fn gemini_native_generate_content_and_stream() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(gemini_mock()).await;
    gw.connection("Gemini", "gemini", &up.base(), &["gemini-3-pro"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let body = json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]});
    let r = gw
        .post(
            "/v1beta/models/gemini-3-pro:generateContent",
            &key,
            body.clone(),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["candidates"][0]["content"]["parts"][0]["text"],
        "hola"
    );
    let req = &up.requests()[0];
    assert_eq!(req.path, "/models/gemini-3-pro:generateContent");
    assert_eq!(req.query, None);
    assert_eq!(req.header("x-goog-api-key").as_deref(), Some(PROVIDER_KEY));
    assert!(req.header("authorization").is_none());
    assert_eq!(
        req.json(),
        body,
        "model/stream routing fields must not leak into the Gemini body"
    );

    let r = gw
        .post(
            "/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse",
            &key,
            body.clone(),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert!(r.text().await.unwrap().contains("hola"));
    let req = &up.requests()[1];
    assert_eq!(req.path, "/models/gemini-3-pro:streamGenerateContent");
    assert_eq!(req.query.as_deref(), Some("alt=sse"));

    let log = gw.wait_for_log(2).await;
    assert!(
        log.iter()
            .all(|r| r["input_tokens"] == 4 && r["output_tokens"] == 2),
        "{log:?}"
    );

    let r = gw
        .post(
            "/v1beta/models/gemini-3-pro:countTokens",
            &key,
            body.clone(),
        )
        .await;
    assert_eq!(r.status(), 400);
    let r = gw.post("/v1beta/models/gemini-3-pro", &key, body).await;
    assert_eq!(r.status(), 400);
}

/// Google's SDKs and REST docs authenticate with `x-goog-api-key` (or `?key=`), not Bearer.
#[tokio::test]
async fn gemini_native_clients_can_authenticate_with_goog_api_key() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(gemini_mock()).await;
    gw.connection("Gemini", "gemini", &up.base(), &["gemini-3-pro"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .http
        .post(gw.url("/v1beta/models/gemini-3-pro:generateContent"))
        .header("x-goog-api-key", &key)
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "Gemini SDK auth header rejected");
}

// ---------------------------------------------------------------------------------------------
// Routing strategies
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn round_robin_alternates_between_targets() {
    let (_d, gw) = setup().await;
    let a = Upstream::start(openai_mock("a")).await;
    let b = Upstream::start(openai_mock("b")).await;
    let ca = gw
        .connection("A", "openai", &a.base(), &["shared", "a-model"])
        .await;
    let cb = gw
        .connection("B", "openai", &b.base(), &["shared", "b-model"])
        .await;
    let (_, key) = gw.create_key("k").await;

    // Implicit pool: two connections advertise the same model.
    let mut seen = Vec::new();
    for _ in 0..6 {
        let v: Value = gw
            .post("/v1/responses", &key, json!({"model":"shared","input":"x"}))
            .await
            .json()
            .await
            .unwrap();
        seen.push(v["id"].as_str().unwrap().to_string());
    }
    assert_eq!((a.count(), b.count()), (3, 3), "{seen:?}");
    assert!(
        seen.windows(2).all(|w| w[0] != w[1]),
        "round robin should alternate: {seen:?}"
    );

    // Explicit route mapping one alias to different upstream model names.
    gw.put_route(
        "pool",
        "round_robin",
        json!([{"connection_id":ca,"model":"a-model"},{"connection_id":cb,"model":"b-model"}]),
    )
    .await;
    for _ in 0..4 {
        assert_eq!(
            gw.post("/v1/responses", &key, json!({"model":"pool","input":"x"}))
                .await
                .status(),
            200
        );
    }
    assert_eq!((a.count(), b.count()), (5, 5));
    let models: Vec<Value> = a.requests()[3..]
        .iter()
        .chain(b.requests()[3..].iter())
        .map(|r| r.json()["model"].clone())
        .collect();
    assert_eq!(models.iter().filter(|m| **m == "a-model").count(), 2);
    assert_eq!(models.iter().filter(|m| **m == "b-model").count(), 2);
}

#[tokio::test]
async fn failover_moves_to_next_target_only_on_retryable_errors() {
    let (_d, gw) = setup().await;
    let busy = Upstream::start(failing_mock(503, r#"{"error":"overloaded"}"#)).await;
    let limited = Upstream::start(failing_mock(429, r#"{"error":"rate"}"#)).await;
    let bad = Upstream::start(failing_mock(400, r#"{"error":"bad"}"#)).await;
    let good = Upstream::start(openai_mock("good")).await;
    let c_busy = gw.connection("Busy", "openai", &busy.base(), &["m"]).await;
    let c_limited = gw
        .connection("Limited", "openai", &limited.base(), &["m"])
        .await;
    let c_bad = gw.connection("Bad", "openai", &bad.base(), &["m"]).await;
    let c_good = gw.connection("Good", "openai", &good.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let t = |c: &str| json!({"connection_id":c,"model":"m"});

    gw.put_route(
        "ha",
        "failover",
        json!([t(&c_busy), t(&c_limited), t(&c_good)]),
    )
    .await;
    for _ in 0..3 {
        let r = gw
            .post("/v1/responses", &key, json!({"model":"ha","input":"x"}))
            .await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["id"], "resp_good");
    }
    assert_eq!(
        (busy.count(), limited.count(), good.count()),
        (1, 1, 3),
        "failover skips known cooling accounts instead of repeatedly exhausting them"
    );
    let log = gw.wait_for_log(3).await;
    assert_eq!(
        log[0]["connection_name"], "Good",
        "log attributes the serving connection"
    );

    // Non-retryable provider errors are returned, not masked by failover.
    gw.put_route("strict", "failover", json!([t(&c_bad), t(&c_good)]))
        .await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"strict","input":"x"}))
        .await;
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "gateway_error");
    assert_eq!(good.count(), 3);

    // When every target is exhausted the last provider status and Retry-After are surfaced.
    gw.put_route("down", "failover", json!([t(&c_busy), t(&c_limited)]))
        .await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"down","input":"x"}))
        .await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "7");
}

#[tokio::test]
async fn failover_skips_unreachable_providers() {
    let (_d, gw) = setup().await;
    let good = Upstream::start(openai_mock("good")).await;
    let dead = gw
        .connection("Dead", "openai", &dead_port().await, &["m"])
        .await;
    let ok = gw.connection("Good", "openai", &good.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    gw.put_route(
        "ha",
        "failover",
        json!([{"connection_id":dead,"model":"m"},{"connection_id":ok,"model":"m"}]),
    )
    .await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"ha","input":"x"}))
        .await;
    assert_eq!(
        r.status(),
        200,
        "a refused connection on the primary should fail over to the secondary"
    );
}

#[tokio::test]
async fn upstream_failures_map_to_gateway_errors() {
    let (_d, gw) = setup().await;
    let dead = dead_port().await;
    gw.connection("Dead", "openai", &dead, &["dead"]).await;
    let leaky = Upstream::start(failing_mock(
        401,
        r#"{"error":"invalid key sk-upstream-secret-DO-NOT-LEAK-7f3a"}"#,
    ))
    .await;
    gw.connection("Leaky", "openai", &leaky.base(), &["leaky"])
        .await;
    let junk = Upstream::start(
        Router::new().route("/responses", post(|| async { "<html>not json</html>" })),
    )
    .await;
    gw.connection("Junk", "openai", &junk.base(), &["junk"])
        .await;
    let slow = Upstream::start(Router::new().route(
        "/responses",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            "late"
        }),
    ))
    .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post("/v1/responses", &key, json!({"model":"dead","input":"x"}))
        .await;
    assert_eq!(r.status(), 502);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["type"],
        "gateway_error"
    );

    let r = gw
        .post("/v1/responses", &key, json!({"model":"leaky","input":"x"}))
        .await;
    assert_eq!(r.status(), 401);
    let text = r.text().await.unwrap();
    assert_no_secret("provider error passthrough", &text);
    assert!(text.contains("Provider rejected"), "{text}");

    let r = gw
        .post("/v1/responses", &key, json!({"model":"junk","input":"x"}))
        .await;
    assert_eq!(r.status(), 502);

    // Request timeout (1 s gateway) against a hung provider.
    let dir = tempfile::tempdir().unwrap();
    let quick = Gateway::start_with(dir.path(), 4, 1).await;
    quick
        .connection("Slow", "openai", &slow.base(), &["slow"])
        .await;
    let (_, qkey) = quick.create_key("k").await;
    let started = std::time::Instant::now();
    let r = quick
        .post("/v1/responses", &qkey, json!({"model":"slow","input":"x"}))
        .await;
    assert_eq!(r.status(), 502);
    assert!(started.elapsed() < Duration::from_secs(5));

    let log = gw.wait_for_log(3).await;
    // Newest first: invalid JSON, provider 401, refused connection. None is a client disconnect.
    let statuses: Vec<_> = log.iter().map(|r| r["status"].as_u64().unwrap()).collect();
    assert_eq!(statuses, vec![502, 401, 502], "{log:?}");
    assert!(log.iter().all(|r| r["error"].is_string()));
    let overview = gw.admin_json("/api/overview").await;
    assert_eq!(overview["requests_failed"], 3);
}

// ---------------------------------------------------------------------------------------------
// Responses WebSocket
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct WsLog {
    connections: AtomicUsize,
    frames: Mutex<Vec<Value>>,
}

/// Responses WebSocket provider: one persistent socket, answers each response.create.
fn ws_mock(log: Arc<WsLog>) -> Router {
    Router::new().route(
        "/responses",
        get(move |ws: WebSocketUpgrade| {
            let log = log.clone();
            async move {
                ws.on_upgrade(move |mut socket| async move {
                    log.connections.fetch_add(1, Ordering::SeqCst);
                    while let Some(Ok(msg)) = socket.recv().await {
                        let AxMessage::Text(t) = msg else { continue };
                        let v: Value = serde_json::from_str(&t).unwrap();
                        let n = {
                            let mut f = log.frames.lock().unwrap();
                            f.push(v.clone());
                            f.len()
                        };
                        if v["type"] != "response.create" {
                            continue;
                        }
                        let id = format!("resp_{n}");
                        let replies = if v["fail"] == true {
                            vec![json!({"type":"error","error":{"message":"upstream broke"}})]
                        } else {
                            vec![
                                json!({"type":"response.created","response":{"id":id}}),
                                json!({"type":"response.output_text.delta","delta":format!("turn {n} ✓")}),
                                completed(&id, &format!("turn {n} ✓")),
                            ]
                        };
                        for r in replies {
                            if socket.send(AxMessage::Text(r.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                })
            }
        }),
    )
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(gw: &Gateway, key: &str) -> Client {
    let mut req = gw.ws_url("/v1/responses").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .expect("gateway WS handshake")
        .0
}

async fn next_text(ws: &mut Client) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("WS frame within 5s")
        {
            Some(Ok(Message::Text(t))) => return t.to_string(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

async fn until_completed(ws: &mut Client) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let v: Value = serde_json::from_str(&next_text(ws).await).unwrap();
        let done = v["type"] == "response.completed" || v["type"] == "error";
        out.push(v);
        if done {
            return out;
        }
    }
}

#[tokio::test]
async fn websocket_session_is_persistent_and_bidirectional() {
    let (_d, gw) = setup().await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_mock(log.clone())).await;
    gw.add_connection(json!({"name":"Codex WS","kind":"codex","base_url":up.base(),"models":["gpt-codex"],"supports_websocket":true,"api_key":PROVIDER_KEY}))
        .await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;

    // Turn 1: flat response.create with a string input.
    ws.send(Message::Text(json!({"type":"response.create","model":"gpt-codex","input":"first","max_output_tokens":10,"stream":true}).to_string().into())).await.unwrap();
    let turn1 = until_completed(&mut ws).await;
    assert_eq!(
        turn1
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "response.created",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    let first_id = turn1[2]["response"]["id"].as_str().unwrap().to_string();

    // Turn 2: nested form, continuing via previous_response_id and returning a tool result.
    let tool_out =
        json!([{"type":"function_call_output","call_id":"call_9","output":"{\"temp\":21}"}]);
    ws.send(Message::Text(json!({"type":"response.create","response":{"model":"gpt-codex","previous_response_id":first_id,"input":tool_out,"tools":[{"type":"function","name":"t"}]}}).to_string().into())).await.unwrap();
    let turn2 = until_completed(&mut ws).await;
    assert_eq!(
        turn2.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "turn 2 ✓"
    );

    // Non-create client frames are relayed verbatim.
    ws.send(Message::Text(
        json!({"type":"response.cancel","response_id":"resp_2"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // Switching models mid-session is refused without closing the socket.
    ws.send(Message::Text(
        json!({"type":"response.create","model":"other-model","input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let err: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(err["type"], "error");

    // Turn 3 still works on the same session.
    ws.send(Message::Text(json!({"type":"response.create","model":"gpt-codex","input":"third","previous_response_id":"resp_2"}).to_string().into())).await.unwrap();
    let turn3 = until_completed(&mut ws).await;
    assert_eq!(turn3.last().unwrap()["type"], "response.completed");

    ws.close(None).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while ws.next().await.is_some() {}
    })
    .await;

    assert_eq!(
        log.connections.load(Ordering::SeqCst),
        1,
        "all turns must share one upstream socket"
    );
    let frames = log.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 4, "{frames:?}");
    let f1 = &frames[0];
    assert_eq!(f1["model"], "gpt-codex");
    assert_eq!(f1["store"], false);
    assert!(
        f1.get("stream").is_none() && f1.get("max_output_tokens").is_none(),
        "{f1}"
    );
    assert_eq!(f1["instructions"], "You are a helpful coding assistant.");
    assert_eq!(
        f1["input"],
        json!([{"role":"user","content":[{"type":"input_text","text":"first"}]}])
    );
    let f2 = &frames[1];
    assert!(
        f2.get("response").is_none(),
        "Responses WS sends create fields at the top level"
    );
    assert_eq!(f2["previous_response_id"], first_id.as_str());
    assert_eq!(f2["input"], tool_out);
    assert_eq!(f2["store"], false);
    assert_eq!(
        frames[2],
        json!({"type":"response.cancel","response_id":"resp_2"})
    );
    assert_eq!(frames[3]["previous_response_id"], "resp_2");

    let hs = &up.requests()[0];
    assert_eq!(
        hs.header("authorization").unwrap(),
        format!("Bearer {PROVIDER_KEY}")
    );
    assert_eq!(hs.header("originator").as_deref(), Some("codex_cli_rs"));
    assert!(
        hs.header("openai-beta")
            .unwrap()
            .starts_with("responses_websockets=")
    );
    assert!(
        !format!("{:?}", hs.headers).contains(&key),
        "client key forwarded upstream"
    );

    let records = gw.wait_for_log(1).await;
    assert_eq!(
        records.len(),
        1,
        "one WebSocket session is one request record"
    );
    assert_eq!(records[0]["transport"], "websocket");
    assert_eq!(records[0]["status"], 200);
    assert_eq!(records[0]["output_tokens"], 7);
}

#[tokio::test]
async fn websocket_errors_are_reported_as_frames() {
    let (_d, gw) = setup().await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_mock(log.clone())).await;
    let (_, key) = gw.create_key("k").await;
    gw.add_connection(json!({"name":"WS","kind":"openai","base_url":up.base(),"models":["ws-model"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    gw.add_connection(json!({"name":"HTTP only","kind":"openai","base_url":up.base(),"models":["http-model"],"api_key":PROVIDER_KEY})).await;
    gw.add_connection(json!({"name":"Gone","kind":"openai","base_url":dead_port().await,"models":["gone-model"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;

    // Handshake requires a client key.
    let err = tokio_tungstenite::connect_async(gw.ws_url("/v1/responses"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, tungstenite::Error::Http(r) if r.status() == 401),
        "{err:?}"
    );

    let expect_error = |frame: Value, needle: &'static str| {
        let gw = &gw;
        let key = key.clone();
        async move {
            let mut ws = ws_connect(gw, &key).await;
            ws.send(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
            let v: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(v["type"], "error", "{v}");
            assert!(
                v["error"]["message"].as_str().unwrap().contains(needle),
                "{v}"
            );
        }
    };
    expect_error(json!({"type":"session.update"}), "response.create").await;
    expect_error(json!({"type":"response.create","input":"x"}), "model").await;
    expect_error(
        json!({"type":"response.create","model":"http-model","input":"x"}),
        "WebSocket",
    )
    .await;
    expect_error(
        json!({"type":"response.create","model":"gone-model","input":"x"}),
        "handshake",
    )
    .await;
    assert_eq!(log.connections.load(Ordering::SeqCst), 0);

    // Provider error events are relayed and recorded as failures.
    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"ws-model","input":"x","fail":true})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(v["error"]["message"], "upstream broke");
    // The OpenAI (non-Codex) connection does not get Codex shaping.
    let f = log.frames.lock().unwrap()[0].clone();
    assert!(f.get("store").is_none() && f["input"] == "x", "{f}");
    drop(ws);
    let records = gw.wait_for_log(1).await;
    assert!(
        records
            .iter()
            .any(|r| r["transport"] == "websocket" && r["status"] == 502),
        "{records:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Concurrency, cancellation and logging
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_limit_is_enforced_and_permits_are_returned() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 2, 30).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let g = gate.clone();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let g = g.clone();
            async move {
                g.acquire().await.unwrap().forget();
                json_response(200, completed("r", "ok")["response"].clone())
            }
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let send = || {
        let http = gw.http.clone();
        let url = gw.url("/v1/responses");
        let key = key.clone();
        tokio::spawn(async move {
            http.post(url)
                .bearer_auth(key)
                .json(&json!({"model":"m","input":"x"}))
                .send()
                .await
                .unwrap()
                .status()
        })
    };
    let first = send();
    let second = send();
    for _ in 0..200 {
        if up.count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(up.count(), 2);
    assert_eq!(gw.admin_json("/api/overview").await["active_requests"], 2);

    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 429);
    assert!(r.text().await.unwrap().contains("concurrency"));
    assert_eq!(up.count(), 2, "rejected requests never reach the provider");

    gate.add_permits(2);
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(second.await.unwrap(), 200);
    gate.add_permits(10);
    for _ in 0..4 {
        assert_eq!(
            gw.post("/v1/responses", &key, json!({"model":"m","input":"x"}))
                .await
                .status(),
            200
        );
    }
    // Six served requests; the 429 is rejected before a request record exists.
    gw.wait_for_log(6).await;
    assert_eq!(gw.admin_json("/api/overview").await["active_requests"], 0);
}

#[tokio::test]
async fn client_disconnect_mid_stream_releases_permit_and_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 1, 60).await;
    let upstream_dropped = Arc::new(AtomicBool::new(false));
    let flag = upstream_dropped.clone();
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let up = Upstream::start(
        openai_mock("fast").route(
            "/hang/responses",
            post(move || {
                let flag = flag.clone();
                async move {
                    let stream = async_stream::stream! {
                        let _guard = OnDrop(flag);
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(&[json!({"type":"response.created","response":{"id":"r"}})])));
                        std::future::pending::<()>().await;
                    };
                    sse_response(Body::from_stream(stream))
                }
            }),
        ),
    )
    .await;
    gw.connection("Hang", "openai", &format!("{}/hang", up.base()), &["hang"])
        .await;
    gw.connection("Fast", "openai", &up.base(), &["fast"]).await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"hang","input":"x","stream":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let mut stream = r.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("response.created"));
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"fast","input":"x"}))
            .await
            .status(),
        429
    );
    drop(stream);

    let mut status = 0;
    for _ in 0..100 {
        status = gw
            .post("/v1/responses", &key, json!({"model":"fast","input":"x"}))
            .await
            .status()
            .as_u16();
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        status, 200,
        "permit was not released after the client disconnected"
    );
    assert!(
        upstream_dropped.load(Ordering::SeqCst),
        "upstream stream should be cancelled with the client"
    );
    let log = gw.wait_for_log(2).await;
    let hang = log.iter().find(|r| r["model"] == "hang").unwrap();
    assert_eq!(hang["status"], 499);
    assert_eq!(hang["transport"], "sse");
}

#[tokio::test]
async fn websocket_session_holds_one_permit_until_closed() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 1, 60).await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_mock(log.clone()).merge(openai_mock("fast"))).await;
    gw.add_connection(json!({"name":"WS","kind":"openai","base_url":up.base(),"models":["ws-model","fast"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;

    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"ws-model","input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    until_completed(&mut ws).await;
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"fast","input":"x"}))
            .await
            .status(),
        429
    );
    let err = tokio_tungstenite::connect_async({
        let mut r = gw.ws_url("/v1/responses").into_client_request().unwrap();
        r.headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
        r
    })
    .await
    .unwrap_err();
    assert!(
        matches!(&err, tungstenite::Error::Http(r) if r.status() == 429),
        "{err:?}"
    );

    // Abrupt drop (no close frame) must also release the session.
    drop(ws);
    let mut status = 0;
    for _ in 0..100 {
        status = gw
            .post("/v1/responses", &key, json!({"model":"fast","input":"x"}))
            .await
            .status()
            .as_u16();
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, 200);
}

#[tokio::test]
async fn request_log_and_storage_never_contain_prompts_or_outputs() {
    let (dir, gw) = setup().await;
    let prompt = "PROMPT-CANARY-a81f the launch codes are 0000";
    let output = "OUTPUT-CANARY-77c2";
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move |Json(v): Json<Value>| async move {
            if v["stream"] == true {
                sse_response(Body::from(sse(&[
                    json!({"type":"response.output_text.delta","delta":output}),
                    completed("r", output),
                ])))
            } else {
                json_response(200, completed("r", output)["response"].clone())
            }
        }),
    ))
    .await;
    let log = Arc::new(WsLog::default());
    let ws_up = Upstream::start(ws_mock(log)).await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    gw.add_connection(json!({"name":"WS","kind":"openai","base_url":ws_up.base(),"models":["w"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":prompt}))
        .await;
    assert!(r.text().await.unwrap().contains(output));
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"m","input":prompt,"stream":true}),
        )
        .await;
    assert!(r.text().await.unwrap().contains(output));
    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"w","input":prompt})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    until_completed(&mut ws).await;
    ws.close(None).await.unwrap();
    drop(ws);

    let records = gw.wait_for_log(3).await;
    for r in &records {
        assert!(
            r["latency_ms"].is_u64() && r["timestamp"].is_string() && r["id"].is_string(),
            "{r}"
        );
    }
    for path in ["/api/requests", "/api/overview"] {
        let text = gw.admin_get(path).await.text().await.unwrap();
        assert!(
            !text.contains("PROMPT-CANARY") && !text.contains("OUTPUT-CANARY"),
            "{path} logged content: {text}"
        );
    }
    let leaks = [
        dir_contains(dir.path(), b"PROMPT-CANARY"),
        dir_contains(dir.path(), b"OUTPUT-CANARY"),
    ]
    .concat();
    assert!(leaks.is_empty(), "prompt/output written to disk: {leaks:?}");

    // Filtering.
    let only_ws = gw.admin_json("/api/requests?model=w").await;
    assert_eq!(only_ws.as_array().unwrap().len(), 1);
    assert_eq!(gw.admin_json("/api/requests?status=error").await, json!([]));
    assert_eq!(
        gw.admin_json("/api/requests?limit=1")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn playground_is_admin_only_and_proxies() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(openai_mock("pg")).await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/api/playground", &key, json!({"model":"m","input":"hi"}))
        .await;
    assert_eq!(r.status(), 401);
    let r = gw
        .admin_send(
            Method::POST,
            "/api/playground",
            json!({"model":"m","input":"hi"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], "resp_pg");
    let r = gw
        .admin_send(
            Method::POST,
            "/api/playground",
            json!({"model":"m","input":"hi","transport":"sse"}),
        )
        .await;
    assert!(r.text().await.unwrap().contains("hello from pg"));
}

#[tokio::test]
async fn idle_websocket_sessions_are_closed_after_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 1, 1).await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_mock(log)).await;
    gw.add_connection(json!({"name":"WS","kind":"openai","base_url":up.base(),"models":["w"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"w","input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    until_completed(&mut ws).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "idle session should be closed by the gateway"
    );
    let records = gw.wait_for_log(1).await;
    assert_eq!(records[0]["status"], 504, "{records:?}");
}

/// The request timeout should bound a stalled provider, not a long stream that keeps producing.
#[tokio::test]
async fn steadily_progressing_streams_are_not_cut_by_request_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 4, 1).await;
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(|| async {
            let stream = async_stream::stream! {
                for i in 0..8 {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(&[json!({"type":"response.output_text.delta","delta":i.to_string()})])));
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                yield Ok(Bytes::from(sse(&[completed("r", "01234567")])));
            };
            sse_response(Body::from_stream(stream))
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"m","input":"x","stream":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let body = r.text().await;
    assert!(
        body.as_ref()
            .is_ok_and(|b| b.contains("response.completed")),
        "stream truncated after ~1s: {body:?}"
    );
}
