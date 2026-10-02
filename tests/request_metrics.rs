//! Request-record observability: route alias, failover trace, per-attempt outcomes, time to
//! first byte and first output, WebSocket first-turn timings, compatibility with rows written
//! before these fields existed, and credential-safe error labels. Fake providers only.

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
use std::{path::Path, time::Duration};
use support::*;
use switchyard::{credentials, store::Connection};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}
async fn dead_port() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}
fn fail(status: u16, body: Value) -> Router {
    Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { json_response(status, body) }
        }),
    )
}
fn ok_json(id: &'static str) -> Router {
    Router::new().route(
        "/responses",
        post(move || async move { json_response(200, completed(id, "ok")["response"].clone()) }),
    )
}
async fn latest(gw: &Gateway) -> Value {
    gw.wait_for_log(1).await[0].clone()
}
fn attempts(record: &Value) -> Vec<(String, u64, Value)> {
    record["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["connection_name"].as_str().unwrap().to_string(),
                a["status"].as_u64().unwrap(),
                a["error"].clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Failover trace
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn failover_trace_lists_every_account_with_constant_labels() {
    let (_d, gw) = setup().await;
    let limited = Upstream::start(fail(
        429,
        json!({"error":{"type":"rate_limit_error","message":"slow down"}}),
    ))
    .await;
    let good = Upstream::start(ok_json("resp_good")).await;
    let a = gw
        .connection("Limited", "openai", &limited.base(), &["m-a"])
        .await;
    let b = gw
        .connection("Dead", "openai", &dead_port().await, &["m-b"])
        .await;
    let c = gw
        .connection("Good", "openai", &good.base(), &["m-c"])
        .await;
    gw.put_route(
        "pool",
        "failover",
        json!([{"connection_id":a,"model":"m-a"},{"connection_id":b,"model":"m-b"},{"connection_id":c,"model":"m-c"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"pool","input":"x"}))
        .await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();

    let rec = latest(&gw).await;
    assert_eq!(rec["status"], 200);
    assert_eq!(rec["route"], "pool");
    assert_eq!(rec["connection_name"], "Good");
    assert_eq!(rec["failovers"], 2);
    assert_eq!(
        attempts(&rec),
        vec![
            ("Limited".into(), 429, json!("rate_limited")),
            ("Dead".into(), 0, json!("connect_failed")),
            ("Good".into(), 200, Value::Null),
        ]
    );
    let models: Vec<&str> = rec["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["m-a", "m-b", "m-c"], "upstream model per attempt");
    let ttfb = rec["ttfb_ms"].as_u64().expect("ttfb recorded");
    assert!(ttfb <= rec["latency_ms"].as_u64().unwrap());
    assert!(
        rec["first_token_ms"].is_null(),
        "a single JSON body has no incremental output"
    );

    // A direct model name is not a route alias.
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m-c","input":"x"}))
        .await;
    r.bytes().await.unwrap();
    let rec = gw.wait_for_log(2).await[0].clone();
    assert!(rec["route"].is_null());
    assert_eq!(rec["failovers"], 0);
    assert_eq!(attempts(&rec).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// 401 rotation on the same account is an attempt, not a failover
// ---------------------------------------------------------------------------------------------

fn jwt(tag: &str) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = json!({"exp":chrono::Utc::now().timestamp()+86400,"sub":"auth0|m","tag":tag,
        "https://api.openai.com/auth":{"chatgpt_account_id":"ws-1"}});
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}
fn write_source(path: &Path, token: &str) {
    std::fs::write(
        path,
        json!({"tokens":{"access_token":token,"refresh_token":"rt-shared","account_id":"ws-1"}})
            .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn same_account_credential_retry_is_two_attempts_and_no_failover() {
    let (_d, gw) = setup().await;
    let (old, new) = (jwt("old"), jwt("new"));
    let accepted = new.clone();
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move |h: HeaderMap| {
            let accepted = accepted.clone();
            async move {
                let auth = h["authorization"].to_str().unwrap().to_string();
                if auth != format!("Bearer {accepted}") {
                    return json_response(401, json!({"error":{"code":"token_expired"}}));
                }
                sse_response(Body::from(sse(&[
                    json!({"type":"response.created","response":{"id":"r"}}),
                    json!({"type":"response.output_text.delta","delta":"OK"}),
                    completed("r", "OK"),
                ])))
            }
        }),
    ))
    .await;
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    write_source(&file, &old);
    let mut c = credentials::import(&gw.app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    c.base_url = up.base();
    c.name = "Codex".into();
    c.models = vec!["gpt-codex".into()];
    gw.app.store.put("connection", &c.id, &c).unwrap();
    write_source(&file, &new); // the CLI refreshed its own login
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"gpt-codex","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let rec = latest(&gw).await;
    assert_eq!(rec["failovers"], 0, "{rec}");
    assert_eq!(
        attempts(&rec),
        vec![
            ("Codex".into(), 401, json!("auth_rejected")),
            ("Codex".into(), 200, Value::Null),
        ]
    );
    // Collected Codex SSE still reports when output started.
    assert!(rec["first_token_ms"].as_u64().is_some());
    let text = serde_json::to_string(&rec).unwrap();
    assert!(!text.contains(&old) && !text.contains(&new) && !text.contains("rt-shared"));
}

// ---------------------------------------------------------------------------------------------
// Split SSE: time to first byte vs first output
// ---------------------------------------------------------------------------------------------

/// Sends `(delay_ms, bytes)` chunks in order.
fn paced(chunks: Vec<(u64, String)>) -> Body {
    let s = async_stream::stream! {
        for (delay, chunk) in chunks {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            yield Ok::<Bytes, std::io::Error>(Bytes::from(chunk));
        }
    };
    Body::from_stream(s)
}

#[tokio::test]
async fn ttfb_and_first_token_are_measured_on_split_sse() {
    let (_d, gw) = setup().await;
    let created = sse(&[json!({"type":"response.created","response":{"id":"r"}})]);
    let empty = sse(&[json!({"type":"response.output_text.delta","delta":""})]);
    let delta = sse(&[json!({"type":"response.output_text.delta","delta":"Hello"})]);
    let (head, tail) = delta.split_at(delta.len() / 2);
    let done = sse(&[completed("r", "Hello")]);
    let chunks = vec![
        (150, created),
        (50, empty),
        (300, head.to_string()),
        (50, tail.to_string()),
        (0, done),
    ];
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let chunks = chunks.clone();
            async move { sse_response(paced(chunks)) }
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
    assert!(r.text().await.unwrap().contains("Hello"));
    let rec = latest(&gw).await;
    let ttfb = rec["ttfb_ms"].as_u64().unwrap();
    let first = rec["first_token_ms"].as_u64().unwrap();
    assert!((140..1000).contains(&ttfb), "ttfb {ttfb}");
    assert!(
        first >= ttfb + 380,
        "empty deltas and half-received events are not output: ttfb {ttfb}, first {first}"
    );
    assert!(first <= rec["latency_ms"].as_u64().unwrap());
}

#[tokio::test]
async fn tool_arguments_and_thinking_count_as_first_output() {
    let (_d, gw) = setup().await;
    let tool = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n\
                event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"t\",\"input\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n";
    let args = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":1}\"}}\n\n\
                event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let thinking = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n\
                    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let silent = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n\
                  event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let up = Upstream::start(Router::new().route(
        "/messages",
        post(move |b: Bytes| async move {
            let v: Value = serde_json::from_slice(&b).unwrap();
            let chunks = match v["model"].as_str().unwrap() {
                "tool" => vec![(0, tool.to_string()), (300, args.to_string())],
                "thinking" => vec![(250, thinking.to_string())],
                _ => vec![(0, silent.to_string())],
            };
            sse_response(paced(chunks))
        }),
    ))
    .await;
    gw.connection(
        "Anth",
        "anthropic",
        &up.base(),
        &["tool", "thinking", "silent"],
    )
    .await;
    let (_, key) = gw.create_key("k").await;
    for (n, model) in ["tool", "thinking", "silent"].into_iter().enumerate() {
        let r = gw
            .http
            .post(gw.url("/v1/messages"))
            .header("x-api-key", &key)
            .json(&json!({"model":model,"max_tokens":5,"stream":true,"messages":[{"role":"user","content":"x"}]}))
            .send()
            .await
            .unwrap();
        r.bytes().await.unwrap();
        let rec = gw.wait_for_log(n + 1).await[0].clone();
        assert_eq!(rec["model"], model);
        let first = rec["first_token_ms"].as_u64();
        match model {
            "tool" => assert!(first.unwrap() >= 290, "tool args at {first:?}"),
            "thinking" => assert!(first.unwrap() >= 240, "thinking at {first:?}"),
            _ => assert_eq!(first, None, "no output, no first token"),
        }
        assert!(rec["ttfb_ms"].as_u64().is_some());
    }
}

// ---------------------------------------------------------------------------------------------
// WebSocket: one record per session, timings from the first turn
// ---------------------------------------------------------------------------------------------

fn ws_upstream() -> Router {
    Router::new().route(
        "/responses",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                let mut n = 0;
                while let Some(Ok(AxMessage::Text(t))) = socket.recv().await {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    n += 1;
                    let id = format!("resp_{n}");
                    let (delay, events) = match v["mode"].as_str().unwrap_or("") {
                        "slow" => (
                            200,
                            vec![
                                json!({"type":"response.created","response":{"id":id}}),
                                json!({"type":"response.output_text.delta","delta":"first"}),
                                completed(&id, "first"),
                            ],
                        ),
                        "fail" => (
                            0,
                            vec![
                                json!({"type":"error","error":{"code":"server_error","message":"x"}}),
                            ],
                        ),
                        _ => (
                            0,
                            vec![
                                json!({"type":"response.created","response":{"id":id}}),
                                json!({"type":"response.output_text.delta","delta":"later"}),
                                completed(&id, "later"),
                            ],
                        ),
                    };
                    for (i, e) in events.into_iter().enumerate() {
                        if i < 2 {
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                        }
                        let _ = socket.send(AxMessage::Text(e.to_string().into())).await;
                    }
                }
            })
            .into_response()
        }),
    )
}
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn ws_connect(gw: &Gateway, key: &str) -> Client {
    let mut req = gw.ws_url("/v1/responses").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}
async fn turn(ws: &mut Client, frame: Value) -> Value {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap();
        if let Some(Ok(Message::Text(t))) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "response.completed" || v["type"] == "error" {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn websocket_record_times_the_first_turn_only() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(ws_upstream()).await;
    let id = gw.add_connection(json!({"name":"WS","kind":"openai","base_url":up.base(),"models":["m"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await["id"].as_str().unwrap().to_string();
    gw.put_route(
        "ws-pool",
        "failover",
        json!([{"connection_id":id,"model":"m"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    // Slow first turn, fast second turn: the record keeps the first turn's timings.
    let mut ws = ws_connect(&gw, &key).await;
    turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-pool","input":"x","mode":"slow"}),
    )
    .await;
    turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-pool","input":"y"}),
    )
    .await;
    ws.close(None).await.unwrap();
    let rec = latest(&gw).await;
    assert_eq!(rec["transport"], "websocket");
    assert_eq!(rec["route"], "ws-pool");
    assert_eq!(attempts(&rec), vec![("WS".into(), 101, Value::Null)]);
    let ttfb = rec["ttfb_ms"].as_u64().unwrap();
    let first = rec["first_token_ms"].as_u64().unwrap();
    assert!(ttfb >= 190, "first turn first frame: {ttfb}");
    assert!(first >= ttfb + 190, "first turn first delta: {first}");

    // A first turn without output leaves first_token_ms empty even if a later turn has output.
    let mut ws = ws_connect(&gw, &key).await;
    turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-pool","input":"x","mode":"fail"}),
    )
    .await;
    turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-pool","input":"y"}),
    )
    .await;
    ws.close(None).await.unwrap();
    let rec = gw.wait_for_log(2).await[0].clone();
    assert!(rec["ttfb_ms"].as_u64().is_some());
    assert!(rec["first_token_ms"].is_null(), "{rec}");
    assert_eq!(rec["status"], 200, "the session's last turn succeeded");
}

// ---------------------------------------------------------------------------------------------
// Rows written before these fields existed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn rows_written_by_older_versions_still_load() {
    let (_d, gw) = setup().await;
    let old = json!({"id":"old-1","timestamp":"2026-09-30T00:00:00Z","model":"m","connection_id":"c",
        "connection_name":"C","transport":"http","status":200,"latency_ms":12,"input_tokens":1,
        "output_tokens":2,"error":null});
    let db = rusqlite::Connection::open(gw.data.join("switchyard.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute("INSERT INTO requests(value) VALUES(?)", [old.to_string()])
        .unwrap();
    drop(db);

    let rows = gw.app.store.requests(10);
    let r = rows
        .iter()
        .find(|r| r.id == "old-1")
        .expect("old row loads");
    assert_eq!((r.failovers, r.attempts.len()), (0, 0));
    assert!(r.route.is_none() && r.ttfb_ms.is_none() && r.first_token_ms.is_none());
    let api = gw.wait_for_log(1).await;
    assert_eq!(api[0]["id"], "old-1");
    assert_eq!(api[0]["attempts"], json!([]));
}

// ---------------------------------------------------------------------------------------------
// Error codes that echo credentials never reach clients, the log or the database
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reflected_credentials_in_error_codes_are_never_persisted() {
    let (_d, gw) = setup().await;
    const SHORT_KEY: &str = "k9x";
    let (_, key) = gw.create_key("k").await;
    let client_key = key.clone();
    let up = Upstream::start(
        Router::new()
            .route(
                "/long/responses",
                post(|| async {
                    json_response(400, json!({"error":{"code":PROVIDER_KEY,"type":PROVIDER_KEY,"message":"no"}}))
                }),
            )
            .route(
                "/short/responses",
                post(|| async {
                    json_response(
                        400,
                        json!({"error":{"code":SHORT_KEY,"param":SHORT_KEY,"message":format!("bad key {SHORT_KEY} here")}}),
                    )
                }),
            )
            .route(
                "/client/responses",
                post(move || {
                    let k = client_key.clone();
                    async move { json_response(400, json!({"error":{"code":k,"message":"no"}})) }
                }),
            )
            .route(
                "/account/responses",
                post(|| async {
                    json_response(400, json!({"error":{"code":"acct-42","message":"workspace acct-42"}}))
                }),
            )
            .route(
                "/stream/responses",
                post(|| async {
                    sse_response(Body::from(sse(&[
                        json!({"type":"response.created","response":{"id":"r"}}),
                        json!({"type":"error","error":{"code":PROVIDER_KEY,"message":"x"}}),
                    ])))
                }),
            )
            .route(
                "/native/responses",
                post(|| async {
                    json_response(400, json!({"error":{"code":"context_length_exceeded","message":"too long"}}))
                }),
            ),
    )
    .await;
    let base = up.base();
    gw.connection("Long", "openai", &format!("{base}/long"), &["long"])
        .await;
    gw.add_connection(json!({"name":"Short","kind":"openai","base_url":format!("{base}/short"),"models":["short"],"api_key":SHORT_KEY}))
        .await;
    gw.connection("Client", "openai", &format!("{base}/client"), &["client"])
        .await;
    gw.connection("Stream", "openai", &format!("{base}/stream"), &["stream"])
        .await;
    gw.connection("Native", "openai", &format!("{base}/native"), &["native"])
        .await;
    let account = Connection {
        id: "acct-conn".into(),
        name: "Account".into(),
        kind: "codex".into(),
        base_url: format!("{base}/account"),
        enabled: true,
        models: vec!["account".into()],
        supports_websocket: false,
        created_at: switchyard::store::now(),
        api_key: PROVIDER_KEY.into(),
        refresh_token: String::new(),
        expires_at: 0,
        account_id: "acct-42".into(),
        oauth: false,
        credential_source: "api_key".into(),
        source_path: String::new(),
        account_identity: String::new(),
    };
    gw.app
        .store
        .put("connection", &account.id, &account)
        .unwrap();

    let mut client_bodies = String::new();
    for model in ["long", "short", "client", "account", "native"] {
        let r = gw
            .post("/v1/responses", &key, json!({"model":model,"input":"x"}))
            .await;
        assert_eq!(r.status(), 400, "{model}");
        client_bodies.push_str(&r.text().await.unwrap());
    }
    let r = gw
        .post(
            "/v1/responses",
            &key,
            json!({"model":"stream","input":"x","stream":true}),
        )
        .await;
    r.bytes().await.unwrap();

    // Client-facing errors: credentials redacted, short configured key included.
    for secret in [PROVIDER_KEY, &key, "acct-42"] {
        assert!(
            !client_bodies.contains(secret),
            "{secret} leaked: {client_bodies}"
        );
    }
    assert!(
        !client_bodies.contains(&format!(" {SHORT_KEY} "))
            && !client_bodies.contains(&format!("\"{SHORT_KEY}\"")),
        "{client_bodies}"
    );
    assert!(
        client_bodies.contains("\"code\":\"context_length_exceeded\""),
        "non-secret native codes are preserved"
    );

    // Request log, API and database: labels only, never the echoed value.
    let log = gw.wait_for_log(6).await;
    let by_model = |m: &str| log.iter().find(|r| r["model"] == m).unwrap().clone();
    for m in ["long", "short", "client", "account"] {
        assert_eq!(by_model(m)["error"], "Provider rejected request", "{m}");
    }
    assert_eq!(
        by_model("stream")["error"],
        "Provider rejected request in stream"
    );
    assert_eq!(
        by_model("native")["error"],
        "Provider rejected request (context_length_exceeded)"
    );
    for r in &log {
        for a in r["attempts"].as_array().unwrap() {
            assert!(
                a["error"].is_null() || a["error"] == "request_rejected",
                "{a}"
            );
        }
    }
    let db = rusqlite::Connection::open(gw.data.join("switchyard.db")).unwrap();
    let rows: Vec<String> = db
        .prepare("SELECT value FROM requests")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let stored = rows.join("\n");
    for secret in [PROVIDER_KEY, key.as_str(), "acct-42", SHORT_KEY] {
        assert!(
            !stored.contains(secret),
            "{secret} persisted in request rows"
        );
    }
}
