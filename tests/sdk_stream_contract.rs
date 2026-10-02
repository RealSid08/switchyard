//! Contract observed live from the ChatGPT Codex backend through the OpenAI Python SDK 3.23:
//! completed items arrive as `response.output_item.done`, and the terminal `response.completed`
//! carries `response.output: []` on both SSE and WebSocket. The SDK stream helper's
//! `get_final_response()` and `ResponsesConnection` users read the terminal response, so they saw
//! no output and lost tool calls. For Codex connections only, the gateway fills that terminal
//! output from the completed items; every other byte and frame is forwarded unchanged.

mod support;

use axum::{
    Router,
    body::Body,
    extract::ws::{Message as AxMessage, WebSocketUpgrade},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use support::*;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

/// The terminal `response` object as observed live (field set abridged, values representative).
fn live_response(id: &str, status: &str, output: Value) -> Value {
    json!({"id":id,"object":"response","status":status,"created_at":1790000000,"completed_at":1790000001,
        "model":"gpt-6.1-sol","output":output,"error":null,"incomplete_details":null,"instructions":"x",
        "metadata":{},"parallel_tool_calls":true,"previous_response_id":null,"store":false,
        "text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"usage":{"input_tokens":20,"output_tokens":5,"total_tokens":25},
        "tool_usage":{},"access_programs":[],"reasoning":{"effort":"medium"}})
}
fn message_item(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"message","role":"assistant","status":"completed","phase":"final",
        "content":[{"type":"output_text","text":text,"annotations":[],"logprobs":[]}]})
}
fn call_item(id: &str) -> Value {
    json!({"id":id,"type":"function_call","status":"completed","call_id":"call_1","name":"get_time","arguments":"{\"zone\":\"UTC\"}"})
}
/// The observed event sequence for one text item, followed by `extra` events and the terminal.
fn live_events(id: &str, items: &[(u64, Value)], terminal: &str) -> Vec<Value> {
    let mut seq = 0;
    let mut next = |mut v: Value| {
        v["sequence_number"] = json!(seq);
        seq += 1;
        v
    };
    let mut events = vec![
        next(json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":1.0}}})),
        next(
            json!({"type":"response.created","response":live_response(id, "in_progress", json!([]))}),
        ),
        next(
            json!({"type":"response.in_progress","response":live_response(id, "in_progress", json!([]))}),
        ),
    ];
    for (index, item) in items {
        let item_id = item["id"].clone();
        events.push(next(
            json!({"type":"response.output_item.added","output_index":index,"item":item}),
        ));
        if item["type"] == "message" {
            let text = item["content"][0]["text"].clone();
            events.push(next(
                json!({"type":"response.output_text.delta","item_id":item_id,"output_index":index,
                "content_index":0,"delta":text,"logprobs":[],"obfuscation":"abc"}),
            ));
            events.push(next(
                json!({"type":"response.output_text.done","item_id":item_id,"output_index":index,
                "content_index":0,"text":text,"logprobs":[]}),
            ));
        } else {
            events.push(next(
                json!({"type":"response.function_call_arguments.done","item_id":item_id,
                "output_index":index,"arguments":item["arguments"]}),
            ));
        }
    }
    // Items complete in index order on the wire except where a test reorders them.
    for (index, item) in items {
        events.push(next(
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        ));
    }
    events.push(next(json!({"type":terminal,"response":live_response(id, if terminal == "response.completed" {"completed"} else {"incomplete"}, json!([]))})));
    events
}

/// Client-side SSE parsing (WHATWG): raw blocks with parseable JSON data, in order. A block cut
/// mid-event by a dropped connection is skipped, as SDK clients do.
fn blocks(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|b| {
            let data: String = b
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|d| d.strip_prefix(' ').unwrap_or(d))
                })
                .collect::<Vec<_>>()
                .join("\n");
            (b.to_string(), serde_json::from_str(&data))
        })
        .filter_map(|(raw, parsed)| parsed.ok().map(|v| (raw, v)))
        .collect()
}

fn sse_upstream(body: String) -> Router {
    Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(Body::from(body)) }
        }),
    )
}

async fn stream_text(gw: &Gateway, key: &str, model: &str) -> String {
    gw.post(
        "/v1/responses",
        key,
        json!({"model":model,"input":"Reply exactly OK","stream":true}),
    )
    .await
    .text()
    .await
    .unwrap()
}

#[tokio::test]
async fn codex_sse_terminal_output_is_filled_and_every_other_event_is_untouched() {
    let (_d, gw) = setup().await;
    let items = [(0, message_item("msg_1", "OK")), (1, call_item("fc_1"))];
    let body = sse(&live_events("resp_live", &items, "response.completed"));
    let up = Upstream::start(sse_upstream(body.clone())).await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-codex"])
        .await;
    gw.connection("OpenAI", "openai", &up.base(), &["gpt-native"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let text = stream_text(&gw, &key, "gpt-codex").await;
    let got = blocks(&text);
    let sent = blocks(&body);
    assert_eq!(got.len(), sent.len(), "event count and order preserved");
    for ((got_raw, _), (sent_raw, _)) in got.iter().zip(&sent).take(sent.len() - 1) {
        assert_eq!(got_raw, sent_raw, "non-terminal events are byte-identical");
    }
    let (raw, terminal) = got.last().unwrap();
    assert!(raw.starts_with("event: response.completed\n"));
    let mut expected = sent.last().unwrap().1.clone();
    expected["response"]["output"] = json!([items[0].1, items[1].1]);
    assert_eq!(*terminal, expected, "only response.output changes");

    // Native OpenAI streams are never rewritten, even with the same shape.
    assert_eq!(stream_text(&gw, &key, "gpt-native").await, body);

    let log = gw.wait_for_log(2).await;
    assert!(log.iter().all(|r| r["status"] == 200), "{log:?}");
}

#[tokio::test]
async fn codex_fill_follows_output_index_and_leaves_complete_terminals_alone() {
    let (_d, gw) = setup().await;
    // Items completing out of order are placed by output_index.
    let mut events = live_events(
        "resp_order",
        &[(0, message_item("msg_a", "first")), (1, call_item("fc_b"))],
        "response.incomplete",
    );
    let (a, b) = (events.len() - 3, events.len() - 2);
    events.swap(a, b);
    let reordered = sse(&events);
    // A terminal that already carries output is forwarded byte for byte.
    let mut full = live_events(
        "resp_full",
        &[(0, message_item("msg_f", "OK"))],
        "response.completed",
    );
    let last = full.len() - 1;
    full[last]["response"]["output"] = json!([message_item("msg_f", "OK")]);
    let full = sse(&full);
    let up = Upstream::start(
        Router::new()
            .route(
                "/order/responses",
                post(move || {
                    let b = reordered.clone();
                    async move { sse_response(Body::from(b)) }
                }),
            )
            .route(
                "/full/responses",
                post({
                    let full = full.clone();
                    move || {
                        let b = full.clone();
                        async move { sse_response(Body::from(b)) }
                    }
                }),
            ),
    )
    .await;
    gw.connection(
        "Order",
        "codex",
        &format!("{}/order", up.base()),
        &["order"],
    )
    .await;
    gw.connection("Full", "codex", &format!("{}/full", up.base()), &["full"])
        .await;
    let (_, key) = gw.create_key("k").await;

    let got = blocks(&stream_text(&gw, &key, "order").await);
    let terminal = &got.last().unwrap().1;
    assert_eq!(terminal["type"], "response.incomplete");
    let ids: Vec<&str> = terminal["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["msg_a", "fc_b"]);

    assert_eq!(stream_text(&gw, &key, "full").await, full);
}

#[tokio::test]
async fn codex_fill_survives_crlf_multiline_and_split_utf8() {
    let (_d, gw) = setup().await;
    let item = message_item("msg_u", "Grüße 🦀");
    let events = live_events("resp_u", &[(0, item.clone())], "response.completed");
    // Multi-line data with CRLF endings for every event, cut inside every multibyte character.
    let body: String = events
        .iter()
        .map(|e| {
            let s = e.to_string();
            let (head, tail) = s.split_at(s.find(",\"").unwrap() + 1);
            format!(
                "event: {}\r\ndata: {head}\r\ndata: {tail}\r\n\r\n",
                e["type"].as_str().unwrap()
            )
        })
        .collect();
    let bytes = body.into_bytes();
    let mut cuts: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b >= 0xC0)
        .map(|(i, _)| i + 1)
        .collect();
    cuts.push(bytes.len());
    let mut chunks = Vec::new();
    let mut last = 0;
    for c in cuts {
        chunks.push(bytes[last..c].to_vec());
        last = c;
    }
    let expected_prefix = String::from_utf8(bytes.clone()).unwrap();
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
    let text = stream_text(&gw, &key, "gpt-codex").await;
    let terminal_at = expected_prefix.rfind("event: response.completed").unwrap();
    assert_eq!(
        &text[..terminal_at],
        &expected_prefix[..terminal_at],
        "events before the terminal are byte-identical"
    );
    let terminal = &blocks(&text.replace("\r\n", "\n"))
        .last()
        .unwrap()
        .1
        .clone();
    assert_eq!(terminal["response"]["output"], json!([item]));
}

#[tokio::test]
async fn codex_stream_cut_mid_event_still_reports_partial_output() {
    let (_d, gw) = setup().await;
    let events = live_events(
        "resp_cut",
        &[(0, message_item("msg_c", "half"))],
        "response.completed",
    );
    let whole = sse(&events[..4]);
    let partial = "event: response.output_text.done\ndata: {\"type\":\"response.output_te";
    let mut raw =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec();
    for chunk in [whole.as_str(), partial] {
        raw.extend_from_slice(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes());
    }
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(raw).await;
            });
        }
    });
    gw.connection("Codex", "codex", &base, &["gpt-codex"]).await;
    let (_, key) = gw.create_key("k").await;
    let text = stream_text(&gw, &key, "gpt-codex").await;
    assert!(
        text.starts_with(&whole),
        "complete events delivered before the cut"
    );
    let last = blocks(&text).last().unwrap().1.clone();
    assert_eq!(last["error"]["type"], "upstream_interrupted");
    assert_eq!(gw.wait_for_log(1).await[0]["status"], 502);
}

// ---------------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------------

fn ws_upstream() -> Router {
    Router::new().route(
        "/responses",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                let mut turn = 0;
                while let Some(Ok(AxMessage::Text(_))) = socket.recv().await {
                    turn += 1;
                    let items = if turn == 1 {
                        vec![(0, message_item("msg_t1", "OK")), (1, call_item("fc_t1"))]
                    } else {
                        vec![(0, message_item("msg_t2", "AGAIN"))]
                    };
                    for e in live_events(&format!("resp_t{turn}"), &items, "response.completed") {
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
async fn ws_turn(gw: &Gateway, ws: &mut Client, model: &str) -> Vec<String> {
    let _ = gw;
    ws.send(Message::Text(
        json!({"type":"response.create","model":model,"input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let mut frames = Vec::new();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap();
        if let Some(Ok(Message::Text(t))) = msg {
            let done = t.contains("\"type\":\"response.completed\"");
            frames.push(t.to_string());
            if done {
                return frames;
            }
        }
    }
}

#[tokio::test]
async fn codex_websocket_terminal_output_is_filled_per_turn() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(ws_upstream()).await;
    gw.add_connection(json!({"name":"Codex","kind":"codex","base_url":up.base(),"models":["gpt-codex"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let up2 = Upstream::start(ws_upstream()).await;
    gw.add_connection(json!({"name":"OpenAI","kind":"openai","base_url":up2.base(),"models":["gpt-native"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;
    let connect = || async {
        let mut req = gw.ws_url("/v1/responses").into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    };

    let mut ws = connect().await;
    let turn1 = ws_turn(&gw, &mut ws, "gpt-codex").await;
    let turn2 = ws_turn(&gw, &mut ws, "gpt-codex").await;
    let expected1 = live_events(
        "resp_t1",
        &[(0, message_item("msg_t1", "OK")), (1, call_item("fc_t1"))],
        "response.completed",
    );
    assert_eq!(turn1.len(), expected1.len());
    for (got, sent) in turn1.iter().zip(&expected1).take(expected1.len() - 1) {
        assert_eq!(
            *got,
            sent.to_string(),
            "Codex metadata and item frames unchanged"
        );
    }
    let t1: Value = serde_json::from_str(turn1.last().unwrap()).unwrap();
    let mut want = expected1.last().unwrap().clone();
    want["response"]["output"] = json!([message_item("msg_t1", "OK"), call_item("fc_t1")]);
    assert_eq!(t1, want);
    let t2: Value = serde_json::from_str(turn2.last().unwrap()).unwrap();
    assert_eq!(
        t2["response"]["output"],
        json!([message_item("msg_t2", "AGAIN")]),
        "items never leak between turns"
    );
    ws.close(None).await.unwrap();

    // Native OpenAI WebSocket frames are relayed untouched.
    let mut ws = connect().await;
    let native = ws_turn(&gw, &mut ws, "gpt-native").await;
    let t: Value = serde_json::from_str(native.last().unwrap()).unwrap();
    assert_eq!(t["response"]["output"], json!([]));
}
