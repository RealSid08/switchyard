//! Contract of the real Claude Code CLI (2.1.287) talking to Switchyard, as captured live:
//! `POST /v1/messages?beta=true`, a gateway client key as `Authorization: Bearer` (or `x-api-key`
//! in `--bare` mode), eleven `anthropic-beta` values, Stainless SDK headers, adaptive thinking,
//! a three-block system prompt without the Claude Code identity, and a streaming response.
//! Also covers the native `POST /v1/messages/count_tokens` handler. Loopback mocks and fake
//! credentials only.

mod support;

use axum::{
    Router,
    body::{Body, Bytes},
    http::HeaderMap,
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use support::*;
use switchyard::proxy::{self, CLAUDE_CODE_IDENTITY};

const CLI_BETAS: &str = "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,effort-2025-11-24,dangerous-tool-use-2026-09-03,afk-mode-2026-01-31";

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

/// A Claude subscription account owned by the gateway (no refresh needed during the test).
fn oauth_claude(gw: &Gateway, base: &str) {
    let c = json!({"id":"claude-sub","name":"Claude","kind":"anthropic","base_url":base,"enabled":true,
        "models":["claude-opus-5-5"],"supports_websocket":false,"created_at":"2026-10-02T00:00:00Z",
        "api_key":PROVIDER_KEY,"refresh_token":"rt","expires_at":chrono::Utc::now().timestamp()+86400,
        "account_id":"","oauth":true,"credential_source":"oauth"});
    gw.app.store.put("connection", "claude-sub", &c).unwrap();
}

/// The request body Claude Code sends for `claude -p "Reply exactly OK" --tools ""`.
fn cli_body() -> Value {
    json!({
        "model": "claude-opus-5-5",
        "max_tokens": 128000,
        "stream": true,
        "messages": [{"role":"user","content":[{"type":"text","text":"Reply exactly OK"}]}],
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.287; cc_entrypoint=sdk-cli;"},
            {"type":"text","text":"You are an agent for software engineering tasks.","cache_control":{"type":"ephemeral"}},
            {"type":"text","text":"Environment: /tmp/cwd"}
        ],
        "tools": [],
        "thinking": {"type":"adaptive","display":"omitted"},
        "context_management": {"edits":[]},
        "output_config": {"effort":"high"},
        "metadata": {"user_id":"session-user"},
        "safeguards": {}
    })
}

fn cli_request(gw: &Gateway, path: &str, auth: (&str, &str)) -> reqwest::RequestBuilder {
    gw.http
        .post(gw.url(path))
        .header(auth.0, auth.1)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", CLI_BETAS)
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("x-app", "cli")
        .header("x-claude-code-session-id", "session-123")
        .header("x-stainless-lang", "js")
        .header("x-stainless-retry-count", "0")
        .header("user-agent", "claude-cli/2.1.287 (external, sdk-cli)")
        .header("accept", "application/json")
}

fn anthropic_stream() -> &'static str {
    concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cli\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
}

fn header_values(c: &Captured, name: &str) -> Vec<String> {
    c.headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn claude_code_cli_request_reaches_a_subscription_account_intact() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/messages",
        post(|| async { sse_response(Body::from(anthropic_stream())) }),
    ))
    .await;
    oauth_claude(&gw, &up.base());
    let (_, key) = gw.create_key("cli").await;

    // Normal mode: ANTHROPIC_AUTH_TOKEN becomes `Authorization: Bearer`.
    let r = cli_request(
        &gw,
        "/v1/messages?beta=true",
        ("authorization", &format!("Bearer {key}")),
    )
    .json(&cli_body())
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = r.text().await.unwrap();
    assert!(text.contains("\"text\":\"OK\"") && text.contains("message_stop"));
    assert!(!text.contains("upstream_interrupted"));

    let c = &up.requests()[0];
    assert_eq!(
        header_values(c, "authorization"),
        vec![format!("Bearer {PROVIDER_KEY}")]
    );
    assert!(c.header("x-api-key").is_none());
    assert!(
        !format!("{:?}", c.headers).contains(&key),
        "gateway client key forwarded upstream"
    );
    assert_eq!(header_values(c, "anthropic-version"), vec!["2023-06-01"]);
    let betas: Vec<String> = header_values(c, "anthropic-beta")
        .iter()
        .flat_map(|v| {
            v.split(',')
                .map(|b| b.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    for beta in CLI_BETAS.split(',').chain(["oauth-2025-04-20"]) {
        assert_eq!(
            betas.iter().filter(|b| *b == beta).count(),
            1,
            "{beta} in {betas:?}"
        );
    }
    for private in ["x-claude-code-session-id", "x-stainless-lang", "x-app"] {
        assert!(c.header(private).is_none(), "{private} forwarded");
    }

    let sent = c.json();
    let expected = cli_body();
    let system = sent["system"].as_array().unwrap();
    assert_eq!(
        system[0],
        json!({"type":"text","text":CLAUDE_CODE_IDENTITY})
    );
    assert_eq!(
        &system[1..],
        expected["system"].as_array().unwrap().as_slice()
    );
    for field in [
        "model",
        "max_tokens",
        "stream",
        "messages",
        "tools",
        "thinking",
        "context_management",
        "output_config",
        "metadata",
        "safeguards",
    ] {
        assert_eq!(sent[field], expected[field], "{field} altered");
    }

    // --bare mode: ANTHROPIC_API_KEY becomes `x-api-key`.
    let r = cli_request(&gw, "/v1/messages?beta=true", ("x-api-key", &key))
        .json(&cli_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("message_stop"));

    let log = gw.wait_for_log(2).await;
    assert!(
        log.iter()
            .all(|r| r["status"] == 200 && r["transport"] == "sse" && r["output_tokens"] == 4),
        "{log:?}"
    );
}

/// Serves `proxy::count_tokens` with the gateway's state. Client-key authentication is applied
/// by the application router; this exercises the handler itself.
async fn count_tokens_server(gw: &Gateway) -> String {
    let router = Router::new()
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .with_state(gw.app.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, router).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn count_tokens_is_shaped_like_the_message_request_and_not_logged() {
    let (_d, gw) = setup().await;
    let reply = Arc::new(Mutex::new((200u16, json!({"input_tokens": 42}))));
    let r2 = reply.clone();
    let up = Upstream::start(
        Router::new()
            .route(
                "/messages/count_tokens",
                post(move |_h: HeaderMap, _b: Bytes| {
                    let r = r2.clone();
                    async move {
                        let (s, v) = r.lock().unwrap().clone();
                        json_response(s, v)
                    }
                }),
            )
            .route(
                "/messages",
                post(|| async { sse_response(Body::from(anthropic_stream())) }),
            ),
    )
    .await;
    oauth_claude(&gw, &up.base());
    let base = count_tokens_server(&gw).await;
    let mut body = cli_body();
    for k in [
        "max_tokens",
        "stream",
        "metadata",
        "context_management",
        "safeguards",
    ] {
        body.as_object_mut().unwrap().remove(k);
    }

    let r = gw
        .http
        .post(format!("{base}/v1/messages/count_tokens?beta=true"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", CLI_BETAS)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"input_tokens":42}));
    let c = &up.requests()[0];
    assert_eq!(c.path, "/messages/count_tokens");
    assert_eq!(
        header_values(c, "authorization"),
        vec![format!("Bearer {PROVIDER_KEY}")]
    );
    assert_eq!(header_values(c, "anthropic-version"), vec!["2023-06-01"]);
    assert!(
        c.header("anthropic-beta")
            .unwrap()
            .contains("oauth-2025-04-20")
    );
    let sent = c.json();
    assert_eq!(sent["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(sent["system"].as_array().unwrap().len(), 4);
    assert_eq!(sent["thinking"], body["thinking"]);

    // Provider errors keep the native envelope, are redacted, and do not cool the account.
    *reply.lock().unwrap() = (
        429,
        json!({"type":"error","error":{"type":"rate_limit_error","message":format!("slow down {PROVIDER_KEY}")}}),
    );
    let r = gw
        .http
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    let text = r.text().await.unwrap();
    assert!(!text.contains(PROVIDER_KEY), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (v["type"].as_str(), v["error"]["type"].as_str()),
        (Some("error"), Some("rate_limit_error"))
    );

    let (_, key) = gw.create_key("k").await;
    let r = cli_request(&gw, "/v1/messages", ("x-api-key", &key))
        .json(&cli_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "a count_tokens 429 must not cool the inference account"
    );
    r.text().await.unwrap();
    let log = gw.wait_for_log(1).await;
    assert_eq!(
        log.len(),
        1,
        "token counts are not request-log rows: {log:?}"
    );

    // Non-Anthropic models are refused clearly.
    gw.connection("OpenAI", "openai", &up.base(), &["gpt-x"])
        .await;
    let r = gw
        .http
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&json!({"model":"gpt-x","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}
