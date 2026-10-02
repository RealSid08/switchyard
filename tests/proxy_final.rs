//! Proxy production-reliability tests: credential recovery after 401, Claude OAuth request
//! shaping, stream terminal fidelity, error redaction and WebSocket session semantics.
//! Real gateway, loopback mock providers, tempdir state and fake credentials only.

mod support;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::ws::{Message as AxMessage, WebSocketUpgrade},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;
use switchyard::{credentials, proxy::CLAUDE_CODE_IDENTITY, store::Connection};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}
fn codex_token(tag: &str) -> String {
    jwt(
        json!({"exp":chrono::Utc::now().timestamp()+86400,"sub":"auth0|maria","tag":tag,
        "https://api.openai.com/auth":{"chatgpt_account_id":"ws-1"}}),
    )
}
fn write_codex_source(path: &Path, token: &str) {
    std::fs::write(
        path,
        json!({"tokens":{"access_token":token,"refresh_token":"rt-shared","account_id":"ws-1"}})
            .to_string(),
    )
    .unwrap();
}
/// Imports a fake native Codex login and points it at a mock provider.
async fn native_codex(gw: &Gateway, source: &Path, token: &str, base: &str, ws: bool) -> String {
    write_codex_source(source, token);
    let mut c = credentials::import(&gw.app, "codex", Some(source.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    c.base_url = base.into();
    c.models = vec!["gpt-codex".into()];
    c.supports_websocket = ws;
    gw.app.store.put("connection", &c.id, &c).unwrap();
    c.id
}
fn stored(gw: &Gateway, id: &str) -> Connection {
    gw.app.store.get("connection", id).unwrap()
}
fn bearer(h: &HeaderMap) -> String {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_string()
}

/// A Codex backend that accepts exactly one access token.
fn codex_accepting(token: String) -> Router {
    Router::new().route(
        "/responses",
        post(move |h: HeaderMap| {
            let token = token.clone();
            async move {
                if bearer(&h) != token {
                    return json_response(
                        401,
                        json!({"error":{"type":"invalid_request_error","code":"token_expired","message":"expired"}}),
                    );
                }
                sse_response(Body::from(sse(&[
                    json!({"type":"response.created","response":{"id":"resp_ok"}}),
                    completed("resp_ok", "fine"),
                ])))
            }
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// 401 recovery: adopt the owner's newer token once, same account, never loop
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_401_adopts_newer_native_token_and_retries_the_same_account_once() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    let (old, new) = (codex_token("old"), codex_token("new"));
    let up = Upstream::start(codex_accepting(new.clone())).await;
    let id = native_codex(&gw, &file, &old, &up.base(), false).await;
    let (_, key) = gw.create_key("k").await;

    // The Codex CLI rotated its own login; the gateway still holds the old token.
    write_codex_source(&file, &new);
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let seen: Vec<String> = up.requests().iter().map(|c| bearer(&c.headers)).collect();
    assert_eq!(
        seen,
        vec![old.clone(), new.clone()],
        "one retry, same account"
    );
    assert_eq!(stored(&gw, &id).api_key, new);
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 200);

    // The provider rejects the current token and the source has nothing newer: no retry loop.
    let up2 = Upstream::start(codex_accepting("never".into())).await;
    let mut c = stored(&gw, &id);
    c.base_url = up2.base();
    gw.app.store.put("connection", &id, &c).unwrap();
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "token_expired");
    assert_eq!(up2.count(), 1, "no retry without a different token");
}

#[tokio::test]
async fn api_key_401_is_never_retried() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(codex_accepting("other".into())).await;
    gw.connection("Key", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(up.count(), 1);
}

// ---------------------------------------------------------------------------------------------
// Claude OAuth: Claude Code identity system block, user system prompt preserved
// ---------------------------------------------------------------------------------------------

fn anthropic_ok() -> Router {
    Router::new().route(
        "/messages",
        post(|| async {
            json_response(
                200,
                json!({"id":"msg_1","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}}),
            )
        }),
    )
}

#[tokio::test]
async fn claude_oauth_requests_identify_as_claude_code_and_keep_the_user_system_prompt() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(anthropic_ok()).await;
    let oauth = json!({"id":"claude-oauth","name":"Claude","kind":"anthropic","base_url":up.base(),"enabled":true,
        "models":["claude-opus-5-5"],"supports_websocket":false,"created_at":"2026-10-01T00:00:00Z",
        "api_key":PROVIDER_KEY,"refresh_token":"rt","expires_at":chrono::Utc::now().timestamp()+86400,
        "account_id":"","oauth":true,"credential_source":"oauth"});
    gw.app
        .store
        .put("connection", "claude-oauth", &oauth)
        .unwrap();
    let keyed = Upstream::start(anthropic_ok()).await;
    gw.connection("Key", "anthropic", &keyed.base(), &["claude-key"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let send = |model: &'static str, system: Option<Value>| {
        let mut body =
            json!({"model":model,"max_tokens":5,"messages":[{"role":"user","content":"x"}]});
        if let Some(s) = system {
            body["system"] = s;
        }
        gw.http
            .post(gw.url("/v1/messages"))
            .header("x-api-key", &key)
            .json(&body)
            .send()
    };
    let identity = json!({"type":"text","text":CLAUDE_CODE_IDENTITY});

    send("claude-opus-5-5", None).await.unwrap();
    assert_eq!(up.requests()[0].json()["system"], json!([identity]));

    send("claude-opus-5-5", Some(json!("Be terse.")))
        .await
        .unwrap();
    assert_eq!(
        up.requests()[1].json()["system"],
        json!([identity, {"type":"text","text":"Be terse."}])
    );

    let cached =
        json!([{"type":"text","text":"Project rules","cache_control":{"type":"ephemeral"}}]);
    send("claude-opus-5-5", Some(cached.clone())).await.unwrap();
    let got = up.requests()[2].json()["system"].clone();
    assert_eq!(got[0], identity);
    assert_eq!(got[1], cached[0], "user blocks and cache_control preserved");
    assert_eq!(got.as_array().unwrap().len(), 2);

    // Real Claude Code requests already carry the identity (after a billing block): untouched.
    let native = json!([{"type":"text","text":"x-anthropic-billing-header: cc_version=2"},{"type":"text","text":CLAUDE_CODE_IDENTITY},{"type":"text","text":"more"}]);
    send("claude-opus-5-5", Some(native.clone())).await.unwrap();
    assert_eq!(up.requests()[3].json()["system"], native);

    // API-key accounts are never rewritten.
    send("claude-key", Some(json!("Be terse."))).await.unwrap();
    assert_eq!(keyed.requests()[0].json()["system"], "Be terse.");
}

// ---------------------------------------------------------------------------------------------
// Native SSE terminal fidelity
// ---------------------------------------------------------------------------------------------

/// Serves one raw HTTP response per connection and then drops the socket (no chunked trailer).
async fn raw_upstream(response: Vec<u8>) -> String {
    let response: &'static [u8] = Box::leak(response.into_boxed_slice());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 64 * 1024];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(response).await;
                let _ = s.flush().await;
                drop(s);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

async fn stream_text(gw: &Gateway, key: &str, path: &str, body: Value) -> String {
    gw.http
        .post(gw.url(path))
        .bearer_auth(key)
        .header("x-api-key", key)
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap_or_default()
}

#[tokio::test]
async fn connection_drop_after_terminal_event_is_a_clean_success() {
    let (_d, gw) = setup().await;
    let event = sse(&[
        json!({"type":"response.created","response":{"id":"r1"}}),
        completed("r1", "done"),
    ]);
    let mut raw =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec();
    raw.extend_from_slice(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes());
    let base = raw_upstream(raw).await;
    gw.connection("A", "openai", &base, &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let text = stream_text(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert!(text.contains("response.completed"));
    assert!(!text.contains("upstream_interrupted"), "{text}");
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 200, "{}", log[0]);
}

#[tokio::test]
async fn in_band_provider_failures_are_relayed_once_and_recorded() {
    let (_d, gw) = setup().await;
    let anthropic = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n\
                     event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let failed = sse(&[
        json!({"type":"response.created","response":{"id":"r2"}}),
        json!({"type":"response.failed","response":{"id":"r2","status":"failed","error":{"code":"server_error","message":"boom"}}}),
    ]);
    let up = Upstream::start(
        Router::new()
            .route(
                "/messages",
                post(move || async move { sse_response(Body::from(anthropic)) }),
            )
            .route(
                "/responses",
                post(move || {
                    let failed = failed.clone();
                    async move { sse_response(Body::from(failed)) }
                }),
            ),
    )
    .await;
    gw.connection("Anth", "anthropic", &up.base(), &["claude-x"])
        .await;
    gw.connection("OpenAI", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    let text = stream_text(&gw, &key, "/v1/messages", json!({"model":"claude-x","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"x"}]})).await;
    assert!(text.contains("overloaded_error"), "{text}");
    assert!(
        !text.contains("upstream_interrupted"),
        "native error relayed once, no gateway error appended: {text}"
    );
    let text = stream_text(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert!(text.contains("response.failed") && !text.contains("upstream_interrupted"));
    let log = gw.wait_for_log(2).await;
    assert!(log.iter().all(|r| r["status"] == 502), "{log:?}");
    assert!(
        log.iter().any(|r| r["error"]
            .as_str()
            .unwrap_or("")
            .contains("overloaded_error")),
        "{log:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Chat translation: derived usage, stable chunk fields, Codex output reconstruction
// ---------------------------------------------------------------------------------------------

fn codex_router(body: String) -> Router {
    Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(Body::from(body)) }
        }),
    )
}

#[tokio::test]
async fn chat_usage_total_is_derived_and_codex_output_is_rebuilt_from_item_events() {
    let (_d, gw) = setup().await;
    let message = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Rebuilt."}]});
    let body = sse(&[
        json!({"type":"response.created","response":{"id":"resp_x"}}),
        json!({"type":"response.output_text.delta","delta":"Rebuilt."}),
        json!({"type":"response.output_item.done","output_index":0,"item":message}),
        json!({"type":"response.completed","response":{"id":"resp_x","status":"completed","output":[],
            "usage":{"input_tokens":4,"output_tokens":6}}}),
    ]);
    let up = Upstream::start(codex_router(body)).await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let text = stream_text(
        &gw,
        &key,
        "/v1/chat/completions",
        json!({"model":"gpt-codex","stream":true,"messages":[{"role":"user","content":"x"}]}),
    )
    .await;
    let chunks: Vec<Value> = text
        .split("\n\n")
        .filter_map(|b| b.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    let last = chunks.last().unwrap();
    assert_eq!(
        last["usage"],
        json!({"prompt_tokens":4,"completion_tokens":6,"total_tokens":10})
    );
    let created = chunks[0]["created"].as_i64().unwrap();
    assert!(
        chunks
            .iter()
            .all(|c| c["created"] == created && c["id"] == "resp_x")
    );
    assert!(text.trim_end().ends_with("data: [DONE]"));

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["output"][0], message, "output rebuilt from item events");
    let r = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"gpt-codex","messages":[{"role":"user","content":"x"}]}),
        )
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "Rebuilt.");
    assert_eq!(v["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn codex_non_stream_failure_keeps_the_native_error_code() {
    let (_d, gw) = setup().await;
    let body = sse(&[
        json!({"type":"response.created","response":{"id":"r"}}),
        json!({"type":"response.failed","response":{"id":"r","status":"failed","error":{"code":"context_length_exceeded","message":"Too long"}}}),
    ]);
    let up = Upstream::start(codex_router(body)).await;
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
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded");
}

// ---------------------------------------------------------------------------------------------
// Provider error redaction
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reflected_credentials_are_redacted_everywhere_in_structured_errors() {
    let (_d, gw) = setup().await;
    let (_, key) = gw.create_key("k").await;
    let reflected = key.clone();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let reflected = reflected.clone();
            async move {
                json_response(
                    400,
                    json!({"error":{"type":"invalid_request_error","code":"invalid_value","param":reflected,
                        "message":format!("Header was Bearer {PROVIDER_KEY}"),
                        "details":{"seen":[PROVIDER_KEY, "jwt eyJhbGciOiJIUzI1NiJ9.e30.abcdefgh"]}}}),
                )
            }
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(
        !text.contains(PROVIDER_KEY) && !text.contains(&key) && !text.contains("eyJhbGci"),
        "{text}"
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["code"], "invalid_value");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["param"], "[redacted]");
    let log = gw.wait_for_log(1).await;
    assert_eq!(
        log[0]["error"], "Provider rejected request (invalid_value)",
        "log keeps only the native code"
    );
}

// ---------------------------------------------------------------------------------------------
// WebSocket sessions
// ---------------------------------------------------------------------------------------------

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(gw: &Gateway, key: &str) -> Client {
    let mut req = gw.ws_url("/v1/responses").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}
async fn next_json(ws: &mut Client) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame within 5 s")
        {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}
async fn turn(ws: &mut Client, frame: Value) -> Value {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
    loop {
        let v = next_json(ws).await;
        if v["type"] == "response.completed" || v["type"] == "error" {
            return v;
        }
    }
}

#[derive(Default)]
struct WsLog {
    handshakes: AtomicUsize,
    frames: Mutex<Vec<Value>>,
}

/// Upstream Responses WebSocket. `hang` holds responses open; `limit` answers with a usage limit.
fn ws_upstream(log: Arc<WsLog>, accept: Option<String>, mode: &'static str) -> Router {
    Router::new().route(
        "/responses",
        get(move |h: HeaderMap, ws: WebSocketUpgrade| {
            let log = log.clone();
            let accept = accept.clone();
            async move {
                log.handshakes.fetch_add(1, Ordering::SeqCst);
                if accept.as_ref().is_some_and(|t| *t != bearer(&h)) {
                    return (axum::http::StatusCode::UNAUTHORIZED, "expired").into_response();
                }
                ws.on_upgrade(move |mut socket| async move {
                    while let Some(Ok(AxMessage::Text(t))) = socket.recv().await {
                        let v: Value = serde_json::from_str(&t).expect("upstream got valid JSON");
                        let n = {
                            let mut f = log.frames.lock().unwrap();
                            f.push(v);
                            f.len()
                        };
                        let id = format!("resp_{n}");
                        let replies = match mode {
                            "hang" => vec![json!({"type":"response.created","response":{"id":id}})],
                            "limit" => vec![json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":900}})],
                            _ => vec![
                                json!({"type":"response.created","response":{"id":id}}),
                                completed(&id, "ok"),
                            ],
                        };
                        for r in replies {
                            let _ = socket.send(AxMessage::Text(r.to_string().into())).await;
                        }
                    }
                })
                .into_response()
            }
        }),
    )
}
async fn ws_conn(gw: &Gateway, name: &str, base: &str) -> String {
    gw.add_connection(json!({"name":name,"kind":"openai","base_url":base,"models":["m"],"supports_websocket":true,"api_key":PROVIDER_KEY}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn control_frames_before_the_first_frame_are_handled() {
    let (_d, gw) = setup().await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_upstream(log.clone(), None, "ok")).await;
    ws_conn(&gw, "A", &up.base()).await;
    let (_, key) = gw.create_key("k").await;

    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Ping(Bytes::from_static(b"hi")))
        .await
        .unwrap();
    ws.send(Message::Pong(Bytes::new())).await.unwrap();
    let done = turn(
        &mut ws,
        json!({"type":"response.create","model":"m","input":"x"}),
    )
    .await;
    assert_eq!(done["type"], "response.completed", "{done}");

    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Binary(Bytes::from_static(b"\x00")))
        .await
        .unwrap();
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("response.create")
    );
}

#[tokio::test]
async fn malformed_mid_session_frames_are_reported_and_the_session_continues() {
    let (_d, gw) = setup().await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_upstream(log.clone(), None, "ok")).await;
    ws_conn(&gw, "A", &up.base()).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    turn(
        &mut ws,
        json!({"type":"response.create","model":"m","input":"x"}),
    )
    .await;
    ws.send(Message::Text("{not json".into())).await.unwrap();
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    let done = turn(
        &mut ws,
        json!({"type":"response.create","model":"m","input":"y"}),
    )
    .await;
    assert_eq!(done["type"], "response.completed");
    assert_eq!(
        log.frames.lock().unwrap().len(),
        2,
        "malformed frame never forwarded"
    );
    ws.close(None).await.unwrap();
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 200);
}

#[tokio::test]
async fn closing_during_an_active_response_records_499() {
    let (_d, gw) = setup().await;
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_upstream(log.clone(), None, "hang")).await;
    ws_conn(&gw, "A", &up.base()).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"m","input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "response.created");
    ws.close(None).await.unwrap();
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 499, "{}", log[0]);
}

#[tokio::test]
async fn websocket_usage_limit_cools_the_account_for_new_sessions() {
    let (_d, gw) = setup().await;
    let (la, lb) = (Arc::new(WsLog::default()), Arc::new(WsLog::default()));
    let limited = Upstream::start(ws_upstream(la.clone(), None, "limit")).await;
    let good = Upstream::start(ws_upstream(lb.clone(), None, "ok")).await;
    let a = ws_conn(&gw, "Limited", &limited.base()).await;
    let b = ws_conn(&gw, "Good", &good.base()).await;
    gw.put_route(
        "pool",
        "failover",
        json!([{"connection_id":a,"model":"m"},{"connection_id":b,"model":"m"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    let mut ws = ws_connect(&gw, &key).await;
    let v = turn(
        &mut ws,
        json!({"type":"response.create","model":"pool","input":"x"}),
    )
    .await;
    assert_eq!(v["error"]["type"], "usage_limit_reached");
    drop(ws);
    let mut ws = ws_connect(&gw, &key).await;
    let v = turn(
        &mut ws,
        json!({"type":"response.create","model":"pool","input":"x"}),
    )
    .await;
    assert_eq!(v["type"], "response.completed", "{v}");
    assert_eq!(
        la.handshakes.load(Ordering::SeqCst),
        1,
        "limited account skipped"
    );
}

#[tokio::test]
async fn websocket_handshake_401_adopts_newer_native_token_on_the_same_account() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    let (old, new) = (codex_token("old"), codex_token("new"));
    let log = Arc::new(WsLog::default());
    let up = Upstream::start(ws_upstream(log.clone(), Some(new.clone()), "ok")).await;
    let id = native_codex(&gw, &file, &old, &up.base(), true).await;
    write_codex_source(&file, &new);
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    let v = turn(
        &mut ws,
        json!({"type":"response.create","model":"gpt-codex","input":"x"}),
    )
    .await;
    assert_eq!(v["type"], "response.completed", "{v}");
    assert_eq!(log.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(log.frames.lock().unwrap().len(), 1, "one inference frame");
    assert_eq!(stored(&gw, &id).api_key, new);
}
