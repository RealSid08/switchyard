//! Real HTTP routing through the Antigravity adapter, using local Cloud Code fixtures.
mod support;
use axum::{
    Json, Router,
    body::Body,
    http::{Response, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use support::*;
use switchyard::store::Connection;

fn complete() -> Value {
    json!({"response":{"responseId":"ag-fixture","candidates":[{"content":{"role":"model","parts":[{"text":"READY"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}}})
}
async fn connection(gw: &Gateway, upstream: &Upstream, name: &str) -> String {
    let id = gw
        .connection(name, "antigravity", &upstream.base(), &["gemini-3-flash"])
        .await;
    let mut c: Connection = gw.app.store.get("connection", &id).unwrap();
    c.account_id = "fixture-project".into();
    gw.app.store.put("connection", &id, &c).unwrap();
    id
}
#[tokio::test]
async fn chat_messages_and_gemini_reach_cloud_code_with_client_usage() {
    let upstream = Upstream::start(Router::new().route(
        "/v1internal:generateContent",
        post(|| async { Json(complete()) }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    connection(&gw, &upstream, "Antigravity fixture").await;
    let (key_id, key) = gw.create_key("OpenCode").await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model":"gemini-3-flash","messages":[{"role":"user","content":"Hello"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"gemini-3-flash","max_tokens":32,"messages":[{"role":"user","content":"Hello"}]}),
        ),
        (
            "/v1beta/models/gemini-3-flash:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}),
        ),
    ] {
        let response = gw.post(path, &key, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let v: Value = response.json().await.unwrap();
        assert!(v.to_string().contains("READY"), "{v}");
    }
    for request in upstream.requests() {
        let body = request.json();
        assert_eq!(body["project"], "fixture-project");
        assert_eq!(body["model"], "gemini-3-flash");
        assert!(body["request"]["contents"].is_array());
        assert_eq!(
            request.header("authorization").as_deref(),
            Some(format!("Bearer {PROVIDER_KEY}").as_str())
        );
        assert!(
            request
                .header("user-agent")
                .unwrap()
                .contains("antigravity")
        );
    }
    let usage = gw.admin_json("/api/usage?window=24h").await;
    assert_eq!(usage["totals"]["units"]["succeeded"], 3);
    assert_eq!(usage["totals"]["tokens"]["input"], 30);
    assert_eq!(usage["totals"]["tokens"]["output"], 6);
    assert_eq!(usage["by_client"][0]["client_key_id"], key_id);
}
#[tokio::test]
async fn stream_unwraps_and_emits_one_completion_for_duplicate_terminal() {
    let upstream = Upstream::start(Router::new().route("/v1internal:streamGenerateContent",post(|| async {
        let first=json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"READY"}]}}]}});
        let final_value=complete();
        Response::builder().header("content-type","text/event-stream")
            .body(Body::from(format!("data: {first}\n\ndata: {final_value}\n\ndata: {final_value}\n\n"))).unwrap()
    }))).await;
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    connection(&gw, &upstream, "stream").await;
    let (_, key) = gw.create_key("agent").await;
    let response=gw.post("/v1/chat/completions",&key,json!({"model":"gemini-3-flash","messages":[{"role":"user","content":"Hello"}],"stream":true})).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("READY"), "{text}");
    assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
    let usage = gw.admin_json("/api/usage?window=24h").await;
    assert_eq!(usage["totals"]["units"]["succeeded"], 1);
    assert_eq!(usage["totals"]["tokens"]["output"], 2);
}
#[tokio::test]
async fn truncated_stream_is_failed_and_never_sent_to_fallback() {
    let upstream=Upstream::start(Router::new().route("/v1internal:streamGenerateContent",post(||async{
        let v=json!({"response":{"candidates":[{"content":{"parts":[{"text":"PARTIAL"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}}});
        Response::builder().header("content-type","text/event-stream").body(Body::from(format!("data: {v}\n\n"))).unwrap()
    }))).await;
    let fallback = Upstream::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post(|| async { Json(complete()) }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let first = connection(&gw, &upstream, "first").await;
    let second = connection(&gw, &fallback, "fallback").await;
    let r=gw.put_route("route","failover",json!([{"connection_id":first,"model":"gemini-3-flash"},{"connection_id":second,"model":"gemini-3-flash"}])).await;
    assert_eq!(r.status(), 200);
    let (_, key) = gw.create_key("agent").await;
    let response = gw
        .post(
            "/v1/chat/completions",
            &key,
            json!({"model":"route","messages":[{"role":"user","content":"Hello"}],"stream":true}),
        )
        .await;
    let text = response.text().await.unwrap();
    assert!(text.contains("PARTIAL"));
    assert!(text.contains("upstream_interrupted"), "{text}");
    assert!(!text.contains("[DONE]"));
    assert_eq!(fallback.count(), 0);
    let usage = gw.admin_json("/api/usage?window=24h").await;
    assert_eq!(usage["totals"]["units"]["failed"], 1);
    assert_eq!(usage["totals"]["tokens"]["output"], 1);
}
