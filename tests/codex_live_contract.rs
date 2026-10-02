//! Contract observed against a live Codex response: terminal output can be empty.
mod support;
use axum::{Router, routing::post};
use serde_json::{Value, json};
use support::*;
#[tokio::test]
async fn codex_nonstream_restores_completed_items_when_terminal_output_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let body = sse(&[
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_one","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK"}]}}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"id":"fc_one","type":"function_call","call_id":"call_one","name":"test_tool","arguments":"{}"}}),
        json!({"type":"response.completed","response":{"id":"resp_one","status":"completed","output":[],"usage":{"input_tokens":20,"output_tokens":5,"total_tokens":25}}}),
    ]);
    let up = Upstream::start(Router::new().route(
        "/responses",
        post(move || {
            let body = body.clone();
            async move { sse_response(body.into()) }
        }),
    ))
    .await;
    gw.connection("Codex", "codex", &up.base(), &["gpt-test"])
        .await;
    let (_, key) = gw.create_key("test").await;
    let r = gw
        .http
        .post(gw.url("/v1/responses"))
        .bearer_auth(&key)
        .json(&json!({"model":"gpt-test","input":"OK"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["output"][0]["content"][0]["text"], "OK",
        "Codex live terminal event output is empty; completed items must be reconstructed"
    );
    assert_eq!(v["output"][1]["call_id"], "call_one");
    let r = gw
        .http
        .post(gw.url("/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model":"gpt-test","messages":[{"role":"user","content":"OK"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "OK");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_one"
    );
}
