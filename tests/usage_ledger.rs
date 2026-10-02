//! Usage ledger: provider token semantics, exact costs, unknown-is-not-zero, failover and
//! failure accounting, route aliases, per-turn WebSocket accounting with duplicate terminals,
//! durability past the 1000-row request log and restarts, filters, bounds, external sources and
//! client attribution. Loopback fake providers only.

mod support;

use axum::{
    Router,
    body::Body,
    extract::{
        Request,
        ws::{Message as AxMessage, WebSocketUpgrade},
    },
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use support::*;
use switchyard::{
    store::RequestRecord,
    usage::{self, ClientIdentity, ExternalUsage, Semantics, Tokens, UsageAccumulator},
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

async fn usage_json(gw: &Gateway, query: &str) -> (u16, Value) {
    let r = gw.admin_get(&format!("/api/usage{query}")).await;
    (r.status().as_u16(), r.json().await.unwrap())
}
/// Units are written when a request finishes; poll until `n` are visible.
async fn wait_units(gw: &Gateway, query: &str, n: i64) -> Value {
    for _ in 0..200 {
        let (status, v) = usage_json(gw, query).await;
        assert_eq!(status, 200, "{v}");
        let units = v["totals"]["units"]["total"]
            .as_i64()
            .or_else(|| {
                v["by_source"].as_array().map(|a| {
                    a.iter()
                        .map(|s| s["metrics"]["units"]["total"].as_i64().unwrap())
                        .sum()
                })
            })
            .unwrap_or(0);
        if units >= n {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "usage never reached {n} units: {:?}",
        usage_json(gw, query).await
    );
}

fn openai_usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},
        "output_tokens":output,"output_tokens_details":{"reasoning_tokens":reasoning},
        "total_tokens":input+output})
}
fn responses_body(id: &str, usage: Value) -> Value {
    json!({"id":id,"object":"response","status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
        "usage":usage})
}
fn json_upstream(path: &'static str, status: u16, body: Value) -> Router {
    Router::new().route(
        path,
        post(move || {
            let body = body.clone();
            async move { json_response(status, body) }
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// Provider semantics (pure)
// ---------------------------------------------------------------------------------------------

#[test]
fn provider_token_semantics_never_overlap() {
    // OpenAI Responses: cached and cache-write tokens are inside input_tokens.
    let (t, s) = usage::tokens_from_usage(
        &json!({"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":600,"cache_write_tokens":100},
            "output_tokens":50,"output_tokens_details":{"reasoning_tokens":20}}}),
        "codex",
    )
    .unwrap();
    assert_eq!(s, Semantics::Subset);
    assert_eq!(
        (t.input, t.cache_read, t.cache_write, t.output, t.reasoning),
        (Some(300), Some(600), Some(100), Some(50), Some(20))
    );
    assert_eq!(
        t.total(),
        1050,
        "total equals the provider's input + output"
    );

    // Chat Completions shape.
    let (t, _) = usage::tokens_from_usage(
        &json!({"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens":3,
            "completion_tokens_details":{"reasoning_tokens":1}}}),
        "openai",
    )
    .unwrap();
    assert_eq!(
        (t.input, t.cache_read, t.output, t.reasoning),
        (Some(6), Some(4), Some(3), Some(1))
    );

    // No split reported: cache is unknown, not zero.
    let (t, _) = usage::tokens_from_usage(
        &json!({"usage":{"prompt_tokens":10,"completion_tokens":3}}),
        "openai",
    )
    .unwrap();
    assert_eq!(
        (t.input, t.cache_read, t.cache_write),
        (Some(10), None, None)
    );

    // Anthropic: cache tokens are additive, with the TTL split.
    let (t, s) = usage::tokens_from_usage(
        &json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"cache_read_input_tokens":100,
            "cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20},"output_tokens":1}}}),
        "anthropic",
    )
    .unwrap();
    assert_eq!(s, Semantics::Additive);
    assert_eq!(
        (
            t.input,
            t.cache_read,
            t.cache_write,
            t.cache_write_5m,
            t.cache_write_1h,
            t.output
        ),
        (Some(5), Some(100), Some(30), Some(10), Some(20), Some(1))
    );
    assert_eq!(
        t.reasoning, None,
        "Anthropic does not report thinking separately"
    );

    // Gemini (and the Antigravity `response.usageMetadata` wrapper): cached is inside the
    // prompt; thoughts are reported apart from candidates and belong to output.
    for v in [
        json!({"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":7,"thoughtsTokenCount":5}}),
        json!({"response":{"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":7,"thoughtsTokenCount":5}}}),
    ] {
        let (t, _) = usage::tokens_from_usage(&v, "antigravity").unwrap();
        assert_eq!(
            (t.input, t.cache_read, t.cache_write, t.output, t.reasoning),
            (Some(60), Some(40), Some(0), Some(12), Some(5))
        );
    }
    // Gemini omits zero counters.
    let (t, _) =
        usage::tokens_from_usage(&json!({"usageMetadata":{"promptTokenCount":9}}), "gemini")
            .unwrap();
    assert_eq!(
        (t.input, t.cache_read, t.output, t.reasoning),
        (Some(9), Some(0), Some(0), Some(0))
    );
    assert!(usage::tokens_from_usage(&json!({"type":"ping"}), "openai").is_none());
}

#[test]
fn repeated_snapshots_replace_and_never_add() {
    // Anthropic stream: message_start then cumulative message_delta.
    let mut acc = UsageAccumulator::default();
    acc.observe(&json!({"type":"message_start","message":{"id":"msg_9","usage":{"input_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":0,"output_tokens":1}}}), "anthropic");
    acc.observe(
        &json!({"type":"message_delta","usage":{"output_tokens":40}}),
        "anthropic",
    );
    acc.observe(
        &json!({"type":"message_delta","usage":{"output_tokens":40}}),
        "anthropic",
    );
    assert_eq!(
        (acc.tokens.input, acc.tokens.cache_read, acc.tokens.output),
        (Some(20), Some(5), Some(40))
    );
    assert_eq!(acc.response_id.as_deref(), Some("msg_9"));

    // Subset semantics: a later unsplit total must not mix with an earlier split.
    let mut acc = UsageAccumulator::default();
    acc.observe(&json!({"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":5}}), "openai");
    acc.observe(
        &json!({"usage":{"input_tokens":100,"output_tokens":5}}),
        "openai",
    );
    let t = acc.tokens;
    assert_eq!(
        (t.input, t.cache_read),
        (Some(100), None),
        "no 100 + 60 double count"
    );
    assert_eq!(t.total(), 105);
}

// ---------------------------------------------------------------------------------------------
// HTTP accounting and costs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn http_usage_is_priced_exactly_and_route_aliases_use_the_upstream_model() {
    let (_d, gw) = setup().await;
    // 400K input of which 100K cached = 300K uncached (> 272K long-context threshold applies to
    // total input 400K), 10K output of which 2K reasoning.
    let up = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("resp_priced", openai_usage(400_000, 100_000, 10_000, 2_000)),
    ))
    .await;
    let c = gw
        .connection("Key", "openai", &up.base(), &["gpt-6.1-sol"])
        .await;
    gw.put_route(
        "smart",
        "failover",
        json!([{"connection_id":c,"model":"gpt-6.1-sol"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .post("/v1/responses", &key, json!({"model":"smart","input":"x"}))
        .await;
    assert_eq!(r.status(), 200);

    let v = wait_units(&gw, "?window=24h", 1).await;
    let t = &v["totals"];
    assert_eq!(
        t["units"],
        json!({"total":1,"succeeded":1,"failed":0,"cancelled":0})
    );
    let tok = &t["tokens"];
    assert_eq!(
        (
            tok["input"].as_i64(),
            tok["cache_read"].as_i64(),
            tok["output"].as_i64(),
            tok["reasoning"].as_i64(),
            tok["total"].as_i64()
        ),
        (
            Some(300_000),
            Some(100_000),
            Some(10_000),
            Some(2_000),
            Some(410_000)
        )
    );
    assert_eq!(tok["known_units"]["cache_read"], 1);
    // Long context (> 272K input): $4 × 0.3 + $0.20 × 0.1 + $15 × 0.01 = 1.37 USD.
    assert_eq!(t["cost"]["estimated_micros"], 1_370_000);
    assert_eq!(t["cost"]["estimated_usd"], "1.370000");
    assert_eq!(t["cost"]["api_estimated_micros"], 1_370_000);
    assert!(t["cost"]["subscription_equivalent_micros"].is_null());
    assert!(
        t["cost"]["reported_micros"].is_null(),
        "no provider-reported cost"
    );
    assert_eq!(t["cache"]["read_ratio"], json!(0.25));
    assert_eq!(
        v["by_model"][0]["model"], "gpt-6.1-sol",
        "alias priced as its upstream model"
    );
    assert_eq!(v["by_model"][0]["priced"], true);
    assert_eq!(v["by_account"][0]["billing"], "api_key");
    assert_eq!(v["by_account"][0]["origin"], "gateway");
    assert_eq!(v["coverage"]["overlap"], "none");
    assert_eq!(v["series"].as_array().unwrap().len(), 24);
    assert_eq!(v["facets"]["models"], json!(["gpt-6.1-sol"]));
}

#[tokio::test]
async fn anthropic_cache_ttl_costs_and_subscription_value() {
    let (_d, gw) = setup().await;
    let body = json!({"id":"msg_1","type":"message","content":[{"type":"text","text":"ok"}],
        "usage":{"input_tokens":1_000_000,"cache_read_input_tokens":1_000_000,"cache_creation_input_tokens":3_000_000,
            "cache_creation":{"ephemeral_5m_input_tokens":1_000_000,"ephemeral_1h_input_tokens":2_000_000},"output_tokens":1_000_000}});
    let up = Upstream::start(json_upstream("/messages", 200, body)).await;
    // An OAuth (subscription) Claude account.
    let conn = json!({"id":"claude-sub","name":"Claude","kind":"anthropic","base_url":up.base(),"enabled":true,
        "models":["claude-opus-5-5"],"supports_websocket":false,"created_at":"2026-10-03T00:00:00Z",
        "api_key":PROVIDER_KEY,"refresh_token":"rt","expires_at":chrono::Utc::now().timestamp()+86400,
        "account_id":"","oauth":true,"credential_source":"oauth"});
    gw.app.store.put("connection", "claude-sub", &conn).unwrap();
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .json(&json!({"model":"claude-opus-5-5","max_tokens":5,"messages":[{"role":"user","content":"x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = wait_units(&gw, "", 1).await;
    let t = &v["totals"];
    // Opus 5.5: $4 input + $0.20 read + 1M × $5 (5m) + 2M × $8 (1h) + $20 output = 45.20 USD.
    assert_eq!(t["cost"]["estimated_micros"], 45_200_000);
    assert_eq!(t["cost"]["subscription_equivalent_micros"], 45_200_000);
    assert!(t["cost"]["api_estimated_micros"].is_null());
    assert_eq!(t["tokens"]["cache_write_5m"], 1_000_000);
    assert_eq!(t["tokens"]["cache_write_1h"], 2_000_000);
    assert_eq!(
        t["tokens"]["total"], 6_000_000,
        "additive cache: 1M + 1M + 3M + 1M"
    );
    assert_eq!(v["by_account"][0]["billing"], "subscription");
    assert!(
        t["tokens"]["known_units"]["reasoning"] == 0,
        "thinking not reported: unknown"
    );
}

#[tokio::test]
async fn unknown_models_stay_unpriced_and_overrides_price_only_new_usage() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("resp_local", openai_usage(1_000_000, 0, 1_000_000, 0)),
    ))
    .await;
    gw.connection("Local", "openai", &up.base(), &["my-local-model"])
        .await;
    let (_, key) = gw.create_key("k").await;
    gw.post(
        "/v1/responses",
        &key,
        json!({"model":"my-local-model","input":"x"}),
    )
    .await;
    let v = wait_units(&gw, "", 1).await;
    assert!(
        v["totals"]["cost"]["estimated_micros"].is_null(),
        "unknown cost is null, not 0"
    );
    assert_eq!(v["totals"]["cost"]["unpriced_units"], 1);
    assert_eq!(v["by_model"][0]["priced"], false);
    let p = gw.admin_json("/api/usage/pricing").await;
    assert_eq!(p["unpriced_models"][0]["model"], "my-local-model");
    assert!(
        p["rates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["model"] == "claude-opus-5-5" && r["usd_per_mtok"]["cache_read"] == "0.2")
    );

    // Invalid overrides are rejected with the field named.
    for (body, needle) in [
        (
            json!({"overrides":[{"model":"my-local-model","usd_per_mtok":{"input":"1"}}]}),
            "input and output",
        ),
        (
            json!({"overrides":[{"model":"my-local-model","usd_per_mtok":{"input":"1.0000001","output":"1"}}]}),
            "input",
        ),
        (
            json!({"overrides":[{"model":"","usd_per_mtok":{"input":"1","output":"1"}}]}),
            "model",
        ),
        (json!({"nope":[]}), "overrides"),
    ] {
        let r = gw
            .admin_send(reqwest::Method::PUT, "/api/usage/pricing/overrides", body)
            .await;
        assert_eq!(r.status(), 400);
        let e: Value = r.json().await.unwrap();
        assert!(
            e["error"]["message"].as_str().unwrap().contains(needle),
            "{e}"
        );
    }
    let r = gw
        .admin_send(
            reqwest::Method::PUT,
            "/api/usage/pricing/overrides",
            json!({"overrides":[{"model":"my-local-model","usd_per_mtok":{"input":"1.25","output":"10"}}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["unpriced_models"].as_array().unwrap().is_empty());
    gw.post(
        "/v1/responses",
        &key,
        json!({"model":"my-local-model","input":"x"}),
    )
    .await;
    let v = wait_units(&gw, "", 2).await;
    let cost = &v["totals"]["cost"];
    assert_eq!(
        cost["estimated_micros"], 11_250_000,
        "only the new unit is priced"
    );
    assert_eq!(
        (
            cost["priced_units"].as_i64(),
            cost["unpriced_units"].as_i64()
        ),
        (Some(1), Some(1))
    );
}

#[tokio::test]
async fn failover_counts_attempts_but_only_served_usage() {
    let (_d, gw) = setup().await;
    let busy = Upstream::start(json_upstream(
        "/responses",
        503,
        json!({"error":{"message":"busy"}}),
    ))
    .await;
    let good = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("resp_ok", openai_usage(100, 0, 10, 0)),
    ))
    .await;
    let a = gw
        .connection("Busy", "openai", &busy.base(), &["gpt-6.1-sol"])
        .await;
    let b = gw
        .connection("Good", "openai", &good.base(), &["gpt-6.1-sol"])
        .await;
    gw.put_route(
        "ha",
        "failover",
        json!([{"connection_id":a,"model":"gpt-6.1-sol"},{"connection_id":b,"model":"gpt-6.1-sol"}]),
    )
    .await;
    let (_, key) = gw.create_key("k").await;
    assert_eq!(
        gw.post("/v1/responses", &key, json!({"model":"ha","input":"x"}))
            .await
            .status(),
        200
    );
    let v = wait_units(&gw, "", 1).await;
    let t = &v["totals"];
    assert_eq!(t["units"]["succeeded"], 1);
    assert_eq!(t["attempts"], json!({"total":2,"failed":1,"failovers":1}));
    assert_eq!(t["tokens"]["input"], 100);
    let accounts = v["by_account"].as_array().unwrap();
    assert_eq!(
        accounts.len(),
        1,
        "usage belongs to the serving account only"
    );
    assert_eq!(accounts[0]["connection_name"], "Good");

    // Filter by account.
    let (_, only) = usage_json(&gw, &format!("?connection_id={b}")).await;
    assert_eq!(only["totals"]["units"]["total"], 1);
    let (_, none) = usage_json(&gw, &format!("?connection_id={a}")).await;
    assert_eq!(none["totals"]["units"]["total"], 0);
    assert!(
        none["totals"]["latency_ms"]["avg"].is_null(),
        "no samples, no average"
    );
    assert!(none["totals"]["success_rate"].is_null());
}

#[tokio::test]
async fn a_failed_stream_keeps_the_usage_it_observed() {
    let (_d, gw) = setup().await;
    // Anthropic stream that reports input usage, then the connection is cut mid-message.
    let partial = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cut\",\"usage\":{\"input_tokens\":120,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n\
                   event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n";
    let up = Upstream::start(Router::new().route(
        "/messages",
        post(move || async move { sse_response(Body::from(partial)) }),
    ))
    .await;
    gw.connection("Anth", "anthropic", &up.base(), &["claude-haiku-4-5"])
        .await;
    let (_, key) = gw.create_key("k").await;
    let r = gw
        .http
        .post(gw.url("/v1/messages"))
        .header("x-api-key", &key)
        .json(&json!({"model":"claude-haiku-4-5","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"x"}]}))
        .send()
        .await
        .unwrap();
    r.bytes().await.unwrap();
    let v = wait_units(&gw, "", 1).await;
    let t = &v["totals"];
    assert_eq!(t["units"]["failed"], 1);
    assert_eq!(
        t["tokens"]["input"], 120,
        "observed usage of a failed unit is kept"
    );
    assert_eq!(t["tokens"]["output"], 1);
    assert!(t["cost"]["estimated_micros"].as_i64().is_some());
    assert!(t["first_token_ms"]["samples"] == 1);
}

// ---------------------------------------------------------------------------------------------
// Durability, retention, coverage
// ---------------------------------------------------------------------------------------------

fn record(i: usize) -> RequestRecord {
    RequestRecord {
        id: format!("seed-{i}"),
        timestamp: switchyard::store::now(),
        model: "gpt-6.1-sol".into(),
        connection_id: "c1".into(),
        connection_name: "Seed".into(),
        transport: "http".into(),
        status: 200,
        latency_ms: 10,
        input_tokens: Some(1),
        output_tokens: Some(1),
        error: None,
        route: None,
        failovers: 0,
        attempts: Vec::new(),
        ttfb_ms: None,
        first_token_ms: None,
    }
}

#[tokio::test]
async fn ledger_outlives_the_request_log_and_restarts_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let mut acc = UsageAccumulator::default();
    acc.observe(&json!({"usage":openai_usage(10, 0, 5, 0)}), "openai");
    let cx = usage::UnitContext {
        provider: Some("openai"),
        billing: "api_key",
        upstream_model: None,
        client: None,
        usage: &acc,
    };
    for i in 0..1005 {
        let r = record(i);
        let e = usage::event_for_record(&gw.app.store, &r, &cx);
        gw.app.store.record_with_usage(&r, &[e]).unwrap();
    }
    // The same unit again is ignored.
    let r = record(0);
    let e = usage::event_for_record(&gw.app.store, &r, &cx);
    assert_eq!(gw.app.store.record_usage(&[e]).unwrap(), 0);

    assert_eq!(
        gw.app.store.requests(5000).len(),
        1000,
        "request log is capped"
    );
    let v = wait_units(&gw, "?window=all", 1005).await;
    assert_eq!(
        v["totals"]["units"]["total"], 1005,
        "ledger keeps every unit"
    );
    assert_eq!(v["totals"]["tokens"]["input"], 10_050);
    assert_eq!(v["window"]["granularity"], "month");
    assert_eq!(v["coverage"]["complete_for_window"], true);

    let gw = gw.restart().await;
    let v = wait_units(&gw, "?window=7d", 1005).await;
    assert_eq!(v["totals"]["units"]["total"], 1005, "survives restart");
    let days = v["series"].as_array().unwrap().len();
    assert!(
        (7..=8).contains(&days),
        "rolling 7 days spans 7 or 8 UTC days: {days}"
    );
    drop(dir);
}

#[tokio::test]
async fn requests_from_before_the_ledger_are_reported_as_partial_coverage() {
    let (_d, gw) = setup().await;
    // Older versions wrote request rows but no ledger units.
    let mut old = record(1);
    old.timestamp = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    gw.app.store.record(&old).unwrap();
    let (_, v) = usage_json(&gw, "?window=all").await;
    assert_eq!(v["coverage"]["complete_for_window"], false);
    assert_eq!(v["coverage"]["excluded_legacy_requests"], 1);
    assert!(
        v["totals"]["units"]["total"] == 0,
        "legacy rows are not invented into usage"
    );

    let up = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("r", openai_usage(1, 0, 1, 0)),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    let (_, key) = gw.create_key("k").await;
    gw.post("/v1/responses", &key, json!({"model":"m","input":"x"}))
        .await;
    let v = wait_units(&gw, "?window=all", 1).await;
    assert_eq!(v["coverage"]["excluded_legacy_requests"], 1);
    assert!(
        v["coverage"]["message"]
            .as_str()
            .unwrap()
            .contains("1 older requests")
    );
    let (_, day) = usage_json(&gw, "?window=24h").await;
    assert_eq!(
        day["coverage"]["complete_for_window"], false,
        "window starts before the ledger"
    );
}

#[tokio::test]
async fn query_bounds_and_validation() {
    let (_d, gw) = setup().await;
    for (q, needle) in [
        ("?window=90d", "window"),
        ("?from=2026-01-01&to=2027-01-02", "366"),
        ("?from=2026-02-01&to=2026-01-01", "before"),
        ("?from=2026-02-01", "both"),
        ("?from=yesterday&to=today", "YYYY-MM-DD"),
        ("?source=everything", "source"),
    ] {
        let (status, v) = usage_json(&gw, q).await;
        assert_eq!(status, 400, "{q}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains(needle),
            "{q}: {v}"
        );
    }
    let long = "m".repeat(201);
    assert_eq!(usage_json(&gw, &format!("?model={long}")).await.0, 400);
    let (status, v) = usage_json(&gw, "?from=2026-01-01&to=2026-12-31").await;
    assert_eq!(status, 200);
    assert_eq!(v["series"].as_array().unwrap().len(), 365);
    assert_eq!(v["window"]["granularity"], "day");
    let (_, v) = usage_json(&gw, "?window=30d").await;
    let n = v["series"].as_array().unwrap().len();
    assert!((30..=31).contains(&n), "{n}");
}

// ---------------------------------------------------------------------------------------------
// External sources and overlap
// ---------------------------------------------------------------------------------------------

fn external(id: &str, disjoint: bool) -> ExternalUsage {
    ExternalUsage {
        collector: "native_codex".into(),
        native_id: id.into(),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        provider: "openai".into(),
        model: "gpt-6.1-sol".into(),
        account_label: Some("Codex on this Mac".into()),
        client_id: None,
        client_name: Some("Codex CLI".into()),
        billing: "subscription".into(),
        tokens: Tokens {
            input: Some(1000),
            cache_read: Some(0),
            cache_write: Some(0),
            output: Some(100),
            reasoning: Some(0),
            ..Tokens::default()
        },
        estimated_cost_micros: None,
        reported_cost_micros: None,
        disjoint,
    }
}

#[tokio::test]
async fn external_usage_stays_separate_unless_proven_disjoint() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("resp_gw_1", openai_usage(10, 0, 1, 0)),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["gpt-6.1-sol"])
        .await;
    let (_, key) = gw.create_key("k").await;
    gw.post(
        "/v1/responses",
        &key,
        json!({"model":"gpt-6.1-sol","input":"x"}),
    )
    .await;
    wait_units(&gw, "", 1).await;
    assert_eq!(
        usage::known_response_ids(&gw.app.store, &["resp_gw_1".into(), "resp_native".into()]),
        vec!["resp_gw_1".to_string()],
        "collectors can exclude native rows the gateway already counted"
    );

    let store = &gw.app.store;
    assert_eq!(
        usage::record_external(store, &[external("n1", false), external("n1", false)]).unwrap(),
        1,
        "idempotent"
    );
    let (_, gateway) = usage_json(&gw, "").await;
    assert_eq!(
        gateway["totals"]["units"]["total"], 1,
        "default view is gateway only"
    );
    let (_, ext) = usage_json(&gw, "?source=external").await;
    assert_eq!(ext["totals"]["units"]["total"], 1);
    assert_eq!(
        ext["by_client"][0]["client_key_id"],
        "external:native_codex"
    );
    assert_eq!(ext["by_client"][0]["origin"], "external");
    assert_eq!(ext["by_account"][0]["connection_name"], "Codex on this Mac");
    assert!(
        ext["by_account"][0]["connection_id"].is_null(),
        "never attributed to a gateway account"
    );

    let (_, all) = usage_json(&gw, "?source=all").await;
    assert_eq!(all["combined"], false);
    assert!(all["totals"].is_null(), "possible overlap: no summed total");
    assert_eq!(all["coverage"]["overlap"], "possible");
    assert_eq!(all["by_source"].as_array().unwrap().len(), 2);
    assert!(
        all["series"][23]["metrics"].is_null()
            && all["series"][23]["by_source"]["gateway"].is_object()
    );
    assert!(!all["warnings"].as_array().unwrap().is_empty());
    assert!(
        all["by_model"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["source"].is_string())
    );

    // Proven-disjoint external usage can be combined.
    let (_d2, gw2) = setup().await;
    usage::record_external(&gw2.app.store, &[external("n2", true)]).unwrap();
    let (_, all) = usage_json(&gw2, "?source=all").await;
    assert_eq!(all["combined"], true);
    assert_eq!(all["totals"]["units"]["total"], 1);
    assert_eq!(all["coverage"]["overlap"], "deduplicated");
}

// ---------------------------------------------------------------------------------------------
// Client attribution
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn units_are_attributed_to_the_client_key_public_identity() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(json_upstream(
        "/responses",
        200,
        responses_body("r", openai_usage(1, 0, 1, 0)),
    ))
    .await;
    gw.connection("A", "openai", &up.base(), &["m"]).await;
    // The proxy handler behind a middleware that attaches the identity, as client_auth does.
    let router = Router::new()
        .route("/v1/responses", post(switchyard::proxy::responses))
        .layer(middleware::from_fn(
            |mut req: Request, next: Next| async move {
                req.extensions_mut().insert(ClientIdentity {
                    id: "key-123".into(),
                    name: "Cursor".into(),
                });
                next.run(req).await
            },
        ))
        .with_state(gw.app.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, router).await;
    });
    let r = gw
        .http
        .post(format!("{base}/v1/responses"))
        .json(&json!({"model":"m","input":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // The dashboard playground is attributed to "playground".
    let r = gw
        .admin_send(
            reqwest::Method::POST,
            "/api/playground",
            json!({"model":"m","input":"x"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let v = wait_units(&gw, "", 2).await;
    let mut clients: Vec<(String, String)> = v["by_client"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["client_key_id"].as_str().unwrap().into(),
                c["client_key_name"].as_str().unwrap().into(),
            )
        })
        .collect();
    clients.sort();
    assert_eq!(
        clients,
        vec![
            ("key-123".into(), "Cursor".into()),
            ("playground".into(), "Playground".into())
        ]
    );
    let (_, one) = usage_json(&gw, "?client_key_id=key-123").await;
    assert_eq!(one["totals"]["units"]["total"], 1);
    assert_eq!(
        one["facets"]["clients"].as_array().unwrap().len(),
        2,
        "facets ignore the client filter"
    );
}

// ---------------------------------------------------------------------------------------------
// WebSocket turns
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
                    let id = format!("resp_turn_{n}");
                    let done = json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[],
                        "usage":openai_usage(100 * n, 0, 10 * n, 0)}});
                    let mut frames = vec![json!({"type":"response.created","response":{"id":id}})];
                    if v["mode"] != "hang" {
                        frames.push(json!({"type":"response.output_text.delta","delta":"hi"}));
                        frames.push(done.clone());
                        if v["mode"] == "duplicate" {
                            frames.push(done); // the same terminal twice
                        }
                    }
                    for f in frames {
                        let _ = socket.send(AxMessage::Text(f.to_string().into())).await;
                    }
                }
            })
            .into_response()
        }),
    )
}
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn connect(gw: &Gateway, key: &str) -> Client {
    let mut req = gw.ws_url("/v1/responses").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}
async fn turn(ws: &mut Client, extra: Value) -> Vec<Value> {
    let mut create = json!({"type":"response.create","model":"gpt-6.1-sol","input":"x"});
    for (k, v) in extra.as_object().unwrap() {
        create[k] = v.clone();
    }
    ws.send(Message::Text(create.to_string().into()))
        .await
        .unwrap();
    let mut got = Vec::new();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap();
        if let Some(Ok(Message::Text(t))) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            let done = v["type"] == "response.completed";
            got.push(v);
            if done || extra["mode"] == "hang" {
                return got;
            }
        }
    }
}

#[tokio::test]
async fn websocket_turns_are_separate_units_and_duplicates_count_once() {
    let (_d, gw) = setup().await;
    let up = Upstream::start(ws_upstream()).await;
    gw.add_connection(json!({"name":"WS","kind":"openai","base_url":up.base(),"models":["gpt-6.1-sol"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = connect(&gw, &key).await;
    turn(&mut ws, json!({})).await; // turn 1: 100 in / 10 out
    turn(&mut ws, json!({"mode":"duplicate"})).await; // turn 2: 200 / 20, terminal sent twice
    tokio::time::sleep(Duration::from_millis(100)).await; // let the duplicate arrive
    turn(&mut ws, json!({"mode":"hang"})).await; // turn 3: never completes
    ws.close(None).await.unwrap();

    let v = wait_units(&gw, "", 3).await;
    let t = &v["totals"];
    assert_eq!(
        t["units"],
        json!({"total":3,"succeeded":2,"failed":0,"cancelled":1}),
        "{t}"
    );
    assert_eq!(
        t["tokens"]["input"], 300,
        "the repeated terminal did not double count"
    );
    assert_eq!(t["tokens"]["output"], 30);
    assert_eq!(
        t["attempts"]["total"], 1,
        "the handshake belongs to turn 1 only"
    );
    // $2 × 300 + $10 × 30 per MTok = 0.0009 USD.
    assert_eq!(t["cost"]["estimated_micros"], 900);
    assert_eq!(t["first_token_ms"]["samples"], 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, again) = usage_json(&gw, "").await;
    assert_eq!(
        again["totals"]["units"]["total"], 3,
        "nothing extra on session end"
    );

    // The session request record keeps its first-turn semantics: one row per session.
    let log = gw.wait_for_log(1).await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["transport"], "websocket");
    assert_eq!(
        log[0]["status"], 499,
        "the session ended with a turn in flight"
    );
}

#[tokio::test]
async fn websocket_handshake_failure_is_one_failed_unit() {
    let (_d, gw) = setup().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    gw.add_connection(json!({"name":"Gone","kind":"openai","base_url":dead,"models":["gpt-6.1-sol"],"supports_websocket":true,"api_key":PROVIDER_KEY})).await;
    let (_, key) = gw.create_key("k").await;
    let mut ws = connect(&gw, &key).await;
    ws.send(Message::Text(
        json!({"type":"response.create","model":"gpt-6.1-sol","input":"x"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    while let Ok(Some(Ok(_))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {}
    let v = wait_units(&gw, "", 1).await;
    assert_eq!(v["totals"]["units"]["failed"], 1);
    assert_eq!(v["totals"]["attempts"]["failed"], 1);
    assert_eq!(v["totals"]["tokens"]["usage_missing_units"], 1);
    assert!(v["totals"]["cost"]["estimated_micros"].is_null());
}

#[test]
fn older_rows_and_events_deserialize() {
    let old: RequestRecord = serde_json::from_value(
        json!({"id":"r","timestamp":"2026-01-01T00:00:00Z","model":"m",
        "connection_id":"c","connection_name":"C","transport":"http","status":200,"latency_ms":1,
        "input_tokens":null,"output_tokens":null,"error":null}),
    )
    .unwrap();
    assert!(old.attempts.is_empty());
    // A ledger unit written before response ids and audio tokens were recorded.
    let back: usage::UsageEvent = serde_json::from_value(json!({
        "event_id":"r","ts_ms":0,"source":"gateway","request_id":"r","turn":null,"transport":"http",
        "connection_id":"c","connection_name":"C","provider":"openai","billing":"api_key",
        "requested_model":"m","route":null,"model":"m","client_key_id":null,"client_key_name":null,
        "status":200,"outcome":"succeeded","error":null,"attempts":1,"failed_attempts":0,"failovers":0,
        "latency_ms":1,"ttfb_ms":null,"first_token_ms":null,
        "tokens":{"input":1,"cache_read":null,"cache_write":null,"cache_write_5m":null,"cache_write_1h":null,"output":1,"reasoning":null},
        "cost_micros":null,"pricing_version":null,"unpriced_reason":"unknown_model","reported_cost_micros":null
    }))
    .unwrap();
    assert!(back.response_id.is_none() && back.tokens.audio_input.is_none());
}

#[test]
fn a_repeated_terminal_never_closes_a_later_turn() {
    use usage::WsTurns;
    let done = |id: &str, input: u64| json!({"type":"response.completed","response":{"id":id,"usage":openai_usage(input, 0, 1, 0)}});
    let mut t = WsTurns::new();
    t.frame(
        &json!({"type":"response.created","response":{"id":"r1"}}),
        "openai",
    );
    t.frame(&done("r1", 10), "openai");
    let first = t.close(200);
    assert_eq!(first.len(), 1);
    assert_eq!((first[0].n, first[0].usage.tokens.input), (1, Some(10)));

    t.begin();
    t.frame(
        &json!({"type":"response.created","response":{"id":"r2"}}),
        "openai",
    );
    t.frame(&done("r1", 10), "openai"); // the same terminal again, while turn 2 is open
    assert!(t.close(200).is_empty(), "duplicate closes nothing");
    assert!(t.has_open(), "turn 2 is still open");
    t.frame(&done("r2", 20), "openai");
    let second = t.close(200);
    assert_eq!(
        (
            second[0].n,
            second[0].response_id.as_deref(),
            second[0].usage.tokens.input
        ),
        (2, Some("r2"), Some(20))
    );

    // A session-level end closes every open turn with its partial usage.
    t.begin();
    t.begin();
    t.frame(
        &json!({"type":"response.created","response":{"id":"r3"}}),
        "openai",
    );
    let rest = t.close(499);
    assert_eq!(
        rest.iter().map(|c| (c.n, c.status)).collect::<Vec<_>>(),
        vec![(3, 499), (4, 499)]
    );
    assert!(!t.has_open());
}
