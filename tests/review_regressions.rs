//! Protocol regressions found in review and live use. Real gateway, loopback mock providers,
//! tempdir state, fake credentials only.

mod support;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    response::Response,
    routing::post,
};
use reqwest::Method;
use serde_json::{Value, json};
use support::*;

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

/// A plain response with exactly the given headers (no implicit content-type).
fn raw(status: u16, headers: &[(&str, &str)], body: impl Into<Body>) -> Response {
    let mut r = Response::builder().status(status);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.body(body.into()).unwrap()
}

/// Client-side SSE parsing per the WHATWG rules: blank line dispatches, data lines join with \n.
fn sse_data(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|block| {
            let data: Vec<&str> = block
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|d| d.strip_prefix(' ').unwrap_or(d))
                })
                .collect();
            (!data.is_empty()).then(|| data.join("\n"))
        })
        .collect()
}

fn chat_chunks(text: &str) -> (Vec<Value>, bool) {
    let data = sse_data(text);
    let done = data.last().is_some_and(|d| d == "[DONE]");
    (
        data.iter()
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap_or_else(|_| panic!("bad chunk {d}")))
            .collect(),
        done,
    )
}

/// Insert a connection record directly (used for OAuth-flavoured accounts without any import or
/// network refresh). Unknown/new fields fall back to serde defaults.
fn put_connection(gw: &Gateway, v: Value) -> String {
    let id = v["id"].as_str().unwrap().to_string();
    gw.app.store.put("connection", &id, &v).unwrap();
    id
}

fn oauth_anthropic(base: &str) -> Value {
    json!({"id":"oauth-claude","name":"Claude OAuth","kind":"anthropic","base_url":base,"enabled":true,
        "models":["claude-opus-5-5"],"supports_websocket":false,"created_at":"2026-10-01T00:00:00Z",
        "api_key":PROVIDER_KEY,"refresh_token":"","expires_at":chrono::Utc::now().timestamp()+86400,
        "account_id":"","oauth":true})
}

fn codex_events(id: &str, events: &[Value]) -> String {
    let mut all =
        vec![json!({"type":"response.created","response":{"id":id,"status":"in_progress"}})];
    all.extend_from_slice(events);
    sse(&all)
}

// ---------------------------------------------------------------------------------------------
// Codex SSE without Content-Type (observed live from the ChatGPT backend)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn codex_sse_without_content_type_is_still_parsed() {
    let (_d, gw) = setup().await;
    let body = codex_events(
        "resp_nct",
        &[
            json!({"type":"response.output_text.delta","delta":"hello"}),
            completed("resp_nct", "hello"),
        ],
    );
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { raw(200, &[], body) }
        }),
    ))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    // Non-streaming Responses: collected from SSE.
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], "resp_nct");
    // Streaming Responses: forwarded as SSE.
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x","stream":true}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert!(r.text().await.unwrap().contains("response.completed"));
    // Chat translation, both modes.
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    assert_eq!(
        r.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "hello"
    );
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let (chunks, done) = chat_chunks(&r.text().await.unwrap());
    assert!(done);
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "hello");
    let log = gw.wait_for_log(4).await;
    assert!(log.iter().all(|r| r["status"] == 200), "{log:?}");
}

// ---------------------------------------------------------------------------------------------
// Anthropic SDK headers
// ---------------------------------------------------------------------------------------------

fn anthropic_echo() -> Router {
    Router::new().route(
        "/messages",
        post(|| async { json_response(200, json!({"id":"msg_1","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}})) }),
    )
}

fn values(c: &Captured, name: &str) -> Vec<String> {
    c.headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

fn betas(c: &Captured) -> Vec<String> {
    values(c, "anthropic-beta")
        .iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
async fn anthropic_sdk_headers_are_not_duplicated_and_oauth_beta_is_merged() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(anthropic_echo()).await;
    gw.connection("Claude key", "anthropic", &up.base(), &["claude-key"])
        .await;
    let up2 = Upstream::start(anthropic_echo()).await;
    put_connection(&gw, oauth_anthropic(&up2.base()));
    let (_, key) = gw.create_key("k").await;
    let send = |model: &'static str, betas: &'static [&'static str]| {
        let mut r = gw
            .http
            .post(gw.url("/v1/messages"))
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01");
        for b in betas {
            r = r.header("anthropic-beta", *b);
        }
        r.json(&json!({"model":model,"max_tokens":5,"messages":[{"role":"user","content":"x"}]}))
            .send()
    };

    // API-key account: exactly one version header, SDK beta passed through.
    let r = send("claude-key", &["prompt-caching-2024-07-31"])
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let c = &up.requests()[0];
    assert_eq!(values(c, "anthropic-version"), vec!["2023-06-01"]);
    assert_eq!(betas(c), vec!["prompt-caching-2024-07-31"]);
    assert_eq!(values(c, "x-api-key"), vec![PROVIDER_KEY]);

    // OAuth account: one version header; OAuth beta merged with the SDK's betas, not replaced.
    let r = send(
        "claude-opus-5-5",
        &["prompt-caching-2024-07-31,interleaved-thinking-2025-05-14"],
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let c = &up2.requests()[0];
    assert_eq!(
        values(c, "anthropic-version"),
        vec!["2023-06-01"],
        "duplicate anthropic-version"
    );
    let mut got = betas(c);
    got.sort();
    assert_eq!(
        got,
        vec![
            "interleaved-thinking-2025-05-14",
            "oauth-2025-04-20",
            "prompt-caching-2024-07-31"
        ]
    );
    assert!(c.header("x-api-key").is_none());
    assert_eq!(
        values(c, "authorization"),
        vec![format!("Bearer {PROVIDER_KEY}")]
    );

    // OAuth account without SDK betas still sends the OAuth beta exactly once.
    send("claude-opus-5-5", &[]).await.unwrap();
    assert_eq!(betas(&up2.requests()[1]), vec!["oauth-2025-04-20"]);
    // Client already includes the OAuth beta: not duplicated.
    send("claude-opus-5-5", &["oauth-2025-04-20"])
        .await
        .unwrap();
    assert_eq!(betas(&up2.requests()[2]), vec!["oauth-2025-04-20"]);
}

/// HTTP allows a list header to be split over repeated lines; every beta must survive.
#[tokio::test]
async fn repeated_anthropic_beta_header_lines_are_all_forwarded() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(anthropic_echo()).await;
    put_connection(&gw, oauth_anthropic(&up.base()));
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .json(&json!({"model":"claude-opus-5-5","max_tokens":5,"messages":[{"role":"user","content":"x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut got = betas(&up.requests()[0]);
    got.sort();
    assert_eq!(
        got,
        vec![
            "interleaved-thinking-2025-05-14",
            "oauth-2025-04-20",
            "prompt-caching-2024-07-31"
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Native provider errors are preserved (codes) and redacted (credentials)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn native_error_codes_are_preserved_and_credentials_redacted() {
    let (_d, gw) = setup().await;
    let leak = format!("Incorrect API key provided: {PROVIDER_KEY}. account acct-777");
    let up = Upstream::start(
        Router::new()
            .route(
                "/ctx/responses",
                post({
                    let leak = leak.clone();
                    move || async move {
                        json_response(400, json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","param":"input","message":format!("Your input exceeds the context window. {leak}")}}))
                    }
                }),
            )
            .route(
                "/usage/responses",
                post(|| async {
                    raw(
                        429,
                        &[("content-type", "application/json")],
                        json!({"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_in_seconds":1234}}).to_string(),
                    )
                }),
            )
            .route(
                "/anth/messages",
                post({
                    let leak = leak.clone();
                    move || async move {
                        raw(
                            429,
                            &[("content-type", "application/json"), ("retry-after", "17")],
                            json!({"type":"error","error":{"type":"rate_limit_error","message":format!("Number of request tokens has exceeded your rate limit. {leak}")}}).to_string(),
                        )
                    }
                }),
            ),
    )
    .await;
    gw.connection("Ctx", "openai", &format!("{}/ctx", up.base()), &["ctx"])
        .await;
    put_connection(
        &gw,
        json!({"id":"codex-acct","name":"Codex","kind":"codex","base_url":format!("{}/usage",up.base()),"enabled":true,"models":["gpt-codex"],
            "supports_websocket":false,"created_at":"2026-10-01T00:00:00Z","api_key":PROVIDER_KEY,"refresh_token":"","expires_at":0,"account_id":"acct-777","oauth":true}),
    );
    gw.connection(
        "Anth",
        "anthropic",
        &format!("{}/anth", up.base()),
        &["claude-x"],
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post("/v1/responses", &key, json!({"model":"ctx","input":"x"}))
        .await;
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(
        !text.contains(PROVIDER_KEY),
        "provider key echoed to client: {text}"
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["param"], "input");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("context window")
    );

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 429);
    let retry: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1200..=1235).contains(&retry),
        "Retry-After derived from resets_in_seconds: {retry}"
    );
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "usage_limit_reached");
    assert_eq!(v["error"]["plan_type"], "plus");
    assert_eq!(v["error"]["resets_in_seconds"], 1234);

    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .json(
            &json!({"model":"claude-x","max_tokens":5,"messages":[{"role":"user","content":"x"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "17");
    let text = r.text().await.unwrap();
    assert!(!text.contains(PROVIDER_KEY), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["type"], "error",
        "Anthropic SDKs expect the native error envelope"
    );
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("[redacted]")
    );

    let log = gw.wait_for_log(3).await;
    assert!(!serde_json::to_string(&log).unwrap().contains(PROVIDER_KEY));
}

// ---------------------------------------------------------------------------------------------
// response.incomplete
// ---------------------------------------------------------------------------------------------

fn incomplete(id: &str, text: &str) -> Value {
    json!({"type":"response.incomplete","response":{"id":id,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],
        "usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}})
}

#[tokio::test]
async fn response_incomplete_ends_cleanly_and_maps_to_length() {
    let (_d, gw) = setup().await;
    let body = codex_events(
        "resp_inc",
        &[
            json!({"type":"response.output_text.delta","delta":"trunc"}),
            incomplete("resp_inc", "trunc"),
        ],
    );
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
            json!({"model":"gpt-codex","input":"x","stream":true}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert!(text.contains("response.incomplete"));
    assert!(
        !text.contains("upstream_interrupted"),
        "incomplete is a clean end, not an interruption: {text}"
    );

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["status"], "incomplete");

    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert!(!text.contains("upstream_interrupted"), "{text}");
    let (chunks, done) = chat_chunks(&text);
    assert!(done, "[DONE] terminator: {text}");
    assert_eq!(
        chunks
            .iter()
            .filter(|c| c["choices"][0]["finish_reason"].is_string())
            .count(),
        1
    );
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "length"
    );

    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "length");
    assert_eq!(v["choices"][0]["message"]["content"], "trunc");

    let log = gw.wait_for_log(4).await;
    assert!(log.iter().all(|r| r["status"] == 200), "{log:?}");
}

// ---------------------------------------------------------------------------------------------
// Large and multi-line SSE events
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn native_sse_event_between_1_and_16_mib_is_forwarded_byte_exact() {
    let (_d, gw) = setup().await;
    // e.g. a large tool-call argument or base64 image in a single event.
    let big = "é".repeat(1_600_000); // 3.2 MB of 2-byte chars
    let body = codex_events(
        "resp_big",
        &[
            json!({"type":"response.output_text.delta","delta":big}),
            completed("resp_big", "done"),
        ],
    )
    .into_bytes();
    let expected = body.clone();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            // 64 KiB + 1 chunks split codepoints throughout.
            async move { sse_response(chunked_body(split_every(&body, 65_537))) }
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"m","input":"x","stream":true}),
        )
        .await;
    let got = r.bytes().await.unwrap();
    assert_eq!(got.len(), expected.len());
    assert!(got == expected, "passthrough altered bytes");

    // Codex collection parses the large event too.
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    // And chat translation delivers it intact.
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let (chunks, _) = chat_chunks(&r.text().await.unwrap());
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text.len(), big.len());
    let log = gw.wait_for_log(3).await;
    assert!(log.iter().all(|r| r["status"] == 200), "{log:?}");
}

#[tokio::test]
async fn sse_event_over_16_mib_fails_closed() {
    let (_d, gw) = setup().await;
    let big = "x".repeat(17 * 1024 * 1024);
    let body = format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\n");
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(chunked_body(split_every(body.as_bytes(), 1 << 20))) }
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
    let _ = r.bytes().await; // body may legitimately error out
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 502, "{}", log[0]);
}

#[tokio::test]
async fn multi_line_data_events_with_crlf_and_split_utf8_are_parsed() {
    let (_d, gw) = setup().await;
    // One JSON object spread over several `data:` lines (legal SSE), CRLF endings, a comment
    // line, and an `id:` field.
    let body = concat!(
        ": keep-alive\r\n\r\n",
        "event: response.created\r\ndata: {\"type\":\"response.created\",\r\ndata: \"response\":{\"id\":\"resp_ml\"}}\r\n\r\n",
        "id: 7\r\nevent: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata:\"delta\":\"Grüße 🦀\"}\r\n\r\n",
        "event: response.completed\r\ndata: {\"type\":\"response.completed\",\r\ndata: \"response\":{\"id\":\"resp_ml\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Grüße 🦀\"}]}],\r\ndata: \"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5}}}\r\n\r\n",
    )
    .as_bytes()
    .to_vec();
    // Cut inside every multi-byte character.
    let mut cuts: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, b)| **b >= 0xC0)
        .map(|(i, _)| i + 1)
        .collect();
    cuts.push(body.len());
    let mut chunks = Vec::new();
    let mut last = 0;
    for c in cuts {
        chunks.push(body[last..c].to_vec());
        last = c;
    }
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let chunks = chunks.clone();
            async move { sse_response(chunked_body(chunks)) }
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
            json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let (chunks, done) = chat_chunks(&r.text().await.unwrap());
    assert!(done);
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Grüße 🦀");
    assert!(chunks.iter().all(|c| c["id"] == "resp_ml"));
    assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 3);

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["output"][0]["content"][0]["text"],
        "Grüße 🦀"
    );
}

// ---------------------------------------------------------------------------------------------
// Admin token file integrity
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn empty_or_corrupt_admin_token_refuses_to_start() {
    let good = format!("sy_admin_{}", "a1".repeat(32));
    let cases = [
        String::new(),
        "   \n".into(),
        "sy_admin_".into(),
        "sy_admin_short".into(),
        format!("sy_admin_{}", "zz".repeat(32)),
        format!("Bearer {good}"),
        format!("{good}extra"),
    ];
    for (i, contents) in cases.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("admin-token"), contents).unwrap();
        let r =
            switchyard::app::AppState::new(dir.path().to_path_buf(), "127.0.0.1".into(), 1, 4, 30);
        assert!(
            r.is_err(),
            "case {i} ({contents:?}) started with a bad token"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("admin-token")).unwrap(),
            *contents,
            "case {i}: token file must not be rewritten"
        );
    }
    // A valid token with a trailing newline (hand-edited) is accepted and used as-is.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("admin-token"), format!("{good}\n")).unwrap();
    let gw = Gateway::start(dir.path()).await;
    assert_eq!(gw.admin, good);
    assert_eq!(gw.admin_get("/api/overview").await.status(), 200);
}

#[tokio::test]
async fn empty_credentials_never_authenticate() {
    let (_d, gw) = setup().await;
    for (name, value) in [
        ("authorization", "Bearer "),
        ("authorization", "Bearer"),
        ("x-api-key", ""),
        ("authorization", ""),
    ] {
        let r = gw
            .http
            .get(gw.url("/api/overview"))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "admin {name}={value:?}");
        let r = gw
            .http
            .get(gw.url("/v1/models"))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "client {name}={value:?}");
        let r = gw
            .http
            .post(gw.url("/api/session"))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "session {name}={value:?}");
    }
}

#[tokio::test]
async fn inference_bodies_between_8_and_64_mib_are_proxied() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(
        Router::new()
            .route(
                "/responses",
                post(|Json(v): Json<Value>| async move {
                    json_response(
                        200,
                        json!({"id":"resp_l","received":v["input"].as_str().map(str::len)}),
                    )
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::disable()),
    )
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let input = "a".repeat(20 * 1024 * 1024);
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":input}))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["received"], input.len());
    // Admin surface keeps the 8 MiB cap.
    let r = gw
        .admin_send(
            Method::POST,
            "/api/playground",
            json!({"model":"m","input":input}),
        )
        .await;
    assert_eq!(r.status(), 413);
}

// ---------------------------------------------------------------------------------------------
// Chat Completions -> Responses translation (Codex)
// ---------------------------------------------------------------------------------------------

fn recording_codex(body: String) -> Router {
    Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(Body::from(body)) }
        }),
    )
}

#[tokio::test]
async fn chat_request_shapes_translate_to_responses() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(recording_codex(codex_events(
        "resp_s",
        &[completed("resp_s", "ok")],
    )))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let schema = json!({"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false});
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex",
                "messages":[
                    {"role":"system","content":[{"type":"text","text":"Rule one."},{"type":"text","text":"Rule two."}]},
                    {"role":"developer","content":"Dev note."},
                    {"role":"user","content":[{"type":"text","text":"What is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA","detail":"high"}}]},
                    {"role":"assistant","content":[{"type":"text","text":"Calling."}],"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":1}"}}]},
                    {"role":"tool","tool_call_id":"call_1","content":"{\"r\":2}"}
                ],
                "tools":[{"type":"function","function":{"name":"lookup","description":"d","parameters":{"type":"object"},"strict":true}}],
                "tool_choice":{"type":"function","function":{"name":"lookup"}},
                "response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":schema,"strict":true}},
                "reasoning_effort":"high","parallel_tool_calls":false}),
        )
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let p = up.requests()[0].json();
    assert_eq!(p["instructions"], "Rule one.\nRule two.\nDev note.");
    assert_eq!(
        p["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"What is this?"},{"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"high"}]},
            {"role":"assistant","content":[{"type":"output_text","text":"Calling."}]},
            {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},
            {"type":"function_call_output","call_id":"call_1","output":"{\"r\":2}"}
        ])
    );
    assert_eq!(
        p["tools"],
        json!([{"type":"function","name":"lookup","description":"d","parameters":{"type":"object"},"strict":true}])
    );
    assert_eq!(p["tool_choice"], json!({"type":"function","name":"lookup"}));
    assert_eq!(
        p["text"],
        json!({"format":{"type":"json_schema","name":"answer","schema":schema,"strict":true}})
    );
    assert_eq!(p["reasoning"], json!({"effort":"high"}));
    assert_eq!(p["parallel_tool_calls"], false);
    for k in [
        "messages",
        "response_format",
        "reasoning_effort",
        "max_tokens",
    ] {
        assert!(
            p.get(k).is_none(),
            "{k} leaked into the Responses payload: {p}"
        );
    }

    // String tool_choice and json_object format pass through in Responses shape.
    gw.post(
        "/v1/chat/completions",
        &key,
        json!({"model":"gpt-codex","messages":[{"role":"user","content":"x"}],"tool_choice":"required","response_format":{"type":"json_object"}}),
    )
    .await;
    let p = up.requests()[1].json();
    assert_eq!(p["tool_choice"], "required");
    assert_eq!(p["text"], json!({"format":{"type":"json_object"}}));
    assert_eq!(p["instructions"], "You are a helpful coding assistant.");
}

fn rich_codex_stream() -> String {
    codex_events(
        "resp_rich",
        &[
            json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"SECRET-REASONING-TRACE"}),
            json!({"type":"response.reasoning_text.delta","item_id":"rs_1","delta":"SECRET-REASONING-RAW"}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"SECRET-REASONING-TRACE"}],"encrypted_content":"SECRET-ENCRYPTED"}}),
            json!({"type":"response.output_text.delta","delta":"Answer."}),
            json!({"type":"response.completed","response":{"id":"resp_rich","status":"completed","output":[
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"SECRET-REASONING-TRACE"}],"content":[{"type":"reasoning_text","text":"SECRET-REASONING-RAW"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer."}]}],
                "usage":{"input_tokens":21,"output_tokens":34,"total_tokens":55,"output_tokens_details":{"reasoning_tokens":30}}}}),
        ],
    )
}

#[tokio::test]
async fn chat_stream_chunks_are_well_formed_with_usage_and_no_reasoning() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(recording_codex(rich_codex_stream())).await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let r = gw.post("/v1/chat/completions", &key, json!({"model":"gpt-codex","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"x"}]})).await;
    let text = r.text().await.unwrap();
    assert!(
        !text.contains("SECRET-"),
        "reasoning leaked into Chat stream: {text}"
    );
    let (chunks, done) = chat_chunks(&text);
    assert!(done);
    assert!(chunks.len() >= 3, "{text}");
    assert!(
        chunks.iter().all(|c| c["id"] == "resp_rich"),
        "chunk ids must be the stable upstream id: {text}"
    );
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk" && c["model"] == "gpt-codex")
    );
    assert_eq!(
        chunks[0]["choices"][0]["delta"]["role"], "assistant",
        "first chunk announces the role"
    );
    assert_eq!(
        chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"]["role"].is_string())
            .count(),
        1
    );
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Answer.");
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert_eq!(last["usage"]["prompt_tokens"], 21);
    assert_eq!(last["usage"]["completion_tokens"], 34);
    assert_eq!(last["usage"]["total_tokens"], 55);
    assert!(
        last["usage"].get("input_tokens").is_none(),
        "native Chat usage field names only: {last}"
    );
    assert!(
        chunks.iter().all(|c| c["created"].is_i64()),
        "every chunk carries created"
    );

    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert!(
        !text.contains("SECRET-"),
        "reasoning leaked into Chat completion: {text}"
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["id"], "resp_rich");
    assert_eq!(
        v["choices"][0]["message"],
        json!({"role":"assistant","content":"Answer."})
    );
    assert_eq!(
        v["usage"],
        json!({"prompt_tokens":21,"completion_tokens":34,"total_tokens":55})
    );
}

#[tokio::test]
async fn chat_stream_tool_calls_have_stable_indices_and_finish_reason() {
    let (_d, gw) = setup().await;
    let body = codex_events(
        "resp_tools",
        &[
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"one","arguments":""}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"two","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"b\":"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"a\":1}"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"2}"}),
            completed("resp_tools", ""),
        ],
    );
    let up = Upstream::start(recording_codex(body)).await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let (chunks, done) = chat_chunks(&r.text().await.unwrap());
    assert!(done);
    let mut args = [String::new(), String::new()];
    let mut names = [String::new(), String::new()];
    for c in &chunks {
        if let Some(calls) = c["choices"][0]["delta"]["tool_calls"].as_array() {
            for call in calls {
                let i = call["index"].as_u64().unwrap() as usize;
                if let Some(n) = call["function"]["name"].as_str() {
                    names[i].push_str(n);
                }
                args[i].push_str(call["function"]["arguments"].as_str().unwrap_or(""));
            }
        }
    }
    assert_eq!(names, ["one".to_string(), "two".to_string()]);
    assert_eq!(args, ["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
}

#[tokio::test]
async fn openai_native_chat_stream_usage_passes_through() {
    let (_d, gw) = setup().await;
    let body = "data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n\
                data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17}}\n\n\
                data: [DONE]\n\n";
    let up = Upstream::start(Router::new().route(
        "/chat/completions",
        post(move || async move { sse_response(Body::from(Bytes::from_static(body.as_bytes()))) }),
    ))
    .await;
    gw.connection("OpenAI", "openai", &up.base(), &["gpt-x"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw.post("/v1/chat/completions", &key, json!({"model":"gpt-x","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"x"}]})).await;
    assert_eq!(
        r.text().await.unwrap(),
        body,
        "native Chat streams are a byte-exact passthrough"
    );
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 200);
    assert_eq!(log[0]["input_tokens"], 12);
    assert_eq!(log[0]["output_tokens"], 5);
}
