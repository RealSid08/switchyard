//! Read-only OpenCode imports (Zen and Go API keys from `auth.json`) through the admin API, and
//! that the resulting connections route through the existing OpenAI and Anthropic kinds.
//! Fake keys and temporary files only; nothing contacts opencode.ai.

mod support;

use axum::{Router, routing::post};
use reqwest::Method;
use serde_json::{Value, json};
use support::*;

const ZEN_KEY: &str = "sk-zen-fake-key-0001";
const GO_KEY: &str = "sk-go-fake-key-0002";

fn auth_file(dir: &std::path::Path, v: Value) -> std::path::PathBuf {
    let p = dir.join("auth.json");
    std::fs::write(&p, v.to_string()).unwrap();
    p
}

async fn import(gw: &Gateway, path: &std::path::Path) -> reqwest::Response {
    gw.admin_send(
        Method::POST,
        "/api/import",
        json!({"source":"opencode","path":path}),
    )
    .await
}

#[tokio::test]
async fn opencode_zen_and_go_keys_become_one_connection_per_api_family() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let src = tempfile::tempdir().unwrap();
    let file = auth_file(
        src.path(),
        json!({
            "opencode": {"type":"api","key":ZEN_KEY},
            "opencode-go": {"type":"api","key":GO_KEY},
            "openai": {"type":"api","key":"sk-openai-not-imported"},
            "anthropic": {"type":"oauth","refresh":"r","access":"a","expires":1}
        }),
    );
    let before = std::fs::read(&file).unwrap();
    let r = import(&gw, &file).await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    for secret in [ZEN_KEY, GO_KEY, "sk-openai-not-imported"] {
        assert!(!text.contains(secret), "import response leaked a key");
    }
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["imported"], 4, "{v}");
    let mut got: Vec<(String, String, String)> = v["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().into(),
                c["kind"].as_str().unwrap().into(),
                c["base_url"].as_str().unwrap().into(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            (
                "OpenCode Go".into(),
                "openai".into(),
                "https://opencode.ai/zen/go/v1".into()
            ),
            (
                "OpenCode Go (Messages)".into(),
                "anthropic".into(),
                "https://opencode.ai/zen/go/v1".into()
            ),
            (
                "OpenCode Zen".into(),
                "openai".into(),
                "https://opencode.ai/zen/v1".into()
            ),
            (
                "OpenCode Zen (Messages)".into(),
                "anthropic".into(),
                "https://opencode.ai/zen/v1".into()
            ),
        ]
    );
    assert!(
        v["connections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["credential_source"] == "native_opencode")
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "auth.json is never written"
    );

    // Reimport is idempotent; a user's edits survive.
    let zen = v["connections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "OpenCode Zen")
        .unwrap()
        .clone();
    let id = zen["id"].as_str().unwrap();
    let r = gw
        .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":"Zen (mine)","kind":"openai","base_url":zen["base_url"],"models":["gpt-6.1-sol"],"enabled":false}))
        .await;
    assert_eq!(r.status(), 200);
    let again: Value = import(&gw, &file).await.json().await.unwrap();
    assert_eq!(again["imported"], 4);
    let all = gw.admin_json("/api/connections").await;
    assert_eq!(all.as_array().unwrap().len(), 4);
    let mine = all
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap();
    assert_eq!(
        (mine["name"].as_str(), mine["enabled"].as_bool()),
        (Some("Zen (mine)"), Some(false))
    );

    // A rotated key is a different credential: it imports as new connections, never silently
    // replacing the old key's accounts.
    auth_file(
        src.path(),
        json!({"opencode-go": {"type":"api","key":"sk-go-rotated"}}),
    );
    let v: Value = import(&gw, &file).await.json().await.unwrap();
    assert_eq!(v["imported"], 2);
    assert_eq!(
        gw.admin_json("/api/connections")
            .await
            .as_array()
            .unwrap()
            .len(),
        6
    );
}

#[tokio::test]
async fn opencode_import_errors_are_clear() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let src = tempfile::tempdir().unwrap();
    let cases = [
        (
            json!({"openai":{"type":"api","key":"x"}}),
            "No OpenCode Zen or Go API key",
        ),
        (
            json!({"opencode":{"type":"oauth","access":"x"}}),
            "No OpenCode Zen or Go API key",
        ),
        (
            json!({"opencode":{"type":"api","key":""}}),
            "No OpenCode Zen or Go API key",
        ),
    ];
    for (body, needle) in cases {
        let file = auth_file(src.path(), body);
        let r = import(&gw, &file).await;
        assert_eq!(r.status(), 400);
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"]["message"].as_str().unwrap().contains(needle),
            "{v}"
        );
    }
    let r = import(&gw, &src.path().join("missing.json")).await;
    assert_eq!(r.status(), 400);
    let r = gw
        .admin_send(Method::POST, "/api/import", json!({"source":"cursor"}))
        .await;
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("opencode"));
    assert_eq!(gw.admin_json("/api/connections").await, json!([]));
}

/// The imported connections use the existing kinds: Responses and Chat with a Bearer key on the
/// OpenAI-compatible connection, Messages with `x-api-key` on the Anthropic one.
#[tokio::test]
async fn imported_opencode_connections_route_through_existing_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let up = Upstream::start(
        Router::new()
            .route("/responses", post(|| async { json_response(200, json!({"id":"resp_1","object":"response","status":"completed","output":[]})) }))
            .route("/chat/completions", post(|| async { json_response(200, json!({"id":"c1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})) }))
            .route("/messages", post(|| async { json_response(200, json!({"id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})) })),
    )
    .await;
    let src = tempfile::tempdir().unwrap();
    let file = auth_file(
        src.path(),
        json!({"opencode-go": {"type":"api","key":GO_KEY}}),
    );
    let v: Value = import(&gw, &file).await.json().await.unwrap();
    for c in v["connections"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let r = gw
            .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":c["name"],"kind":c["kind"],"base_url":up.base(),"models":c["models"]}))
            .await;
        assert_eq!(r.status(), 200);
    }
    let (_, key) = gw.create_key("opencode").await;
    assert_eq!(
        gw.post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-6-luna","input":"hi"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        gw.post(
            "/v1/chat/completions",
            &key,
            json!({"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]})
        )
        .await
        .status(),
        200
    );
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .json(&json!({"model":"minimax-m3","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let reqs = up.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        reqs[0].header("authorization").unwrap(),
        format!("Bearer {GO_KEY}")
    );
    assert_eq!(
        reqs[1].header("authorization").unwrap(),
        format!("Bearer {GO_KEY}")
    );
    assert_eq!(reqs[2].header("x-api-key").as_deref(), Some(GO_KEY));
    assert!(
        reqs.iter()
            .all(|r| !format!("{:?}", r.headers).contains(&key)),
        "client key never forwarded"
    );
}
