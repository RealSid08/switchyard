//! Authentication of the assembled usage API, including mutation routes.
mod support;

use reqwest::Method;
use serde_json::json;
use support::Gateway;

#[tokio::test]
async fn old_native_usage_remains_idempotent_after_retention_and_restart() {
    use switchyard::usage::{ExternalUsage, Tokens, record_external};
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let event = ExternalUsage {
        collector: "test-history".into(),
        native_id: "historical-provider-event".into(),
        ts_ms: chrono::Utc::now().timestamp_millis() - 401 * 86_400_000,
        provider: "unknown-provider".into(),
        model: "unknown-model".into(),
        account_label: None,
        client_id: None,
        client_name: None,
        billing: "unknown".into(),
        tokens: Tokens {
            input: Some(100),
            output: Some(10),
            ..Default::default()
        },
        estimated_cost_micros: None,
        reported_cost_micros: None,
        disjoint: false,
    };
    assert_eq!(
        record_external(&gw.app.store, std::slice::from_ref(&event)).unwrap(),
        1
    );
    assert_eq!(
        record_external(&gw.app.store, std::slice::from_ref(&event)).unwrap(),
        0
    );
    let gw = gw.restart().await;
    assert_eq!(record_external(&gw.app.store, &[event]).unwrap(), 0);
    let summary = gw.admin_json("/api/usage?window=all&source=external").await;
    assert_eq!(summary["totals"]["units"]["total"], 1);
    assert_eq!(summary["totals"]["tokens"]["input"], 100);
    assert_eq!(summary["totals"]["tokens"]["output"], 10);
}

#[tokio::test]
async fn usage_routes_share_the_admin_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let (_, client_key) = gw.create_key("not an administrator").await;
    let routes = [
        (Method::GET, "/api/usage?window=24h"),
        (Method::GET, "/api/usage/pricing"),
        (Method::PUT, "/api/usage/pricing/overrides"),
        (Method::GET, "/api/usage/sources"),
        (Method::POST, "/api/usage/refresh"),
        (Method::GET, "/api/usage/monitors"),
        (Method::POST, "/api/usage/monitors"),
        (Method::PUT, "/api/usage/monitors/absent"),
        (Method::DELETE, "/api/usage/monitors/absent"),
        (Method::POST, "/api/usage/import"),
    ];
    for (method, path) in routes {
        for credential in [None, Some(client_key.as_str())] {
            let mut request = gw
                .http
                .request(method.clone(), gw.url(path))
                .json(&json!({}));
            if let Some(key) = credential {
                request = request.bearer_auth(key);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                401,
                "{method} {path}"
            );
        }
        let response = gw
            .http
            .request(method.clone(), gw.url(path))
            .bearer_auth(&gw.admin)
            .header("origin", "https://untrusted.example")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{method} {path}");
        if method == Method::GET {
            assert_eq!(gw.admin_get(path).await.status(), 200, "GET {path}");
        }
    }
}

#[tokio::test]
async fn native_sdk_estimates_are_preserved_without_becoming_charges() {
    use switchyard::usage::{ExternalUsage, Tokens, record_external};
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let event = ExternalUsage {
        collector: "native_opencode".into(),
        native_id: "sdk-estimate".into(),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        provider: "opencode_go".into(),
        model: "a-model-with-no-official-card".into(),
        account_label: None,
        client_id: None,
        client_name: None,
        billing: "subscription".into(),
        tokens: Tokens {
            input: Some(100),
            output: Some(10),
            ..Default::default()
        },
        estimated_cost_micros: Some(125_000),
        reported_cost_micros: None,
        disjoint: false,
    };
    record_external(&gw.app.store, &[event]).unwrap();
    let summary = gw.admin_json("/api/usage?window=24h&source=external").await;
    assert_eq!(summary["totals"]["cost"]["estimated_micros"], 125_000);
    assert_eq!(
        summary["totals"]["cost"]["subscription_equivalent_micros"],
        125_000
    );
    assert!(summary["totals"]["cost"]["reported_micros"].is_null());
    assert_eq!(summary["totals"]["cost"]["priced_units"], 1);
}

#[tokio::test]
async fn native_unknown_billing_and_outcome_are_not_guessed() {
    use switchyard::usage::{ExternalUsage, Tokens, record_external};
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    for (id, billing, cost) in [
        ("unknown", "unknown", 200_000),
        ("api", "api_key", 100_000),
        ("plan", "subscription", 300_000),
    ] {
        let event = ExternalUsage {
            collector: "test-history".into(),
            native_id: id.into(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            provider: "codex".into(),
            model: "unknown-model".into(),
            account_label: None,
            client_id: None,
            client_name: None,
            billing: billing.into(),
            tokens: Tokens {
                input: Some(100),
                output: Some(10),
                ..Default::default()
            },
            estimated_cost_micros: Some(cost),
            reported_cost_micros: None,
            disjoint: false,
        };
        record_external(&gw.app.store, &[event]).unwrap();
    }
    let summary = gw.admin_json("/api/usage?window=24h&source=external").await;
    let totals = &summary["totals"];
    assert_eq!(totals["cost"]["estimated_micros"], 600_000);
    assert_eq!(totals["cost"]["api_estimated_micros"], 100_000);
    assert_eq!(totals["cost"]["subscription_equivalent_micros"], 300_000);
    assert_eq!(totals["cost"]["unknown_billing_micros"], 200_000);
    assert_eq!(totals["cost"]["unknown_billing_units"], 1);
    assert_eq!(totals["units"]["unknown"], 3);
    assert_eq!(totals["units"]["succeeded"], 0);
    assert!(totals["success_rate"].is_null());
    assert!(totals["cost"]["reported_micros"].is_null());
}

#[tokio::test]
async fn impossible_provider_counters_remain_unknown_without_poisoning_totals() {
    use switchyard::usage::{ExternalUsage, Tokens, record_external};
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    for id in ["a", "b"] {
        record_external(
            &gw.app.store,
            &[ExternalUsage {
                collector: "corrupt-history".into(),
                native_id: id.into(),
                ts_ms: chrono::Utc::now().timestamp_millis(),
                provider: "openai".into(),
                model: "gpt-6.1-sol".into(),
                account_label: None,
                client_id: None,
                client_name: None,
                billing: "unknown".into(),
                tokens: Tokens {
                    input: Some(u64::MAX),
                    output: Some(u64::MAX),
                    cache_read: Some(u64::MAX),
                    cache_write: Some(u64::MAX),
                    ..Default::default()
                },
                estimated_cost_micros: None,
                reported_cost_micros: None,
                disjoint: false,
            }],
        )
        .unwrap();
    }
    let s = gw.admin_json("/api/usage?window=24h&source=external").await;
    assert_eq!(s["totals"]["units"]["total"], 2);
    assert_eq!(s["totals"]["tokens"]["usage_missing_units"], 2);
    assert!(s["totals"]["cost"]["estimated_micros"].is_null());
}
