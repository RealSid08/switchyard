//! Real asynchronous import APIs using private fixtures; never native user data.
mod support;
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use support::Gateway;

async fn import(gw: &Gateway, source: &str, path: &Path) {
    let r = gw
        .http
        .post(gw.url("/api/usage/native/import"))
        .bearer_auth(&gw.admin)
        .json(&json!({"source":source,"path":path}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
}
async fn complete(gw: &Gateway) -> Value {
    for _ in 0..200 {
        let v = gw.admin_json("/api/usage/native").await;
        if v["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["enabled"] == true)
            .all(|s| s["status"] == "complete")
            && v["job"]["running"] == false
        {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "imports did not drain: {}",
        gw.admin_json("/api/usage/native").await
    );
}
fn line(v: Value) -> String {
    format!("{v}\n")
}

#[tokio::test]
async fn queued_sources_drain_read_only_and_reimports_never_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fixtures");
    fs::create_dir_all(root.join("sessions")).unwrap();
    fs::create_dir_all(root.join("projects")).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let ts = chrono::Utc::now().timestamp_millis();
    let codex = line(
        json!({"type":"session_meta","payload":{"id":"session-a","model_provider":"openai"}}),
    ) + &line(json!({"type":"turn_context","payload":{"model":"unknown"}}))
        + &line(
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":12},"last_token_usage":{"input_tokens":10,"output_tokens":2}}}}),
        );
    let claude = line(
        json!({"type":"assistant","timestamp":now,"requestId":"request-a","message":{"id":"msg-a","model":"unknown","content":[{"text":"PRIVATE-PROMPT-MUST-NOT-BE-STORED"}],"usage":{"input_tokens":20,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    );
    let codex_path = root.join("sessions/a.jsonl");
    let claude_path = root.join("projects/a.jsonl");
    fs::write(&codex_path, &codex).unwrap();
    fs::write(&claude_path, claude.clone() + &claude).unwrap();
    let db_path = root.join("opencode.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("CREATE TABLE session_message(id TEXT PRIMARY KEY,type TEXT,time_created INTEGER,time_updated INTEGER,data TEXT)").unwrap();
    let data = json!({"model":{"providerID":"opencode-go","id":"unknown"},"tokens":{"input":30,"output":4,"reasoning":2,"cache":{"read":0,"write":0}},"time":{"created":ts,"completed":ts+1},"cost":0.125,"content":"PRIVATE-PROMPT-MUST-NOT-BE-STORED"});
    db.execute(
        "INSERT INTO session_message VALUES('message-a','assistant',?1,?1,?2)",
        rusqlite::params![ts, data.to_string()],
    )
    .unwrap();
    drop(db);
    let db_before = fs::read(&db_path).unwrap();
    fs::create_dir_all(dir.path().join("gateway")).unwrap();
    let gw = Gateway::start(&dir.path().join("gateway")).await;
    for source in ["codex", "claude", "opencode"] {
        import(&gw, source, &root).await;
    }
    let status = complete(&gw).await;
    assert!(
        !status
            .to_string()
            .contains(&root.to_string_lossy().to_string())
    );
    assert_eq!(
        status["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["imported_events"].as_u64().unwrap())
            .sum::<u64>(),
        3
    );
    assert_eq!(fs::read_to_string(&codex_path).unwrap(), codex);
    assert_eq!(
        fs::read_to_string(&claude_path).unwrap(),
        claude.clone() + &claude
    );
    assert_eq!(fs::read(&db_path).unwrap(), db_before);
    let summary = gw.admin_json("/api/usage?window=24h&source=external").await;
    assert_eq!(summary["totals"]["units"]["total"], 3);
    assert_eq!(summary["totals"]["units"]["unknown"], 3);
    assert_eq!(summary["totals"]["tokens"]["total"], 71);
    assert_eq!(
        summary["totals"]["cost"]["subscription_equivalent_micros"],
        125_000
    );
    assert!(summary["totals"]["cost"]["reported_micros"].is_null());
    for source in ["codex", "claude", "opencode"] {
        import(&gw, source, &root).await;
    }
    complete(&gw).await;
    assert_eq!(
        gw.admin_json("/api/usage?window=24h&source=external").await["totals"]["units"]["total"],
        3
    );
    let gw = gw.restart().await;
    for source in ["codex", "claude", "opencode"] {
        import(&gw, source, &root).await;
    }
    complete(&gw).await;
    assert_eq!(
        gw.admin_json("/api/usage?window=24h&source=external").await["totals"]["units"]["total"],
        3
    );
    let r = gw
        .http
        .delete(gw.url("/api/usage/native/claude"))
        .bearer_auth(&gw.admin)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let status = gw.admin_json("/api/usage/native").await;
    assert_eq!(
        status["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "claude")
            .unwrap()["enabled"],
        false
    );
    // Removal disables future reads, preserving already-imported accounting.
    assert_eq!(
        gw.admin_json("/api/usage?window=24h&source=external").await["totals"]["units"]["total"],
        3
    );
}

#[tokio::test]
async fn removing_an_inflight_import_prevents_later_ledger_writes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("sessions")).unwrap();
    fs::create_dir_all(dir.path().join("gateway")).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let mut contents = line(
        json!({"type":"session_meta","payload":{"id":"cancel-session","model_provider":"openai"}}),
    );
    contents += &line(json!({"type":"turn_context","payload":{"model":"unknown"}}));
    for n in 1..=50_000u64 {
        contents += &line(
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":n},"last_token_usage":{"input_tokens":1,"output_tokens":0}}}}),
        );
    }
    fs::write(dir.path().join("sessions/cancel.jsonl"), contents).unwrap();
    let gw = Gateway::start(&dir.path().join("gateway")).await;
    import(&gw, "codex", dir.path()).await;
    assert_eq!(
        gw.admin_json("/api/usage/native").await["job"]["running"],
        true
    );
    let r = gw
        .http
        .delete(gw.url("/api/usage/native/codex"))
        .bearer_auth(&gw.admin)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let count =
        gw.admin_json("/api/usage?window=24h&source=external").await["totals"]["units"]["total"]
            .clone();
    complete(&gw).await;
    assert_eq!(
        gw.admin_json("/api/usage?window=24h&source=external").await["totals"]["units"]["total"],
        count
    );
    let status = gw.admin_json("/api/usage/native").await;
    assert_eq!(
        status["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "codex")
            .unwrap()["enabled"],
        false
    );
}
