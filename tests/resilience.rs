//! Multi-account resilience, stream integrity, retention and credential import tests.
//! Real gateway + real loopback mock providers; all state in tempdirs; no network.

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
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// ---------------------------------------------------------------------------------------------
// Scripted provider accounts
// ---------------------------------------------------------------------------------------------

/// Per-account behaviour: queued one-shot failures, persistent per-model failures.
#[derive(Default)]
struct Script {
    queue: VecDeque<(u16, Option<&'static str>)>,
    model_fail: HashMap<String, (u16, Option<&'static str>)>,
    served: usize,
}
type Shared = Arc<Mutex<Script>>;

fn script() -> Shared {
    Arc::new(Mutex::new(Script::default()))
}

/// An OpenAI-compatible account whose response ids are unique and name the account.
fn account(name: &'static str, s: Shared) -> Router {
    Router::new().route(
        "/responses",
        post(move |Json(v): Json<Value>| {
            let s = s.clone();
            async move {
                let model = v["model"].as_str().unwrap_or("").to_string();
                let outcome = {
                    let mut s = s.lock().unwrap();
                    match s
                        .model_fail
                        .get(&model)
                        .copied()
                        .or_else(|| s.queue.pop_front())
                    {
                        Some(f) => Err(f),
                        None => {
                            s.served += 1;
                            Ok(s.served)
                        }
                    }
                };
                match outcome {
                    Err((status, retry)) => {
                        let mut r = Response::builder()
                            .status(status)
                            .header("content-type", "application/json");
                        if let Some(retry) = retry {
                            r = r.header("retry-after", retry);
                        }
                        r.body(Body::from(r#"{"error":{"message":"nope"}}"#))
                            .unwrap()
                    }
                    Ok(n) => {
                        let id = format!("resp_{name}_{n}");
                        if v["stream"] == true {
                            sse_response(Body::from(sse(&[
                                json!({"type":"response.created","response":{"id":id}}),
                                completed(&id, name),
                            ])))
                        } else {
                            let mut r = completed(&id, name)["response"].clone();
                            r["served_by"] = json!(name);
                            json_response(200, r)
                        }
                    }
                }
            }
        }),
    )
}

/// Upstream requests that reached an account's inference endpoint (including rejected ones).
fn hits(up: &Upstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.path == "/responses")
        .count()
}

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

async fn pair(gw: &Gateway, a: &Upstream, b: &Upstream, models: &[&str]) -> (String, String) {
    (
        gw.connection("A", "openai", &a.base(), models).await,
        gw.connection("B", "openai", &b.base(), models).await,
    )
}

async fn respond(gw: &Gateway, key: &str, body: Value) -> (u16, Value) {
    let r = gw.post("/v1/responses", key, body).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn targets(ids: &[&str], model: &str) -> Value {
    json!(
        ids.iter()
            .map(|c| json!({"connection_id":c,"model":model}))
            .collect::<Vec<_>>()
    )
}

async fn dead_port() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------------------------
// Cooldowns
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cooled_account_recovers_after_short_retry_after() {
    let (_d, gw) = setup().await;
    let (sa, sb) = (script(), script());
    sa.lock().unwrap().queue.push_back((429, Some("1")));
    let a = Upstream::start(account("a", sa.clone())).await;
    let b = Upstream::start(account("b", sb)).await;
    let (ca, cb) = pair(&gw, &a, &b, &["m"]).await;
    gw.put_route("ha", "failover", targets(&[&ca, &cb], "m"))
        .await;
    let (_, key) = gw.create_key("k").await;

    let (s, v) = respond(&gw, &key, json!({"model":"ha","input":"x"})).await;
    assert_eq!((s, v["served_by"].as_str()), (200, Some("b")), "{v}");
    // While cooling, the primary is skipped rather than re-hit.
    let (s, v) = respond(&gw, &key, json!({"model":"ha","input":"x"})).await;
    assert_eq!((s, v["served_by"].as_str()), (200, Some("b")));
    assert_eq!(hits(&a), 1);

    tokio::time::sleep(Duration::from_millis(2100)).await;
    let (s, v) = respond(&gw, &key, json!({"model":"ha","input":"x"})).await;
    assert_eq!(
        (s, v["served_by"].as_str()),
        (200, Some("a")),
        "primary should be back after Retry-After"
    );
    assert_eq!(hits(&a), 2);
}

#[tokio::test]
async fn cooldown_is_isolated_per_model() {
    let (_d, gw) = setup().await;
    let (sa, sb) = (script(), script());
    sa.lock()
        .unwrap()
        .model_fail
        .insert("m1".into(), (429, Some("120")));
    let a = Upstream::start(account("a", sa)).await;
    let b = Upstream::start(account("b", sb)).await;
    let (ca, cb) = pair(&gw, &a, &b, &["m1", "m2"]).await;
    gw.put_route("one", "failover", targets(&[&ca, &cb], "m1"))
        .await;
    gw.put_route("two", "failover", targets(&[&ca, &cb], "m2"))
        .await;
    let (_, key) = gw.create_key("k").await;

    let (_, v) = respond(&gw, &key, json!({"model":"one","input":"x"})).await;
    assert_eq!(v["served_by"], "b");
    let (_, v) = respond(&gw, &key, json!({"model":"two","input":"x"})).await;
    assert_eq!(
        v["served_by"], "a",
        "a model-specific rate limit must not bench the whole account"
    );
    let (_, v) = respond(&gw, &key, json!({"model":"one","input":"x"})).await;
    assert_eq!(v["served_by"], "b");
    assert_eq!(hits(&a), 2, "m1 attempt + m2 success only");
}

#[tokio::test]
async fn rejected_auth_rotates_to_healthy_account() {
    let (_d, gw) = setup().await;
    for status in [401u16, 403] {
        let (sa, sb) = (script(), script());
        sa.lock()
            .unwrap()
            .model_fail
            .insert("m".into(), (status, None));
        let a = Upstream::start(account("a", sa)).await;
        let b = Upstream::start(account("b", sb)).await;
        let (ca, cb) = pair(&gw, &a, &b, &["m"]).await;
        let alias = format!("auth{status}");
        gw.put_route(&alias, "failover", targets(&[&ca, &cb], "m"))
            .await;
        let (_, key) = gw.create_key("k").await;
        for _ in 0..3 {
            let (s, v) = respond(&gw, &key, json!({"model":alias,"input":"x"})).await;
            assert_eq!(
                (s, v["served_by"].as_str()),
                (200, Some("b")),
                "{status}: {v}"
            );
        }
        assert_eq!(
            hits(&a),
            1,
            "{status}: rejected account is benched, not retried on every request"
        );
    }
}

#[tokio::test]
async fn all_accounts_cooling_reports_429_with_retry_after_without_upstream_calls() {
    let (_d, gw) = setup().await;
    let s = script();
    s.lock().unwrap().queue.push_back((429, Some("30")));
    let a = Upstream::start(account("a", s)).await;
    gw.connection("A", "openai", &a.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "30");

    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 429);
    let retry: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((28..=31).contains(&retry), "{retry}");
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"].as_str().unwrap().contains("cooling"),
        "{v}"
    );
    assert_eq!(v["error"]["retry_after_seconds"], retry);
    assert_eq!(hits(&a), 1, "cooling accounts are not contacted");
}

/// Fixing an account's credential is the user's recovery action; it should take effect at once.
#[tokio::test]
async fn updating_an_account_credential_clears_its_cooldown() {
    let (_d, gw) = setup().await;
    let s = script();
    s.lock().unwrap().queue.push_back((401, None));
    let a = Upstream::start(account("a", s)).await;
    let id = gw.connection("A", "openai", &a.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let (status, _) = respond(&gw, &key, json!({"model":"m","input":"x"})).await;
    assert_eq!(status, 401);

    let r = gw
        .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":"A","kind":"openai","base_url":a.base(),"models":["m"],"api_key":"sk-fixed"}))
        .await;
    assert_eq!(r.status(), 200);
    let (status, v) = respond(&gw, &key, json!({"model":"m","input":"x"})).await;
    assert_eq!(
        status, 200,
        "request after credential fix was blocked by a stale cooldown: {v}"
    );
}

// ---------------------------------------------------------------------------------------------
// previous_response_id affinity (HTTP)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn previous_response_id_sticks_to_owning_account() {
    let (_d, gw) = setup().await;
    let a = Upstream::start(account("a", script())).await;
    let b = Upstream::start(account("b", script())).await;
    pair(&gw, &a, &b, &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    // Implicit round-robin pool; whichever account answers owns the conversation.
    let (_, first) = respond(&gw, &key, json!({"model":"m","input":"x"})).await;
    let owner = first["served_by"].as_str().unwrap().to_string();
    let mut previous = first["id"].as_str().unwrap().to_string();
    for turn in 0..5 {
        let stream = turn % 2 == 1;
        let r = gw
            .post(
                "/v1/responses",
                &key,
                json!({"model":"m","input":"next","previous_response_id":previous,"stream":stream}),
            )
            .await;
        assert_eq!(r.status(), 200, "turn {turn}");
        if stream {
            let text = r.text().await.unwrap();
            let done: Value = text
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .map(|d| serde_json::from_str::<Value>(d).unwrap())
                .find(|e| e["type"] == "response.completed")
                .unwrap();
            previous = done["response"]["id"].as_str().unwrap().to_string();
        } else {
            let v: Value = r.json().await.unwrap();
            assert_eq!(
                v["served_by"],
                owner.as_str(),
                "turn {turn} switched accounts"
            );
            previous = v["id"].as_str().unwrap().to_string();
        }
        assert!(
            previous.starts_with(&format!("resp_{owner}_")),
            "turn {turn}: {previous}"
        );
    }
    let (owner_up, other_up) = if owner == "a" { (&a, &b) } else { (&b, &a) };
    assert_eq!(hits(owner_up), 6);
    assert_eq!(hits(other_up), 0);
    assert!(
        owner_up.requests()[1..]
            .iter()
            .all(|r| r.json()["previous_response_id"].is_string())
    );
}

#[tokio::test]
async fn owner_cooling_or_disabled_returns_409_instead_of_switching() {
    let (_d, gw) = setup().await;
    let (sa, sb) = (script(), script());
    let a = Upstream::start(account("a", sa.clone())).await;
    let b = Upstream::start(account("b", sb)).await;
    let (ca, cb) = pair(&gw, &a, &b, &["m"]).await;
    gw.put_route("ha", "failover", targets(&[&ca, &cb], "m"))
        .await;
    let (_, key) = gw.create_key("k").await;

    let (_, v) = respond(&gw, &key, json!({"model":"ha","input":"x"})).await;
    assert_eq!(v["served_by"], "a");
    let owned = v["id"].as_str().unwrap().to_string();

    // A goes into cooldown; new conversations fail over to B.
    sa.lock().unwrap().queue.push_back((429, Some("60")));
    let (_, v) = respond(&gw, &key, json!({"model":"ha","input":"x"})).await;
    assert_eq!(v["served_by"], "b");
    let (a_hits, b_hits) = (hits(&a), hits(&b));

    let (status, v) = respond(
        &gw,
        &key,
        json!({"model":"ha","input":"x","previous_response_id":owned}),
    )
    .await;
    assert_eq!(status, 409, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("previous_response_id")
    );
    assert_eq!(
        (hits(&a), hits(&b)),
        (a_hits, b_hits),
        "neither account may be tried"
    );

    // Disabled owner: same refusal.
    let r = gw
        .admin_send(
            Method::PUT,
            &format!("/api/connections/{ca}"),
            json!({"name":"A","kind":"openai","base_url":a.base(),"models":["m"],"enabled":false}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let (status, _) = respond(
        &gw,
        &key,
        json!({"model":"ha","input":"x","previous_response_id":owned}),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(hits(&b), b_hits);
}

#[tokio::test]
async fn unknown_previous_response_id_is_refused_only_when_ambiguous() {
    let (_d, gw) = setup().await;
    let a = Upstream::start(account("a", script())).await;
    let b = Upstream::start(account("b", script())).await;
    let solo = Upstream::start(account("solo", script())).await;
    pair(&gw, &a, &b, &["pooled"]).await;
    gw.connection("Solo", "openai", &solo.base(), &["single"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let (status, v) = respond(
        &gw,
        &key,
        json!({"model":"pooled","input":"x","previous_response_id":"resp_from_elsewhere"}),
    )
    .await;
    assert_eq!(status, 409);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown previous_response_id"),
        "{v}"
    );
    assert_eq!(hits(&a) + hits(&b), 0);

    let (status, _) = respond(
        &gw,
        &key,
        json!({"model":"single","input":"x","previous_response_id":"resp_from_elsewhere"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "a single account is unambiguous; let the provider decide"
    );
    assert_eq!(
        solo.requests()[0].json()["previous_response_id"],
        "resp_from_elsewhere"
    );
}

// ---------------------------------------------------------------------------------------------
// WebSocket handshake failover and affinity
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct WsLog {
    connections: AtomicUsize,
    frames: Mutex<Vec<Value>>,
}

/// A Responses WebSocket account. Ids are `resp_{prefix}_{n}`.
fn ws_account(prefix: &'static str, log: Arc<WsLog>) -> Router {
    Router::new().route(
        "/responses",
        get(move |ws: WebSocketUpgrade| {
            let log = log.clone();
            async move {
                ws.on_upgrade(move |mut socket| async move {
                    log.connections.fetch_add(1, Ordering::SeqCst);
                    while let Some(Ok(AxMessage::Text(t))) = socket.recv().await {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        let n = {
                            let mut f = log.frames.lock().unwrap();
                            f.push(v.clone());
                            f.len()
                        };
                        let id = format!("resp_{prefix}_{n}");
                        for e in [
                            json!({"type":"response.created","response":{"id":id}}),
                            completed(&id, prefix),
                        ] {
                            let _ = socket.send(AxMessage::Text(e.to_string().into())).await;
                        }
                    }
                })
            }
        }),
    )
}

/// An account whose WebSocket handshake is rejected (e.g. revoked token or rate limited).
fn ws_rejecting(status: u16) -> Router {
    Router::new().route(
        "/responses",
        get(move || async move {
            Response::builder()
                .status(status)
                .header("retry-after", "60")
                .body(Body::empty())
                .unwrap()
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

async fn next_json(ws: &mut Client) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("WS frame within 5s")
        {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

async fn ws_turn(ws: &mut Client, frame: Value) -> Value {
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

async fn ws_account_conn(gw: &Gateway, name: &str, base: &str, models: &[&str]) -> String {
    gw.add_connection(json!({"name":name,"kind":"openai","base_url":base,"models":models,"supports_websocket":true,"api_key":PROVIDER_KEY}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn websocket_handshake_fails_over_before_any_inference_frame() {
    let (_d, gw) = setup().await;
    let rejecting = Upstream::start(ws_rejecting(401)).await;
    let good_log = Arc::new(WsLog::default());
    let good = Upstream::start(ws_account("good", good_log.clone())).await;
    let c_rej = ws_account_conn(&gw, "Rejecting", &rejecting.base(), &["m"]).await;
    let c_dead = ws_account_conn(&gw, "Dead", &dead_port().await, &["m"]).await;
    let c_good = ws_account_conn(&gw, "Good", &good.base(), &["m"]).await;
    gw.put_route(
        "ws-ha",
        "failover",
        targets(&[&c_rej, &c_dead, &c_good], "m"),
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    let mut ws = ws_connect(&gw, &key).await;
    let done = ws_turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-ha","input":"x"}),
    )
    .await;
    assert_eq!(done["type"], "response.completed", "{done}");
    assert_eq!(done["response"]["id"], "resp_good_1");
    // The rejecting account saw a GET handshake only; no inference frame was ever sent to it.
    assert_eq!(rejecting.requests().len(), 1);
    assert_eq!(rejecting.requests()[0].method, "GET");
    assert_eq!(good_log.frames.lock().unwrap().len(), 1);
    ws.close(None).await.unwrap();
    drop(ws);
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["connection_name"], "Good");
    assert_eq!(log[0]["status"], 200);

    // Failed accounts are cooling: the next session goes straight to the healthy one.
    let mut ws = ws_connect(&gw, &key).await;
    let done = ws_turn(
        &mut ws,
        json!({"type":"response.create","model":"ws-ha","input":"x"}),
    )
    .await;
    assert_eq!(done["type"], "response.completed");
    assert_eq!(rejecting.requests().len(), 1);
    assert_eq!(good_log.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn websocket_all_handshakes_failing_is_reported() {
    let (_d, gw) = setup().await;
    let rejecting = Upstream::start(ws_rejecting(403)).await;
    ws_account_conn(&gw, "Rejecting", &rejecting.base(), &["m"]).await;
    ws_account_conn(&gw, "Dead", &dead_port().await, &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    let v = ws_turn(
        &mut ws,
        json!({"type":"response.create","model":"m","input":"x"}),
    )
    .await;
    assert_eq!(v["type"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("handshake"),
        "{v}"
    );
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 502);
}

/// A client reconnecting with `previous_response_id` in its first frame must land on the
/// account that owns that response, exactly like the HTTP path.
#[tokio::test]
async fn websocket_follow_up_session_returns_to_owning_account() {
    let (_d, gw) = setup().await;
    let (la, lb) = (Arc::new(WsLog::default()), Arc::new(WsLog::default()));
    let a = Upstream::start(ws_account("a", la.clone())).await;
    let b = Upstream::start(ws_account("b", lb.clone())).await;
    ws_account_conn(&gw, "A", &a.base(), &["m"]).await;
    ws_account_conn(&gw, "B", &b.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    let mut ws = ws_connect(&gw, &key).await;
    let done = ws_turn(
        &mut ws,
        json!({"type":"response.create","model":"m","input":"x"}),
    )
    .await;
    let mut previous = done["response"]["id"].as_str().unwrap().to_string();
    let owner = if previous.starts_with("resp_a_") {
        "a"
    } else {
        "b"
    };
    ws.close(None).await.unwrap();
    drop(ws);

    for session in 0..4 {
        let mut ws = ws_connect(&gw, &key).await;
        let done = ws_turn(&mut ws, json!({"type":"response.create","model":"m","input":"again","previous_response_id":previous})).await;
        assert_eq!(
            done["type"], "response.completed",
            "session {session}: {done}"
        );
        previous = done["response"]["id"].as_str().unwrap().to_string();
        assert!(
            previous.starts_with(&format!("resp_{owner}_")),
            "session {session} moved to another account: {previous}"
        );
        ws.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn websocket_unknown_previous_response_id_in_pool_is_refused() {
    let (_d, gw) = setup().await;
    let (la, lb) = (Arc::new(WsLog::default()), Arc::new(WsLog::default()));
    let a = Upstream::start(ws_account("a", la.clone())).await;
    let b = Upstream::start(ws_account("b", lb.clone())).await;
    ws_account_conn(&gw, "A", &a.base(), &["m"]).await;
    ws_account_conn(&gw, "B", &b.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = ws_connect(&gw, &key).await;
    let v = ws_turn(&mut ws, json!({"type":"response.create","model":"m","input":"x","previous_response_id":"resp_unknown"})).await;
    assert_eq!(
        v["type"], "error",
        "ambiguous continuation forwarded to a guessed account: {v}"
    );
    assert_eq!(
        la.frames.lock().unwrap().len() + lb.frames.lock().unwrap().len(),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// Stream integrity: no false 200s
// ---------------------------------------------------------------------------------------------

/// Serve one canned raw HTTP response per connection, then hang up (possibly mid-body).
async fn raw_upstream(response: impl Into<Vec<u8>>) -> String {
    let response: &'static [u8] = Box::leak(response.into().into_boxed_slice());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Read headers and the declared body before answering.
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = s.write_all(response).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(s);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

async fn stream_case(gw: &Gateway, key: &str, path: &str, body: Value) -> (u16, String, Value) {
    let r = gw.post(path, key, body).await;
    let status = r.status().as_u16();
    let text = r
        .text()
        .await
        .unwrap_or_else(|e| format!("<body error: {e}>"));
    let log = gw.wait_for_log(1).await;
    (status, text, log[0].clone())
}

#[tokio::test]
async fn sse_clean_eof_without_completion_is_flagged() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(|| async {
            sse_response(Body::from(sse(&[
                json!({"type":"response.created","response":{"id":"r"}}),
                json!({"type":"response.output_text.delta","delta":"half an ans"}),
            ])))
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let (status, text, rec) = stream_case(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        text.contains("half an ans"),
        "partial output still delivered"
    );
    assert!(
        text.trim_end().ends_with('}')
            && text.contains("event: error")
            && text.contains("partial_output"),
        "{text}"
    );
    assert_eq!(rec["status"], 502, "{rec}");
}

/// Parse SSE as a client would (WHATWG rules): blank line dispatches, data lines join with \n.
fn sse_event_data(text: &str) -> Vec<String> {
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

/// Provider TCP connection dies in the middle of an event line.
#[tokio::test]
async fn sse_abrupt_connection_drop_is_flagged() {
    let (_d, gw) = setup().await;
    let whole = format!(
        "event: x\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"hi"})
    );
    let partial = "event: x\ndata: {\"type\":\"response.output_text.delta\",\"del";
    let mut raw =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec();
    for chunk in [whole.as_str(), partial] {
        raw.extend_from_slice(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes());
    }
    let base = raw_upstream(raw).await;
    gw.connection("A", "openai", &base, &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let (status, text, rec) = stream_case(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(rec["status"], 502, "{rec}");
    let events = sse_event_data(&text);
    assert_eq!(
        serde_json::from_str::<Value>(&events[0]).unwrap()["delta"],
        "hi",
        "{text}"
    );
    let last = events.last().unwrap();
    let parsed: Value = serde_json::from_str(last).unwrap_or_else(|_| {
        panic!("gateway error event is not parseable SSE; it was glued onto the cut line:\n{text}")
    });
    assert_eq!(parsed["error"]["type"], "upstream_interrupted");
}

#[tokio::test]
async fn sse_upstream_error_event_is_logged_as_failure() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(|| async {
            sse_response(Body::from(sse(&[
                json!({"type":"response.created","response":{"id":"r"}}),
                json!({"type":"error","error":{"message":"server_error"}}),
            ])))
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let (_, text, rec) = stream_case(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert!(text.contains("server_error"));
    assert_eq!(rec["status"], 502);
}

#[tokio::test]
async fn sse_stall_beyond_read_timeout_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 4, 1).await;
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(|| async {
            let s = async_stream::stream! {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(&[json!({"type":"response.created","response":{"id":"r"}})])));
                std::future::pending::<()>().await;
            };
            sse_response(Body::from_stream(s))
        }),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let started = std::time::Instant::now();
    let (_, text, rec) = stream_case(
        &gw,
        &key,
        "/v1/responses",
        json!({"model":"m","input":"x","stream":true}),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "stall not detected promptly"
    );
    assert!(text.contains("upstream_interrupted"), "{text}");
    assert_eq!(rec["status"], 502);
}

#[tokio::test]
async fn completion_markers_per_protocol_decide_success() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(
        Router::new()
            .route("/ok/chat/completions", post(|| async { sse_response(Body::from("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n")) }))
            .route("/cut/chat/completions", post(|| async { sse_response(Body::from("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")) }))
            .route(
                "/ok/messages",
                post(|| async {
                    sse_response(Body::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"))
                }),
            )
            .route(
                "/cut/messages",
                post(|| async { sse_response(Body::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n")) }),
            ),
    )
    .await;
    gw.connection("OK", "openai", &format!("{}/ok", up.base()), &["chat-ok"])
        .await;
    gw.connection(
        "Cut",
        "openai",
        &format!("{}/cut", up.base()),
        &["chat-cut"],
    )
    .await;
    gw.connection(
        "OKA",
        "anthropic",
        &format!("{}/ok", up.base()),
        &["claude-ok"],
    )
    .await;
    gw.connection(
        "CutA",
        "anthropic",
        &format!("{}/cut", up.base()),
        &["claude-cut"],
    )
    .await;
    let (_, key) = gw.create_key("k").await;

    let cases = [
        ("/v1/chat/completions", "chat-ok", 200),
        ("/v1/chat/completions", "chat-cut", 502),
        ("/v1/messages", "claude-ok", 200),
        ("/v1/messages", "claude-cut", 502),
    ];
    for (i, (path, model, expected)) in cases.into_iter().enumerate() {
        let body = json!({"model":model,"stream":true,"max_tokens":5,"messages":[{"role":"user","content":"x"}]});
        let r = gw.post(path, &key, body).await;
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
        let log = gw.wait_for_log(i + 1).await;
        assert_eq!(log[0]["model"], model);
        assert_eq!(log[0]["status"], expected, "{model}: {}", log[0]);
    }
}

/// Gemini streams end with a candidate `finishReason`; a stream cut before that is partial.
#[tokio::test]
async fn gemini_stream_cut_before_finish_reason_is_not_logged_as_success() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(Router::new().route(
        "/models/{action}",
        post(|| async {
            sse_response(Body::from(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            ))
        }),
    ))
    .await;
    gw.connection("G", "gemini", &up.base(), &["gem"]).await;
    let (_, key) = gw.create_key("k").await;
    let (_, _, rec) = stream_case(
        &gw,
        &key,
        "/v1beta/models/gem:streamGenerateContent",
        json!({"contents":[]}),
    )
    .await;
    assert_ne!(
        rec["status"], 200,
        "truncated Gemini stream logged as success: {rec}"
    );
}

#[tokio::test]
async fn truncated_non_streaming_body_is_a_502() {
    let (_d, gw) = setup().await;
    let base = raw_upstream(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 400\r\n\r\n{\"id\":\"resp_1\",\"output\":[").await;
    gw.connection("A", "openai", &base, &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    assert_eq!(r.status(), 502);
    let log = gw.wait_for_log(1).await;
    assert_eq!(log[0]["status"], 502);
    assert!(
        log[0]["error"].as_str().unwrap().contains("interrupted"),
        "{}",
        log[0]
    );
}

// ---------------------------------------------------------------------------------------------
// Cancellation under a small in-flight limit
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn abandoned_non_streaming_request_frees_the_only_slot() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start_with(dir.path(), 1, 60).await;
    let released = Arc::new(AtomicBool::new(false));
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let flag = released.clone();
    let up = Upstream::start(account("fast", script()).route(
        "/hang/responses",
        post(move || {
            let flag = flag.clone();
            async move {
                let _guard = OnDrop(flag);
                std::future::pending::<()>().await;
                "never"
            }
        }),
    ))
    .await;
    gw.connection("Hang", "openai", &format!("{}/hang", up.base()), &["hang"])
        .await;
    gw.connection("Fast", "openai", &up.base(), &["fast"]).await;
    let (_, key) = gw.create_key("k").await;

    let impatient = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(400))
        .build()
        .unwrap();
    let err = impatient
        .post(gw.url("/v1/responses"))
        .bearer_auth(&key)
        .json(&json!({"model":"hang","input":"x"}))
        .send()
        .await
        .unwrap_err();
    assert!(err.is_timeout());

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
    assert_eq!(status, 200, "slot leaked after the client gave up");
    let log = gw.wait_for_log(2).await;
    let hang = log.iter().find(|r| r["model"] == "hang").unwrap();
    assert_eq!(hang["status"], 499, "{hang}");
    for _ in 0..100 {
        if released.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        released.load(Ordering::SeqCst),
        "upstream request should be abandoned with the client"
    );
    assert_eq!(gw.admin_json("/api/overview").await["active_requests"], 0);
}

// ---------------------------------------------------------------------------------------------
// Request history retention and lifetime counters
// ---------------------------------------------------------------------------------------------

async fn check_history(gw: &Gateway) {
    let history = gw.admin_json("/api/requests?limit=5000").await;
    let history = history.as_array().unwrap();
    assert_eq!(history.len(), 1000);
    assert_eq!(history[0]["status"], 400, "newest first");
    assert_eq!(history[1]["status"], 200);
    assert_eq!(history[999]["id"], "seed-5", "oldest five seeds pruned");
    let o = gw.admin_json("/api/overview").await;
    assert_eq!(o["requests_total"], 1005);
    // Seeds: 101 failures (every 10th of 0..1003); real traffic: one 200, one 400.
    assert_eq!(o["requests_failed"], 102);
    assert_eq!(o["requests_success"], 1005 - 102);
    assert_eq!(o["transport_counts"]["http"], 502 + 2);
    assert_eq!(o["transport_counts"]["sse"], 501);
}

#[tokio::test]
async fn request_history_keeps_last_1000_while_counters_are_lifetime() {
    let (dir, gw) = setup().await;
    let s = script();
    let up = Upstream::start(account("a", s.clone())).await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;

    // Seed history directly through the store, then add real traffic on top.
    for i in 0..1003u64 {
        let r = switchyard::store::RequestRecord {
            id: format!("seed-{i}"),
            timestamp: switchyard::store::now(),
            model: "seed".into(),
            connection_id: "c".into(),
            connection_name: "c".into(),
            transport: if i % 2 == 0 {
                "http".into()
            } else {
                "sse".into()
            },
            status: if i % 10 == 0 { 500 } else { 200 },
            latency_ms: i,
            input_tokens: None,
            output_tokens: None,
            error: None,
        };
        gw.app.store.record(&r).unwrap();
    }
    assert_eq!(
        respond(&gw, &key, json!({"model":"m","input":"x"})).await.0,
        200
    );
    s.lock().unwrap().queue.push_back((400, None));
    assert_eq!(
        respond(&gw, &key, json!({"model":"m","input":"x"})).await.0,
        400
    );
    gw.wait_for_log(1000).await;
    for _ in 0..100 {
        if gw.admin_json("/api/overview").await["requests_total"] == 1005 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    check_history(&gw).await;
    let gw = gw.restart().await;
    check_history(&gw).await;

    let db = rusqlite::Connection::open(dir.path().join("switchyard.db")).unwrap();
    let rows: i64 = db
        .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1000, "history table itself is bounded");
}

// ---------------------------------------------------------------------------------------------
// Credential import from local CLI auth files (fake files only)
// ---------------------------------------------------------------------------------------------

fn jwt(payload: Value) -> String {
    use base64::Engine;
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}

fn snapshot(p: &Path) -> (Vec<u8>, std::time::SystemTime, u32) {
    let m = std::fs::metadata(p).unwrap();
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    };
    #[cfg(not(unix))]
    let permissions = u32::from(m.permissions().readonly());
    (
        std::fs::read(p).unwrap(),
        m.modified().unwrap(),
        permissions,
    )
}

async fn import(gw: &Gateway, source: &str, path: &Path) -> reqwest::Response {
    gw.admin_send(
        Method::POST,
        "/api/import",
        json!({"source":source,"path":path}),
    )
    .await
}

fn stored(gw: &Gateway, id: &str) -> switchyard::store::Connection {
    gw.app.store.get("connection", id).unwrap()
}

#[tokio::test]
async fn codex_import_is_idempotent_and_leaves_the_source_untouched() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let exp = chrono::Utc::now().timestamp() + 7200;
    let access = jwt(json!({"exp":exp,"sub":"user"}));
    let file = src.path().join("auth.json");
    std::fs::write(
        &file,
        json!({"OPENAI_API_KEY":null,"tokens":{"id_token":"id","access_token":access,"refresh_token":"rt-fake","account_id":"acct-123"},"last_refresh":"2026-10-01T00:00:00Z"}).to_string(),
    )
    .unwrap();
    let before = snapshot(&file);

    let r = gw
        .http
        .post(gw.url("/api/import"))
        .json(&json!({"source":"codex","path":file}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "import is an admin operation");

    let r = import(&gw, "codex", &file).await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(
        !text.contains(&access) && !text.contains("rt-fake"),
        "import response leaked tokens: {text}"
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["imported"], 1);
    let c = &v["connections"][0];
    assert_eq!(c["kind"], "codex");
    assert_eq!(c["supports_websocket"], true);
    assert_eq!(c["credential_present"], true);
    let id = c["id"].as_str().unwrap().to_string();
    let s = stored(&gw, &id);
    assert_eq!(
        (s.account_id.as_str(), s.expires_at, s.oauth),
        ("acct-123", exp, true)
    );

    // Re-import: same identity, no duplicate, user model edits kept.
    let r = gw
        .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":"Codex","kind":"codex","base_url":"https://chatgpt.com/backend-api/codex","models":["gpt-custom"],"supports_websocket":true}))
        .await;
    assert_eq!(r.status(), 200);
    let r = import(&gw, "codex", &file).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["connections"][0]["id"], id.as_str());
    assert_eq!(
        gw.admin_json("/api/connections")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(stored(&gw, &id).models, vec!["gpt-custom".to_string()]);

    assert_eq!(
        snapshot(&file),
        before,
        "source auth file must not be modified"
    );
    assert_eq!(
        std::fs::read_dir(src.path()).unwrap().count(),
        1,
        "no files written next to the source"
    );
}

#[tokio::test]
async fn reimport_preserves_user_disabled_state_and_name() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    std::fs::write(&file, json!({"tokens":{"access_token":jwt(json!({"exp":chrono::Utc::now().timestamp()+7200})),"account_id":"acct-9"}}).to_string()).unwrap();
    let v: Value = import(&gw, "codex", &file).await.json().await.unwrap();
    let id = v["connections"][0]["id"].as_str().unwrap().to_string();
    let r = gw
        .admin_send(Method::PUT, &format!("/api/connections/{id}"), json!({"name":"Work account","kind":"codex","base_url":"https://chatgpt.com/backend-api/codex","models":["gpt-6.1-sol"],"enabled":false,"supports_websocket":true}))
        .await;
    assert_eq!(r.status(), 200);
    import(&gw, "codex", &file).await;
    let after = stored(&gw, &id);
    assert!(
        !after.enabled,
        "re-import silently re-enabled an account the user disabled"
    );
    assert_eq!(
        after.name, "Work account",
        "re-import overwrote the user's account name"
    );
}

#[tokio::test]
async fn codex_api_key_and_claude_files_import_as_expected() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let key_file = src.path().join("codex-key.json");
    std::fs::write(
        &key_file,
        json!({"OPENAI_API_KEY":"sk-fake-openai"}).to_string(),
    )
    .unwrap();
    let v: Value = import(&gw, "codex", &key_file).await.json().await.unwrap();
    assert_eq!(v["connections"][0]["kind"], "openai");
    assert_eq!(v["connections"][0]["supports_websocket"], false);
    assert!(!stored(&gw, v["connections"][0]["id"].as_str().unwrap()).oauth);

    let claude = src.path().join(".credentials.json");
    let exp_ms = (chrono::Utc::now().timestamp() + 3600) * 1000;
    std::fs::write(&claude, json!({"claudeAiOauth":{"accessToken":"sk-ant-oat-fake","refreshToken":"sk-ant-ort-fake","expiresAt":exp_ms,"scopes":["user:inference"]}}).to_string()).unwrap();
    let before = snapshot(&claude);
    let v: Value = import(&gw, "claude", &claude).await.json().await.unwrap();
    let c = &v["connections"][0];
    assert_eq!(c["kind"], "anthropic");
    let s = stored(&gw, c["id"].as_str().unwrap());
    assert!(s.oauth);
    assert_eq!(s.expires_at, exp_ms / 1000);
    let again: Value = import(&gw, "claude", &claude).await.json().await.unwrap();
    assert_eq!(again["connections"][0]["id"], c["id"]);
    assert_eq!(
        gw.admin_json("/api/connections")
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(snapshot(&claude), before);
}

#[tokio::test]
async fn cliproxy_directory_import_picks_supported_accounts_only() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let p = src.path();
    std::fs::write(p.join("codex-a.json"), json!({"type":"codex","email":"a@example.com","access_token":"tok-a","account_id":"acct-a"}).to_string()).unwrap();
    std::fs::write(
        p.join("claude-b.json"),
        json!({"type":"claude","email":"b@example.com","access_token":"tok-b"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        p.join("gemini-c.json"),
        json!({"type":"gemini","email":"c@example.com","access_token":"tok-c"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        p.join("empty.json"),
        json!({"type":"codex","email":"d@example.com"}).to_string(),
    )
    .unwrap();
    std::fs::write(p.join("notes.txt"), "not json").unwrap();
    let before: Vec<_> = ["codex-a.json", "claude-b.json"]
        .iter()
        .map(|f| snapshot(&p.join(f)))
        .collect();

    let v: Value = import(&gw, "cliproxy", p).await.json().await.unwrap();
    assert_eq!(v["imported"], 2, "{v}");
    let mut names: Vec<_> = v["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("a@example.com".into(), "codex".into()),
            ("b@example.com".into(), "anthropic".into())
        ]
    );
    let v: Value = import(&gw, "cliproxy", p).await.json().await.unwrap();
    assert_eq!(v["imported"], 2);
    assert_eq!(
        gw.admin_json("/api/connections")
            .await
            .as_array()
            .unwrap()
            .len(),
        2,
        "directory re-import is idempotent"
    );
    let after: Vec<_> = ["codex-a.json", "claude-b.json"]
        .iter()
        .map(|f| snapshot(&p.join(f)))
        .collect();
    assert_eq!(before, after);
}

#[tokio::test]
async fn import_errors_are_clear_and_side_effect_free() {
    let (_d, gw) = setup().await;
    let src = tempfile::tempdir().unwrap();
    let p = src.path();
    std::fs::write(p.join("bad.json"), "{not json").unwrap();
    std::fs::write(p.join("big.json"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    std::fs::write(p.join("nothing.json"), json!({"tokens":{}}).to_string()).unwrap();
    let many = p.join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..102 {
        std::fs::write(many.join(format!("{i}.json")), "{}").unwrap();
    }
    let cases: [(&str, std::path::PathBuf, &str); 6] = [
        ("codex", p.join("missing.json"), "not found"),
        ("codex", p.join("bad.json"), "Invalid credential JSON"),
        ("codex", p.join("big.json"), "1 MiB"),
        ("codex", p.join("nothing.json"), "No Codex access token"),
        ("cliproxy", many.clone(), "at most 100"),
        ("chrome", p.join("bad.json"), "Supported imports"),
    ];
    for (source, path, needle) in cases {
        let r = import(&gw, source, &path).await;
        assert_eq!(r.status(), 400, "{source} {path:?}");
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"]["message"].as_str().unwrap().contains(needle),
            "{source} {path:?}: {v}"
        );
    }
    let r = gw
        .admin_send(Method::POST, "/api/import", json!({"source":"cliproxy"}))
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(gw.admin_json("/api/connections").await, json!([]));
}
