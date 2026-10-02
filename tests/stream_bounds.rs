//! Cumulative output must stay bounded even when every SSE event is individually valid.
mod support;
use axum::{Router, routing::post};
use serde_json::{Value, json};
use support::*;
#[tokio::test]
async fn nonstream_codex_assembly_has_a_cumulative_response_limit() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::start(dir.path()).await;
    let text = "a".repeat(1024 * 1024);
    let mut events = Vec::new();
    for index in 0..17 {
        events.push(json!({"type":"response.output_item.done","output_index":index,"item":{"id":format!("msg_{index}"),"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}}));
    }
    events.push(json!({"type":"response.completed","response":{"id":"resp_large","status":"completed","output":[]}}));
    let body = sse(&events);
    let upstream = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(body.into()) }
        }),
    ))
    .await;
    gateway
        .connection("Codex", "codex", &upstream.base(), &["m"])
        .await;
    let (_, key) = gateway.create_key("test").await;
    let response = gateway
        .post(
            "/v1/responses",
            &key,
            json!({"model":"m","input":"test","stream":false}),
        )
        .await;
    assert_eq!(
        response.status(),
        502,
        "Assembly must enforce the documented 16 MiB nonstream response limit across completed items"
    );
    let body: Value = response.json().await.unwrap();
    assert!(body["error"]["message"].is_string());
    let records = gateway.wait_for_log(1).await;
    assert_eq!(records[0]["status"], 502);
}

// ---------------------------------------------------------------------------------------------
// Streaming Codex output accumulation (SSE and WebSocket)
// ---------------------------------------------------------------------------------------------

const MIB: usize = 1024 * 1024;

fn item_done(index: u64, id: &str, text_len: usize) -> Value {
    json!({"type":"response.output_item.done","output_index":index,"item":{"id":id,"type":"message",
        "role":"assistant","content":[{"type":"output_text","text":"a".repeat(text_len)}]}})
}
fn created(id: &str) -> Value {
    json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}})
}
fn terminal(id: &str, instructions_len: usize) -> Value {
    json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[],
        "instructions":"i".repeat(instructions_len),"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
}

/// Two Codex accounts behind one route, both serving `body`; an unknown
/// `previous_response_id` is refused in such a pool, which proves no affinity was recorded.
/// Returns the client key and the mock provider, which must stay alive for the test.
async fn codex_pool(gateway: &Gateway, body: String) -> (String, Upstream) {
    let upstream = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(body.into()) }
        }),
    ))
    .await;
    let a = gateway
        .connection("A", "codex", &upstream.base(), &["m"])
        .await;
    let b = gateway
        .connection("B", "codex", &upstream.base(), &["m"])
        .await;
    gateway
        .put_route(
            "pool",
            "round_robin",
            json!([{"connection_id":a,"model":"m"},{"connection_id":b,"model":"m"}]),
        )
        .await;
    (gateway.create_key("test").await.1, upstream)
}

async fn stream_text(gateway: &Gateway, key: &str) -> String {
    gateway
        .post(
            "/v1/responses",
            key,
            json!({"model":"pool","input":"test","stream":true}),
        )
        .await
        .text()
        .await
        .unwrap()
}

fn last_event(text: &str) -> Value {
    let block = text
        .split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .last()
        .unwrap();
    let data = block
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

async fn assert_no_affinity(gateway: &Gateway, key: &str, response_id: &str) {
    let r = gateway
        .post(
            "/v1/responses",
            key,
            json!({"model":"pool","input":"next","previous_response_id":response_id}),
        )
        .await;
    assert_eq!(
        r.status(),
        409,
        "a failed response must not become routable via previous_response_id"
    );
}

fn assert_overflow(event: &Value, needle: &str) {
    assert_eq!(event["type"], "error", "{event}");
    let e = &event["error"];
    assert_eq!(e["type"], "output_too_large");
    assert_eq!(e["retryable"], false);
    assert_eq!(e["partial_output"], true);
    assert!(e["message"].as_str().unwrap().contains(needle), "{e}");
}

#[tokio::test]
async fn streamed_codex_accumulation_over_16_mib_fails_visibly() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::start(dir.path()).await;
    let mut events = vec![created("resp_big")];
    for i in 0..17 {
        events.push(item_done(i, &format!("msg_{i}"), MIB));
    }
    events.push(terminal("resp_big", 0));
    let body = sse(&events);
    let (key, _upstream) = codex_pool(&gateway, body.clone()).await;

    let text = stream_text(&gateway, &key).await;
    // Everything before the item that crossed the bound is forwarded byte for byte.
    let forwarded: String = sse(&events[..16]);
    assert!(text.starts_with(&forwarded), "earlier events unchanged");
    assert!(!text.contains("response.completed"), "no success terminal");
    assert_overflow(&last_event(&text), "16 MiB or 4096 items");
    let record = &gateway.wait_for_log(1).await[0];
    assert_eq!(record["status"], 502);
    assert_eq!(record["error"], "Codex output exceeds 16 MiB or 4096 items");
    assert_no_affinity(&gateway, &key, "resp_big").await;
}

#[tokio::test]
async fn streamed_codex_terminal_that_cannot_be_encoded_fails_visibly() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::start(dir.path()).await;
    // 15 MiB of items is within the accumulation bound, but the filled terminal (with a 1.5 MiB
    // field elsewhere in the response) would exceed the 16 MiB event bound.
    let mut events = vec![created("resp_wide")];
    for i in 0..15 {
        events.push(item_done(i, &format!("msg_{i}"), MIB));
    }
    events.push(terminal("resp_wide", 3 * MIB / 2));
    let body = sse(&events);
    let (key, _upstream) = codex_pool(&gateway, body).await;

    let text = stream_text(&gateway, &key).await;
    assert!(text.starts_with(&sse(&events[..16])), "items all forwarded");
    assert!(
        !text.contains("event: response.completed"),
        "no terminal claiming success with output: []"
    );
    assert_overflow(&last_event(&text), "final response");
    let record = &gateway.wait_for_log(1).await[0];
    assert_eq!(record["status"], 502);
    assert_eq!(record["error"], "Codex final response exceeds 16 MiB");
    assert_no_affinity(&gateway, &key, "resp_wide").await;
}

#[tokio::test]
async fn streamed_codex_item_count_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::start(dir.path()).await;
    let mut events = vec![created("resp_many")];
    for i in 0..4097 {
        events.push(item_done(i, &format!("m{i}"), 1));
    }
    events.push(terminal("resp_many", 0));
    let (key, _upstream) = codex_pool(&gateway, sse(&events)).await;
    let text = stream_text(&gateway, &key).await;
    assert_overflow(&last_event(&text), "4096 items");
    assert!(!text.contains("response.completed"));
    assert_eq!(gateway.wait_for_log(1).await[0]["status"], 502);
}

#[tokio::test]
async fn replaced_items_at_the_same_index_are_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::start(dir.path()).await;
    // 20 MiB observed in total, but each event replaces index 0, so only 1 MiB is held.
    let mut events = vec![created("resp_dup")];
    for i in 0..20 {
        events.push(item_done(0, &format!("v{i}"), MIB));
    }
    events.push(terminal("resp_dup", 0));
    let (key, _upstream) = codex_pool(&gateway, sse(&events)).await;
    let text = stream_text(&gateway, &key).await;
    let last = last_event(&text);
    assert_eq!(last["type"], "response.completed", "no false overflow");
    let output = last["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["id"], "v19", "the latest item at the index wins");
    assert_eq!(gateway.wait_for_log(1).await[0]["status"], 200);
}

// ---------------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------------

mod ws {
    use super::*;
    use axum::{
        extract::ws::{Message as AxMessage, WebSocketUpgrade},
        response::IntoResponse,
        routing::get,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    fn upstream(frames: Vec<Value>) -> Router {
        Router::new().route(
            "/responses",
            get(move |ws: WebSocketUpgrade| {
                let frames = frames.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxMessage::Text(_))) = socket.recv().await {
                            for f in &frames {
                                if socket
                                    .send(AxMessage::Text(f.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    })
                    .into_response()
                }
            }),
        )
    }

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn session(gateway: &Gateway, key: &str) -> Client {
        let mut req = gateway
            .ws_url("/v1/responses")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    /// Sends one `response.create` and collects text frames until the socket closes.
    async fn run(ws: &mut Client, extra: Value) -> Vec<Value> {
        let mut create = json!({"type":"response.create","model":"pool","input":"x"});
        for (k, v) in extra.as_object().unwrap() {
            create[k] = v.clone();
        }
        ws.send(Message::Text(create.to_string().into()))
            .await
            .unwrap();
        let mut frames = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .expect("frame or close within 10 s")
            {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let done = v["type"] == "response.completed" || v["type"] == "error";
                    frames.push(v);
                    if done && frames.last().unwrap()["type"] == "response.completed" {
                        return frames;
                    }
                }
                Some(Ok(_)) => {}
                None | Some(Err(_)) => return frames,
            }
        }
    }

    async fn pool(gateway: &Gateway, frames: Vec<Value>) -> (String, Upstream) {
        let up = Upstream::start(upstream(frames)).await;
        let mut ids = Vec::new();
        for name in ["A", "B"] {
            let c = gateway
                .add_connection(json!({"name":name,"kind":"codex","base_url":up.base(),
                "models":["m"],"supports_websocket":true,"api_key":PROVIDER_KEY}))
                .await;
            ids.push(c["id"].as_str().unwrap().to_string());
        }
        gateway
            .put_route(
                "pool",
                "round_robin",
                json!([{"connection_id":ids[0],"model":"m"},{"connection_id":ids[1],"model":"m"}]),
            )
            .await;
        (gateway.create_key("test").await.1, up)
    }

    #[tokio::test]
    async fn websocket_codex_accumulation_over_16_mib_closes_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::start(dir.path()).await;
        let mut frames = vec![created("resp_ws_big")];
        for i in 0..17 {
            frames.push(item_done(i, &format!("msg_{i}"), MIB));
        }
        frames.push(terminal("resp_ws_big", 0));
        let (key, _upstream) = pool(&gateway, frames.clone()).await;

        let mut ws = session(&gateway, &key).await;
        let got = run(&mut ws, json!({})).await;
        assert_eq!(&got[..16], &frames[..16], "earlier frames unchanged");
        assert!(got.iter().all(|f| f["type"] != "response.completed"));
        assert_overflow(got.last().unwrap(), "16 MiB or 4096 items");
        let record = &gateway.wait_for_log(1).await[0];
        assert_eq!(record["transport"], "websocket");
        assert_eq!(record["status"], 502);
        assert_eq!(record["error"], "Codex output exceeds 16 MiB or 4096 items");

        // No affinity: a continuation of the failed response is refused in the pool.
        let mut ws = session(&gateway, &key).await;
        let got = run(&mut ws, json!({"previous_response_id":"resp_ws_big"})).await;
        assert_eq!(got[0]["type"], "error");
        assert!(
            got[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("previous_response_id"),
            "{}",
            got[0]
        );
    }

    #[tokio::test]
    async fn websocket_codex_terminal_that_cannot_be_encoded_closes_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::start(dir.path()).await;
        let mut frames = vec![created("resp_ws_wide")];
        for i in 0..15 {
            frames.push(item_done(i, &format!("msg_{i}"), MIB));
        }
        frames.push(terminal("resp_ws_wide", 3 * MIB / 2));
        let (key, _upstream) = pool(&gateway, frames.clone()).await;
        let mut ws = session(&gateway, &key).await;
        let got = run(&mut ws, json!({})).await;
        assert_eq!(&got[..16], &frames[..16]);
        assert!(got.iter().all(|f| f["type"] != "response.completed"));
        assert_overflow(got.last().unwrap(), "final response");
        assert_eq!(gateway.wait_for_log(1).await[0]["status"], 502);
    }
}
